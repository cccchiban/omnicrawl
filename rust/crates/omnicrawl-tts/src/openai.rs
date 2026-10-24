//! OpenAI 兼容的 `POST {base_url}/audio/speech` 通道。
//!
//! 请求体只发服务端认识的键：`speed == 1.0` 与空 `instructions` 都不下发，
//! 免得把 `tts-1` 这类不支持 `instructions` 的模型带进 400。

use std::io::Read;
use std::time::Duration;

use serde_json::json;

use crate::codec::{preview, AudioPayload};

/// 单次请求的文本长度上限（字符）。接口上限是 4096，这里留出余量。
pub const MAX_CHUNK_CHARS: usize = 2000;

const USER_AGENT: &str = "omnicrawl-tts";

/// 已归一化的 OpenAI 通道参数（由 `omnicrawl-config` 的 `[tts_api]` 段映射而来）。
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiTtsConfig {
    /// 接口根地址，末尾不带斜杠。
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub instructions: String,
    /// 语速倍数；1.0 时不下发该字段。
    pub speed: f64,
    pub timeout_seconds: u64,
}

impl Default for OpenAiTtsConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini-tts".to_string(),
            voice: "alloy".to_string(),
            instructions: String::new(),
            speed: 1.0,
            timeout_seconds: 120,
        }
    }
}

impl OpenAiTtsConfig {
    pub fn speech_url(&self) -> String {
        format!("{}/audio/speech", self.base_url.trim_end_matches('/'))
    }

    /// 请求体：`speed` 与 `instructions` 为默认/空时不下发。
    pub fn request_body(&self, text: &str, voice: &str) -> String {
        let mut body = json!({
            "model": self.model,
            "input": text,
            "voice": voice,
            // 只接受 WAV：本地播放走 `winmm`（只认 WAV），且能直接解析出时长。
            "response_format": "wav",
        });
        if (self.speed - 1.0).abs() > f64::EPSILON {
            body["speed"] = json!(self.speed);
        }
        if !self.instructions.trim().is_empty() {
            body["instructions"] = json!(self.instructions.trim());
        }
        body.to_string()
    }

    /// 合成一段文本，返回音频字节（WAV）与它的格式。
    pub fn synthesize(&self, text: &str, voice: &str) -> Result<(Vec<u8>, AudioPayload), String> {
        if self.api_key.trim().is_empty() {
            return Err(
                "OpenAI 语音合成未配置密钥：请在设置面板（/settings → TTS）填写 API Key，\
                 或直接写 config.toml 的 [tts_api] api_key。"
                    .to_string(),
            );
        }
        let timeout = Duration::from_secs(self.timeout_seconds.max(1));
        let response = agent()
            .post(&self.speech_url())
            .config()
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_recv_body(Some(timeout))
            .build()
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {}", self.api_key.trim()))
            .header("User-Agent", USER_AGENT)
            .send(self.request_body(text, voice))
            .map_err(|error| format!("OpenAI 语音合成请求失败：{error}"))?;

        let status = response.status().as_u16();
        let mut bytes: Vec<u8> = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读取 OpenAI 语音合成响应失败：{error}"))?;
        if status >= 400 {
            return Err(format!(
                "OpenAI 语音合成返回 HTTP {status}：{}",
                preview(&bytes)
            ));
        }
        Ok((bytes, AudioPayload::Wav))
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn request_body_omits_defaults() {
        let config = OpenAiTtsConfig::default();
        let body: Value =
            serde_json::from_str(&config.request_body("你好", "alloy")).expect("解析请求体");
        assert_eq!(body["model"], "gpt-4o-mini-tts");
        assert_eq!(body["voice"], "alloy");
        assert_eq!(body["response_format"], "wav");
        assert!(body.get("speed").is_none(), "1.0 不下发 speed");
        assert!(body.get("instructions").is_none(), "空指令不下发");
    }

    #[test]
    fn request_body_carries_speed_and_instructions() {
        let config = OpenAiTtsConfig {
            speed: 1.5,
            instructions: "  用轻快的语气  ".to_string(),
            ..OpenAiTtsConfig::default()
        };
        let body: Value =
            serde_json::from_str(&config.request_body("你好", "coral")).expect("解析请求体");
        assert_eq!(body["speed"], 1.5);
        assert_eq!(body["instructions"], "用轻快的语气");
    }

    #[test]
    fn speech_url_strips_trailing_slash() {
        let config = OpenAiTtsConfig {
            base_url: "https://tts.example.com/v1/".to_string(),
            ..OpenAiTtsConfig::default()
        };
        assert_eq!(
            config.speech_url(),
            "https://tts.example.com/v1/audio/speech"
        );
    }

    #[test]
    fn missing_api_key_is_reported_before_any_request() {
        let config = OpenAiTtsConfig {
            api_key: "   ".to_string(),
            ..OpenAiTtsConfig::default()
        };
        let error = config
            .synthesize("你好", "alloy")
            .expect_err("缺密钥应当报错");
        assert!(error.contains("未配置密钥"), "{error}");
    }
}
