"""TTS（MOSS-TTS-Nano ONNX）配置的读取、校验与写回。

配置段示例（config.toml）：

.. code-block:: yaml

    [tts]
    enabled = true
    model_dir = ""            # 空则使用 ~/.omnicrawl/tts/models（可被 OMNICRAWL_TTS_MODEL_DIR 覆盖）
    voice = "Junhao"          # 内置音色名（参考音频优先于音色）
    auto_play = true          # 合成完成后自动播放
    thread_count = 4          # onnxruntime CPU 线程数
    device = "auto"          # 推理设备：auto / cpu / cuda
    streaming = true         # codec 流式解码（低首字节延迟，默认开启：显存占用低）
    output_dir = ".omnicrawl/.agent_tmp/tts"   # 生成音频默认保存目录

说明：
- 推理基于 ONNX Runtime，支持 CPU/CUDA 设备；CUDA 需要安装兼容的 onnxruntime-gpu；模型首次使用自动下载约 763MB。
- ``voice`` 在可用音色中选取：模型内置音色（18 个：Junhao/Zhiming/Weiguo/Xiaoyu/...）
  或用户自定义克隆音色（``~/.omnicrawl/tts/custom_voices.json``）；
  ``tts_synthesize`` 工具调用时可传 ``prompt_audio`` 参考音频做即时克隆。
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

from ..core.runtime import RuntimeConfigError, load_config_data, save_config_data

DEFAULT_TTS_VOICE = "Junhao"
DEFAULT_TTS_OUTPUT_DIR = ".omnicrawl/.agent_tmp/tts"
_TTS_THREAD_COUNT_OPTIONS = (1, 2, 4, 8)


class TTSConfigError(RuntimeError):
    """TTS 配置无效或无法写回。"""


@dataclass(frozen=True)
class TTSConfiguration:
    """TTS（MOSS-TTS-Nano ONNX）开关与参数。"""

    enabled: bool = False
    model_dir: str = ""
    voice: str = DEFAULT_TTS_VOICE
    auto_play: bool = True
    thread_count: int = 4
    device: str = "auto"
    streaming: bool = True
    output_dir: str = DEFAULT_TTS_OUTPUT_DIR

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise TTSConfigError("tts.enabled 必须是布尔值。")
        if not isinstance(self.auto_play, bool):
            raise TTSConfigError("tts.auto_play 必须是布尔值。")
        if not isinstance(self.streaming, bool):
            raise TTSConfigError("tts.streaming 必须是布尔值。")
        if not isinstance(self.device, str) or self.device.strip().lower() not in {"auto", "cpu", "cuda"}:
            raise TTSConfigError("tts.device 必须是 auto/cpu/cuda 之一。")
        if not isinstance(self.model_dir, str):
            raise TTSConfigError("tts.model_dir 必须是字符串。")
        voice = self.voice.strip()
        if not voice:
            raise TTSConfigError("tts.voice 不能为空。")
        if (
            isinstance(self.thread_count, bool)
            or not isinstance(self.thread_count, int)
            or self.thread_count not in _TTS_THREAD_COUNT_OPTIONS
        ):
            raise TTSConfigError("tts.thread_count 必须是 1/2/4/8。")
        output_dir = self.output_dir.strip()
        if not output_dir:
            raise TTSConfigError("tts.output_dir 不能为空。")
        object.__setattr__(self, "device", self.device.strip().lower())
        object.__setattr__(self, "voice", voice)
        object.__setattr__(self, "model_dir", self.model_dir.strip())
        object.__setattr__(self, "output_dir", output_dir)

    def resolved_model_dir(self) -> Path:
        """解析模型目录：优先配置值，其次环境变量/默认目录。"""
        if self.model_dir:
            return Path(self.model_dir).expanduser().resolve()
        return _default_tts_model_dir()


def _default_tts_model_dir() -> Path:
    """默认模型目录：~/.omnicrawl/tts/models（OMNICRAWL_TTS_MODEL_DIR 可覆盖）。"""
    return Path(
        os.getenv("OMNICRAWL_TTS_MODEL_DIR")
        or (Path.home() / ".omnicrawl" / "tts" / "models")
    ).expanduser()


def load_tts_configuration(config_path: str | Path | None = None) -> TTSConfiguration:
    """读取 TTS 配置；缺少 ``tts`` 段时返回关闭的默认配置。"""

    try:
        data = load_config_data(config_path)
        raw_section = data.get("tts", {})
        if raw_section in (None, ""):
            raw_section = {}
        if not isinstance(raw_section, Mapping):
            raise TTSConfigError("配置段 tts 必须是对象。")
        return TTSConfiguration(
            enabled=bool(raw_section.get("enabled", False)),
            model_dir=str(raw_section.get("model_dir") or ""),
            voice=str(raw_section.get("voice") or DEFAULT_TTS_VOICE),
            auto_play=bool(raw_section.get("auto_play", True)),
            thread_count=int(raw_section.get("thread_count") or 4),
            device=str(raw_section.get("device") or "auto"),
            streaming=bool(raw_section.get("streaming", True)),
            output_dir=str(raw_section.get("output_dir") or DEFAULT_TTS_OUTPUT_DIR),
        )
    except TTSConfigError:
        raise
    except (ValueError, TypeError) as exc:
        raise TTSConfigError(f"配置段 tts 数值字段无效：{exc}") from exc
    except RuntimeConfigError as exc:
        raise TTSConfigError(str(exc)) from exc


def save_tts_configuration(
    configuration: TTSConfiguration,
    config_path: str | Path | None = None,
) -> Path:
    """保留其他配置段，只更新完整的 TTS 配置。"""

    if not isinstance(configuration, TTSConfiguration):
        raise TTSConfigError("TTS 配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["tts"] = {
            "enabled": configuration.enabled,
            "model_dir": configuration.model_dir,
            "voice": configuration.voice,
            "auto_play": configuration.auto_play,
            "thread_count": configuration.thread_count,
            "device": configuration.device,
            "streaming": configuration.streaming,
            "output_dir": configuration.output_dir,
        }
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise TTSConfigError(str(exc)) from exc


__all__ = [
    "DEFAULT_TTS_OUTPUT_DIR",
    "DEFAULT_TTS_VOICE",
    "TTSConfigError",
    "TTSConfiguration",
    "load_tts_configuration",
    "save_tts_configuration",
]
