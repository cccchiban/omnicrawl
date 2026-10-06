//! TTS 语音合成工具执行体（引擎在 `omnicrawl-tts`）。
//!
//! 两条后端，由配置决定：
//!
//! * **接口合成**（默认）：`[tts_api]` 段启用时走 OpenAI 兼容
//!   `POST {base_url}/audio/speech`，不依赖 ONNX 运行时。
//! * **本地推理**：`[tts_api].enabled = false` 时回落到 MOSS-TTS-Nano ONNX；
//!   发布构建默认没编译它（`onnx` feature 关闭），此时会报出明确的不可用原因。
//!
//! 其余语义基准是 `omnicrawl/agent/controllers/tools/implementations.py::_tool_tts_synthesize`：
//! 本地引擎按模型目录/线程数/设备缓存复用，合成后按配置自动播放；成功与失败都回 JSON 信封
//! （失败是 `ok=false` 加 `error`，而不是错误码前缀文本），让模型不再重复调用。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use omnicrawl_config::features::tts::TtsConfiguration;
use omnicrawl_core::ToolResult;
use omnicrawl_tts::api::{synthesize_speech, ApiTtsConfig};
use omnicrawl_tts::config::TtsConfig;
use omnicrawl_tts::engine::{TtsEngine, TtsResult};
use omnicrawl_tts::player::play_wav;
use serde_json::{json, Map, Value};

use super::error::text_success;

/// TTS 工具的运行期配置。
pub struct TtsOptions {
    pub configuration: TtsConfiguration,
    /// 接口后端（由宿主把 `[tts_api]` 段映射而来，密钥已解析）；`None` 表示走本地推理。
    pub api: Option<ApiTtsConfig>,
    /// 已解析的模型目录（不随工作区变化）。
    pub model_dir: PathBuf,
    /// 相对输出目录的基准。
    pub workspace_root: PathBuf,
    engine: Mutex<Option<CachedEngine>>,
}

struct CachedEngine {
    signature: String,
    engine: TtsEngine,
}

/// 一次调用的入参（已按 Python 的取值口径归一化）。
struct SynthesisRequest {
    text: String,
    prompt_audio: Option<PathBuf>,
    output_path: PathBuf,
}

impl TtsOptions {
    pub fn new(
        configuration: TtsConfiguration,
        api: Option<ApiTtsConfig>,
        model_dir: PathBuf,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            configuration,
            api,
            model_dir,
            workspace_root,
            engine: Mutex::new(None),
        }
    }

    /// 本次调用是否走接口合成。
    fn uses_api(&self) -> bool {
        self.api.is_some()
    }

    /// 音色名：接口后端取 `[tts_api].voice`，本地后端取 `[tts].voice`。
    ///
    /// 本地后端固定用设置里的音色（模型可能猜一个不存在的音色名，如 default）。
    fn configured_voice(&self) -> String {
        if let Some(api) = self.api.as_ref() {
            if !api.voice.trim().is_empty() {
                return api.voice.clone();
            }
        }
        let configured = self.configuration.voice.trim();
        if configured.is_empty() {
            "Junhao".to_string()
        } else {
            configured.to_string()
        }
    }

    /// 合成一次语音：成功回结果摘要 JSON，失败回 `{"ok": false, "error": ...}`。
    pub fn tts_synthesize(&self, arguments: &Map<String, Value>) -> ToolResult {
        let text = text_argument(arguments, "text").trim().to_string();
        if text.is_empty() {
            return error_result("text 不能为空。");
        }
        let voice = self.configured_voice();
        let request = SynthesisRequest {
            text,
            prompt_audio: path_argument(arguments, "prompt_audio"),
            output_path: path_argument(arguments, "path")
                .unwrap_or_else(|| self.default_output_path()),
        };
        // 参考音频（语音克隆）只有本地推理管线吃得下：接口后端拿到它只能忽略，
        // 那会让用户以为换了音色。这里直接报错并把出路写清楚。
        if self.uses_api() && request.prompt_audio.is_some() {
            return error_result(
                "接口合成不支持参考音频（prompt_audio）：请在 [tts_api].voice 里填服务端支持的音色名，\
                 或改用带本地推理的构建（--features omnicrawl-tts/onnx）。",
            );
        }
        let result = match self.run(&request, &voice) {
            Ok(result) => result,
            Err(message) => return error_result(&message),
        };
        if self.configuration.auto_play {
            play_wav(&result.audio_path, false);
        }
        let voice_field = match request.prompt_audio.as_ref() {
            Some(path) => path.to_string_lossy().to_string(),
            None => voice,
        };
        let summary = json!({
            "ok": true,
            "audio_path": result.audio_path.to_string_lossy(),
            "sample_rate": result.sample_rate,
            "duration_seconds": round_two(result.duration_seconds),
            "voice": voice_field,
            "text_chunks": result.text_chunks.len(),
        });
        text_success(summary.to_string())
    }

    fn default_output_path(&self) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        self.output_directory().join(format!("tts_{stamp}.wav"))
    }

    fn signature(&self) -> String {
        format!(
            "{}|{}|{}",
            self.model_dir.display(),
            self.configuration.thread_count,
            self.configuration.device
        )
    }

    fn run(&self, request: &SynthesisRequest, voice: &str) -> Result<TtsResult, String> {
        if let Some(api) = self.api.as_ref() {
            // 接口后端没有引擎缓存：每次调用就是一次 HTTP 请求，配置变化即刻生效。
            return synthesize_speech(api, &request.text, Some(voice), &request.output_path);
        }
        let signature = self.signature();
        let mut cache = self
            .engine
            .lock()
            .map_err(|_| "TTS 引擎缓存不可用。".to_string())?;
        let stale = match cache.as_ref() {
            Some(cached) => cached.signature != signature,
            None => true,
        };
        if stale {
            if let Some(mut previous) = cache.take() {
                previous.engine.close();
            }
            let engine = TtsEngine::new(self.engine_config())?;
            *cache = Some(CachedEngine { signature, engine });
        }
        let cached = cache
            .as_mut()
            .ok_or_else(|| "TTS 引擎初始化失败。".to_string())?;
        // 参考音频存在时音色改由它决定：与 Python 一致，此时不传音色名。
        let voice_arg = if request.prompt_audio.is_some() {
            None
        } else {
            Some(voice)
        };
        cached.engine.synthesize(
            &request.text,
            voice_arg,
            request.prompt_audio.as_deref(),
            Some(request.output_path.as_path()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    fn engine_config(&self) -> TtsConfig {
        TtsConfig {
            model_dir: Some(self.model_dir.clone()),
            thread_count: self.configuration.thread_count,
            device: Some(self.configuration.device.clone()),
            output_dir: self.output_directory(),
            ..TtsConfig::default()
        }
    }

    fn output_directory(&self) -> PathBuf {
        let configured = self.configuration.output_dir.trim();
        let directory = Path::new(if configured.is_empty() {
            ".omnicrawl/.agent_tmp/tts"
        } else {
            configured
        });
        if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            self.workspace_root.join(directory)
        }
    }
}

fn error_result(message: &str) -> ToolResult {
    let output = json!({ "ok": false, "error": message }).to_string();
    ToolResult {
        ok: false,
        output: output.clone(),
        full_output: output,
        error_code: None,
        retryable: false,
    }
}

/// Python 侧 `str(arguments.get(key) or "")` 的标量子集。
fn text_argument(arguments: &Map<String, Value>, key: &str) -> String {
    match arguments.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        _ => String::new(),
    }
}

