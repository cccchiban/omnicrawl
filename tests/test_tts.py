"""TTS 包单元测试。

覆盖不依赖模型的部分：文本归一化、音频 I/O 与重采样、配置默认值。
引擎合成测试需要先安装 sentencepiece + 下载 ONNX 模型，另行验证。
"""

from __future__ import annotations

import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

from omnicrawl.tts.audio import load_reference_audio, read_wav, resample_linear, write_wav
from omnicrawl.tts.config import TTSConfig
from omnicrawl.tts.normalize import (
    prepare_tts_request_texts,
    normalize_tts_text,
    resolve_text_normalization_language,
)
from omnicrawl.tts import onnx_runtime
from omnicrawl.tts.onnx_runtime import (
    _normalize_execution_provider,
    _resolve_ort_providers,
    _resolve_requested_provider,
)

NORMALIZE_CASES = [
    ("中文空格", "这 是 一 段 测试。", "这是一段测试。"),
    ("英文空格", "This   is   a   test.", "This is a test."),
    ("中英混排", "这是Anthropic的npm包", "这是 Anthropic 的 npm 包。"),
    ("markdown", "# 标题\n- 列表项\n- 第二项", "标题。列表项。第二项。"),
    ("dot token", "别把 .env、.npmrc、.gitignore 提交上去。", "别把 .env、.npmrc、.gitignore 提交上去。"),
    ("文件版本", "Bug 的讨论可以精确到 v2.3.1 (Build 15)。", "Bug 的讨论可以精确到 v2.3.1 (Build 15)。"),
    ("结构性括号", "【公告】今天 20:00 维护", "公告今天20:00维护。"),
    ("表情符号", "今天天气真好啊😊🌞 记得带伞☔", "今天天气真好啊记得带伞。"),
    ("表情zwj", "周末一起去爬山🧗‍♂️怎么样", "周末一起去爬山怎么样。"),
    ("斜杠中文", "请选择启用/禁用该功能", "请选择启用、禁用该功能。"),
    ("斜杠英文", "支持 A/B 两种方案", "支持 A B 两种方案。"),
    ("斜杠日期", "截止 2024/05/01 提交", "截止 2024/05/01 提交。"),
    ("斜杠url", "见 https://example.com/a/b 说明", "见 https://example.com/a/b 说明。"),
    ("斜杠路径", "文件在 src/omnicrawl/main.py", "文件在 src/omnicrawl/main.py。"),
    ("双引号", '他说"马上到"就挂了', "他说马上到就挂了。"),
    ("弯引号", "他说“马上到”就挂了", "他说马上到就挂了。"),
    ("装饰符", "标题 *强调* 内容 ~删除~ #标签", "标题强调内容删除标签。"),
    ("竖线反斜杠", "a|b\\c 启用\\禁用", "a b c 启用、禁用。"),
    ("撇号保留", "I don't know", "I don't know。"),
    ("重复标点", "真的假的？？？！！！", "真的假的？！"),
    ("省略号", "太离谱了......", "太离谱了。"),
    ("URL", "仓库地址是 https://github.com/instructkr/claude-code", "仓库地址是 https://github.com/instructkr/claude-code。"),
    ("句末补标点", "今天发布", "今天发布。"),
    ("幂等", "这是一段。\n\n第二段——继续", "这是一段。第二段。继续。"),
]


@pytest.mark.parametrize("name,source,expected", NORMALIZE_CASES, ids=[c[0] for c in NORMALIZE_CASES])
def test_normalize_tts_text(name: str, source: str, expected: str) -> None:
    actual = normalize_tts_text(source)
    assert actual == expected
    # 幂等性：第二次归一化不再改变结果
    assert normalize_tts_text(actual) == actual


def test_normalize_idempotent_with_protected_spans() -> None:
    text = "关注@biscuit0228_并转发#thetime_tbs"
    once = normalize_tts_text(text)
    twice = normalize_tts_text(once)
    assert once == twice


