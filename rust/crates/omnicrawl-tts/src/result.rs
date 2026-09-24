//! 合成结果类型：本地 ONNX 推理与接口合成两条链路共用。
//!
//! 放在独立模块（而不是 `engine` 里）是因为发布构建默认**不编译**本地推理
//! （见 crate 文档的 `onnx` feature），而接口后端与宿主工具都要用这个结构。
//! 字段口径与 `omnicrawl/tts/engine.py` 的合成结果保持一致。

use std::path::PathBuf;

use crate::audio::AudioBuffer;

/// 一次合成（可能包含多块）的结果。
#[derive(Debug, Clone)]
pub struct TtsResult {
    pub audio_path: PathBuf,
    pub sample_rate: u32,
    /// 声道优先波形 `[channels, samples]`。
    pub waveform: AudioBuffer,
    pub duration_seconds: f64,
    /// 生成的音频 token 帧数；接口合成没有这个概念，恒为 0。
    pub audio_token_frames: usize,
    pub text_chunks: Vec<String>,
    pub sample_mode: String,
    pub voice: String,
}

impl TtsResult {
    /// 按采样率算时长（与 Python `len(waveform[0]) / sample_rate` 同口径）。
    pub fn duration_from_waveform(waveform: &AudioBuffer, sample_rate: u32) -> f64 {
        if sample_rate == 0 {
            return 0.0;
        }
        let frames = waveform.first().map(Vec::len).unwrap_or(0);
        frames as f64 / f64::from(sample_rate)
    }
}
