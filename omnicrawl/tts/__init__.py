"""TTS（文本转语音）功能包。

基于 MOSS-TTS-Nano（OpenMOSS）的 ONNX CPU 推理版本，纯 CPU 运行、不依赖 PyTorch。
支持 20 种语言、内置音色与参考音频语音克隆、长文本自动分块。

快速开始::

    from omnicrawl.tts import TtsEngine

    engine = TtsEngine()                       # 首次使用自动下载模型到 ~/.omnicrawl/tts/models
    result = engine.synthesize("欢迎使用 MOSS-TTS-Nano。")
    print(result.audio_path)

包导入保持轻量：模型探测与下载（``builtin_voice_names``/``models_ready``/
``ensure_model_dir``）只依赖标准库，``TtsEngine`` 在用到时才加载（需要
numpy/sentencepiece/onnxruntime）。这样缺失引擎依赖时，设置页仍能正常
启动、显示模型状态并下载模型。
"""

from .config import DEFAULT_MODEL_DIR, TTSConfig, resolve_model_dir
from .download import builtin_voice_names, ensure_model_dir, models_ready
from .player import play_wav

__all__ = [
    "DEFAULT_MODEL_DIR",
    "TTSConfig",
    "TtsEngine",
    "TtsResult",
    "builtin_voice_names",
    "ensure_model_dir",
    "models_ready",
    "play_wav",
    "resolve_model_dir",
]


def __getattr__(name: str):
    """PEP 562：惰性加载 TTS 引擎（避免包导入时强依赖 numpy/sentencepiece/onnxruntime）。"""
    if name in {"TtsEngine", "TtsResult"}:
        from .engine import TtsEngine, TtsResult

        return {"TtsEngine": TtsEngine, "TtsResult": TtsResult}[name]
    raise AttributeError(f"module 'omnicrawl.tts' has no attribute {name!r}")
