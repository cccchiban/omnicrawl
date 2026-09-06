"""用户自定义音色库（语音克隆产物）的读取、追加与删除。

克隆出的音色不写进模型 manifest（manifest 会随模型重装被覆盖，也不便
迁移），而是存到独立 JSON：``~/.omnicrawl/tts/custom_voices.json``。
用户级音色资产固定存放于此，不随模型目录/模型重装变化。

音色条目结构与模型 manifest 的 ``builtin_voices`` 条目保持一致：

.. code-block:: json

    {
      "voice": "Fairy",
      "display_name": "CN 我的克隆音色",
      "group": "Custom",
      "audio_file": "fairy_ref.wav",
      "prompt_audio_codes": [[...]]
    }

只依赖标准库，不引入 numpy/onnxruntime，设置页与轻量探测可直接使用。
"""

from __future__ import annotations

import json
import re
import threading
from pathlib import Path
from typing import Any

from .config import resolve_model_dir

# 自定义音色库文件名（固定位于 ~/.omnicrawl/tts/ 下）。
CUSTOM_VOICES_FILENAME = "custom_voices.json"

# 与模型 manifest 内置音色同结构，便于引擎统一消费。
CUSTOM_VOICE_GROUP = "Custom"

# 音色标识允许的字符：字母、数字、中文、下划线、连字符、点、空格。
_VOICE_NAME_RE = re.compile(r"^[\w\u4e00-\u9fff ._-]{1,40}$")

_LOGGER = None  # 延迟导入 logging，避免模块级开销


def _logger():
    global _LOGGER
    if _LOGGER is None:
        import logging

        _LOGGER = logging.getLogger(__name__)
    return _LOGGER


def custom_voices_path() -> Path:
    """返回自定义音色库 JSON 路径。

    克隆音色是用户资产，固定存放于 ``~/.omnicrawl/tts/custom_voices.json``，
    不随模型目录（OMNICRAWL_TTS_MODEL_DIR）或模型重装变化。
    """
    return Path.home() / ".omnicrawl" / "tts" / CUSTOM_VOICES_FILENAME


def _lock() -> threading.Lock:
    """进程内写锁，避免多线程同时写坏 JSON。"""
    lock = getattr(_lock, "_instance", None)
    if lock is None:
        lock = threading.Lock()
        _lock._instance = lock  # type: ignore[attr-defined]
    return lock


def _read_raw() -> dict[str, Any]:
    path = custom_voices_path()
    if not path.is_file():
        return {"version": 1, "voices": []}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        _logger().warning("读取自定义音色库失败（%s），按空库处理：%s", path, exc)
        return {"version": 1, "voices": []}
    if not isinstance(data, dict):
        return {"version": 1, "voices": []}
    voices = data.get("voices")
    if not isinstance(voices, list):
        voices = []
    return {"version": data.get("version", 1), "voices": voices}


def load_custom_voices() -> list[dict[str, Any]]:
    """读取全部自定义音色条目（无效条目跳过）。"""
    voices: list[dict[str, Any]] = []
    for row in _read_raw().get("voices", []):
        if not isinstance(row, dict):
            continue
        voice = str(row.get("voice") or "").strip()
        codes = row.get("prompt_audio_codes")
        if not voice or not isinstance(codes, list) or not codes:
            continue
        voices.append(row)
    return voices


def list_custom_voice_names() -> list[str]:
    """返回自定义音色名列表（保留入库顺序）。"""
    return [str(row.get("voice", "")).strip() for row in load_custom_voices() if str(row.get("voice", "")).strip()]


def validate_voice_name(voice: str) -> str:
    """校验并规范化自定义音色名；非法时抛 ValueError。"""
    name = str(voice or "").strip()
    if not name:
        raise ValueError("音色名称不能为空。")
    if len(name) > 40:
        raise ValueError("音色名称过长（最多 40 个字符）。")
    if not _VOICE_NAME_RE.match(name):
        raise ValueError("音色名称只能包含中文、字母、数字、空格、下划线、连字符或点。")
    return name


