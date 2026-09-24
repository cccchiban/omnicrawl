//! 语音合成接口后端：OpenAI 兼容 `POST {base_url}/audio/speech`。
//!
//! 与本地 ONNX 推理（[`crate::engine`]）并列的另一条链路，发布构建默认走这条：
//! `ort` 的预编译库不全（例如 musl 目标没有分发件），而接口后端只需要 HTTP。
//!
//! 几处刻意的取舍：
//!
//! * **只接受 WAV**：Windows 侧播放走 `winmm` 的 `PlaySoundW`（只认 WAV），
//!   Rust 侧也按 WAV 解析出时长与波形，换其它格式要么得引入解码器、要么只能落盘不能自检。
//! * **长文本自己分块**：OpenAI 的 `input` 有长度上限，按句末标点贪心切块，
//!   再把各段的波形拼起来（同一模型同一采样率，直接拼接即可）。
//! * **错误文案带服务端原文**：4xx/5xx 的响应体里通常写着真正的原因
//!   （模型名不对、音色不存在、余额不足），截断后放进错误信息里便于排障。

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::audio::{read_wav_bytes, write_wav, AudioBuffer};
use crate::result::TtsResult;

const USER_AGENT: &str = "omnicrawl-tts";
/// 单次请求的文本长度上限（字符）。OpenAI 的 `input` 上限是 4096 字符，
/// 这里留出余量，超过就按句末标点切块。
const MAX_CHUNK_CHARS: usize = 2000;
/// 错误响应体截断长度：够看清原因，又不至于把整页 HTML 塞进错误信息。
const ERROR_BODY_LIMIT: usize = 400;

/// 接口后端配置（宿主把 `[tts_api]` 段映射成它，与 `[tts]`→`TtsConfig` 同一路数）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApiTtsConfig {
    /// 接口根地址，末尾不带斜杠。
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    /// 目前只支持 `wav`（见模块文档）。
    pub response_format: String,
    /// 语速倍数；1.0 时不下发该字段。
    pub speed: f64,
    pub timeout_seconds: u64,
}

impl Default for ApiTtsConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini-tts".to_string(),
            voice: "alloy".to_string(),
            response_format: "wav".to_string(),
            speed: 1.0,
            timeout_seconds: 120,
        }
    }
}

impl ApiTtsConfig {
    /// 合成接口地址（`base_url` 末尾的斜杠在这里兜底裁掉）。
    pub fn speech_url(&self) -> String {
        format!("{}/audio/speech", self.base_url.trim_end_matches('/'))
    }

    /// 请求体：与 OpenAI `audio/speech` 一致，`speed == 1.0` 时不下发。
    fn request_body(&self, text: &str, voice: &str) -> String {
        let mut body = json!({
            "model": self.model,
            "input": text,
            "voice": voice,
            "response_format": self.response_format,
        });
        if (self.speed - 1.0).abs() > f64::EPSILON {
            body["speed"] = Value::from(self.speed);
        }
        body.to_string()
    }
}