def test_resolve_text_normalization_language() -> None:
    assert resolve_text_normalization_language(text="中文内容", voice="Junhao") == "zh"
    assert resolve_text_normalization_language(text="hello world", voice="Junhao") == "en"
    assert resolve_text_normalization_language(text="12345", voice="Ava") == "en"
    assert resolve_text_normalization_language(text="12345", voice="Junhao") == "zh"


def test_prepare_tts_request_texts_robust_only() -> None:
    result = prepare_tts_request_texts(
        text="这 是 一 段 测试",
        enable_wetext=False,
        enable_normalize_tts_text=True,
    )
    assert result["text"] == "这是一段测试。"
    assert result["normalization_method"] == "robust"
    assert result["wetext_processing_enabled"] is False


def test_prepare_tts_request_texts_empty_text() -> None:
    result = prepare_tts_request_texts(
        text="",
        enable_wetext=False,
        enable_normalize_tts_text=True,
    )
    assert result["text"] == ""


def test_tts_config_defaults() -> None:
    config = TTSConfig()
    assert config.device == "auto"
    assert config.execution_provider == "cpu"
    assert config.sample_mode == "fixed"
    assert config.voice == "Junhao"
    assert config.enable_wetext is False
    overrides = config.generation_overrides()
    assert overrides["text_temperature"] == 1.0
    assert overrides["audio_temperature"] == 0.8
    assert overrides["audio_repetition_penalty"] == 1.2


@pytest.mark.parametrize("device", ["auto", "cpu", "cuda"])
def test_normalize_execution_provider_accepts_devices(device: str) -> None:
    assert _normalize_execution_provider(device) == device


def test_normalize_execution_provider_rejects_unknown_device() -> None:
    with pytest.raises(ValueError, match="auto, cpu, cuda"):
        _normalize_execution_provider("tpu")


def test_resolve_ort_providers_cpu_returns_plain_cpu(monkeypatch: pytest.MonkeyPatch) -> None:
    providers = _resolve_ort_providers("cpu")
    assert providers == ["CPUExecutionProvider"]


def test_resolve_ort_providers_cuda_uses_arena_same_as_requested(monkeypatch: pytest.MonkeyPatch) -> None:
    """CUDA provider 必须带 kSameAsRequested 显存选项（4GB 小显存卡优化）。"""
    fake_ort = SimpleNamespace(
        get_available_providers=lambda: ["CUDAExecutionProvider", "CPUExecutionProvider"],
        InferenceSession=object,
    )
    monkeypatch.setattr(onnx_runtime.ort, "get_available_providers", fake_ort.get_available_providers)

    providers = _resolve_ort_providers("cuda")

    assert providers[0] == (
        "CUDAExecutionProvider",
        {"arena_extend_strategy": "kSameAsRequested"},
    )
    assert providers[1] == "CPUExecutionProvider"