/// Python 侧 `arguments.get(key) or None`：缺失与空串都视为未提供。
fn path_argument(arguments: &Map<String, Value>, key: &str) -> Option<PathBuf> {
    let value = text_argument(arguments, key);
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn round_two(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地后端（`api = None`）的工具选项。
    fn options() -> TtsOptions {
        let configuration = TtsConfiguration {
            enabled: true,
            auto_play: false,
            ..TtsConfiguration::default()
        };
        TtsOptions::new(
            configuration,
            None,
            PathBuf::from("/models/tts"),
            PathBuf::from("/workspace"),
        )
    }

    /// 接口后端的工具选项（密钥已解析）。
    fn api_options(api_key: &str) -> TtsOptions {
        let configuration = TtsConfiguration {
            enabled: true,
            auto_play: false,
            ..TtsConfiguration::default()
        };
        let api = ApiTtsConfig {
            base_url: "https://tts.example.com/v1".to_string(),
            api_key: api_key.to_string(),
            model: "cosyvoice-2".to_string(),
            voice: "longxiaochun".to_string(),
            ..ApiTtsConfig::default()
        };
        TtsOptions::new(
            configuration,
            Some(api),
            PathBuf::from("/models/tts"),
            PathBuf::from("/workspace"),
        )
    }

    fn arguments(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), Value::String((*value).to_string())))
            .collect()
    }

    #[test]
    fn empty_text_is_rejected_with_json_envelope() {
        let result = options().tts_synthesize(&arguments(&[("text", "   ")]));
        assert!(!result.ok);
        assert_eq!(result.output, r#"{"ok":false,"error":"text 不能为空。"}"#);
    }

    #[test]
    fn default_output_path_uses_workspace_and_timestamp() {
        let path = options().default_output_path();
        assert!(path.starts_with("/workspace/.omnicrawl/.agent_tmp/tts"));
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("tts_") && name.ends_with(".wav"), "{name}");
    }

    #[test]
    fn absolute_output_dir_is_not_joined_with_workspace() {
        // 用平台原生的绝对路径：Windows 上盘符（`D:/...`）才算绝对，POSIX 上要以 `/` 开头。
        let absolute = if cfg!(windows) { "D:/tts-out" } else { "/tts-out" };
        let configuration = TtsConfiguration {
            enabled: true,
            output_dir: absolute.to_string(),
            ..TtsConfiguration::default()
        };
        let options = TtsOptions::new(
            configuration,
            None,
            PathBuf::from("/models"),
            PathBuf::from("/ws"),
        );
        assert!(options.default_output_path().starts_with(absolute));
    }

    /// 后端选择：`api` 为 `Some` 就是接口合成，音色取 `[tts_api].voice`。
    #[test]
    fn api_backend_uses_its_own_voice() {
        assert!(api_options("key").uses_api());
        assert_eq!(api_options("key").configured_voice(), "longxiaochun");
        // 本地后端仍然取 `[tts].voice`。
        assert!(!options().uses_api());
        assert_eq!(options().configured_voice(), "Junhao");
    }

    /// 接口后端下缺密钥：在发请求之前就报错，且信封里带着排查指向。
    #[test]
    fn api_backend_without_key_reports_before_any_request() {
        let result = api_options("   ").tts_synthesize(&arguments(&[("text", "你好")]));
        assert!(!result.ok);
        assert!(result.output.contains("未配置密钥"), "{}", result.output);
    }

    /// 接口后端不支持参考音频：直接报错而不是静默忽略（否则用户以为换了音色）。
    #[test]
    fn api_backend_rejects_reference_audio() {
        let result = api_options("key").tts_synthesize(&arguments(&[
            ("text", "你好"),
            ("prompt_audio", "D:/voices/ref.wav"),
        ]));
        assert!(!result.ok);
        assert!(result.output.contains("不支持参考音频"), "{}", result.output);
    }
}
