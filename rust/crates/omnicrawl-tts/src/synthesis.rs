//! 合成入口：按服务商选路、长文本分块、波形拼接与落盘。
//!
//! 两家都有单次请求的长度上限，因此统一「切块 → 逐块请求 → 沿时间轴拼接 → 只写一次 WAV」。
//! 采样率不一致时直接报错而不是静默重采样：同一家同一模型不该出现这种情况，出现了就是
//! 配置或服务端行为变了，报出来比听一段变速音频好。

use std::path::Path;

use crate::audio::{write_wav, AudioBuffer};
use crate::gemini::GeminiTtsConfig;
use crate::openai::OpenAiTtsConfig;
use crate::result::TtsResult;
use crate::text::split_text;

/// 合成通道（与 `[tts_cloud].provider` 一一对应）。
#[derive(Debug, Clone, PartialEq)]
pub enum TtsBackend {
    OpenAi(OpenAiTtsConfig),
    Gemini(GeminiTtsConfig),
}

impl TtsBackend {
    /// 服务商名（结果里回报用）。
    pub fn provider(&self) -> &'static str {
        match self {
            TtsBackend::OpenAi(_) => "openai",
            TtsBackend::Gemini(_) => "gemini",
        }
    }

    /// 实际使用的音色名。
    pub fn voice(&self) -> &str {
        match self {
            TtsBackend::OpenAi(config) => &config.voice,
            TtsBackend::Gemini(config) => &config.voice,
        }
    }
}

/// 合成一段文本并落盘；文本过长时自动分块。
///
/// `output_path` 必须是调用方解析好的路径（宿主已经把相对目录接到工作区上）。
pub fn synthesize(
    backend: &TtsBackend,
    text: &str,
    output_path: &Path,
) -> Result<TtsResult, String> {
    // Gemini 的文本上限按字节算，因此切块上限用它自己的口径。
    let limit = match backend {
        TtsBackend::OpenAi(_) => crate::openai::MAX_CHUNK_CHARS,
        TtsBackend::Gemini(config) => chunk_chars_for_bytes(config),
    };
    let chunks = split_text(text, limit);
    if chunks.is_empty() {
        return Err("text 不能为空。".to_string());
    }

    let mut merged: AudioBuffer = Vec::new();
    let mut sample_rate = 0u32;
    for chunk in &chunks {
        let (bytes, payload) = match backend {
            TtsBackend::OpenAi(config) => config.synthesize(chunk, &config.voice)?,
            TtsBackend::Gemini(config) => config.synthesize(chunk)?,
        };
        let (waveform, rate) = payload.decode(&bytes).map_err(|error| {
            format!(
                "解析{}返回的音频失败：{error}",
                provider_label(backend.provider())
            )
        })?;
        if sample_rate == 0 {
            sample_rate = rate;
        } else if rate != sample_rate {
            return Err(format!(
                "各分块返回的采样率不一致（{sample_rate} 与 {rate}），无法拼接。"
            ));
        }
        append_waveform(&mut merged, &waveform);
    }
    if sample_rate == 0 {
        return Err("合成没有产出音频。".to_string());
    }

    let audio_path = write_wav(output_path, &merged, sample_rate)?;
    let duration_seconds = TtsResult::duration_from_waveform(&merged, sample_rate);
    Ok(TtsResult {
        audio_path,
        sample_rate,
        waveform: merged,
        duration_seconds,
        text_chunks: chunks,
        voice: backend.voice().to_string(),
    })
}

/// Gemini 的切块上限：文本字段限 4000 字节，按字符数换算并留出余量。
fn chunk_chars_for_bytes(config: &GeminiTtsConfig) -> usize {
    let bytes = crate::gemini::MAX_CHUNK_BYTES.saturating_sub(config.instructions.trim().len());
    // UTF-8 一个汉字 3 字节，取字节数的三分之一保证不越界，且至少留 1 个字符。
    (bytes / 3).max(1)
}

fn provider_label(provider: &str) -> &'static str {
    match provider {
        "gemini" => "Gemini",
        _ => "OpenAI",
    }
}

/// 把后一段波形接到前一段后面（声道数不一致时按最小声道数对齐，正常不会发生）。
fn append_waveform(merged: &mut AudioBuffer, chunk: &AudioBuffer) {
    if chunk.is_empty() {
        return;
    }
    if merged.is_empty() {
        *merged = chunk.to_vec();
        return;
    }
    if merged.len() != chunk.len() {
        merged.truncate(chunk.len().min(merged.len()));
    }
    for (lane, addition) in merged.iter_mut().zip(chunk.iter()) {
        lane.extend_from_slice(addition);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_waveforms_are_concatenated_per_channel() {
        let mut merged = AudioBuffer::new();
        append_waveform(&mut merged, &vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        append_waveform(&mut merged, &vec![vec![5.0], vec![6.0]]);
        assert_eq!(merged, vec![vec![1.0, 2.0, 5.0], vec![3.0, 4.0, 6.0]]);
    }

    #[test]
    fn backend_reports_provider_and_voice() {
        let openai = TtsBackend::OpenAi(OpenAiTtsConfig {
            voice: "coral".to_string(),
            ..OpenAiTtsConfig::default()
        });
        assert_eq!(openai.provider(), "openai");
        assert_eq!(openai.voice(), "coral");

        let gemini = TtsBackend::Gemini(GeminiTtsConfig::default());
        assert_eq!(gemini.provider(), "gemini");
        assert_eq!(gemini.voice(), "Kore");
    }

    #[test]
    fn gemini_chunk_budget_shrinks_with_instructions() {
        let plain = chunk_chars_for_bytes(&GeminiTtsConfig::default());
        let verbose = chunk_chars_for_bytes(&GeminiTtsConfig {
            instructions: "用轻快的语气朗读".repeat(20),
            ..GeminiTtsConfig::default()
        });
        assert!(plain > verbose, "{plain} 应当大于 {verbose}");
        assert!(verbose >= 1);
    }

    #[test]
    fn empty_text_is_rejected() {
        let backend = TtsBackend::OpenAi(OpenAiTtsConfig {
            api_key: "test".to_string(),
            ..OpenAiTtsConfig::default()
        });
        let error = synthesize(&backend, "   ", Path::new("out.wav")).expect_err("空文本应当报错");
        assert_eq!(error, "text 不能为空。");
    }
}
