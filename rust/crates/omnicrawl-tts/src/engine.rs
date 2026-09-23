//! MOSS-TTS-Nano ONNX 语音合成引擎：`omnicrawl/tts/engine.py` 的等价实现。
//!
//! 高层流程（对齐官方 `OnnxTtsRuntime`）：文本归一化 → 解析音色（内置音色 /
//! 参考音频语音克隆）→ 按 token 预算分块 → 逐块自回归生成音频帧 → codec 解码成
//! 波形 → 拼接写出 WAV。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::audio::{load_reference_audio, write_wav, AudioBuffer};
use crate::config::TtsConfig;
use crate::download::ensure_model_dir;
use crate::normalize::prepare_tts_request_texts;
use crate::paths::resolve_lenient;
use crate::runtime::OrtRuntime;
use crate::sampler::{normalize_sample_mode, Pcg64, SAMPLE_MODE_GREEDY};
use crate::tokenizer::TtsTokenizer;
use crate::voices;

const DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_SHORT_SECONDS: f64 = 0.40;
const DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_LONG_SECONDS: f64 = 0.24;

const SENTENCE_END_PUNCTUATION: &str = ".!?。！？；;";
const CLAUSE_SPLIT_PUNCTUATION: &str = "，,、；;：:";
const CLOSING_PUNCTUATION: &str = "\"'”’)]}）】》」』";

/// 一次合成的结果。
#[derive(Debug, Clone)]
pub struct TtsResult {
    pub audio_path: PathBuf,
    pub sample_rate: u32,
    /// 声道优先波形 `[channels, samples]`。
    pub waveform: AudioBuffer,
    pub duration_seconds: f64,
    /// 生成的音频 token 帧数。
    pub audio_token_frames: usize,
    pub text_chunks: Vec<String>,
    pub sample_mode: String,
    pub voice: String,
}

/// 单块合成的中间结果。
#[derive(Debug, Clone)]
pub struct ChunkSynthesis {
    pub text: String,
    pub text_token_ids: Vec<i32>,
    pub generated_frames: Vec<Vec<i32>>,
    pub waveform: AudioBuffer,
}

// ---------------------------------------------------------------------------
// 文本分句 / 分块工具（vendored from onnx_tts_runtime.py）
// ---------------------------------------------------------------------------

fn contains_cjk(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character as u32,
            0x4e00..=0x9fff | 0x3400..=0x4dbf | 0x3040..=0x30ff | 0xac00..=0xd7af
        )
    })
}

fn prepare_text_for_sentence_chunking(text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("待合成的文本不能为空。".to_string());
    }
    let mut prepared = trimmed.replace(['\r', '\n'], " ");
    while prepared.contains("  ") {
        prepared = prepared.replace("  ", " ");
    }
    if contains_cjk(&prepared) {
        if !prepared
            .chars()
            .last()
            .map(|character| SENTENCE_END_PUNCTUATION.contains(character))
            .unwrap_or(false)
        {
            prepared.push('。');
        }
        return Ok(prepared);
    }

    let mut characters: Vec<char> = prepared.chars().collect();
    if characters
        .first()
        .map(|character| character.is_lowercase())
        .unwrap_or(false)
    {
        if let Some(upper) = characters[0].to_uppercase().next() {
            characters[0] = upper;
        }
    }
    let mut prepared: String = characters.into_iter().collect();
    if prepared
        .chars()
        .last()
        .map(|character| character.is_alphanumeric())
        .unwrap_or(false)
    {
        prepared.push('.');
    }
    if prepared.split_whitespace().count() < 5 {
        prepared = format!("        {prepared}");
    }
    Ok(prepared)
}

fn split_text_by_punctuation(text: &str, punctuation: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut index = 0usize;
    while index < characters.len() {
        let character = characters[index];
        current.push(character);
        if punctuation.contains(character) {
            let mut lookahead = index + 1;
            while lookahead < characters.len()
                && CLOSING_PUNCTUATION.contains(characters[lookahead])
            {
                current.push(characters[lookahead]);
                lookahead += 1;
            }
            let sentence = current.trim().to_string();
            if !sentence.is_empty() {
                sentences.push(sentence);
            }
            current.clear();
            while lookahead < characters.len() && characters[lookahead].is_whitespace() {
                lookahead += 1;
            }
            index = lookahead;
            continue;
        }
        index += 1;
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        sentences.push(tail);
    }
    sentences
}

