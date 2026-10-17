//! TTS 语音合成工具执行体（云服务通道在 `omnicrawl-tts`）。
//!
//! 单链路两通道：`[tts_cloud].provider` 选 OpenAI（`audio/speech`）或 Gemini（TTS 模型），
//! 参数取各自段（`[tts_api]` / `[tts_gemini]`）。音色由通道配置决定，工具面不暴露音色参数。
//!
//! 合成后按配置自动播放；成功与失败都回 JSON 信封（失败是 `ok=false` 加 `error`），
//! 让模型不再重复调用。

use std::path::{Path, PathBuf};

use omnicrawl_config::features::tts::TtsConfiguration;
use omnicrawl_config::features::tts_cloud::TtsCloudConfiguration;
use omnicrawl_core::ToolResult;
use omnicrawl_tts::player::play_wav;
use omnicrawl_tts::result::TtsResult;
use omnicrawl_tts::synthesis::{synthesize, TtsBackend};
use serde_json::{json, Map, Value};

use super::error::text_success;

/// TTS 工具的运行期配置。
pub struct TtsOptions {
    pub configuration: TtsConfiguration,
    /// 本次生效的通道与参数（切换 `[tts_cloud].provider` 后由宿主重建）。
    pub backend: TtsBackend,
    /// 相对输出目录的基准。
    pub workspace_root: PathBuf,
    /// 回合朗读的文件序号（同秒内多次朗读不互相覆盖）。
    reply_index: std::sync::atomic::AtomicU64,
}

/// 一次调用的入参（已按取值口径归一化）。
struct SynthesisRequest {
    text: String,
    output_path: PathBuf,
}

impl TtsOptions {
    pub fn new(
        configuration: TtsConfiguration,
        backend: TtsBackend,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            configuration,
            backend,
            workspace_root,
            reply_index: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 合成一次语音：成功回结果摘要 JSON，失败回 `{"ok": false, "error": ...}`。
    pub fn tts_synthesize(&self, arguments: &Map<String, Value>) -> ToolResult {
        let text = text_argument(arguments, "text").trim().to_string();
        if text.is_empty() {
            return error_result("text 不能为空。");
        }
        let request = SynthesisRequest {
            text,
            output_path: path_argument(arguments, "path")
                .unwrap_or_else(|| self.default_output_path()),
        };
        let result = match self.run(&request) {
            Ok(result) => result,
            Err(message) => return error_result(&message),
        };
        if self.configuration.auto_play {
            play_wav(&result.audio_path, false);
        }
        let summary = json!({
            "ok": true,
            "audio_path": result.audio_path.to_string_lossy(),
            "sample_rate": result.sample_rate,
            "duration_seconds": round_two(result.duration_seconds),
            "provider": self.backend.provider(),
            "voice": result.voice,
            "text_chunks": result.text_chunks.len(),
        });
        text_success(summary.to_string())
    }

    /// 朗读一段文本：合成到回复输出路径并立即播放。
    ///
    /// 供 TUI 在回合结束时直读助手回复用，**不经工具面**（因此不触发审批），
    /// 播放也不受 `auto_play` 约束——开启 `speak_replies` 就是要求出声。
    pub fn speak_text(&self, text: &str) -> Result<PathBuf, String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("文本为空，没有可朗读的内容。".to_string());
        }
        let request = SynthesisRequest {
            text,
            output_path: self.reply_output_path(),
        };
        let result = self.run(&request)?;
        play_wav(&result.audio_path, false);
        Ok(result.audio_path)
    }

    fn default_output_path(&self) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        self.output_directory().join(format!("tts_{stamp}.wav"))
    }

    /// 回合朗读的落盘路径：与工具产物分开命名，并用序号避开同秒内的覆盖。
    fn reply_output_path(&self) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let index = self.reply_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.output_directory()
            .join(format!("reply_{stamp}_{index}.wav"))
    }

    fn run(&self, request: &SynthesisRequest) -> Result<TtsResult, String> {
        synthesize(&self.backend, &request.text, &request.output_path)
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

/// 把两条通道的配置映射成合成后端（宿主在启动与保存设置后各调一次）。
pub fn tts_backend(
    cloud: &TtsCloudConfiguration,
    openai: omnicrawl_config::features::tts_cloud::TtsApiConfiguration,
    gemini: omnicrawl_config::features::tts_cloud::GeminiTtsConfiguration,
) -> TtsBackend {
    if cloud.provider == "gemini" {
        TtsBackend::Gemini(omnicrawl_tts::gemini::GeminiTtsConfig {
            base_url: gemini.base_url,
            api_key: gemini.api_key,
            model: gemini.model,
            voice: gemini.voice,
            instructions: gemini.instructions,
            timeout_seconds: gemini.timeout_seconds.max(1) as u64,
        })
    } else {
        TtsBackend::OpenAi(omnicrawl_tts::openai::OpenAiTtsConfig {
            base_url: openai.base_url,
            api_key: openai.api_key,
            model: openai.model,
            voice: openai.voice,
            instructions: openai.instructions,
            speed: openai.speed,
            timeout_seconds: openai.timeout_seconds.max(1) as u64,
        })
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
    use omnicrawl_config::features::tts_cloud::{GeminiTtsConfiguration, TtsApiConfiguration};

    fn options() -> TtsOptions {
        let configuration = TtsConfiguration {
            enabled: true,
            auto_play: false,
            ..TtsConfiguration::default()
        };
        let backend = tts_backend(
            &TtsCloudConfiguration::default(),
            TtsApiConfiguration {
                api_key: "test".to_string(),
                ..TtsApiConfiguration::default()
            },
            GeminiTtsConfiguration::default(),
        );
        TtsOptions::new(configuration, backend, PathBuf::from("/workspace"))
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
        let backend = tts_backend(
            &TtsCloudConfiguration::default(),
            TtsApiConfiguration::default(),
            GeminiTtsConfiguration::default(),
        );
        let options = TtsOptions::new(configuration, backend, PathBuf::from("/ws"));
        assert!(options.default_output_path().starts_with(absolute));
    }

    /// 通道由 `[tts_cloud].provider` 决定，音色跟着该通道的配置走。
    #[test]
    fn provider_selects_the_backend_and_its_voice() {
        let openai = tts_backend(
            &TtsCloudConfiguration {
                provider: "openai".to_string(),
            },
            TtsApiConfiguration {
                voice: "coral".to_string(),
                ..TtsApiConfiguration::default()
            },
            GeminiTtsConfiguration::default(),
        );
        assert_eq!(openai.provider(), "openai");
        assert_eq!(openai.voice(), "coral");

        let gemini = tts_backend(
            &TtsCloudConfiguration {
                provider: "gemini".to_string(),
            },
            TtsApiConfiguration::default(),
            GeminiTtsConfiguration {
                voice: "Puck".to_string(),
                ..GeminiTtsConfiguration::default()
            },
        );
        assert_eq!(gemini.provider(), "gemini");
        assert_eq!(gemini.voice(), "Puck");
    }

    /// 未声明的参数（工具面只认 text 与 path）不参与合成，多传也不影响结果。
    #[test]
    fn unknown_arguments_are_ignored() {
        let result = options().tts_synthesize(&arguments(&[
            ("text", "你好"),
            ("unknown_option", "ignored"),
        ]));
        // 没有真实网络，合成会失败；关键是它没把未知参数当成文本或路径。
        assert!(!result.ok);
        assert!(!result.output.contains("unknown_option"), "{}", result.output);
        assert!(!result.output.contains("text 不能为空"), "{}", result.output);
    }
}
