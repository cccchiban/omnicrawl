//! 云服务返回的音频字节解码：WAV（带 RIFF 头）与裸 PCM。

use crate::audio::{read_wav_bytes, AudioBuffer};

/// 一家的响应音频格式：决定怎么把字节变成波形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPayload {
    /// 带 RIFF 头的容器，采样率从头里读。
    Wav,
    /// 裸 PCM：样本按小端 16 位、单声道，采样率由调用方给定。
    Pcm16 { sample_rate: u32 },
}

impl AudioPayload {
    /// 把响应字节解码成（声道优先波形, 采样率）。
    pub fn decode(self, bytes: &[u8]) -> Result<(AudioBuffer, u32), String> {
        match self {
            AudioPayload::Wav => {
                if !looks_like_wav(bytes) {
                    return Err(format!(
                        "响应不是 WAV 音频：{}",
                        preview(bytes)
                    ));
                }
                read_wav_bytes(bytes)
            }
            AudioPayload::Pcm16 { sample_rate } => {
                if bytes.is_empty() {
                    return Err("响应没有音频数据。".to_string());
                }
                if bytes.len() % 2 != 0 {
                    return Err(format!(
                        "PCM 字节数不是偶数（{}），无法按 16 位样本解析。",
                        bytes.len()
                    ));
                }
                let samples: Vec<f32> = bytes
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
                    .collect();
                Ok((vec![samples], sample_rate))
            }
        }
    }
}

/// WAV 头判定：`RIFF....WAVE`。不合法就直接报错，避免把 JSON 错误当音频写盘。
pub fn looks_like_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

/// 字节前缀的可见预览（错误信息用）。
pub fn preview(bytes: &[u8]) -> String {
    truncate(&String::from_utf8_lossy(bytes), 300)
}

/// 按字符边界截断（错误信息用），避免截出半个 UTF-8 字符。
pub fn truncate(text: &str, limit: usize) -> String {
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
    fn pcm16_decodes_to_normalized_samples() {
        let bytes = [0x00, 0x00, 0xff, 0x7f, 0x00, 0x80];
        let (waveform, rate) = AudioPayload::Pcm16 { sample_rate: 24_000 }
            .decode(&bytes)
            .expect("解码应当成功");
        assert_eq!(rate, 24_000);
        assert_eq!(waveform.len(), 1);
        assert_eq!(waveform[0][0], 0.0);
        assert!((waveform[0][1] - 0.999_97).abs() < 1e-4);
        assert_eq!(waveform[0][2], -1.0);
    }

    #[test]
    fn odd_pcm_length_is_rejected() {
        let error = AudioPayload::Pcm16 { sample_rate: 24_000 }
            .decode(&[0x00, 0x01, 0x02])
            .expect_err("奇数长度应当报错");
        assert!(error.contains("不是偶数"), "{error}");
    }

    #[test]
    fn wav_payload_rejects_json_errors() {
        let error = AudioPayload::Wav
            .decode(br#"{"error":{"message":"bad model"}}"#)
            .expect_err("JSON 应当被拒绝");
        assert!(error.contains("不是 WAV 音频"), "{error}");
    }
}
