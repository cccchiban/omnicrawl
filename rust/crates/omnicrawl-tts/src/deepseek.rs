//! DeepSeek TTS 通道：`/v1/tools/tts` 的整段与 WebSocket 流式两种传输。
//!
//! 两条通路的输入输出口径不同：
//!
//! * **整段**（`POST {base_url}`）：JSON 请求体一次给全文，响应体是音频字节
//!   （`format` 默认 `wav`）。
//! * **流式**（`wss://{base_url}`，**默认**）：按 `{"event":"text","text":...}` 分段喂文字，
//!   最后发 `{"event":"finish_text"}`；音频以二进制帧下发，会话以
//!   `{"event":"finish","code":...,"message":...}` 结束，`code` 为 `success` 才算成功。
//!
//! 官方两种传输的单次上限都是 10,000 计费字符（汉字算 2 个），因此切块按**计费字符**算，
//! 这与另外两家按「字符 / 字节」的口径不同。

use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::codec::{preview, truncate, AudioPayload};
use crate::ws::{connect_wss, WsMessage, WsPoll};

/// 单次请求的计费字符上限（官方限制 10,000，留出余量）。
pub const MAX_BILLED_CHARS: usize = 9_000;

/// DeepSeek TTS 输出：流式默认裸 PCM，24kHz 16 位单声道；整段默认 WAV。
pub const SAMPLE_RATE: u32 = 24_000;

/// 会话最长 30 分钟；本地按配置的超时兜底，不把 30 分钟当默认等待。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

const USER_AGENT: &str = omnicrawl_version::USER_AGENT;

/// 传输方式：流式走 WebSocket（默认），整段走一次 POST。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepSeekTransport {
    WebSocket,
    Http,
}

impl DeepSeekTransport {
    pub fn parse(text: &str) -> DeepSeekTransport {
        match text.trim().to_lowercase().as_str() {
            "http" => DeepSeekTransport::Http,
            _ => DeepSeekTransport::WebSocket,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DeepSeekTransport::WebSocket => "websocket",
            DeepSeekTransport::Http => "http",
        }
    }
}

/// 已归一化的 DeepSeek 通道参数（由 `omnicrawl-config` 的 `[tts_deepseek]` 段映射而来）。
#[derive(Debug, Clone, PartialEq)]
pub struct DeepSeekTtsConfig {
    /// 接口地址，末尾不带斜杠（`https://api.deepseek.com/v1/tools/tts`）。
    pub base_url: String,
    pub api_key: String,
    /// 音色：`stella`（女声，默认）或 `tide`（男声）。
    pub voice: String,
    pub transport: DeepSeekTransport,
    /// 朗读前是否去掉 Markdown 格式。
    pub filter_markdown: bool,
    pub timeout_seconds: u64,
}

impl Default for DeepSeekTtsConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1/tools/tts".to_string(),
            api_key: String::new(),
            voice: "stella".to_string(),
            transport: DeepSeekTransport::WebSocket,
            filter_markdown: true,
            timeout_seconds: 120,
        }
    }
}

