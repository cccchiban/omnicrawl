"""自定义音色库（TTS 语音克隆产物）单元测试。

不依赖 ONNX 模型；通过 monkeypatch 把音色库路径隔离到临时目录。
"""

from __future__ import annotations

from pathlib import Path

import pytest

from omnicrawl.tts import custom_voices as cv


@pytest.fixture()
def isolated_voice_store(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """把自定义音色库指向临时路径，返回其 JSON 路径。"""
    store = tmp_path / "custom_voices.json"
    monkeypatch.setattr(cv, "custom_voices_path", lambda: store)
    return store


def test_empty_store_has_no_names(isolated_voice_store: Path) -> None:
    assert cv.list_custom_voice_names() == []
    assert cv.load_custom_voices() == []


def test_add_and_list(isolated_voice_store: Path) -> None:
    entry = cv.add_custom_voice(
        voice="Fairy",
        prompt_audio_codes=[[1, 2], [3, 4]],
        display_name="CN 测试克隆",
        source_audio_path="/tmp/fairy_ref.wav",
    )
    assert entry["voice"] == "Fairy"
    assert entry["group"] == cv.CUSTOM_VOICE_GROUP
    assert cv.list_custom_voice_names() == ["Fairy"]
    rows = cv.load_custom_voices()
    assert len(rows) == 1
    assert rows[0]["prompt_audio_codes"] == [[1, 2], [3, 4]]
    # 文件已落盘
    assert isolated_voice_store.is_file()


def test_add_same_name_overwrites(isolated_voice_store: Path) -> None:
    cv.add_custom_voice(voice="Fairy", prompt_audio_codes=[[1, 2]])
    cv.add_custom_voice(voice="Fairy", prompt_audio_codes=[[9, 9]])
    rows = cv.load_custom_voices()
    assert len(rows) == 1
    assert rows[0]["prompt_audio_codes"] == [[9, 9]]


def test_delete_voice(isolated_voice_store: Path) -> None:
    cv.add_custom_voice(voice="A", prompt_audio_codes=[[1]])
    cv.add_custom_voice(voice="B", prompt_audio_codes=[[2]])
    assert cv.delete_custom_voice("A") is True
    assert cv.delete_custom_voice("A") is False
    assert cv.list_custom_voice_names() == ["B"]


@pytest.mark.parametrize("name", ["", "   ", "a" * 41, "bad name!"])
def test_validate_voice_name_rejects(name: str) -> None:
    with pytest.raises(ValueError):
        cv.validate_voice_name(name)


def test_validate_voice_name_accepts_chinese() -> None:
    assert cv.validate_voice_name("中文 合法") == "中文 合法"


def test_validate_voice_name_strips() -> None:
    assert cv.validate_voice_name("  Fairy  ") == "Fairy"


def test_invalid_codes_rejected(isolated_voice_store: Path) -> None:
    with pytest.raises(ValueError):
        cv.add_custom_voice(voice="X", prompt_audio_codes=[])
    with pytest.raises(ValueError):
        cv.add_custom_voice(voice="X", prompt_audio_codes=[[1, "a"]])


def test_available_voice_names_merges_builtin_and_custom(
    isolated_voice_store: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # 模拟 manifest 内置音色。
    monkeypatch.setattr(cv, "builtin_voice_rows", lambda _model_dir=None: [
        {"voice": "Junhao", "prompt_audio_codes": [[1]]},
        {"voice": "Zhiming", "prompt_audio_codes": [[2]]},
    ])
    cv.add_custom_voice(voice="Fairy", prompt_audio_codes=[[3]])

    names = cv.available_voice_names(None)
    assert names == ["Junhao", "Zhiming", "Fairy"]


def test_engine_list_available_voices_merges_custom(
    isolated_voice_store: Path,
) -> None:
    """TtsEngine.list_available_voices = 内置 + 自定义（假 runtime，不加载模型）。"""
    from omnicrawl.tts.engine import TtsEngine

    engine = object.__new__(TtsEngine)  # 绕过 __init__，不触碰 ONNX 会话。

    class _FakeRuntime:
        def list_builtin_voices(self):
            return [{"voice": "Junhao", "prompt_audio_codes": [[1]]}]

    engine._runtime = _FakeRuntime()
    cv.add_custom_voice(voice="Fairy", prompt_audio_codes=[[2]])

    rows = engine.list_available_voices()
    assert [row["voice"] for row in rows] == ["Junhao", "Fairy"]
    assert rows[-1]["group"] == cv.CUSTOM_VOICE_GROUP