fn join_sentence_parts(left: &str, right: &str) -> String {
    if left.is_empty() {
        return right.to_string();
    }
    if right.is_empty() {
        return left.to_string();
    }
    if contains_cjk(left) || contains_cjk(right) {
        format!("{left}{right}")
    } else {
        format!("{left} {right}")
    }
}

/// 沿时间轴拼接音频块（声道优先布局）。
fn concat_waveforms(waveforms: &[AudioBuffer]) -> AudioBuffer {
    let channel_count = waveforms.iter().map(Vec::len).max().unwrap_or(0).max(1);
    if waveforms.is_empty() {
        return Vec::new();
    }
    let mut result: AudioBuffer = vec![Vec::new(); channel_count];
    for waveform in waveforms {
        if waveform.iter().all(|lane| lane.is_empty()) {
            continue;
        }
        for (channel, lane) in waveform.iter().enumerate() {
            if let Some(target) = result.get_mut(channel) {
                target.extend_from_slice(lane);
            }
        }
    }
    result
}

fn expand_user(path: &str) -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if path == "~" {
        return home;
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => PathBuf::from(home).join(rest).to_string_lossy().to_string(),
        None => path.to_string(),
    }
}

/// MOSS-TTS-Nano ONNX 语音合成引擎。
pub struct TtsEngine {
    pub config: TtsConfig,
    pub model_dir: PathBuf,
    pub runtime: OrtRuntime,
    tokenizer: TtsTokenizer,
    output_dir: PathBuf,
    seed: Option<i64>,
}

impl TtsEngine {
    /// 加载模型（缺失时按配置自动下载）并创建推理运行时与分词器。
    pub fn new(config: TtsConfig) -> Result<Self, String> {
        let model_dir = ensure_model_dir(config.model_dir.as_deref(), None)?;
        let device = match config.device.as_deref() {
            Some("auto") if config.execution_provider != "cpu" => config.execution_provider.clone(),
            Some(device) if !device.trim().is_empty() => device.to_string(),
            _ => config.execution_provider.clone(),
        };
        let mut runtime = OrtRuntime::new(
            &model_dir,
            config.thread_count.max(1) as usize,
            Some(config.max_new_frames),
            Some(config.do_sample),
            Some(&config.sample_mode),
            &device,
        )?;
        runtime.apply_generation_defaults(&config.generation_overrides())?;

        let tokenizer_relative = runtime.manifest["model_files"]["tokenizer_model"]
            .as_str()
            .unwrap_or("tokenizer.model")
            .to_string();
        let tokenizer_path = runtime.resolve_manifest_relative_path(&tokenizer_relative);
        let tokenizer = TtsTokenizer::open(&tokenizer_path)?;

        let output_dir = resolve_lenient(&PathBuf::from(expand_user(
            &config.output_dir.to_string_lossy(),
        )));
        std::fs::create_dir_all(&output_dir)
            .map_err(|error| format!("创建输出目录 {} 失败：{error}", output_dir.display()))?;

        let seed = config.seed;
        Ok(Self {
            config,
            model_dir,
            runtime,
            tokenizer,
            output_dir,
            seed,
        })
    }

    // ------------------------------------------------------------------
    // 基础能力
    // ------------------------------------------------------------------

    pub fn sample_rate(&self) -> u32 {
        self.runtime.sample_rate()
    }

    pub fn channels(&self) -> usize {
        self.runtime.channels()
    }

    pub fn list_builtin_voices(&self) -> Vec<Value> {
        self.runtime.builtin_voices()
    }