impl DeepSeekTtsConfig {
    /// 流式地址：`wss://` + 路径 + 查询参数（`voice_id` / `format` / `filter_markdown`）。
    ///
    /// 官方文档把 `format` 的流式默认值写成 `pcm`，这里显式下发，避免依赖服务端默认。
    pub fn websocket_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        let base = base
            .strip_prefix("https://")
            .map(|rest| format!("wss://{rest}"))
            .or_else(|| base.strip_prefix("http://").map(|rest| format!("ws://{rest}")))
            .unwrap_or_else(|| base.to_string());
        format!(
            "{base}?voice_id={}&format=pcm&filter_markdown={}",
            url_encode(&self.voice),
            self.filter_markdown
        )
    }

    /// 整段请求体：`text` 必填，其余按配置下发。
    pub fn request_body(&self, text: &str) -> String {
        json!({
            "text": text,
            "voice_id": self.voice.trim(),
            "format": "wav",
            "filter_markdown": self.filter_markdown,
        })
        .to_string()
    }

    /// 合成一段文本，按配置的传输取回音频字节与它的格式。
    pub fn synthesize(&self, text: &str) -> Result<(Vec<u8>, AudioPayload), String> {
        if self.api_key.trim().is_empty() {
            return Err(
                "DeepSeek 语音合成未配置密钥：请在设置面板（/settings → TTS）填写 API Key，\
                 或直接写 config.toml 的 [tts_deepseek] api_key。"
                    .to_string(),
            );
        }
        match self.transport {
            DeepSeekTransport::WebSocket => self.synthesize_stream(text),
            DeepSeekTransport::Http => self.synthesize_once(text),
        }
    }

    /// 整段合成：一次 POST，响应体就是音频。
    fn synthesize_once(&self, text: &str) -> Result<(Vec<u8>, AudioPayload), String> {
        let timeout = Duration::from_secs(self.timeout_seconds.max(1));
        let response = agent()
            .post(&self.base_url)
            .config()
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_recv_body(Some(timeout))
            .build()
            .header("Content-Type", "application/json")
            .header("Authorization", &format!("Bearer {}", self.api_key.trim()))
            .header("User-Agent", USER_AGENT)
            .send(self.request_body(text))
            .map_err(|error| format!("DeepSeek 语音合成请求失败：{error}"))?;

        let status = response.status().as_u16();
        let mut bytes: Vec<u8> = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读取 DeepSeek 语音合成响应失败：{error}"))?;
        if status >= 400 {
            return Err(format!(
                "DeepSeek 语音合成返回 HTTP {status}：{}",
                preview(&bytes)
            ));
        }
        Ok((bytes, AudioPayload::Wav))
    }

    /// 流式合成：把整段文字一次发送后收齐二进制音频帧。
    ///
    /// 单块文本已经由调用方切到计费上限以内，因此不再按句分批发——一次
    /// `text` + 一次 `finish_text` 就能让服务端边合成边回音频。
    fn synthesize_stream(&self, text: &str) -> Result<(Vec<u8>, AudioPayload), String> {
        let timeout = Duration::from_secs(self.timeout_seconds.max(1));
        let authorization = format!("Bearer {}", self.api_key.trim());
        let mut client = connect_wss(
            &self.websocket_url(),
            &[("Authorization", authorization.as_str())],
            POLL_INTERVAL,
            USER_AGENT,
        )?;

        client.send_text(&json!({ "event": "text", "text": text }).to_string())?;
        client.send_text(&json!({ "event": "finish_text" }).to_string())?;

        let deadline = Instant::now() + timeout;
        let mut audio: Vec<u8> = Vec::new();
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "DeepSeek 流式合成超时（{} 秒内未收齐音频）。",
                    timeout.as_secs()
                ));
            }
            match client.poll()? {
                WsPoll::Idle => continue,
                WsPoll::Message(WsMessage::Binary(chunk)) => audio.extend_from_slice(&chunk),
                WsPoll::Message(WsMessage::Text(event)) => {
                    if let Some(error) = finish_error(&event) {
                        return Err(error);
                    }
                    // code=success 的 finish 就是会话终点：音频已收齐，不必再等连接关闭
                    // （服务端不一定会主动 close，等下去只会空转到超时）。
                    if is_finish(&event) {
                        break;
                    }
                }
                WsPoll::Message(WsMessage::Close) => break,
            }
        }
        if audio.is_empty() {
            return Err("DeepSeek 流式合成没有产出音频。".to_string());
        }
        Ok((
            audio,
            AudioPayload::Pcm16 {
                sample_rate: SAMPLE_RATE,
            },
        ))
    }
}

/// 这条文本事件是否是会话终点消息。
pub fn is_finish(event: &str) -> bool {
    matches!(
        serde_json::from_str::<Value>(event).ok().as_ref().and_then(|value| {
            value.get("event").and_then(Value::as_str)
        }),
        Some("finish")
    )
}

/// 解析一条文本事件：是 `finish` 且 `code != success` 就给错误文案。
///
/// 中途的其它事件（如进度）不参与判定；`code=success` 的 `finish` 由 [`is_finish`] 判定收尾。
pub fn finish_error(event: &str) -> Option<String> {
    let value: Value = serde_json::from_str(event).ok()?;
    if value.get("event").and_then(Value::as_str)? != "finish" {
        return None;
    }
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if code == "success" {
        return None;
    }
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(format!(
        "DeepSeek 流式合成失败（{code}）：{}",
        truncate(message, 300)
    ))
}

