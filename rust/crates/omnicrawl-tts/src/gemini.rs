//! Gemini TTS 通道：`POST {base_url}/models/{model}:generateContent`。
//!
//! 两处与 OpenAI 通道不同：
//!
//! * **音频是 base64 包在 JSON 里**，默认还是裸 PCM（24kHz 单声道），不是容器文件；
//! * **没有独立的指令字段**，语气用自然语言写在正文前面（官方文档的用法）。
//!
//! 单次请求的文本上限按官方限制的 4000 字节取（比字符数更严），因此切块按字节算。

use std::io::Read;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::codec::{preview, truncate, AudioPayload};

/// 单次请求的文本上限（字节）。官方限制文本字段不超过 4000 字节。
pub const MAX_CHUNK_BYTES: usize = 3500;
/// Gemini TTS 输出的采样率（裸 PCM，16 位单声道）。
pub const SAMPLE_RATE: u32 = 24_000;

const USER_AGENT: &str = "omnicrawl-tts";

/// 已归一化的 Gemini 通道参数（由 `omnicrawl-config` 的 `[tts_gemini]` 段映射而来）。
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiTtsConfig {
    /// 接口根地址，末尾不带斜杠。
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub instructions: String,
    pub timeout_seconds: u64,
}

impl Default for GeminiTtsConfig {
    fn default() -> Self {
        Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            api_key: String::new(),
            model: "gemini-2.5-flash-preview-tts".to_string(),
            voice: "Kore".to_string(),
            instructions: String::new(),
            timeout_seconds: 120,
        }
    }
}

impl GeminiTtsConfig {
    pub fn generate_url(&self) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model.trim()
        )
    }

    /// 请求体：`responseModalities` 必须含 `AUDIO`，否则模型会回文本。
    ///
    /// 不下发 `languageCode`：让模型按输入文本自己判断该用哪种语言读。
    pub fn request_body(&self, text: &str) -> String {
        json!({
            "contents": [{ "parts": [{ "text": self.prompt_text(text) }] }],
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": {
                        "prebuiltVoiceConfig": { "voiceName": self.voice.trim() },
                    },
                },
            },
        })
        .to_string()
    }

    /// 语气指令作为正文前缀（Gemini 没有独立的指令字段）。
    pub fn prompt_text(&self, text: &str) -> String {
        let instructions = self.instructions.trim();
        if instructions.is_empty() {
            text.to_string()
        } else {
            format!("{instructions}\n\n{text}")
        }
    }

    /// 合成一段文本，返回音频字节（裸 PCM）与它的格式。
    pub fn synthesize(&self, text: &str) -> Result<(Vec<u8>, AudioPayload), String> {
        if self.api_key.trim().is_empty() {
            return Err(
                "Gemini 语音合成未配置密钥：请在设置面板（/settings → TTS）填写 API Key，\
                 或直接写 config.toml 的 [tts_gemini] api_key。"
                    .to_string(),
            );
        }
        let timeout = Duration::from_secs(self.timeout_seconds.max(1));
        let response = agent()
            .post(&self.generate_url())
            .config()
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_recv_body(Some(timeout))
            .build()
            .header("Content-Type", "application/json")
            .header("x-goog-api-key", self.api_key.trim())
            .header("User-Agent", USER_AGENT)
            .send(self.request_body(text))
            .map_err(|error| format!("Gemini 语音合成请求失败：{error}"))?;

        let status = response.status().as_u16();
        let mut bytes: Vec<u8> = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读取 Gemini 语音合成响应失败：{error}"))?;
        if status >= 400 {
            return Err(format!(
                "Gemini 语音合成返回 HTTP {status}：{}",
                preview(&bytes)
            ));
        }
        let audio = decode_audio(&bytes)?;
        Ok((audio, AudioPayload::Pcm16 { sample_rate: SAMPLE_RATE }))
    }
}

