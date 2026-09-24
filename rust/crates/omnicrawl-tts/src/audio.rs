//! 音频 I/O：`omnicrawl/tts/audio.py` 的等价实现（不依赖 numpy）。
//!
//! 波形统一用「声道优先」的 `Vec<Vec<f32>>`（外层声道、内层采样）表示，与 Python 的
//! `[channels, samples]` 布局一致；取值域 [-1, 1]。

use std::path::{Path, PathBuf};

/// 声道优先的波形：[声道][采样]。
pub type AudioBuffer = Vec<Vec<f32>>;

fn python_round(value: f64) -> f64 {
    if (value - value.trunc()).abs() == 0.5 {
        let floor = value.floor();
        return if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        };
    }
    value.round()
}

fn expand_user(path: &str) -> String {
    if path == "~" {
        return home_directory().to_string_lossy().to_string();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home_directory().join(rest).to_string_lossy().to_string(),
        None => path.to_string(),
    }
}

fn home_directory() -> PathBuf {
    for name in ["USERPROFILE", "HOME"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

fn resolve_path(path: &Path) -> PathBuf {
    crate::paths::resolve_lenient(&PathBuf::from(expand_user(&path.to_string_lossy())))
}

/// 读取 WAV 文件，返回（声道优先波形, 采样率）；支持 8/16/24/32 位 PCM。
pub fn read_wav(path: &Path) -> Result<(AudioBuffer, u32), String> {
    let path = resolve_path(path);
    let bytes = std::fs::read(&path).map_err(|error| format!("{error}"))?;
    read_wav_bytes(&bytes)
}

/// 从内存里的 WAV 字节解码出（声道优先波形, 采样率）。
///
/// 与 [`read_wav`] 同一套解码逻辑，供不需要落盘临时文件的调用方使用
/// （例如接口合成直接拿到响应体就地解析）。支持 8/16/24/32 位 PCM。
pub fn read_wav_bytes(bytes: &[u8]) -> Result<(AudioBuffer, u32), String> {
    let (channels, sample_width, sample_rate, raw) = parse_wav(bytes)?;
    if channels == 0 {
        return Err("WAV 声道数为 0。".to_string());
    }
    let samples = decode_samples(sample_width, &raw)?;
    let frames = samples.len() / channels;
    let mut waveform: AudioBuffer = vec![Vec::with_capacity(frames); channels];
    for frame in 0..frames {
        for (channel, lane) in waveform.iter_mut().enumerate() {
            lane.push(samples[frame * channels + channel]);
        }
    }
    Ok((waveform, sample_rate))
}

/// 仅解析 RIFF/WAVE 的 `fmt ` 与 `data` 块；非 PCM（wave 模块同款限制）直接报错。
fn parse_wav(bytes: &[u8]) -> Result<(usize, usize, u32, Vec<u8>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("不是合法的 WAV 文件。".to_string());
    }
    let mut offset = 12usize;
    let mut format: Option<(u16, usize, u32)> = None;
    while offset + 8 <= bytes.len() {
        let chunk_id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let body_start = offset + 8;
        if body_start + size > bytes.len() {
            break;
        }
        match chunk_id {
            b"fmt " => {
                if size < 16 {
                    return Err("WAV 的 fmt 块过短。".to_string());
                }
                let audio_format = u16::from_le_bytes([bytes[body_start], bytes[body_start + 1]]);
                let channels =
                    u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]) as usize;
                let sample_rate = u32::from_le_bytes([
                    bytes[body_start + 4],
                    bytes[body_start + 5],
                    bytes[body_start + 6],
                    bytes[body_start + 7],
                ]);
                let bits =
                    u16::from_le_bytes([bytes[body_start + 14], bytes[body_start + 15]]) as usize;
                if audio_format != 1 {
                    return Err(format!("不支持的 WAV 格式（编码 {audio_format}）。"));
                }
                format = Some((bits as u16, channels, sample_rate));
            }
            b"data" => {
                let Some((bits, channels, sample_rate)) = format else {
                    return Err("WAV 的 data 块出现在 fmt 块之前。".to_string());
                };
                let width = (bits as usize).div_ceil(8);
                return Ok((
                    channels,
                    width,
                    sample_rate,
                    bytes[body_start..body_start + size].to_vec(),
                ));
            }
            _ => {}
        }
        offset = body_start + size + (size % 2);
    }
    Err("WAV 缺少 data 块。".to_string())
}

fn decode_samples(sample_width: usize, raw: &[u8]) -> Result<Vec<f32>, String> {
    match sample_width {
        1 => Ok(raw
            .iter()
            .map(|byte| (*byte as f32 - 128.0) / 128.0)
            .collect()),
        2 => Ok(raw
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
            .collect()),
        3 => Ok(raw
            .chunks_exact(3)
            .map(|triple| {
                let value = i32::from_le_bytes([triple[0], triple[1], triple[2], 0]);
                let signed = ((value + (1 << 23)) % (1 << 24)) - (1 << 23);
                signed as f32 / 8_388_608.0
            })
            .collect()),
        4 => Ok(raw
            .chunks_exact(4)
            .map(|quad| {
                i32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]) as f32 / 2_147_483_648.0
            })
            .collect()),
        other => Err(format!(
            "不支持的 WAV 位深：{} 位（仅支持 8/16/24/32 位 PCM）。",
            other * 8
        )),
    }
}