/// 查询参数里的最小百分号编码：只保 `A-Za-z0-9._-`，其余按字节转义。
fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.trim().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 计费字符数：汉字算 2，其余算 1（官方计费口径，也是长度上限的口径）。
pub fn billed_chars(text: &str) -> usize {
    text.chars()
        .map(|ch| if is_wide(ch) { 2 } else { 1 })
        .sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x3040..=0x30FF
        | 0xAC00..=0xD7AF | 0x20000..=0x3FFFF)
}

/// 共用的 ureq 代理：4xx/5xx 不当作传输错误，交给调用方读状态码与响应体。
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_uses_wss_and_query_parameters() {
        let config = DeepSeekTtsConfig {
            voice: "tide".to_string(),
            ..DeepSeekTtsConfig::default()
        };
        assert_eq!(
            config.websocket_url(),
            "wss://api.deepseek.com/v1/tools/tts?voice_id=tide&format=pcm&filter_markdown=true"
        );
    }

    #[test]
    fn websocket_url_strips_trailing_slash_and_keeps_http_hosts() {
        let config = DeepSeekTtsConfig {
            base_url: "http://127.0.0.1:8080/v1/tools/tts/".to_string(),
            ..DeepSeekTtsConfig::default()
        };
        assert_eq!(
            config.websocket_url(),
            "ws://127.0.0.1:8080/v1/tools/tts?voice_id=stella&format=pcm&filter_markdown=true"
        );
    }

    #[test]
    fn stream_transport_is_the_default() {
        assert_eq!(
            DeepSeekTtsConfig::default().transport,
            DeepSeekTransport::WebSocket
        );
    }

    #[test]
    fn request_body_carries_text_voice_and_format() {
        let config = DeepSeekTtsConfig {
            voice: "tide".to_string(),
            filter_markdown: false,
            ..DeepSeekTtsConfig::default()
        };
        let body: Value = serde_json::from_str(&config.request_body("你好")).expect("解析请求体");
        assert_eq!(body["text"], "你好");
        assert_eq!(body["voice_id"], "tide");
        assert_eq!(body["format"], "wav");
        assert_eq!(body["filter_markdown"], false);
    }

    #[test]
    fn finish_success_is_not_an_error() {
        assert_eq!(finish_error(r#"{"event":"finish","code":"success"}"#), None);
    }

    #[test]
    fn finish_failure_carries_code_and_message() {
        let error = finish_error(
            r#"{"event":"finish","code":"content_filter","message":"内容不合规"}"#,
        )
        .expect("应当报错");
        assert!(error.contains("content_filter"), "{error}");
        assert!(error.contains("内容不合规"), "{error}");
    }

    #[test]
    fn non_finish_events_are_ignored() {
        assert_eq!(finish_error(r#"{"event":"text","text":"hi"}"#), None);
        assert_eq!(finish_error("not json"), None);
    }

    #[test]
    fn billed_chars_count_han_as_two() {
        assert_eq!(billed_chars("ab"), 2);
        assert_eq!(billed_chars("你好"), 4);
        assert_eq!(billed_chars("你好 ab"), 7);
    }

    /// 非 `finish` 事件不参与判定。
    #[test]
    fn only_finish_events_are_terminal() {
        assert!(is_finish(r#"{"event":"finish","code":"success"}"#));
        assert!(is_finish(r#"{"event":"finish","code":"service_error","message":"x"}"#));
        assert!(!is_finish(r#"{"event":"text","text":"hi"}"#));
        assert!(!is_finish("not json"));
    }

    #[test]
    fn missing_api_key_is_reported_before_any_request() {
        let config = DeepSeekTtsConfig {
            api_key: "   ".to_string(),
            ..DeepSeekTtsConfig::default()
        };
        let error = config.synthesize("你好").expect_err("缺密钥应当报错");
        assert!(error.contains("未配置密钥"), "{error}");
    }

    #[test]
    fn transport_parsing_falls_back_to_streaming() {
        assert_eq!(
            DeepSeekTransport::parse("HTTP"),
            DeepSeekTransport::Http
        );
        assert_eq!(
            DeepSeekTransport::parse(""),
            DeepSeekTransport::WebSocket
        );
        assert_eq!(
            DeepSeekTransport::parse("websocket"),
            DeepSeekTransport::WebSocket
        );
    }
}
