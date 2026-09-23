//! MOSS-TTS-Nano 的 ONNX 推理核心：`omnicrawl/tts/onnx_runtime.py` 的等价实现。
//!
//! 全部 session 的输入输出名与形状由模型自带的 meta JSON 驱动
//! （`tts_browser_onnx_meta.json` / `codec_browser_onnx_meta.json`），与 Python 侧
//! 共用同一套模型文件：prefill → local decoder/cached/fixed frame → decode step →
//! codec 全量/流式解码。
//!
//! 设备：本实现只处理 CPU（`CPUExecutionProvider`）；`device=cuda` 明确报错。

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::{Tensor, TensorValueType};
use serde_json::Value;

use crate::audio::AudioBuffer;
use crate::download::MANIFEST_CANDIDATE_RELATIVE_PATHS;
use crate::paths::resolve_lenient;
use crate::sampler::{
    apply_repetition_penalty, argmax_with_repetition_penalty, normalize_sample_mode,
    sample_from_scores, Pcg64, SAMPLE_MODE_FIXED, SAMPLE_MODE_GREEDY,
};

pub const DEVICE_AUTO: &str = "auto";
pub const EXECUTION_PROVIDER_CPU: &str = "cpu";
pub const EXECUTION_PROVIDER_CUDA: &str = "cuda";

/// 旧模型目录名的等价映射（与 Python `MODEL_DIR_ALIAS_MAP` 一致）。
const MODEL_DIR_ALIAS_MAP: [(&str, &str); 2] = [
    ("MOSS-TTS-Nano-ONNX-CPU", "MOSS-TTS-Nano-100M-ONNX"),
    (
        "MOSS-Audio-Tokenizer-Nano-ONNX-CPU",
        "MOSS-Audio-Tokenizer-Nano-ONNX",
    ),
];

/// 一次合成的请求行：文本 token 与音频槽组成 `[seq, n_vq + 1]`。
#[derive(Debug, Clone)]
pub struct RequestRows {
    pub input_ids: Vec<Vec<i32>>,
    pub attention_mask: Vec<Vec<i32>>,
}

/// 一次自回归生成的结果：全部音频帧 + 流式解码出的波形（非流式时为 `None`）。
#[derive(Debug, Clone)]
pub struct GeneratedAudio {
    pub generated_frames: Vec<Vec<i32>>,
    pub streamed_waveform: Option<AudioBuffer>,
}

/// 跨 step 传递的张量（形状 + 数据）。
#[derive(Debug, Clone, PartialEq)]
enum TensorData {
    F32(Vec<usize>, Vec<f32>),
    I32(Vec<usize>, Vec<i32>),
}

impl TensorData {
    fn f32(&self) -> (Vec<usize>, Vec<f32>) {
        match self {
            TensorData::F32(shape, data) => (shape.clone(), data.clone()),
            TensorData::I32(shape, data) => (
                shape.clone(),
                data.iter().map(|value| *value as f32).collect(),
            ),
        }
    }
}

/// 设备名归一化；`auto` 保留到探测阶段。
pub fn normalize_execution_provider(raw: &str) -> Result<String, String> {
    let normalized = if raw.trim().is_empty() {
        DEVICE_AUTO.to_string()
    } else {
        raw.trim().to_lowercase()
    };
    match normalized.as_str() {
        DEVICE_AUTO | EXECUTION_PROVIDER_CPU | EXECUTION_PROVIDER_CUDA => Ok(normalized),
        "cpuexecutionprovider" => Ok(EXECUTION_PROVIDER_CPU.to_string()),
        "gpu" | "cudaexecutionprovider" => Ok(EXECUTION_PROVIDER_CUDA.to_string()),
        _ => Err("device/execution_provider must be one of: auto, cpu, cuda".to_string()),
    }
}

/// 解析实际执行设备：`auto` / `cpu` 都落 CPU；`cuda` 尚未接入 Rust 实现。
pub fn resolve_execution_provider(device: &str) -> Result<String, String> {
    match normalize_execution_provider(device)?.as_str() {
        EXECUTION_PROVIDER_CUDA => Err(
            "CUDAExecutionProvider 尚未接入 Rust TTS 实现：请改用 device=cpu 或 auto。".to_string(),
        ),
        _ => Ok(EXECUTION_PROVIDER_CPU.to_string()),
    }
}

fn last_hidden(shape: &[usize], data: &[f32]) -> Result<Vec<f32>, String> {
    match shape {
        [1, _, hidden] | [1, hidden] => {
            let start = data.len().saturating_sub(*hidden);
            Ok(data[start..].to_vec())
        }
        other => Err(format!("Unexpected global_hidden shape: {other:?}")),
    }
}

fn extract_f32(
    outputs: &ort::session::SessionOutputs<'_>,
    name: &str,
) -> Result<TensorData, String> {
    let value = outputs
        .get(name)
        .ok_or_else(|| format!("模型输出缺少 {name}"))?;
    let tensor = value
        .downcast_ref::<TensorValueType<f32>>()
        .map_err(|error| format!("输出 {name} 不是 float32 张量：{error}"))?;
    let (shape, data) = tensor
        .try_extract_tensor::<f32>()
        .map_err(|error| format!("读取输出 {name} 失败：{error}"))?;
    Ok(TensorData::F32(
        shape.iter().map(|dim| *dim as usize).collect(),
        data.to_vec(),
    ))
}

fn extract_i32(
    outputs: &ort::session::SessionOutputs<'_>,
    name: &str,
) -> Result<(Vec<i32>, Vec<usize>), String> {
    let value = outputs
        .get(name)
        .ok_or_else(|| format!("模型输出缺少 {name}"))?;
    let tensor = value
        .downcast_ref::<TensorValueType<i32>>()
        .map_err(|error| format!("输出 {name} 不是 int32 张量：{error}"))?;
    let (shape, data) = tensor
        .try_extract_tensor::<i32>()
        .map_err(|error| format!("读取输出 {name} 失败：{error}"))?;
    Ok((
        data.to_vec(),
        shape.iter().map(|dim| *dim as usize).collect(),
    ))
}

fn extract_tensor_i32(
    outputs: &ort::session::SessionOutputs<'_>,
    name: &str,
) -> Result<TensorData, String> {
    let (data, shape) = extract_i32(outputs, name)?;
    Ok(TensorData::I32(shape, data))
}

/// codec 流式解码的跨帧状态（transformer 偏移与 attention cache）。
#[derive(Debug, Clone, Default)]
pub struct CodecStreamingState {
    transformer: Vec<(String, String, Vec<usize>)>,
    attention: Vec<AttentionCacheSpec>,
    feeds: HashMap<String, TensorData>,
}

#[derive(Debug, Clone)]
struct AttentionCacheSpec {
    offset_input_name: String,
    offset_output_name: String,
    offset_shape: Vec<usize>,
    keys_input_name: String,
    keys_output_name: String,
    values_input_name: String,
    values_output_name: String,
    positions_input_name: String,
    positions_output_name: String,
    cache_shape: Vec<usize>,
    positions_shape: Vec<usize>,
}