/// 调接口合成一段语音并落盘，返回与本地引擎同构的结果。
///
/// 文本过长时按句末标点切块，逐块请求后把波形拼起来，最后只写一次 WAV。
/// `output_path` 需要是调用方已解析好的路径（宿主已经把相对目录接到工作区上）。
pub fn synthesize_speech(
    config: &ApiTtsConfig,
    text: &str,
    voice: Option<&str>,
    output_path: &Path,
) -> Result<TtsResult, String> {
    if config.api_key.trim().is_empty() {
        return Err(
            "语音合成接口未配置密钥：请在 [tts_api] 填 api_key，或配置 api_key_env 指向的环境变量（默认 OPENAI_API_KEY）。"
                .to_string(),
        );
    }
    if config.response_format != "wav" {
        return Err(format!(
            "语音合成接口只支持 wav，当前配置为 {}。",
            config.response_format
        ));
    }
    let effective_voice = voice
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or(&config.voice)
        .to_string();
    let chunks = split_text_for_api(text, MAX_CHUNK_CHARS);
    if chunks.is_empty() {
        return Err("text 不能为空。".to_string());
    }

    let agent = agent();
    let mut merged: AudioBuffer = Vec::new();
    let mut sample_rate = 0u32;
    for chunk in &chunks {
        let bytes = request_speech(config, &agent, chunk, &effective_voice)?;
        let (waveform, rate) = read_wav_bytes(&bytes)
            .map_err(|error| format!("解析接口返回的 WAV 失败：{error}"))?;
        if sample_rate == 0 {
            sample_rate = rate;
        } else if rate != sample_rate {
            return Err(format!(
                "接口返回的采样率不一致（{sample_rate} 与 {rate}），无法拼接。"
            ));
        }
        append_waveform(&mut merged, &waveform);
    }

    let audio_path = write_wav(output_path, &merged, sample_rate)?;
    let duration_seconds = TtsResult::duration_from_waveform(&merged, sample_rate);
    Ok(TtsResult {
        audio_path,
        sample_rate,
        waveform: merged,
        duration_seconds,
        // 接口合成没有 token 帧的概念，保持 0 而不是伪造一个数字。
        audio_token_frames: 0,
        text_chunks: chunks,
        sample_mode: "api".to_string(),
        voice: effective_voice,
    })
}

