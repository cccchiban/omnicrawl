"""音频 I/O 工具（无 torchaudio 依赖）。

官方 ONNX 运行时的 `_load_reference_audio` 依赖 torchaudio 完成 WAV 读取与
重采样；本模块用标准库 `wave` + numpy 等价实现，保证纯 CPU 场景不引入 PyTorch。
"""

from __future__ import annotations

import wave
from pathlib import Path

import numpy as np


def read_wav(path: str | Path) -> tuple[np.ndarray, int]:
    """读取 WAV 文件，返回 (waveform, sample_rate)。

    waveform 为 float32 数组，形状 [channels, samples]，取值 [-1, 1]。
    支持 8/16/24/32 位 PCM 与 32 位 IEEE float；其他格式抛出 ValueError。
    """
    path = Path(path).expanduser().resolve()
    with wave.open(str(path), "rb") as wav_file:
        channels = wav_file.getnchannels()
        sample_width = wav_file.getsampwidth()
        sample_rate = wav_file.getframerate()
        frame_count = wav_file.getnframes()
        raw = wav_file.readframes(frame_count)

    if sample_width == 1:
        samples = np.frombuffer(raw, dtype=np.uint8).astype(np.float32)
        samples = (samples - 128.0) / 128.0
    elif sample_width == 2:
        samples = np.frombuffer(raw, dtype="<i2").astype(np.float32) / 32768.0
    elif sample_width == 3:
        # 24 位有符号 PCM：三个字节小端，符号扩展。
        samples = np.zeros(len(raw) // 3, dtype=np.int32)
        for index in range(3):
            samples |= (np.frombuffer(raw, dtype=np.uint8)[index::3].astype(np.int32)) << (8 * index)
        samples = ((samples + (1 << 23)) % (1 << 24)) - (1 << 23)
        samples = samples.astype(np.float32) / 8388608.0
    elif sample_width == 4:
        samples = np.frombuffer(raw, dtype="<i4").astype(np.float32) / 2147483648.0
    else:
        raise ValueError(f"不支持的 WAV 位深：{sample_width * 8} 位（仅支持 8/16/24/32 位 PCM）。")

    waveform = samples.reshape(-1, channels).T.astype(np.float32, copy=False)
    return waveform, int(sample_rate)


def resample_linear(
    waveform: np.ndarray,
    source_rate: int,
    target_rate: int,
) -> np.ndarray:
    """按线性插值把 waveform 重采样到目标采样率。

    waveform 形状 [channels, samples]（或 [samples]）。用于语音克隆参考音频，
    线性插值在该场景下足够；参考音频通道数在调用方另行处理。
    """
    waveform = np.asarray(waveform, dtype=np.float32)
    if source_rate == target_rate or source_rate <= 0 or target_rate <= 0:
        return waveform
    one_dimensional = waveform.ndim == 1
    if one_dimensional:
        waveform = waveform.reshape(1, -1)
    source_length = int(waveform.shape[1])
    target_length = max(1, int(round(source_length * target_rate / source_rate)))
    # 输入采样位置：输出样本 i 对应输入 i * source_rate / target_rate。
    positions = np.arange(target_length, dtype=np.float64) * source_rate / target_rate
    positions = np.clip(positions, 0, source_length - 1)
    left = np.floor(positions).astype(np.int64)
    right = np.minimum(left + 1, source_length - 1)
    fraction = (positions - left).astype(np.float32).reshape(1, -1)
    result = np.empty((waveform.shape[0], target_length), dtype=np.float32)
    for channel_index in range(waveform.shape[0]):
        result[channel_index] = (
            waveform[channel_index, left] * (1.0 - fraction)
            + waveform[channel_index, right] * fraction
        )
    if one_dimensional:
        return result[0]
    return result


def load_reference_audio(
    path: str | Path,
    *,
    target_sample_rate: int,
    target_channels: int,
) -> np.ndarray:
    """加载语音克隆参考音频，转成 codec 期望的 [1, channels, samples] float32。

    重采样用线性插值；单声道→多声道复制，多声道→单声道取均值。
    """
    waveform, sample_rate = read_wav(path)
    if sample_rate != target_sample_rate:
        waveform = resample_linear(waveform, sample_rate, target_sample_rate)
    current_channels = int(waveform.shape[0])
    if current_channels == target_channels:
        pass
    elif current_channels == 1 and target_channels > 1:
        waveform = np.repeat(waveform, target_channels, axis=0)
    elif current_channels > 1 and target_channels == 1:
        waveform = waveform.mean(axis=0, keepdims=True)
    else:
        raise ValueError(
            f"不支持的参考音频声道转换：{current_channels} -> {target_channels}"
        )
    return waveform.reshape(1, target_channels, -1).astype(np.float32, copy=False)


def write_wav(path: str | Path, waveform: np.ndarray, sample_rate: int) -> Path:
    """把 float32 波形写入 16 位 PCM WAV 文件。

    waveform 形状 [samples]（单声道）或 [channels, samples]（声道优先，与
    `read_wav` 一致）；内部转成交错布局 [samples, channels] 写出。
    """
    output_path = Path(path).expanduser().resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    audio = np.asarray(waveform, dtype=np.float32)
    if audio.ndim == 1:
        audio = audio.reshape(1, -1)  # 单声道 -> [1, samples]
    if audio.ndim != 2:
        raise ValueError(f"波形维度必须为 1 或 2，当前：{audio.ndim}")
    channels, _samples = int(audio.shape[0]), int(audio.shape[1])
    # 约定 [channels, samples]；WAV 需要交错布局 [samples, channels]。
    interleaved = np.ascontiguousarray(audio.T)
    clipped = np.clip(interleaved, -1.0, 1.0)
    pcm16 = np.round(clipped * 32767.0).astype(np.int16)
    with wave.open(str(output_path), "wb") as wav_file:
        wav_file.setnchannels(channels)
        wav_file.setsampwidth(2)
        wav_file.setframerate(int(sample_rate))
        wav_file.writeframes(pcm16.tobytes())
    return output_path