def add_custom_voice(
    *,
    voice: str,
    prompt_audio_codes: list[list[int]],
    display_name: str = "",
    audio_file: str = "",
    source_audio_path: str = "",
) -> dict[str, Any]:
    """把一条克隆音色写入自定义库；同名覆盖。

    返回写入的条目 dict。``source_audio_path`` 仅作元数据记录，不复制音频。
    """
    name = validate_voice_name(voice)
    codes: list[list[int]] = []
    for row_value in prompt_audio_codes:
        if not isinstance(row_value, (list, tuple)):
            raise ValueError("prompt_audio_codes 必须是非空二维整数列表。")
        codes.append([int(code_value) for code_value in row_value])
    if not codes:
        raise ValueError("prompt_audio_codes 不能为空。")
    entry: dict[str, Any] = {
        "voice": name,
        "display_name": str(display_name or name),
        "group": CUSTOM_VOICE_GROUP,
        "audio_file": str(audio_file or Path(source_audio_path or name).name),
        "prompt_audio_codes": codes,
    }
    if source_audio_path:
        entry["source_audio_path"] = str(source_audio_path)

    with _lock():
        data = _read_raw()
        voices = data.setdefault("voices", [])
        replaced = False
        for index, row in enumerate(voices):
            if isinstance(row, dict) and str(row.get("voice", "")).strip() == name:
                voices[index] = entry
                replaced = True
                break
        if not replaced:
            voices.append(entry)
        path = custom_voices_path()
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(
                json.dumps(data, ensure_ascii=False, indent=2),
                encoding="utf-8",
            )
        except OSError as exc:
            raise ValueError(f"写入自定义音色库失败：{exc}") from exc
    _logger().info("已%s自定义音色：%s（%s）", "覆盖" if replaced else "添加", name, path)
    return entry


def delete_custom_voice(voice: str) -> bool:
    """删除一条自定义音色；不存在返回 False。"""
    name = str(voice or "").strip()
    with _lock():
        data = _read_raw()
        voices = data.get("voices", [])
        kept: list[dict[str, Any]] = []
        removed = False
        for row in voices:
            if isinstance(row, dict) and str(row.get("voice", "")).strip() == name:
                removed = True
            else:
                kept.append(row)
        if not removed:
            return False
        data["voices"] = kept
        path = custom_voices_path()
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
        except OSError as exc:
            raise ValueError(f"写入自定义音色库失败：{exc}") from exc
    _logger().info("已删除自定义音色：%s", name)
    return True


def builtin_voice_rows(model_dir: str | Path | None = None) -> list[dict[str, Any]]:
    """读取模型 manifest 中的内置音色行（不加载 ONNX session）。模型缺失返回空列表。"""
    resolved = resolve_model_dir(model_dir)
    from .download import _find_manifest_path  # 延迟避免循环导入

    manifest_path = _find_manifest_path(resolved)
    if manifest_path is None:
        return []
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    voices = manifest.get("builtin_voices") or []
    return [row for row in voices if isinstance(row, dict) and row.get("voice")]


def all_voice_names(model_dir: str | Path | None = None) -> list[str]:
    """返回全部可用音色名：内置（manifest 顺序）+ 自定义（自定义库顺序）。

    轻量实现：只读 JSON，不加载 ONNX session；模型缺失时仍返回自定义音色。
    """
    names = [str(row.get("voice", "")).strip() for row in builtin_voice_rows(model_dir) if row.get("voice")]
    names.extend(list_custom_voice_names())
    return names


# 别名：UI/引擎统一用 all_voice_names。
available_voice_names = all_voice_names


__all__ = [
    "CUSTOM_VOICE_GROUP",
    "add_custom_voice",
    "all_voice_names",
    "available_voice_names",
    "builtin_voice_rows",
    "custom_voices_path",
    "delete_custom_voice",
    "list_custom_voice_names",
    "load_custom_voices",
    "validate_voice_name",
]