/// 请求一段文本的音频字节（WAV）。
fn request_speech(
    config: &ApiTtsConfig,
    agent: &ureq::Agent,
    text: &str,
    voice: &str,
) -> Result<Vec<u8>, String> {
    let timeout = Duration::from_secs(config.timeout_seconds.max(1));
    let response = agent
        .post(&config.speech_url())
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", config.api_key.trim()))
        .header("User-Agent", USER_AGENT)
        .send(config.request_body(text, voice))
        .map_err(|error| format!("语音合成接口请求失败：{error}"))?;

    let status = response.status().as_u16();
    let mut bytes: Vec<u8> = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取语音合成接口响应失败：{error}"))?;
    if status >= 400 {
        return Err(format!(
            "语音合成接口返回 HTTP {status}：{}",
            truncate(&String::from_utf8_lossy(&bytes), ERROR_BODY_LIMIT)
        ));
    }
    if !looks_like_wav(&bytes) {
        // 有些兼容服务在 200 里塞 JSON 错误（例如 {"error": ...}），这里把前若干字节带出来。
        return Err(format!(
            "语音合成接口没有返回 WAV 音频（{}）：{}",
            config.response_format,
            truncate(&String::from_utf8_lossy(&bytes), ERROR_BODY_LIMIT)
        ));
    }
    Ok(bytes)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

/// WAV 头判定：`RIFF....WAVE`。不合法就直接报错，避免把 JSON 错误当音频写盘。
fn looks_like_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

/// 把后一段波形接到前一段后面（声道数必须一致）。
fn append_waveform(merged: &mut AudioBuffer, chunk: &AudioBuffer) {
    if chunk.is_empty() {
        return;
    }
    if merged.is_empty() {
        *merged = chunk.to_vec();
        return;
    }
    if merged.len() != chunk.len() {
        // 声道数不一致时按最小声道数对齐，保证不 panic（正常不会发生）。
        merged.truncate(chunk.len().min(merged.len()));
    }
    for (lane, addition) in merged.iter_mut().zip(chunk.iter()) {
        lane.extend_from_slice(addition);
    }
}

/// 长文本切块：先按句末标点/换行切成句子，再贪心合并到不超过 `max_chars`；
/// 单句超限时按字符硬切，保证每块都不超过上限。
pub fn split_text_for_api(text: &str, max_chars: usize) -> Vec<String> {
    let limit = max_chars.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for sentence in split_sentences(text) {
        for piece in hard_split(&sentence, limit) {
            if current.chars().count() + piece.chars().count() > limit && !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            current.push_str(&piece);
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    // 纯空白输入不应产出空块。
    chunks.retain(|chunk| !chunk.trim().is_empty());
    chunks
}

/// 按句末标点与换行切句，标点跟着前一句（保留原有的停顿观感）。
fn split_sentences(text: &str) -> Vec<String> {
    const ENDINGS: [char; 8] = ['。', '！', '？', '；', '!', '?', ';', '\n'];
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if ENDINGS.contains(&ch) {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        sentences.push(current);
    }
    sentences
}

/// 单句超过上限时按字符硬切；未超限时原样返回。
fn hard_split(sentence: &str, limit: usize) -> Vec<String> {
    if sentence.chars().count() <= limit {
        return vec![sentence.to_string()];
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in sentence.chars() {
        current.push(ch);
        if current.chars().count() >= limit {
            pieces.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

/// 按字符边界截断（错误信息用），避免截出半个 UTF-8 字符。
fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(limit).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_a_single_chunk() {
        assert_eq!(split_text_for_api("你好，世界。", 100), vec!["你好，世界。"]);
    }

    #[test]
    fn long_text_is_split_on_sentence_endings_within_budget() {
        let text = "第一句。第二句。第三句。";
        let chunks = split_text_for_api(text, 8);
        assert_eq!(chunks, vec!["第一句。第二句。", "第三句。"]);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 8));
    }

    #[test]
    fn single_sentence_over_budget_is_hard_split() {
        let text = "一二三四五六七八九十";
        let chunks = split_text_for_api(text, 4);
        assert_eq!(chunks, vec!["一二三四", "五六七八", "九十"]);
    }

    #[test]
    fn blank_text_yields_no_chunks() {
        assert!(split_text_for_api("   \n  ", 100).is_empty());
    }

    #[test]
    fn request_body_omits_default_speed() {
        let config = ApiTtsConfig::default();
        let body: Value = serde_json::from_str(&config.request_body("你好", "alloy")).expect("解析");
        assert_eq!(body["model"], "gpt-4o-mini-tts");
        assert_eq!(body["input"], "你好");
        assert_eq!(body["voice"], "alloy");
        assert_eq!(body["response_format"], "wav");
        assert!(body.get("speed").is_none(), "1.0 不下发 speed");

        let faster = ApiTtsConfig {
            speed: 1.5,
            ..ApiTtsConfig::default()
        };
        let body: Value = serde_json::from_str(&faster.request_body("你好", "alloy")).expect("解析");
        assert_eq!(body["speed"], 1.5);
    }

    #[test]
    fn speech_url_strips_trailing_slash() {
        let config = ApiTtsConfig {
            base_url: "https://tts.example.com/v1/".to_string(),
            ..ApiTtsConfig::default()
        };
        assert_eq!(config.speech_url(), "https://tts.example.com/v1/audio/speech");
    }

    #[test]
    fn missing_api_key_is_reported_before_any_request() {
        let config = ApiTtsConfig {
            api_key: "   ".to_string(),
            ..ApiTtsConfig::default()
        };
        let error = synthesize_speech(&config, "你好", None, Path::new("out.wav"))
            .expect_err("缺密钥应当报错");
        assert!(error.contains("未配置密钥"), "{error}");
    }

    #[test]
    fn chunk_waveforms_are_concatenated_per_channel() {
        let mut merged = AudioBuffer::new();
        append_waveform(&mut merged, &vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        append_waveform(&mut merged, &vec![vec![5.0], vec![6.0]]);
        assert_eq!(merged, vec![vec![1.0, 2.0, 5.0], vec![3.0, 4.0, 6.0]]);
    }

    #[test]
    fn wav_header_detection_rejects_json_errors() {
        assert!(looks_like_wav(b"RIFF\x00\x00\x00\x00WAVEfmt "));
        assert!(!looks_like_wav(br#"{"error":{"message":"bad model"}}"#));
    }
}