impl CodecStreamingState {
    fn from_meta(codec_meta: &Value) -> Self {
        let streaming = &codec_meta["streaming_decode"];
        let transformer = streaming["transformer_offsets"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        (
                            row["input_name"].as_str().unwrap_or_default().to_string(),
                            row["output_name"].as_str().unwrap_or_default().to_string(),
                            json_shape(&row["shape"]),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let attention = streaming["attention_caches"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| AttentionCacheSpec {
                        offset_input_name: row["offset_input_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        offset_output_name: row["offset_output_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        keys_input_name: row["cached_keys_input_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        keys_output_name: row["cached_keys_output_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        values_input_name: row["cached_values_input_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        values_output_name: row["cached_values_output_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        positions_input_name: row["cached_positions_input_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        positions_output_name: row["cached_positions_output_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        offset_shape: json_shape(&row["offset_shape"]),
                        cache_shape: json_shape(&row["cache_shape"]),
                        positions_shape: json_shape(&row["positions_shape"]),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut state = Self {
            transformer,
            attention,
            feeds: HashMap::new(),
        };
        state.reset();
        state
    }

    /// 重置所有状态张量：偏移清零、cache 清零、positions 置 -1。
    pub fn reset(&mut self) {
        self.feeds.clear();
        for (input_name, _output_name, shape) in &self.transformer {
            self.feeds.insert(
                input_name.clone(),
                TensorData::I32(shape.clone(), vec![0; shape_product(shape)]),
            );
        }
        for spec in &self.attention {
            self.feeds.insert(
                spec.offset_input_name.clone(),
                TensorData::I32(
                    spec.offset_shape.clone(),
                    vec![0; shape_product(&spec.offset_shape)],
                ),
            );
            self.feeds.insert(
                spec.keys_input_name.clone(),
                TensorData::F32(
                    spec.cache_shape.clone(),
                    vec![0.0; shape_product(&spec.cache_shape)],
                ),
            );
            self.feeds.insert(
                spec.values_input_name.clone(),
                TensorData::F32(
                    spec.cache_shape.clone(),
                    vec![0.0; shape_product(&spec.cache_shape)],
                ),
            );
            self.feeds.insert(
                spec.positions_input_name.clone(),
                TensorData::I32(
                    spec.positions_shape.clone(),
                    vec![-1; shape_product(&spec.positions_shape)],
                ),
            );
        }
    }

    fn append_feeds(
        &self,
        feeds: &mut Vec<(Cow<'static, str>, ort::session::SessionInputValue<'static>)>,
    ) -> Result<(), String> {
        for (name, data) in &self.feeds {
            feeds.push((Cow::Owned(name.clone()), tensor_for(data)?));
        }
        Ok(())
    }

    fn update(&mut self, outputs: &ort::session::SessionOutputs<'_>) -> Result<(), String> {
        for (input_name, output_name, _shape) in &self.transformer {
            let value = extract_tensor_i32(outputs, output_name)?;
            self.feeds.insert(input_name.clone(), value);
        }
        for spec in &self.attention {
            self.feeds.insert(
                spec.offset_input_name.clone(),
                extract_tensor_i32(outputs, &spec.offset_output_name)?,
            );
            self.feeds.insert(
                spec.keys_input_name.clone(),
                extract_f32(outputs, &spec.keys_output_name)?,
            );
            self.feeds.insert(
                spec.values_input_name.clone(),
                extract_f32(outputs, &spec.values_output_name)?,
            );
            self.feeds.insert(
                spec.positions_input_name.clone(),
                extract_tensor_i32(outputs, &spec.positions_output_name)?,
            );
        }
        Ok(())
    }
}

fn json_shape(value: &Value) -> Vec<usize> {
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row.as_u64().unwrap_or(0) as usize)
                .collect()
        })
        .unwrap_or_default()
}

fn shape_product(shape: &[usize]) -> usize {
    shape.iter().product()
}

fn tensor_for(data: &TensorData) -> Result<ort::session::SessionInputValue<'static>, String> {
    match data {
        TensorData::F32(shape, values) => Ok(Tensor::from_array((shape.clone(), values.clone()))
            .map_err(|error| format!("构造 float32 输入失败：{error}"))?
            .into()),
        TensorData::I32(shape, values) => Ok(Tensor::from_array((shape.clone(), values.clone()))
            .map_err(|error| format!("构造 int32 输入失败：{error}"))?
            .into()),
    }
}

fn tensor_i32(shape: Vec<usize>, values: Vec<i32>) -> Result<Tensor<i32>, String> {
    let shape_text = format!("{shape:?}");
    let length = values.len();
    Tensor::from_array((shape, values))
        .map_err(|error| format!("构造 int32 输入失败 {shape_text}（{length} 个元素）：{error}"))
}

fn tensor_f32(shape: Vec<usize>, values: Vec<f32>) -> Result<Tensor<f32>, String> {
    let shape_text = format!("{shape:?}");
    let length = values.len();
    Tensor::from_array((shape, values))
        .map_err(|error| format!("构造 float32 输入失败 {shape_text}（{length} 个元素）：{error}"))
}

/// 初始化 ONNX Runtime 环境，并把 C++ 日志级别提到 FATAL（只留致命错误）。
fn init_environment() -> Result<(), String> {
    let _ = ort::init().with_name("omnicrawl-tts").commit();
    let environment = ort::environment::Environment::current()
        .map_err(|error| format!("初始化 ONNX Runtime 环境失败：{error}"))?;
    environment.set_log_level(ort::logging::LogLevel::Fatal);
    Ok(())
}

/// MOSS-TTS-Nano ONNX 推理运行时（CPU）。
pub struct OrtRuntime {
    pub manifest: Value,
    pub manifest_dir: PathBuf,
    pub tts_meta: Value,
    pub codec_meta: Value,
    pub requested_device: String,
    pub execution_provider: String,
    sessions: HashMap<String, Session>,
    pub streaming: CodecStreamingState,
    pub rng: Pcg64,
}

impl OrtRuntime {
    /// 加载模型目录中的 manifest 与 meta，创建全部 ONNX session。
    pub fn new(
        model_dir: &Path,
        thread_count: usize,
        max_new_frames: Option<i64>,
        do_sample: Option<bool>,
        sample_mode: Option<&str>,
        execution_provider: &str,
    ) -> Result<Self, String> {
        init_environment()?;
        let model_dir = resolve_lenient(model_dir);
        let requested_device = normalize_execution_provider(execution_provider)?;
        let execution_provider = resolve_execution_provider(&requested_device)?;

        let manifest_path = Self::resolve_manifest_path(&model_dir)?;
        let manifest_dir = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| model_dir.clone());
        let mut manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path).map_err(|error| format!("{error}"))?,
        )
        .map_err(|error| format!("解析 {} 失败：{error}", manifest_path.display()))?;

        if max_new_frames.is_some() || do_sample.is_some() || sample_mode.is_some() {
            let defaults = manifest
                .get_mut("generation_defaults")
                .ok_or_else(|| "manifest 缺少 generation_defaults".to_string())?;
            if let Some(frames) = max_new_frames {
                defaults["max_new_frames"] = Value::from(frames);
            }
            if let Some(do_sample) = do_sample {
                defaults["do_sample"] = Value::from(do_sample);
            }
            let effective_do_sample = defaults["do_sample"].as_bool().unwrap_or(true);
            let raw_mode = sample_mode
                .map(str::to_string)
                .or_else(|| defaults["sample_mode"].as_str().map(str::to_string))
                .unwrap_or_default();
            let resolved_mode = normalize_sample_mode(&raw_mode, effective_do_sample);
            defaults["sample_mode"] = Value::from(resolved_mode.clone());
            defaults["do_sample"] = Value::from(resolved_mode != SAMPLE_MODE_GREEDY);
        }

        let mut runtime = Self {
            manifest,
            manifest_dir,
            tts_meta: Value::Null,
            codec_meta: Value::Null,
            requested_device,
            execution_provider,
            sessions: HashMap::new(),
            streaming: CodecStreamingState::default(),
            rng: Pcg64::seeded(1234),
        };

        let tts_meta_path = runtime.resolve_manifest_relative_path(
            runtime.manifest["model_files"]["tts_meta"]
                .as_str()
                .unwrap_or("tts_browser_onnx_meta.json"),
        );
        let codec_meta_path = runtime.resolve_manifest_relative_path(
            runtime.manifest["model_files"]["codec_meta"]
                .as_str()
                .unwrap_or_default(),
        );
        runtime.tts_meta = serde_json::from_str(
            &std::fs::read_to_string(&tts_meta_path).map_err(|error| format!("{error}"))?,
        )
        .map_err(|error| format!("解析 {} 失败：{error}", tts_meta_path.display()))?;
        runtime.codec_meta = serde_json::from_str(
            &std::fs::read_to_string(&codec_meta_path).map_err(|error| format!("{error}"))?,
        )
        .map_err(|error| format!("解析 {} 失败：{error}", codec_meta_path.display()))?;

        runtime.sessions =
            runtime.create_sessions(&tts_meta_path, &codec_meta_path, thread_count)?;
        runtime.streaming = CodecStreamingState::from_meta(&runtime.codec_meta);
        Ok(runtime)
    }

    fn resolve_manifest_path(model_dir: &Path) -> Result<PathBuf, String> {
        let tried: Vec<PathBuf> = MANIFEST_CANDIDATE_RELATIVE_PATHS
            .iter()
            .map(|relative| resolve_lenient(&model_dir.join(relative)))
            .collect();
        for candidate in &tried {
            if candidate.is_file() {
                return Ok(candidate.clone());
            }
        }
        Err(format!(
            "browser_poc_manifest.json not found. tried: {}",
            tried
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// 解析 manifest 中的相对路径；命中旧目录名时回退到等价的规范目录。
    pub fn resolve_manifest_relative_path(&self, relative_path: &str) -> PathBuf {
        let relative = PathBuf::from(relative_path);
        let resolved = resolve_lenient(&self.manifest_dir.join(&relative));
        if resolved.exists() {
            return resolved;
        }
        let text = relative_path.replace('\\', "/");
        for (legacy, canonical) in MODEL_DIR_ALIAS_MAP {
            if !format!("/{text}/").contains(&format!("/{legacy}/")) {
                continue;
            }
            let rewritten =
                resolve_lenient(&self.manifest_dir.join(text.replace(legacy, canonical)));
            if rewritten.exists() {
                return rewritten;
            }
        }
        resolved
    }

    fn create_sessions(
        &self,
        tts_meta_path: &Path,
        codec_meta_path: &Path,
        thread_count: usize,
    ) -> Result<HashMap<String, Session>, String> {
        let tts_dir = tts_meta_path.parent().unwrap_or(Path::new("."));
        let codec_dir = codec_meta_path.parent().unwrap_or(Path::new("."));
        let tts_files = &self.tts_meta["files"];
        let codec_files = &self.codec_meta["files"];

        let mut sessions: HashMap<String, Session> = HashMap::new();
        for (name, key) in [
            ("prefill", "prefill"),
            ("decode", "decode_step"),
            ("local_decoder", "local_decoder"),
            ("local_greedy_frame", "local_greedy_frame"),
            ("local_fixed_sampled_frame", "local_fixed_sampled_frame"),
            ("local_cached_step", "local_cached_step"),
        ] {
            let Some(file_name) = tts_files[key].as_str() else {
                continue;
            };
            let path = tts_dir.join(file_name);
            sessions.insert(name.to_string(), open_session(&path, thread_count)?);
        }
        sessions.insert(
            "codec_encode".to_string(),
            open_session(
                &codec_dir.join(codec_files["encode"].as_str().unwrap_or_default()),
                thread_count,
            )?,
        );
        sessions.insert(
            "codec_decode".to_string(),
            open_session(
                &codec_dir.join(codec_files["decode_full"].as_str().unwrap_or_default()),
                thread_count,
            )?,
        );
        sessions.insert(
            "codec_decode_step".to_string(),
            open_session(
                &codec_dir.join(codec_files["decode_step"].as_str().unwrap_or_default()),
                thread_count,
            )?,
        );
        Ok(sessions)
    }

    // ------------------------------------------------------------------
    // 模型元数据
    // ------------------------------------------------------------------

    pub fn sample_rate(&self) -> u32 {
        self.codec_meta["codec_config"]["sample_rate"]
            .as_u64()
            .unwrap_or(48000) as u32
    }

    pub fn channels(&self) -> usize {
        self.codec_meta["codec_config"]["channels"]
            .as_u64()
            .unwrap_or(2) as usize
    }

    pub fn n_vq(&self) -> usize {
        self.manifest["tts_config"]["n_vq"].as_u64().unwrap_or(16) as usize
    }

    fn audio_pad_token_id(&self) -> i32 {
        self.manifest["tts_config"]["audio_pad_token_id"]
            .as_i64()
            .unwrap_or(1024) as i32
    }

    fn audio_user_slot_token_id(&self) -> i32 {
        self.manifest["tts_config"]["audio_user_slot_token_id"]
            .as_i64()
            .unwrap_or(8) as i32
    }

    fn audio_assistant_slot_token_id(&self) -> i32 {
        self.manifest["tts_config"]["audio_assistant_slot_token_id"]
            .as_i64()
            .unwrap_or(9) as i32
    }

    fn audio_start_token_id(&self) -> i32 {
        self.manifest["tts_config"]["audio_start_token_id"]
            .as_i64()
            .unwrap_or(6) as i32
    }

    fn audio_end_token_id(&self) -> i32 {
        self.manifest["tts_config"]["audio_end_token_id"]
            .as_i64()
            .unwrap_or(7) as i32
    }

    fn audio_codebook_size(&self) -> usize {
        self.tts_meta["model_config"]["audio_codebook_sizes"]
            .as_array()
            .and_then(|sizes| sizes.first())
            .and_then(Value::as_u64)
            .unwrap_or(1024) as usize
    }

    fn default_i64(&self, key: &str) -> i64 {
        self.manifest["generation_defaults"][key]
            .as_i64()
            .unwrap_or(0)
    }

    fn default_f64(&self, key: &str) -> f64 {
        self.manifest["generation_defaults"][key]
            .as_f64()
            .unwrap_or(0.0)
    }

    fn default_bool(&self, key: &str) -> bool {
        self.manifest["generation_defaults"][key]
            .as_bool()
            .unwrap_or(false)
    }

    fn default_str(&self, key: &str) -> String {
        self.manifest["generation_defaults"][key]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// 用配置里的采样参数覆盖 manifest 的 `generation_defaults`。
    pub fn apply_generation_defaults(&mut self, overrides: &Value) -> Result<(), String> {
        let Some(map) = overrides.as_object() else {
            return Ok(());
        };
        let defaults = self
            .manifest
            .get_mut("generation_defaults")
            .ok_or_else(|| "manifest 缺少 generation_defaults".to_string())?;
        for (key, value) in map {
            defaults[key.as_str()] = value.clone();
        }
        Ok(())
    }

    pub fn builtin_voices(&self) -> Vec<Value> {
        self.manifest["builtin_voices"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn text_samples(&self) -> Vec<Value> {
        self.manifest["text_samples"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// 预热全部 session（prefill / decode / local / codec），降低首次合成延迟。
    pub fn warmup(&mut self) -> Result<(), String> {
        let voices = self.builtin_voices();
        let samples = self.text_samples();
        let Some(voice) = voices.first() else {
            return Ok(());
        };
        let Some(sample) = samples.first() else {
            return Ok(());
        };
        let prompt_codes = json_codes(&voice["prompt_audio_codes"]);
        let text_token_ids = json_ids(&sample["text_token_ids"]);
        let request = self.build_voice_clone_request_rows(&prompt_codes, &text_token_ids);
        let prefill = self.run_prefill(&request)?;
        let hidden = prefill.hidden;
        let empty_sets: Vec<HashSet<i32>> = (0..self.n_vq()).map(|_| HashSet::new()).collect();
        if self.sessions.contains_key("local_cached_step") {
            let mut past = self.create_empty_local_cached_past();
            let (_, _, next) = self.run_local_cached_step(&hidden, 0, 0, 0, 0, 0, &past)?;
            past = next;
            let _ = past;
        }
        if self.sessions.contains_key("local_fixed_sampled_frame")
            && self.default_str("sample_mode") == SAMPLE_MODE_FIXED
        {
            let _ = self.run_local_fixed_sampled_frame(&hidden, &empty_sets)?;
        } else if self.sessions.contains_key("local_greedy_frame")
            && !self.default_bool("do_sample")
        {
            let _ = self.run_local_greedy_frame(
                &hidden,
                &empty_sets,
                self.default_f64("audio_repetition_penalty"),
            )?;
        } else {
            let _ = self.run_local_decoder(&hidden, self.audio_assistant_slot_token_id(), &[])?;
        }

        let empty_frames = vec![vec![0i32; self.n_vq()]];
        let _ = self.decode_full_audio(&empty_frames)?;
        self.streaming.reset();
        let _ = self.run_codec_streaming_frames(&empty_frames)?;
        self.streaming.reset();
        Ok(())
    }

    // ------------------------------------------------------------------
    // 请求行构造
    // ------------------------------------------------------------------

    pub fn build_text_rows(&self, token_ids: &[i32]) -> Vec<Vec<i32>> {
        let row_width = self.n_vq() + 1;
        let audio_pad = self.audio_pad_token_id();
        token_ids
            .iter()
            .map(|token_id| {
                let mut row = vec![audio_pad; row_width];
                row[0] = *token_id;
                row
            })
            .collect()
    }

    pub fn build_audio_prefix_rows(
        &self,
        prompt_audio_codes: &[Vec<i32>],
        slot_token_id: Option<i32>,
    ) -> Vec<Vec<i32>> {
        let row_width = self.n_vq() + 1;
        let audio_pad = self.audio_pad_token_id();
        let slot = slot_token_id.unwrap_or_else(|| self.audio_user_slot_token_id());
        prompt_audio_codes
            .iter()
            .map(|codes| {
                let mut row = vec![audio_pad; row_width];
                row[0] = slot;
                for (index, code) in codes.iter().take(self.n_vq()).enumerate() {
                    row[index + 1] = *code;
                }
                row
            })
            .collect()
    }

    pub fn build_voice_clone_request_rows(
        &self,
        prompt_audio_codes: &[Vec<i32>],
        text_token_ids: &[i32],
    ) -> RequestRows {
        let prefix: Vec<i32> = self.manifest["prompt_templates"]["user_prompt_prefix_token_ids"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| row.as_i64().unwrap_or(0) as i32)
                    .collect::<Vec<i32>>()
            })
            .unwrap_or_default()
            .into_iter()
            .chain(std::iter::once(self.audio_start_token_id()))
            .collect();
        let mut suffix: Vec<i32> = vec![self.audio_end_token_id()];
        suffix.extend(
            self.manifest["prompt_templates"]["user_prompt_after_reference_token_ids"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .map(|row| row.as_i64().unwrap_or(0) as i32)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        suffix.extend_from_slice(text_token_ids);
        suffix.extend(
            self.manifest["prompt_templates"]["assistant_prompt_prefix_token_ids"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .map(|row| row.as_i64().unwrap_or(0) as i32)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        suffix.push(self.audio_start_token_id());

        let mut input_ids = self.build_text_rows(&prefix);
        input_ids.extend(self.build_audio_prefix_rows(prompt_audio_codes, None));
        input_ids.extend(self.build_text_rows(&suffix));
        let attention_mask = vec![vec![1i32; input_ids.len()]];
        RequestRows {
            input_ids,
            attention_mask,
        }
    }

    // ------------------------------------------------------------------
    // 单步推理
    // ------------------------------------------------------------------

    fn run_prefill(&mut self, request: &RequestRows) -> Result<PrefillOutput, String> {
        let row_width = self.n_vq() + 1;
        let ids: Vec<i32> = request.input_ids.iter().flatten().copied().collect();
        let ids = tensor_i32(vec![1, request.input_ids.len(), row_width], ids)?;
        let mask: Vec<i32> = request.attention_mask.iter().flatten().copied().collect();
        let mask = tensor_i32(
            vec![1, request.attention_mask.first().map(Vec::len).unwrap_or(0)],
            mask,
        )?;

        let output_names = self.output_names("prefill_output_names", 1)?;
        let (hidden, past) = {
            let session = self
                .sessions
                .get_mut("prefill")
                .ok_or_else(|| "prefill session 未创建".to_string())?;
            let outputs = session
                .run(ort::inputs!["input_ids" => ids, "attention_mask" => mask])
                .map_err(|error| format!("prefill 推理失败：{error}"))?;

            let (hidden_shape, hidden_data) = extract_f32(&outputs, "global_hidden")?.f32();
            let hidden = last_hidden(&hidden_shape, &hidden_data)?;
            let mut past: HashMap<String, TensorData> = HashMap::new();
            for name in &output_names {
                past.insert(
                    name.replace("present_", "past_"),
                    extract_f32(&outputs, name)?,
                );
            }
            (hidden, past)
        };
        let past_valid_length: i64 = request.attention_mask[0]
            .iter()
            .map(|value| *value as i64)
            .sum();
        Ok(PrefillOutput {
            hidden,
            past,
            past_valid_length,
        })
    }

    fn output_names(&self, key: &str, skip: usize) -> Result<Vec<String>, String> {
        let names: Vec<String> = self.tts_meta["onnx"][key]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .skip(skip)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            return Err(format!("tts meta 缺少 onnx.{key}"));
        }
        Ok(names)
    }

    fn run_local_decoder(
        &mut self,
        global_hidden: &[f32],
        text_token_id: i32,
        frame_prefix: &[i32],
    ) -> Result<(Vec<f32>, TensorData), String> {
        let n_vq = self.n_vq();
        let audio_pad = self.audio_pad_token_id();
        let mut prefix = vec![audio_pad; n_vq - 1];
        for (index, token) in frame_prefix.iter().take(n_vq - 1).enumerate() {
            prefix[index] = *token;
        }
        let hidden = tensor_f32(vec![1, global_hidden.len()], global_hidden.to_vec())?;
        let text_token = tensor_i32(vec![1], vec![text_token_id])?;
        let prefix = tensor_i32(vec![1, n_vq - 1], prefix)?;

        let session = self
            .sessions
            .get_mut("local_decoder")
            .ok_or_else(|| "local_decoder session 未创建".to_string())?;
        let outputs = session
            .run(ort::inputs![
                "global_hidden" => hidden,
                "text_token_id" => text_token,
                "audio_prefix_token_ids" => prefix
            ])
            .map_err(|error| format!("local_decoder 推理失败：{error}"))?;
        let (_, text_logits) = extract_f32(&outputs, "text_logits")?.f32();
        Ok((text_logits, extract_f32(&outputs, "audio_logits")?))
    }

    fn create_empty_local_cached_past(&self) -> HashMap<String, TensorData> {
        let layers = self.tts_meta["model_config"]["local_layers"]
            .as_u64()
            .unwrap_or(1) as usize;
        let heads = self.tts_meta["model_config"]["local_heads"]
            .as_u64()
            .unwrap_or(12) as usize;
        let head_dim = self.tts_meta["model_config"]["local_head_dim"]
            .as_u64()
            .unwrap_or(64) as usize;
        let mut past = HashMap::new();
        for layer in 0..layers {
            for kind in ["key", "value"] {
                past.insert(
                    format!("local_past_{kind}_{layer}"),
                    TensorData::F32(vec![1, 0, heads, head_dim], Vec::new()),
                );
            }
        }
        past
    }

    #[allow(clippy::too_many_arguments)]
    fn run_local_cached_step(
        &mut self,
        global_hidden: &[f32],
        text_token_id: i32,
        audio_token_id: i32,
        channel_index: i32,
        step_type: i32,
        past_valid_lengths: i32,
        local_past: &HashMap<String, TensorData>,
    ) -> Result<LocalStepOutput, String> {
        let mut feeds: Vec<(Cow<'static, str>, ort::session::SessionInputValue<'static>)> = vec![(
            Cow::Borrowed("global_hidden"),
            tensor_f32(vec![1, global_hidden.len()], global_hidden.to_vec())?.into(),
        )];
        feeds.push((
            Cow::Borrowed("text_token_id"),
            tensor_i32(vec![1], vec![text_token_id])?.into(),
        ));
        feeds.push((
            Cow::Borrowed("audio_token_id"),
            tensor_i32(vec![1], vec![audio_token_id])?.into(),
        ));
        feeds.push((
            Cow::Borrowed("channel_index"),
            tensor_i32(vec![1], vec![channel_index])?.into(),
        ));
        feeds.push((
            Cow::Borrowed("step_type"),
            tensor_i32(vec![1], vec![step_type])?.into(),
        ));
        feeds.push((
            Cow::Borrowed("past_valid_lengths"),
            tensor_i32(vec![1], vec![past_valid_lengths])?.into(),
        ));
        for (name, data) in local_past {
            feeds.push((Cow::Owned(name.clone()), tensor_for(data)?));
        }

        let output_names = self.output_names("local_cached_output_names", 2)?;
        let (text_logits, audio_logits, next_past) = {
            let session = self
                .sessions
                .get_mut("local_cached_step")
                .ok_or_else(|| "local_cached_step session 未创建".to_string())?;
            let outputs = session
                .run(feeds)
                .map_err(|error| format!("local_cached_step 推理失败：{error}"))?;
            let (_, text_logits) = extract_f32(&outputs, "text_logits")?.f32();
            let audio_logits = extract_f32(&outputs, "audio_logits")?;

            let mut next_past = HashMap::new();
            for name in &output_names {
                next_past.insert(
                    name.replace("local_present_", "local_past_"),
                    extract_f32(&outputs, name)?,
                );
            }
            (text_logits, audio_logits, next_past)
        };
        Ok((text_logits, audio_logits, next_past))
    }

    fn run_local_greedy_frame(
        &mut self,
        global_hidden: &[f32],
        previous_token_sets: &[HashSet<i32>],
        repetition_penalty: f64,
    ) -> Result<(bool, Vec<i32>), String> {
        let mask = self.repetition_seen_mask(previous_token_sets);
        let hidden = tensor_f32(vec![1, global_hidden.len()], global_hidden.to_vec())?;
        let mask = tensor_i32(vec![1, self.n_vq(), self.audio_codebook_size()], mask)?;
        let penalty = tensor_f32(vec![1], vec![repetition_penalty as f32])?;
        let session = self
            .sessions
            .get_mut("local_greedy_frame")
            .ok_or_else(|| "local_greedy_frame session 未创建".to_string())?;
        let outputs = session
            .run(ort::inputs![
                "global_hidden" => hidden,
                "repetition_seen_mask" => mask,
                "repetition_penalty" => penalty
            ])
            .map_err(|error| format!("local_greedy_frame 推理失败：{error}"))?;
        let (should_continue, _) = extract_i32(&outputs, "should_continue")?;
        let (frame, _) = extract_i32(&outputs, "frame_token_ids")?;
        Ok((should_continue.first().copied().unwrap_or(0) != 0, frame))
    }

    fn run_local_fixed_sampled_frame(
        &mut self,
        global_hidden: &[f32],
        previous_token_sets: &[HashSet<i32>],
    ) -> Result<(bool, Vec<i32>), String> {
        let n_vq = self.n_vq();
        let mask = self.repetition_seen_mask(previous_token_sets);
        let assistant_random = clamp_random(self.rng.random());
        let audio_random: Vec<f32> = (0..n_vq).map(|_| clamp_random(self.rng.random())).collect();

        let hidden = tensor_f32(vec![1, global_hidden.len()], global_hidden.to_vec())?;
        let mask = tensor_i32(vec![1, n_vq, self.audio_codebook_size()], mask)?;
        let assistant = tensor_f32(vec![1], vec![assistant_random])?;
        let audio = tensor_f32(vec![1, n_vq], audio_random)?;
        let session = self
            .sessions
            .get_mut("local_fixed_sampled_frame")
            .ok_or_else(|| "local_fixed_sampled_frame session 未创建".to_string())?;
        let outputs = session
            .run(ort::inputs![
                "global_hidden" => hidden,
                "repetition_seen_mask" => mask,
                "assistant_random_u" => assistant,
                "audio_random_u" => audio
            ])
            .map_err(|error| format!("local_fixed_sampled_frame 推理失败：{error}"))?;
        let (should_continue, _) = extract_i32(&outputs, "should_continue")?;
        let (frame, _) = extract_i32(&outputs, "frame_token_ids")?;
        Ok((should_continue.first().copied().unwrap_or(0) != 0, frame))
    }

    fn repetition_seen_mask(&self, previous_token_sets: &[HashSet<i32>]) -> Vec<i32> {
        let n_vq = self.n_vq();
        let codebook_size = self.audio_codebook_size();
        let mut mask = vec![0i32; n_vq * codebook_size];
        for (channel, tokens) in previous_token_sets.iter().enumerate().take(n_vq) {
            for token in tokens {
                if *token >= 0 && (*token as usize) < codebook_size {
                    mask[channel * codebook_size + *token as usize] = 1;
                }
            }
        }
        mask
    }

    fn slice_audio_channel_logits(audio_logits: &TensorData, channel_index: usize) -> Vec<f32> {
        let (shape, data) = audio_logits.f32();
        let per_channel = shape.last().copied().unwrap_or(data.len()).max(1);
        let start = channel_index * per_channel;
        let end = (start + per_channel).min(data.len());
        if start >= data.len() {
            return Vec::new();
        }
        data[start..end].to_vec()
    }

    // ------------------------------------------------------------------
    // codec
    // ------------------------------------------------------------------

    pub fn decode_full_audio(
        &mut self,
        generated_frames: &[Vec<i32>],
    ) -> Result<(Vec<Vec<f32>>, usize), String> {
        if generated_frames.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let n_vq = self.n_vq();
        let mut codes: Vec<i32> = Vec::with_capacity(generated_frames.len() * n_vq);
        for frame in generated_frames {
            for channel in 0..n_vq {
                codes.push(frame.get(channel).copied().unwrap_or(0));
            }
        }
        let codes = tensor_i32(vec![1, generated_frames.len(), n_vq], codes)?;
        let lengths = tensor_i32(vec![1], vec![generated_frames.len() as i32])?;
        let session = self
            .sessions
            .get_mut("codec_decode")
            .ok_or_else(|| "codec_decode session 未创建".to_string())?;
        let outputs = session
            .run(ort::inputs!["audio_codes" => codes, "audio_code_lengths" => lengths])
            .map_err(|error| format!("codec 解码失败：{error}"))?;
        let (audio_shape, audio_data) = extract_f32(&outputs, "audio")?.f32();
        let (lengths, _) = extract_i32(&outputs, "audio_lengths")?;
        let audio_length = lengths.first().copied().unwrap_or(0).max(0) as usize;
        Ok((
            slice_channel_major_audio(&audio_shape, &audio_data, audio_length)?,
            audio_length,
        ))
    }

    /// 流式解码一批帧，返回（按声道拆分的波形, 有效采样数）。
    pub fn run_codec_streaming_frames(
        &mut self,
        frame_rows: &[Vec<i32>],
    ) -> Result<StreamedWaveform, String> {
        if frame_rows.is_empty() {
            return Ok(None);
        }
        let n_vq = self.n_vq();
        let mut codes: Vec<i32> = Vec::with_capacity(frame_rows.len() * n_vq);
        for row in frame_rows {
            for channel in 0..n_vq {
                codes.push(row.get(channel).copied().unwrap_or(0));
            }
        }

        let mut feeds: Vec<(Cow<'static, str>, ort::session::SessionInputValue<'static>)> =
            Vec::new();
        feeds.push((
            Cow::Borrowed("audio_codes"),
            tensor_i32(vec![1, frame_rows.len(), n_vq], codes)?.into(),
        ));
        feeds.push((
            Cow::Borrowed("audio_code_lengths"),
            tensor_i32(vec![1], vec![frame_rows.len() as i32])?.into(),
        ));
        self.streaming.append_feeds(&mut feeds)?;

        let session = self
            .sessions
            .get_mut("codec_decode_step")
            .ok_or_else(|| "codec_decode_step session 未创建".to_string())?;
        let outputs = session
            .run(feeds)
            .map_err(|error| format!("codec 流式解码失败：{error}"))?;
        let (audio_shape, audio_data) = extract_f32(&outputs, "audio")?.f32();
        let (lengths, _) = extract_i32(&outputs, "audio_lengths")?;
        let audio_length = lengths.first().copied().unwrap_or(0).max(0) as usize;
        self.streaming.update(&outputs)?;
        Ok(Some((
            slice_channel_major_audio(&audio_shape, &audio_data, audio_length)?,
            audio_length,
        )))
    }

    /// 参考音频 → prompt audio codes（语音克隆音色）。
    pub fn encode_reference_audio(
        &mut self,
        waveform: &[Vec<f32>],
    ) -> Result<Vec<Vec<i32>>, String> {
        let channels = waveform.len();
        let samples = waveform.first().map(Vec::len).unwrap_or(0);
        let mut flat: Vec<f32> = Vec::with_capacity(channels * samples);
        for lane in waveform {
            flat.extend_from_slice(lane);
        }
        let input = tensor_f32(vec![1, channels, samples], flat)?;
        let lengths = tensor_i32(vec![1], vec![samples as i32])?;
        let default_n_vq = self.n_vq();
        let session = self
            .sessions
            .get_mut("codec_encode")
            .ok_or_else(|| "codec_encode session 未创建".to_string())?;
        let outputs = session
            .run(ort::inputs!["waveform" => input, "input_lengths" => lengths])
            .map_err(|error| format!("codec 编码失败：{error}"))?;
        let (codes, shape) = extract_i32(&outputs, "audio_codes")?;
        let (code_lengths, _) = extract_i32(&outputs, "audio_code_lengths")?;
        let code_length = code_lengths.first().copied().unwrap_or(0).max(0) as usize;
        let n_vq = shape.last().copied().unwrap_or(default_n_vq);
        let mut prompt_codes = Vec::with_capacity(code_length);
        for frame in 0..code_length {
            let mut row = Vec::with_capacity(n_vq);
            for quantizer in 0..n_vq {
                let index = frame * n_vq + quantizer;
                row.push(codes.get(index).copied().unwrap_or(0));
            }
            prompt_codes.push(row);
        }
        Ok(prompt_codes)
    }

    // ------------------------------------------------------------------
    // 自回归生成
    // ------------------------------------------------------------------

    pub fn generate_audio_frames(
        &mut self,
        request: &RequestRows,
        streaming: bool,
    ) -> Result<GeneratedAudio, String> {
        let n_vq = self.n_vq();
        let row_width = n_vq + 1;
        let audio_pad = self.audio_pad_token_id();
        let assistant_slot = self.audio_assistant_slot_token_id();
        let sample_mode = self.default_str("sample_mode");
        let do_sample = self.default_bool("do_sample");
        let max_new_frames = self.default_i64("max_new_frames").max(0) as usize;
        let sample_rate = self.sample_rate();
        if streaming {
            self.streaming.reset();
        }

        let prefill = self.run_prefill(request)?;
        let mut hidden = prefill.hidden;
        // 模型状态游标（不是循环计数）：每步由采样结果推进，不能用 enumerate 替代。
        let mut past_valid_length = prefill.past_valid_length;
        let mut past = prefill.past;

        let mut generated_frames: Vec<Vec<i32>> = Vec::new();
        let mut previous_tokens: Vec<Vec<i32>> = vec![Vec::new(); n_vq];
        let mut previous_sets: Vec<HashSet<i32>> = vec![HashSet::new(); n_vq];
        let mut pending_frames: Vec<Vec<i32>> = Vec::new();
        let mut streamed: AudioBuffer = vec![Vec::new(); self.channels()];
        let mut emitted_samples_total = 0usize;
        let mut first_audio_at: Option<Instant> = None;

        #[allow(clippy::explicit_counter_loop)]
        for _step_index in 0..max_new_frames {
            let frame: Vec<i32>;
            if self.sessions.contains_key("local_greedy_frame") && !do_sample {
                let (should_continue, greedy_frame) = self.run_local_greedy_frame(
                    &hidden,
                    &previous_sets,
                    self.default_f64("audio_repetition_penalty"),
                )?;
                if !should_continue {
                    break;
                }
                frame = greedy_frame;
                for (channel, token) in frame.iter().enumerate() {
                    previous_tokens[channel].push(*token);
                    previous_sets[channel].insert(*token);
                }
            } else if self.sessions.contains_key("local_fixed_sampled_frame")
                && sample_mode == SAMPLE_MODE_FIXED
            {
                let (should_continue, sampled_frame) =
                    self.run_local_fixed_sampled_frame(&hidden, &previous_sets)?;
                if !should_continue {
                    break;
                }
                frame = sampled_frame;
                for (channel, token) in frame.iter().enumerate() {
                    previous_tokens[channel].push(*token);
                    previous_sets[channel].insert(*token);
                }
            } else if self.sessions.contains_key("local_cached_step") {
                let mut local_past = self.create_empty_local_cached_past();
                let mut local_past_valid_length = 0i32;
                let (text_logits, _audio_logits, next_past) = self.run_local_cached_step(
                    &hidden,
                    0,
                    0,
                    0,
                    0,
                    local_past_valid_length,
                    &local_past,
                )?;
                local_past = next_past;
                local_past_valid_length += 1;
                let next_text_token = self.sample_assistant_text_token(&text_logits)?;
                if next_text_token != assistant_slot {
                    break;
                }
                let (_text_logits, audio_logits, next_past) = self.run_local_cached_step(
                    &hidden,
                    next_text_token,
                    0,
                    0,
                    1,
                    local_past_valid_length,
                    &local_past,
                )?;
                local_past = next_past;
                local_past_valid_length += 1;

                let mut sampled_frame: Vec<i32> = Vec::with_capacity(n_vq);
                let channel_logits = Self::slice_audio_channel_logits(&audio_logits, 0);
                let token = self.sample_audio_token(
                    &channel_logits,
                    &previous_tokens[0],
                    &previous_sets[0],
                )?;
                sampled_frame.push(token);
                previous_tokens[0].push(token);
                previous_sets[0].insert(token);

                let mut previous_token = token;
                for channel in 1..n_vq {
                    let (_text_logits, audio_logits, next_past) = self.run_local_cached_step(
                        &hidden,
                        0,
                        previous_token,
                        (channel - 1) as i32,
                        2,
                        local_past_valid_length,
                        &local_past,
                    )?;
                    local_past = next_past;
                    local_past_valid_length += 1;
                    let channel_logits = Self::slice_audio_channel_logits(&audio_logits, channel);
                    let token = self.sample_audio_token(
                        &channel_logits,
                        &previous_tokens[channel],
                        &previous_sets[channel],
                    )?;
                    sampled_frame.push(token);
                    previous_tokens[channel].push(token);
                    previous_sets[channel].insert(token);
                    previous_token = token;
                }
                frame = sampled_frame;
            } else {
                let (text_logits, _) = self.run_local_decoder(&hidden, 0, &[])?;
                let next_text_token = self.sample_assistant_text_token(&text_logits)?;
                if next_text_token != assistant_slot {
                    break;
                }
                let mut sampled_frame: Vec<i32> = Vec::with_capacity(n_vq);
                for channel in 0..n_vq {
                    let (_, audio_logits) =
                        self.run_local_decoder(&hidden, next_text_token, &sampled_frame)?;
                    let channel_logits = Self::slice_audio_channel_logits(&audio_logits, channel);
                    let token = self.sample_audio_token(
                        &channel_logits,
                        &previous_tokens[channel],
                        &previous_sets[channel],
                    )?;
                    sampled_frame.push(token);
                    previous_tokens[channel].push(token);
                    previous_sets[channel].insert(token);
                }
                frame = sampled_frame;
            }

            generated_frames.push(frame.clone());

            let mut next_row = vec![audio_pad; row_width];
            next_row[0] = assistant_slot;
            for (index, token) in frame.iter().enumerate() {
                next_row[index + 1] = *token;
            }

            let mut feeds: Vec<(Cow<'static, str>, ort::session::SessionInputValue<'static>)> =
                Vec::new();
            feeds.push((
                Cow::Borrowed("input_ids"),
                tensor_i32(vec![1, 1, row_width], next_row)?.into(),
            ));
            feeds.push((
                Cow::Borrowed("past_valid_lengths"),
                tensor_i32(vec![1], vec![past_valid_length as i32])?.into(),
            ));
            for name in self.output_names("decode_input_names", 2)? {
                let data = past
                    .get(&name)
                    .ok_or_else(|| format!("缺少历史张量 {name}"))?;
                feeds.push((Cow::Owned(name.clone()), tensor_for(data)?));
            }

            let decode_output_names = self.output_names("decode_output_names", 1)?;
            {
                let session = self
                    .sessions
                    .get_mut("decode")
                    .ok_or_else(|| "decode session 未创建".to_string())?;
                let outputs = session
                    .run(feeds)
                    .map_err(|error| format!("decode 推理失败：{error}"))?;
                let (hidden_shape, hidden_data) = extract_f32(&outputs, "global_hidden")?.f32();
                hidden = last_hidden(&hidden_shape, &hidden_data)?;
                past.clear();
                for name in &decode_output_names {
                    past.insert(
                        name.replace("present_", "past_"),
                        extract_f32(&outputs, name)?,
                    );
                }
            }
            past_valid_length += 1;

            if streaming {
                pending_frames.push(frame);
                self.decode_pending_streaming(
                    &mut pending_frames,
                    &mut streamed,
                    &mut emitted_samples_total,
                    &mut first_audio_at,
                    sample_rate,
                    false,
                )?;
            }
        }
        if streaming {
            self.decode_pending_streaming(
                &mut pending_frames,
                &mut streamed,
                &mut emitted_samples_total,
                &mut first_audio_at,
                sample_rate,
                true,
            )?;
            self.streaming.reset();
        }
        Ok(GeneratedAudio {
            generated_frames,
            streamed_waveform: streaming.then_some(streamed),
        })
    }

    /// 流式解码：按"已播时长 − 已生成时长"的领先量决定每批解码多少帧。
    #[allow(clippy::too_many_arguments)]
    fn decode_pending_streaming(
        &mut self,
        pending: &mut Vec<Vec<i32>>,
        emitted: &mut AudioBuffer,
        emitted_samples_total: &mut usize,
        first_audio_at: &mut Option<Instant>,
        sample_rate: u32,
        force: bool,
    ) -> Result<(), String> {
        if pending.is_empty() {
            return Ok(());
        }
        let budget = resolve_stream_decode_frame_budget(
            *emitted_samples_total,
            sample_rate,
            *first_audio_at,
        );
        if !force && pending.len() < budget.max(1) {
            return Ok(());
        }
        let take = if force {
            pending.len()
        } else {
            pending.len().min(budget.max(1))
        };
        let batch: Vec<Vec<i32>> = pending.drain(..take).collect();
        let Some((lanes, audio_length)) = self.run_codec_streaming_frames(&batch)? else {
            return Ok(());
        };
        if audio_length == 0 {
            return Ok(());
        }
        if first_audio_at.is_none() {
            *first_audio_at = Some(Instant::now());
        }
        *emitted_samples_total += audio_length;
        for (channel, lane) in lanes.into_iter().enumerate() {
            if let Some(target) = emitted.get_mut(channel) {
                target.extend(lane);
            }
        }
        Ok(())
    }

    fn sample_assistant_text_token(&mut self, text_logits: &[f32]) -> Result<i32, String> {
        let candidates = [
            self.audio_assistant_slot_token_id(),
            self.audio_end_token_id(),
        ];
        let scores: Vec<f32> = candidates
            .iter()
            .map(|token| text_logits.get(*token as usize).copied().unwrap_or(0.0))
            .collect();
        let top_k = self.default_i64("text_top_k").min(scores.len() as i64);
        let index = sample_from_scores(
            &scores,
            self.default_bool("do_sample"),
            self.default_f64("text_temperature"),
            top_k,
            self.default_f64("text_top_p"),
            &mut self.rng,
        )?;
        Ok(candidates[index])
    }

    fn sample_audio_token(
        &mut self,
        channel_logits: &[f32],
        previous_tokens: &[i32],
        previous_set: &HashSet<i32>,
    ) -> Result<i32, String> {
        let repetition_penalty = self.default_f64("audio_repetition_penalty");
        if !self.default_bool("do_sample") {
            return Ok(argmax_with_repetition_penalty(
                channel_logits,
                previous_set,
                repetition_penalty,
            ) as i32);
        }
        let penalized =
            apply_repetition_penalty(channel_logits, previous_tokens, repetition_penalty);
        let index = sample_from_scores(
            &penalized,
            true,
            self.default_f64("audio_temperature"),
            self.default_i64("audio_top_k"),
            self.default_f64("audio_top_p"),
            &mut self.rng,
        )?;
        Ok(index as i32)
    }
}

struct PrefillOutput {
    hidden: Vec<f32>,
    past: HashMap<String, TensorData>,
    past_valid_length: i64,
}

/// 本地采样一步的产物：`(hidden, past, local_past)`。
type LocalStepOutput = (Vec<f32>, TensorData, HashMap<String, TensorData>);

/// 流式解码一批帧的产物：按声道拆分的波形与有效采样数；无帧时为 `None`。
type StreamedWaveform = Option<(Vec<Vec<f32>>, usize)>;

fn clamp_random(value: f64) -> f32 {
    value.clamp(0.0, 0.999_999_94) as f32
}

/// 已生成音频相对实际耗时的领先量（秒）；未知或非法采样率时返回 0。
fn compute_stream_lead_seconds(
    emitted_samples_total: usize,
    sample_rate: u32,
    first_audio_at: Option<Instant>,
) -> f64 {
    let Some(first) = first_audio_at else {
        return 0.0;
    };
    if sample_rate == 0 {
        return 0.0;
    }
    let elapsed_seconds = first.elapsed().as_secs_f64().max(0.0);
    emitted_samples_total as f64 / f64::from(sample_rate) - elapsed_seconds
}

/// 每批解码帧数预算：领先越多，一次解码越多（与 Python 的档位一致）。
fn resolve_stream_decode_frame_budget(
    emitted_samples_total: usize,
    sample_rate: u32,
    first_audio_at: Option<Instant>,
) -> usize {
    if first_audio_at.is_none() {
        return 1;
    }
    let lead_seconds =
        compute_stream_lead_seconds(emitted_samples_total, sample_rate, first_audio_at);
    if lead_seconds < 0.20 {
        1
    } else if lead_seconds < 0.55 {
        2
    } else if lead_seconds < 1.10 {
        4
    } else {
        8
    }
}

fn json_codes(value: &Value) -> Vec<Vec<i32>> {
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| item.as_i64().unwrap_or(0) as i32)
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn json_ids(value: &Value) -> Vec<i32> {
    let ids: Vec<i32> = value
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row.as_i64().unwrap_or(0) as i32)
                .collect()
        })
        .unwrap_or_default();
    ids
}

fn slice_channel_major_audio(
    shape: &[usize],
    data: &[f32],
    audio_length: usize,
) -> Result<Vec<Vec<f32>>, String> {
    if shape.len() != 3 || shape[0] != 1 {
        return Err(format!("Unexpected audio tensor shape: {shape:?}"));
    }
    let channels = shape[1];
    let total_samples = shape[2];
    let end = audio_length.min(total_samples);
    let mut lanes = Vec::with_capacity(channels);
    for channel in 0..channels {
        let start = channel * total_samples;
        let stop = start + end;
        lanes.push(data.get(start..stop).unwrap_or_default().to_vec());
    }
    Ok(lanes)
}

fn open_session(path: &Path, thread_count: usize) -> Result<Session, String> {
    Session::builder()
        .map_err(|error| format!("创建 session builder 失败：{error}"))?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(|error| format!("设置图优化级别失败：{error}"))?
        .with_intra_threads(thread_count.max(1))
        .map_err(|error| format!("设置推理线程失败：{error}"))?
        .with_inter_threads(1)
        .map_err(|error| format!("设置线程失败：{error}"))?
        .commit_from_file(path)
        .map_err(|error| format!("加载模型 {} 失败：{error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_normalize_like_python() {
        assert_eq!(normalize_execution_provider("").unwrap(), DEVICE_AUTO);
        assert_eq!(normalize_execution_provider("Auto").unwrap(), DEVICE_AUTO);
        assert_eq!(
            normalize_execution_provider("gpu").unwrap(),
            EXECUTION_PROVIDER_CUDA
        );
        assert_eq!(
            normalize_execution_provider("CUDAExecutionProvider").unwrap(),
            EXECUTION_PROVIDER_CUDA
        );
        assert_eq!(
            normalize_execution_provider("CPU").unwrap(),
            EXECUTION_PROVIDER_CPU
        );
        assert!(normalize_execution_provider("tpu").is_err());
    }

    #[test]
    fn cuda_is_rejected_while_auto_falls_back_to_cpu() {
        assert_eq!(
            resolve_execution_provider("auto").unwrap(),
            EXECUTION_PROVIDER_CPU
        );
        assert_eq!(
            resolve_execution_provider("cpu").unwrap(),
            EXECUTION_PROVIDER_CPU
        );
        let error = resolve_execution_provider("cuda").expect_err("CUDA 尚未接入");
        assert!(error.contains("CUDAExecutionProvider"), "{error}");
    }

    #[test]
    fn last_hidden_takes_the_final_row() {
        let hidden = last_hidden(&[1, 2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).unwrap();
        assert_eq!(hidden, vec![4.0, 5.0, 6.0]);
        let flat = last_hidden(&[1, 3], &[7.0, 8.0, 9.0]).unwrap();
        assert_eq!(flat, vec![7.0, 8.0, 9.0]);
        assert!(last_hidden(&[2, 3], &[1.0]).is_err());
    }

    #[test]
    fn channel_major_audio_slicing_matches_torch_layout() {
        let lanes =
            slice_channel_major_audio(&[1, 2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2).unwrap();
        assert_eq!(lanes, vec![vec![1.0, 2.0], vec![4.0, 5.0]]);
        assert!(slice_channel_major_audio(&[2, 2, 3], &[], 0).is_err());
    }
}