/// 从响应 JSON 里取出 `candidates[0].content.parts[].inlineData.data` 并解码。
///
/// 一个候选可能分成多个 part，按出现顺序拼接。`mimeType` 里的采样率被忽略：
/// Gemini TTS 的裸 PCM 固定 24kHz 单声道（官方文档），按这个口径解析。
fn decode_audio(body: &[u8]) -> Result<Vec<u8>, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("解析 Gemini 响应失败：{error}：{}", preview(body)))?;
    let parts = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut audio: Vec<u8> = Vec::new();
    for part in &parts {
        let Some(encoded) = part
            .get("inlineData")
            .and_then(|inline| inline.get("data"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("Gemini 返回的音频 base64 解码失败：{error}"))?;
        audio.extend_from_slice(&decoded);
    }
    if audio.is_empty() {
        // 被安全策略拦下时这里通常有 finishReason / promptFeedback，带出来便于排障。
        return Err(format!(
            "Gemini 响应里没有音频数据：{}",
            truncate(&String::from_utf8_lossy(body), 300)
        ));
    }
    Ok(audio)
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

    fn response_with(data: &[u8]) -> Vec<u8> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "inlineData": {"mimeType": "audio/L16;rate=24000", "data": encoded},
                    }],
                },
            }],
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn request_body_asks_for_audio_and_sets_voice() {
        let config = GeminiTtsConfig {
            voice: "Puck".to_string(),
            ..GeminiTtsConfig::default()
        };
        let body: Value = serde_json::from_str(&config.request_body("你好")).expect("解析请求体");
        assert_eq!(
            body["generationConfig"]["responseModalities"],
            json!(["AUDIO"])
        );
        let speech = &body["generationConfig"]["speechConfig"];
        assert_eq!(speech["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"], "Puck");
        // 朗读语言交给模型按输入文本自判：请求里不带 languageCode。
        assert!(
            speech.get("languageCode").is_none(),
            "不该下发语言：{speech}"
        );
        assert_eq!(body["contents"][0]["parts"][0]["text"], "你好");
    }

    #[test]
    fn instructions_are_prefixed_to_the_text() {
        let config = GeminiTtsConfig {
            instructions: "  用轻快的语气  ".to_string(),
            ..GeminiTtsConfig::default()
        };
        assert_eq!(config.prompt_text("你好"), "用轻快的语气\n\n你好");
    }

    #[test]
    fn generate_url_strips_trailing_slash() {
        let config = GeminiTtsConfig {
            base_url: "https://tts.example.com/v1beta/".to_string(),
            model: "gemini-2.5-flash-preview-tts".to_string(),
            ..GeminiTtsConfig::default()
        };
        assert_eq!(
            config.generate_url(),
            "https://tts.example.com/v1beta/models/gemini-2.5-flash-preview-tts:generateContent"
        );
    }

    #[test]
    fn audio_parts_are_decoded_and_concatenated() {
        let first = response_with(&[1, 2]);
        let value: Value = serde_json::from_slice(&first).expect("解析");
        let encoded = value["candidates"][0]["content"]["parts"][0]["inlineData"]["data"]
            .as_str()
            .expect("data")
            .to_string();
        let body = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"inlineData": {"data": encoded}},
                        {"inlineData": {"data": encoded}},
                    ],
                },
            }],
        })
        .to_string()
        .into_bytes();
        assert_eq!(decode_audio(&body).expect("解码"), vec![1, 2, 1, 2]);
    }

    #[test]
    fn missing_audio_reports_the_response() {
        let error = decode_audio(br#"{"promptFeedback":{"blockReason":"SAFETY"}}"#)
            .expect_err("没有音频应当报错");
        assert!(error.contains("没有音频数据"), "{error}");
        assert!(error.contains("SAFETY"), "{error}");
    }

    #[test]
    fn missing_api_key_is_reported_before_any_request() {
        let config = GeminiTtsConfig {
            api_key: String::new(),
            ..GeminiTtsConfig::default()
        };
        let error = config.synthesize("你好").expect_err("缺密钥应当报错");
        assert!(error.contains("未配置密钥"), "{error}");
    }
}
