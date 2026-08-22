"""TTS 引擎配置。

配置项与 MOSS-TTS-Nano 官方 `infer_onnx.py` 的参数一一对应，便于按需调整。
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


# 默认模型目录：`~/.omnicrawl/tts/models`（可用 OMNICRAWL_TTS_MODEL_DIR 覆盖）。
DEFAULT_MODEL_DIR = Path(
    os.getenv("OMNICRAWL_TTS_MODEL_DIR")
    or (Path.home() / ".omnicrawl" / "tts" / "models")
).expanduser()


def resolve_model_dir(model_dir: str | Path | None) -> Path:
    """解析模型目录；None 时回退到默认目录（含环境变量覆盖）。"""
    if model_dir is None:
        return DEFAULT_MODEL_DIR
    return Path(model_dir).expanduser().resolve()


@dataclass
class TTSConfig:
    """MOSS-TTS-Nano ONNX 推理配置。

    字段默认值与官方 `infer_onnx.py` 一致；`model_dir` 为 None 时使用
    `~/.omnicrawl/tts/models`（或 OMNICRAWL_TTS_MODEL_DIR）。
    """

    model_dir: str | Path | None = None
    # ONNX Runtime 线程数（intra-op）。
    thread_count: int = 4
    # 设备：auto 自动优先 CUDA、不可用时回退 CPU；显式 cuda 不回退。
    device: str | None = "auto"
    # 兼容旧 API 的 ONNX Runtime provider 参数；device 设置后优先使用 device。
    execution_provider: str = "cpu"
    # 采样模式：greedy / fixed / full。
    sample_mode: str = "fixed"
    # 是否采样（False 时强制 greedy）。
    do_sample: bool = True
    # 最大生成的音频帧数。
    max_new_frames: int = 375
    # 内置音色名（未提供参考音频时使用）。
    voice: str = "Junhao"
    # 语音克隆参考音频路径（提供时覆盖 voice）。
    prompt_audio_path: str | Path | None = None
    # 输出音频目录（synthesize 未指定 output_path 时使用）。
    output_dir: str | Path = "generated_audio"
    # 是否用 codec 流式解码（低首字节延迟）。
    streaming: bool = False
    # 长文本按 token 预算分块。
    voice_clone_max_text_tokens: int = 75
    # WeTextProcessing 文本归一化（需要 pynini，Windows 上默认关闭）。
    enable_wetext: bool = False
    # 纯 Python 稳健文本清洗。
    enable_normalize_tts_text: bool = True

    # 采样参数。
    text_temperature: float = 1.0
    text_top_p: float = 1.0
    text_top_k: int = 50
    audio_temperature: float = 0.8
    audio_top_p: float = 0.95
    audio_top_k: int = 25
    audio_repetition_penalty: float = 1.2
    # 随机种子；None 时使用运行时默认种子。
    seed: int | None = None

    def resolved_model_dir(self) -> Path:
        """返回解析后的绝对模型目录路径。"""
        return resolve_model_dir(self.model_dir)

    def generation_overrides(self) -> dict[str, Any]:
        """返回写入 manifest generation_defaults 的采样参数覆盖。"""
        return {
            "text_temperature": float(self.text_temperature),
            "text_top_p": float(self.text_top_p),
            "text_top_k": int(self.text_top_k),
            "audio_temperature": float(self.audio_temperature),
            "audio_top_p": float(self.audio_top_p),
            "audio_top_k": int(self.audio_top_k),
            "audio_repetition_penalty": float(self.audio_repetition_penalty),
        }