def test_resolve_ort_providers_auto_prefers_cuda_when_available(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_ort = SimpleNamespace(
        get_available_providers=lambda: ["CUDAExecutionProvider", "CPUExecutionProvider"],
        InferenceSession=object,
    )
    monkeypatch.setattr(onnx_runtime.ort, "get_available_providers", fake_ort.get_available_providers)
    monkeypatch.setattr(onnx_runtime, "_cuda_vram_mib", lambda: 8 * 1024)

    assert _resolve_requested_provider("auto") == "cuda"
    providers = _resolve_ort_providers("auto")
    assert providers[0][0] == "CUDAExecutionProvider"


def test_resolve_ort_providers_auto_falls_back_to_cpu(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_ort = SimpleNamespace(
        get_available_providers=lambda: ["CPUExecutionProvider"],
        InferenceSession=object,
    )
    monkeypatch.setattr(onnx_runtime.ort, "get_available_providers", fake_ort.get_available_providers)

    assert _resolve_requested_provider("auto") == "cpu"
    assert _resolve_ort_providers("auto") == ["CPUExecutionProvider"]


def test_auto_falls_back_to_cpu_on_small_vram(monkeypatch: pytest.MonkeyPatch) -> None:
    """device=auto 在 4GB 级小显存卡上必须回退 CPU（长文本 CUDA 必 OOM）。"""
    fake_ort = SimpleNamespace(
        get_available_providers=lambda: ["CUDAExecutionProvider", "CPUExecutionProvider"],
        InferenceSession=object,
    )
    monkeypatch.setattr(onnx_runtime.ort, "get_available_providers", fake_ort.get_available_providers)
    monkeypatch.setattr(onnx_runtime, "_cuda_vram_mib", lambda: 4 * 1024)

    assert _resolve_requested_provider("auto") == "cpu"


def test_auto_keeps_cuda_on_adequate_vram(monkeypatch: pytest.MonkeyPatch) -> None:
    """显存充足（≥6GB）时 auto 仍优先 CUDA。"""
    fake_ort = SimpleNamespace(
        get_available_providers=lambda: ["CUDAExecutionProvider", "CPUExecutionProvider"],
        InferenceSession=object,
    )
    monkeypatch.setattr(onnx_runtime.ort, "get_available_providers", fake_ort.get_available_providers)
    monkeypatch.setattr(onnx_runtime, "_cuda_vram_mib", lambda: 8 * 1024)

    assert _resolve_requested_provider("auto") == "cuda"


def test_cuda_vram_probe_failure_keeps_cuda(monkeypatch: pytest.MonkeyPatch) -> None:
    """显存探测失败（无 nvidia-smi）时 auto 不阻断 CUDA（按可用处理）。"""
    fake_ort = SimpleNamespace(
        get_available_providers=lambda: ["CUDAExecutionProvider", "CPUExecutionProvider"],
        InferenceSession=object,
    )
    monkeypatch.setattr(onnx_runtime.ort, "get_available_providers", fake_ort.get_available_providers)
    monkeypatch.setattr(onnx_runtime, "_cuda_vram_mib", lambda: None)

    assert _resolve_requested_provider("auto") == "cuda"


def test_silence_ort_logging_sets_fatal(monkeypatch: pytest.MonkeyPatch) -> None:
    """静默函数把 ORT 默认日志级别提到 FATAL（4），抑制 C++ 红字。"""
    captured: list[int] = []

    class _FakeOrt:
        @staticmethod
        def set_default_logger_severity(level: int) -> None:
            captured.append(level)

    monkeypatch.setattr(onnx_runtime, "ort", _FakeOrt)
    onnx_runtime.silence_ort_logging()
    assert captured == [4]


def _write_sine_wav(path: Path, sample_rate: int, seconds: float, amplitude: float = 0.3) -> None:
    samples = np.arange(int(sample_rate * seconds), dtype=np.float32) / sample_rate
    mono = (amplitude * np.sin(2 * np.pi * 440 * samples)).astype(np.float32)
    write_wav(path, mono, sample_rate)


def test_write_and_read_wav_mono(tmp_path: Path) -> None:
    path = tmp_path / "mono.wav"
    _write_sine_wav(path, 16000, 1.0)
    waveform, sample_rate = read_wav(path)
    assert sample_rate == 16000
    assert waveform.shape == (1, 16000)
    assert abs(float(np.max(np.abs(waveform))) - 0.3) < 0.02


def test_write_and_read_wav_stereo(tmp_path: Path) -> None:
    path = tmp_path / "stereo.wav"
    samples = np.arange(16000, dtype=np.float32) / 16000
    mono = (0.3 * np.sin(2 * np.pi * 440 * samples)).astype(np.float32)
    write_wav(path, np.stack([mono, mono * 0.5], axis=0), 16000)
    waveform, _ = read_wav(path)
    assert waveform.shape == (2, 16000)


def test_resample_linear_changes_length() -> None:
    samples = np.arange(16000, dtype=np.float32)
    waveform = samples.reshape(1, -1)
    resampled = resample_linear(waveform, 16000, 48000)
    assert resampled.shape == (1, 48000)
    # 端点近似保持
    assert abs(float(resampled[0, -1]) - float(samples[-1])) < 2.0


def test_resample_linear_same_rate_is_identity() -> None:
    waveform = np.random.rand(1, 1000).astype(np.float32)
    resampled = resample_linear(waveform, 48000, 48000)
    assert np.array_equal(resampled, waveform)


def test_load_reference_audio_mono_to_stereo(tmp_path: Path) -> None:
    path = tmp_path / "ref.wav"
    _write_sine_wav(path, 16000, 0.5)
    reference = load_reference_audio(path, target_sample_rate=48000, target_channels=2)
    assert reference.shape == (1, 2, 24000)
    assert reference.dtype == np.float32


def test_play_wav_blocking_flag_passthrough(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """blocking=True 走同步播放（不带 SND_ASYNC），blocking=False 走异步。"""

    from omnicrawl.tts.player import play_wav

    path = tmp_path / "play.wav"
    _write_sine_wav(path, 16000, 0.2)
    captured: list[int] = []

    class _FakeWinsound:
        SND_FILENAME = 0x00020000
        SND_ASYNC = 0x0001
        SND_NODEFAULT = 0x0002

        @classmethod
        def PlaySound(cls, sound: str, flags: int) -> None:
            captured.append(flags)

    monkeypatch.setattr(sys, "platform", "win32")
    monkeypatch.setitem(sys.modules, "winsound", _FakeWinsound)

    assert play_wav(path, blocking=True) is True
    assert play_wav(path, blocking=False) is True
    assert len(captured) == 2
    # 同步：只带 SND_FILENAME；异步：SND_FILENAME | SND_ASYNC
    assert captured[0] == _FakeWinsound.SND_FILENAME
    assert captured[1] == (_FakeWinsound.SND_FILENAME | _FakeWinsound.SND_ASYNC)


def test_play_wav_missing_file_returns_false(tmp_path: Path) -> None:
    from omnicrawl.tts.player import play_wav

    assert play_wav(tmp_path / "missing.wav") is False


def test_load_reference_audio_stereo_to_mono(tmp_path: Path) -> None:
    path = tmp_path / "ref_stereo.wav"
    samples = np.arange(8000, dtype=np.float32) / 8000
    mono = (0.2 * np.sin(2 * np.pi * 330 * samples)).astype(np.float32)
    write_wav(path, np.stack([mono, mono], axis=0), 8000)
    reference = load_reference_audio(path, target_sample_rate=48000, target_channels=1)
    assert reference.shape == (1, 1, 48000)


# 引擎端到端测试：需要已下载的 ONNX 模型（默认 ~/.omnicrawl/tts/models）。
# manifest 可能在根目录或 MOSS-TTS-Nano-100M-ONNX/ 子目录（与官方下载布局一致）。
_ENGINE_MODEL_DIR = Path.home() / ".omnicrawl" / "tts" / "models"
_ENGINE_MODELS_READY = any(
    (_ENGINE_MODEL_DIR / candidate).is_file()
    for candidate in (
        "browser_poc_manifest.json",
        "MOSS-TTS-Nano-100M-ONNX/browser_poc_manifest.json",
    )
)


@pytest.mark.skipif(not _ENGINE_MODELS_READY, reason="ONNX 模型未下载（~/.omnicrawl/tts/models）")
def test_engine_synthesize_builtin_voice(tmp_path: Path) -> None:
    from omnicrawl.tts.engine import TtsEngine
    from omnicrawl.tts.config import TTSConfig

    output_path = tmp_path / "synthesized.wav"
    with TtsEngine(TTSConfig(thread_count=2)) as engine:
        assert len(engine.list_builtin_voices()) > 0
        result = engine.synthesize(
            "测试语音合成。",
            voice="Junhao",
            output_path=output_path,
            max_new_frames=60,
        )
    assert output_path.is_file()
    assert result.sample_rate == 48000
    assert result.duration_seconds > 0
    assert result.audio_token_frames > 0
    waveform, sample_rate = read_wav(output_path)
    assert sample_rate == 48000
    assert waveform.shape[0] == 2  # 立体声
    assert waveform.shape[1] > 0