/// 线性插值重采样；采样率相同或非正时原样返回。
pub fn resample_linear(waveform: &AudioBuffer, source_rate: u32, target_rate: u32) -> AudioBuffer {
    if source_rate == target_rate || source_rate == 0 || target_rate == 0 {
        return waveform.clone();
    }
    let source_length = waveform.first().map(Vec::len).unwrap_or(0);
    if source_length == 0 {
        return waveform.clone();
    }
    let target_length = (python_round(
        source_length as f64 * target_rate as f64 / source_rate as f64,
    ) as i64)
        .max(1) as usize;
    let step = source_rate as f64 / target_rate as f64;
    let mut result: AudioBuffer = Vec::with_capacity(waveform.len());
    for lane in waveform {
        let mut channel = Vec::with_capacity(target_length);
        for index in 0..target_length {
            let position = (index as f64 * step).clamp(0.0, (source_length - 1) as f64);
            let left = position.floor() as usize;
            let right = (left + 1).min(source_length - 1);
            let fraction = (position - left as f64) as f32;
            let sample = lane[left] * (1.0 - fraction) + lane[right] * fraction;
            channel.push(sample);
        }
        result.push(channel);
    }
    result
}

/// 加载语音克隆参考音频：重采样 + 声道转换（单声道复制、多声道取均值）。
pub fn load_reference_audio(
    path: &Path,
    target_sample_rate: u32,
    target_channels: usize,
) -> Result<AudioBuffer, String> {
    let (mut waveform, sample_rate) = read_wav(path)?;
    if sample_rate != target_sample_rate {
        waveform = resample_linear(&waveform, sample_rate, target_sample_rate);
    }
    let current_channels = waveform.len();
    if current_channels == target_channels {
        return Ok(waveform);
    }
    if current_channels == 1 && target_channels > 1 {
        return Ok(vec![waveform[0].clone(); target_channels]);
    }
    if current_channels > 1 && target_channels == 1 {
        let frames = waveform[0].len();
        let mut mixed = Vec::with_capacity(frames);
        for frame in 0..frames {
            let total: f32 = waveform.iter().map(|lane| lane[frame]).sum();
            mixed.push(total / current_channels as f32);
        }
        return Ok(vec![mixed]);
    }
    Err(format!(
        "不支持的参考音频声道转换：{current_channels} -> {target_channels}"
    ))
}

/// 把波形写成 16 位 PCM WAV；波形为声道优先布局。
pub fn write_wav(path: &Path, waveform: &AudioBuffer, sample_rate: u32) -> Result<PathBuf, String> {
    let output_path = resolve_path(path);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{error}"))?;
    }
    if waveform.is_empty() {
        return Err("波形维度必须为 1 或 2，当前：0".to_string());
    }
    let channels = waveform.len();
    let frames = waveform[0].len();
    let mut interleaved: Vec<i16> = Vec::with_capacity(channels * frames);
    for frame in 0..frames {
        for lane in waveform {
            let value = lane.get(frame).copied().unwrap_or(0.0);
            let clipped = value.clamp(-1.0, 1.0) as f64;
            interleaved.push(python_round(clipped * 32767.0) as i16);
        }
    }
    let mut data = Vec::with_capacity(interleaved.len() * 2);
    for sample in &interleaved {
        data.extend_from_slice(&sample.to_le_bytes());
    }

    let mut header: Vec<u8> = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&(channels as u16).to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * channels as u32 * 2;
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&((channels * 2) as u16).to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data.len() as u32).to_le_bytes());

    header.extend_from_slice(&data);
    std::fs::write(&output_path, header).map_err(|error| format!("{error}"))?;
    Ok(output_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tts-audio-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时目录");
        root
    }

    #[test]
    fn write_then_read_round_trips_mono_pcm16() {
        let root = temp_dir("round-trip");
        let path = root.join("mono.wav");
        let waveform: AudioBuffer = vec![vec![0.0, 0.5, -0.5, 1.0, -1.0]];
        write_wav(&path, &waveform, 16_000).expect("写入应当成功");
        let (read_back, rate) = read_wav(&path).expect("读取应当成功");
        assert_eq!(rate, 16_000);
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].len(), waveform[0].len());
        for (actual, expected) in read_back[0].iter().zip(&waveform[0]) {
            assert!((actual - expected).abs() < 1e-4, "{actual} vs {expected}");
        }
    }

    #[test]
    fn resampling_changes_length_and_keeps_shape() {
        let waveform: AudioBuffer = vec![vec![0.0, 1.0, 0.0, -1.0]];
        let upsampled = resample_linear(&waveform, 8_000, 16_000);
        assert_eq!(upsampled[0].len(), 8);
        let same = resample_linear(&waveform, 8_000, 8_000);
        assert_eq!(same, waveform);
    }

    #[test]
    fn unsupported_sample_widths_are_rejected() {
        let error = decode_samples(5, &[0u8; 10]).expect_err("位深不支持应当报错");
        assert_eq!(
            error,
            "不支持的 WAV 位深：40 位（仅支持 8/16/24/32 位 PCM）。"
        );
    }
}