    /// 全部可用音色：模型内置音色 + 用户自定义克隆音色（自定义排在末尾）。
    pub fn list_available_voices(&self) -> Vec<Value> {
        let mut voices = self.runtime.builtin_voices();
        let mut seen: std::collections::HashSet<String> = voices
            .iter()
            .map(|row| {
                row.get("voice")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .collect();
        for row in voices::load_custom_voices(&voices::default_root()) {
            let name = row
                .get("voice")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() || !seen.insert(name) {
                continue;
            }
            voices.push(row);
        }
        voices
    }

    pub fn list_text_samples(&self) -> Vec<Value> {
        self.runtime.text_samples()
    }

    /// 预热所有 ONNX session，降低首次合成延迟。
    pub fn warmup(&mut self) -> Result<(), String> {
        self.runtime.warmup()
    }

    pub fn close(&mut self) {}

    // ------------------------------------------------------------------
    // 分词 / 文本分块
    // ------------------------------------------------------------------

    pub fn encode_text(&self, text: &str) -> Result<Vec<i32>, String> {
        self.tokenizer.encode(text)
    }

    pub fn count_text_tokens(&self, text: &str) -> Result<usize, String> {
        self.tokenizer.count(text)
    }

    pub fn split_text_by_token_budget(
        &self,
        text: &str,
        max_tokens: usize,
    ) -> Result<Vec<String>, String> {
        let mut remaining = text.trim().to_string();
        if remaining.is_empty() {
            return Ok(Vec::new());
        }
        let safe_max_tokens = max_tokens.max(1);
        let mut pieces: Vec<String> = Vec::new();
        while !remaining.is_empty() {
            if self.count_text_tokens(&remaining)? <= safe_max_tokens {
                pieces.push(remaining);
                break;
            }
            let characters: Vec<char> = remaining.chars().collect();
            let (mut low, mut high) = (1usize, characters.len());
            let mut best_prefix_length = 1usize;
            while low <= high {
                let middle = (low + high) / 2;
                let candidate: String = characters[..middle.min(characters.len())].iter().collect();
                let candidate = candidate.trim().to_string();
                if candidate.is_empty() {
                    low = middle + 1;
                    continue;
                }
                if self.count_text_tokens(&candidate)? <= safe_max_tokens {
                    best_prefix_length = middle;
                    low = middle + 1;
                } else if middle == 0 {
                    break;
                } else {
                    high = middle - 1;
                }
            }
            let prefix: String = characters[..best_prefix_length.min(characters.len())]
                .iter()
                .collect();
            let mut cut_index = best_prefix_length;
            let scan_min = prefix.chars().count().saturating_sub(25);
            let prefix_characters: Vec<char> = prefix.chars().collect();
            let mut preferred_index = 0usize;
            for scan_index in (0..prefix_characters.len()).rev() {
                if scan_index < scan_min {
                    break;
                }
                let character = prefix_characters[scan_index];
                if CLAUSE_SPLIT_PUNCTUATION.contains(character)
                    || SENTENCE_END_PUNCTUATION.contains(character)
                    || character == ' '
                {
                    preferred_index = scan_index + 1;
                    break;
                }
            }
            if preferred_index > 0 {
                cut_index = preferred_index;
            }
            let piece: String = characters[..cut_index.min(characters.len())]
                .iter()
                .collect();
            let piece = piece.trim().to_string();
            if piece.is_empty() {
                let fallback: String = characters[..best_prefix_length.min(characters.len())]
                    .iter()
                    .collect();
                cut_index = best_prefix_length;
                pieces.push(fallback.trim().to_string());
            } else {
                pieces.push(piece);
            }
            remaining = characters[cut_index.min(characters.len())..]
                .iter()
                .collect::<String>()
                .trim()
                .to_string();
        }
        Ok(pieces)
    }

    pub fn split_voice_clone_text(
        &self,
        text: &str,
        max_tokens: usize,
    ) -> Result<Vec<String>, String> {
        let normalized = text.trim().to_string();
        if normalized.is_empty() {
            return Ok(Vec::new());
        }
        let safe_max_tokens = max_tokens.max(1);
        let prepared = prepare_text_for_sentence_chunking(&normalized)?;
        let mut sentence_candidates =
            split_text_by_punctuation(&prepared, SENTENCE_END_PUNCTUATION);
        if sentence_candidates.is_empty() {
            sentence_candidates.push(prepared.trim().to_string());
        }

        let mut slices: Vec<(usize, String)> = Vec::new();
        for sentence in sentence_candidates {
            let sentence = sentence.trim().to_string();
            if sentence.is_empty() {
                continue;
            }
            let token_count = self.count_text_tokens(&sentence)?;
            if token_count <= safe_max_tokens {
                slices.push((token_count, sentence));
                continue;
            }
            let mut clause_candidates =
                split_text_by_punctuation(&sentence, CLAUSE_SPLIT_PUNCTUATION);
            if clause_candidates.len() <= 1 {
                clause_candidates = vec![sentence.clone()];
            }
            for clause in clause_candidates {
                let clause = clause.trim().to_string();
                if clause.is_empty() {
                    continue;
                }
                if self.count_text_tokens(&clause)? <= safe_max_tokens {
                    let count = self.count_text_tokens(&clause)?;
                    slices.push((count, clause));
                    continue;
                }
                for piece in self.split_text_by_token_budget(&clause, safe_max_tokens)? {
                    let piece = piece.trim().to_string();
                    if !piece.is_empty() {
                        let count = self.count_text_tokens(&piece)?;
                        slices.push((count, piece));
                    }
                }
            }
        }

        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut current_tokens = 0usize;
        for (token_count, text) in slices {
            if current.is_empty() {
                current = text;
                current_tokens = token_count;
                continue;
            }
            if current_tokens + token_count > safe_max_tokens {
                chunks.push(current.trim().to_string());
                current = text;
                current_tokens = token_count;
            } else {
                current = join_sentence_parts(&current, &text);
                current_tokens = self.count_text_tokens(&current)?;
            }
        }
        if !current.is_empty() {
            chunks.push(current.trim().to_string());
        }
        if chunks.len() > 1 {
            Ok(chunks)
        } else {
            Ok(vec![normalized])
        }
    }

    pub fn estimate_voice_clone_inter_chunk_pause_seconds(&self, text_chunk: &str) -> f64 {
        let word_count = text_chunk
            .split_whitespace()
            .filter(|item| !item.is_empty())
            .count();
        if word_count <= 4 {
            DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_SHORT_SECONDS
        } else {
            DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_LONG_SECONDS
        }
    }

    // ------------------------------------------------------------------
    // 音色解析
    // ------------------------------------------------------------------

    /// 把参考音频编码成 prompt audio codes（语音克隆音色）。
    pub fn encode_reference_audio(
        &mut self,
        reference_audio_path: &Path,
    ) -> Result<Vec<Vec<i32>>, String> {
        let waveform =
            load_reference_audio(reference_audio_path, self.sample_rate(), self.channels())?;
        self.runtime.encode_reference_audio(&waveform)
    }

    /// 把参考音频克隆为一条自定义音色并写入音色库。
    pub fn clone_voice(
        &mut self,
        voice: &str,
        reference_audio_path: &Path,
        display_name: &str,
    ) -> Result<Value, String> {
        let codes = self.encode_reference_audio(reference_audio_path)?;
        let codes: Vec<Vec<i64>> = codes
            .into_iter()
            .map(|row| row.into_iter().map(i64::from).collect())
            .collect();
        voices::add_custom_voice(
            &voices::default_root(),
            voice,
            &codes,
            display_name,
            "",
            &reference_audio_path.to_string_lossy(),
        )
    }

    pub fn resolve_prompt_audio_codes(
        &mut self,
        voice: Option<&str>,
        prompt_audio_path: Option<&Path>,
    ) -> Result<Vec<Vec<i32>>, String> {
        if let Some(path) = prompt_audio_path {
            return self.encode_reference_audio(path);
        }
        let available = self.list_available_voices();
        let resolved_voice = match voice {
            Some(voice) if !voice.trim().is_empty() => voice.trim().to_string(),
            _ => available
                .first()
                .and_then(|row| row.get("voice"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        let row = available
            .iter()
            .find(|row| row.get("voice").and_then(Value::as_str) == Some(resolved_voice.as_str()));
        let Some(row) = row else {
            let names: Vec<String> = available
                .iter()
                .filter_map(|row| row.get("voice").and_then(Value::as_str).map(str::to_string))
                .collect();
            return Err(format!(
                "音色不存在：{resolved_voice}。可用：{}",
                names.join(", ")
            ));
        };
        Ok(json_codes(
            row.get("prompt_audio_codes").unwrap_or(&Value::Null),
        ))
    }

    // ------------------------------------------------------------------
    // 合成
    // ------------------------------------------------------------------

    /// 整段 codec 解码，失败时回退到增量流式解码。
    fn decode_full_audio_safe(&mut self, frames: &[Vec<i32>]) -> Result<AudioBuffer, String> {
        match self.runtime.decode_full_audio(frames) {
            Ok((lanes, _)) => Ok(lanes),
            Err(error) => {
                eprintln!("full codec decode failed, falling back to incremental decode: {error}");
                self.runtime.streaming.reset();
                let channels = self.runtime.channels();
                let mut merged: AudioBuffer = vec![Vec::new(); channels];
                for chunk in frames.chunks(8) {
                    if let Some((lanes, audio_length)) =
                        self.runtime.run_codec_streaming_frames(chunk)?
                    {
                        if audio_length == 0 {
                            continue;
                        }
                        for (channel, lane) in lanes.into_iter().enumerate() {
                            if let Some(target) = merged.get_mut(channel) {
                                target.extend(lane);
                            }
                        }
                    }
                }
                self.runtime.streaming.reset();
                Ok(merged)
            }
        }
    }

    /// 合成单个文本块（自回归生成帧 + codec 解码成波形）。
    pub fn synthesize_single_chunk(
        &mut self,
        text: &str,
        prompt_audio_codes: &[Vec<i32>],
        streaming: bool,
    ) -> Result<ChunkSynthesis, String> {
        let text_token_ids = self.tokenizer.encode(text)?;
        let request = self
            .runtime
            .build_voice_clone_request_rows(prompt_audio_codes, &text_token_ids);
        if !streaming {
            let generated = self.runtime.generate_audio_frames(&request, false)?;
            let waveform = self.decode_full_audio_safe(&generated.generated_frames)?;
            return Ok(ChunkSynthesis {
                text: text.to_string(),
                text_token_ids,
                generated_frames: generated.generated_frames,
                waveform,
            });
        }
        let generated = self.runtime.generate_audio_frames(&request, true)?;
        let waveform = generated.streamed_waveform.unwrap_or_default();
        Ok(ChunkSynthesis {
            text: text.to_string(),
            text_token_ids,
            generated_frames: generated.generated_frames,
            waveform,
        })
    }

    /// 把文本合成为语音并写出 WAV。
    #[allow(clippy::too_many_arguments)]
    pub fn synthesize(
        &mut self,
        text: &str,
        voice: Option<&str>,
        prompt_audio_path: Option<&Path>,
        output_path: Option<&Path>,
        sample_mode: Option<&str>,
        do_sample: Option<bool>,
        streaming: Option<bool>,
        max_new_frames: Option<i64>,
        voice_clone_max_text_tokens: Option<i64>,
        seed: Option<i64>,
    ) -> Result<TtsResult, String> {
        if let Some(frames) = max_new_frames {
            self.runtime.manifest["generation_defaults"]["max_new_frames"] = Value::from(frames);
        }
        let effective_do_sample = do_sample.unwrap_or(self.config.do_sample);
        let raw_sample_mode = sample_mode
            .map(str::to_string)
            .unwrap_or_else(|| self.config.sample_mode.clone());
        let effective_sample_mode = normalize_sample_mode(&raw_sample_mode, effective_do_sample);
        self.runtime.manifest["generation_defaults"]["sample_mode"] =
            Value::from(effective_sample_mode.clone());
        self.runtime.manifest["generation_defaults"]["do_sample"] =
            Value::from(effective_sample_mode != SAMPLE_MODE_GREEDY);
        if let Some(seed) = seed.or(self.seed) {
            self.runtime.rng = Pcg64::seeded(seed);
        }

        let effective_voice = voice.unwrap_or(&self.config.voice).to_string();
        let configured_prompt_audio = self.config.prompt_audio_path.clone();
        let effective_prompt_audio = prompt_audio_path
            .map(Path::to_path_buf)
            .or(configured_prompt_audio);
        if self.config.enable_wetext {
            eprintln!(
                "enable_wetext=True 但 Rust 实现不含 WeTextProcessing，本次合成降级为纯清洗。"
            );
        }

        let prepared = prepare_tts_request_texts(
            text,
            "",
            &effective_voice,
            false,
            self.config.enable_normalize_tts_text,
        )?;
        let prepared_text = prepared["text"].as_str().unwrap_or_default().to_string();
        eprintln!(
            "text normalization method={} language={} text_chars={}",
            prepared["normalization_method"].as_str().unwrap_or("none"),
            prepared["text_normalization_language"]
                .as_str()
                .unwrap_or("n/a"),
            prepared_text.chars().count()
        );

        let prompt_audio_codes = self.resolve_prompt_audio_codes(
            Some(effective_voice.as_str()),
            effective_prompt_audio.as_deref(),
        )?;
        let max_tokens = voice_clone_max_text_tokens
            .unwrap_or(self.config.voice_clone_max_text_tokens)
            .max(1) as usize;
        let text_chunks = self.split_voice_clone_text(&prepared_text, max_tokens)?;
        let effective_streaming = streaming.unwrap_or(self.config.streaming);

        let sample_rate = self.sample_rate();
        let channels = self.channels();
        let mut waveforms: Vec<AudioBuffer> = Vec::new();
        let mut generated_frames: Vec<Vec<i32>> = Vec::new();
        for (index, chunk_text) in text_chunks.iter().enumerate() {
            let chunk =
                self.synthesize_single_chunk(chunk_text, &prompt_audio_codes, effective_streaming)?;
            waveforms.push(chunk.waveform);
            generated_frames.extend(chunk.generated_frames);
            if index < text_chunks.len() - 1 {
                let pause_seconds = self.estimate_voice_clone_inter_chunk_pause_seconds(chunk_text);
                let pause_samples = (sample_rate as f64 * pause_seconds).round().max(0.0) as usize;
                if pause_samples > 0 {
                    waveforms.push(vec![vec![0.0f32; pause_samples]; channels]);
                }
            }
        }
        let waveform = concat_waveforms(&waveforms);

        let resolved_output_path = match output_path {
            Some(path) => resolve_lenient(Path::new(&expand_user(&path.to_string_lossy()))),
            None => resolve_lenient(&self.output_dir.join("moss_tts_nano_output.wav")),
        };
        let audio_path = write_wav(&resolved_output_path, &waveform, sample_rate)?;
        let duration_seconds = if sample_rate > 0 {
            waveform.first().map(Vec::len).unwrap_or(0) as f64 / f64::from(sample_rate)
        } else {
            0.0
        };

        Ok(TtsResult {
            audio_path,
            sample_rate,
            waveform,
            duration_seconds,
            audio_token_frames: generated_frames.len(),
            text_chunks,
            sample_mode: effective_sample_mode,
            voice: effective_voice,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_chunking_matches_python_heuristics() {
        assert_eq!(
            prepare_text_for_sentence_chunking("你好").unwrap(),
            "你好。"
        );
        assert!(prepare_text_for_sentence_chunking("hello world")
            .unwrap()
            .ends_with('.'));
        assert!(prepare_text_for_sentence_chunking("   ").is_err());
    }

    #[test]
    fn punctuation_splitting_keeps_closing_marks() {
        let sentences = split_text_by_punctuation("你好。世界！", SENTENCE_END_PUNCTUATION);
        assert_eq!(sentences, vec!["你好。", "世界！"]);
        let quoted = split_text_by_punctuation("他说：“好。”然后走了。", SENTENCE_END_PUNCTUATION);
        assert!(
            quoted.iter().any(|sentence| sentence.contains("好。”")),
            "{quoted:?}"
        );
    }

    #[test]
    fn joining_prefers_no_space_for_cjk() {
        assert_eq!(join_sentence_parts("你好", "世界"), "你好世界");
        assert_eq!(join_sentence_parts("hello", "world"), "hello world");
        assert_eq!(join_sentence_parts("", "world"), "world");
    }

    #[test]
    fn waveform_concatenation_is_channel_major() {
        let merged = concat_waveforms(&[
            vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            vec![vec![5.0], vec![6.0]],
        ]);
        assert_eq!(merged, vec![vec![1.0, 2.0, 5.0], vec![3.0, 4.0, 6.0]]);
        assert!(concat_waveforms(&[]).is_empty());
    }

    #[test]
    fn inter_chunk_pause_follows_word_count() {
        let engine_pause = |count: usize| {
            let text = std::iter::repeat("word")
                .take(count)
                .collect::<Vec<_>>()
                .join(" ");
            if text.split_whitespace().count() <= 4 {
                DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_SHORT_SECONDS
            } else {
                DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_LONG_SECONDS
            }
        };
        assert_eq!(
            engine_pause(4),
            DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_SHORT_SECONDS
        );
        assert_eq!(
            engine_pause(5),
            DEFAULT_VOICE_CLONE_INTER_CHUNK_PAUSE_LONG_SECONDS
        );
    }
}
