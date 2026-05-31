from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Mapping


DEFAULT_CONFIG_FILENAME = "config.json"
CONFIG_PATH_ENV = "AI_CONFIG_FILE"


class RuntimeConfigError(RuntimeError):
    """本地 JSON 配置文件读取或校验失败时抛出。"""


def default_config_path() -> Path:
    """返回项目默认配置文件路径。"""

    return Path(__file__).resolve().with_name(DEFAULT_CONFIG_FILENAME)


def resolve_config_path(config_path: str | Path | None = None) -> Path:
    """解析配置文件路径。

    解析顺序为：显式参数 > `AI_CONFIG_FILE` > 项目根目录下的 `config.json`。
    """

    if config_path is not None:
        return Path(config_path).expanduser()

    raw_env = os.getenv(CONFIG_PATH_ENV, "").strip()
    if raw_env:
        return Path(raw_env).expanduser()

    return default_config_path()


def load_config_data(config_path: str | Path | None = None) -> dict[str, Any]:
    """读取并解析 JSON 配置文件。

    文件不存在时返回空字典，便于沿用环境变量或内置默认值。
    """

    path = resolve_config_path(config_path)
    if not path.exists():
        return {}

    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise RuntimeConfigError(f"读取配置文件失败：{path}，{exc}") from exc

    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise RuntimeConfigError(
            f"配置文件 JSON 解析失败：{path}，第 {exc.lineno} 行第 {exc.colno} 列：{exc.msg}"
        ) from exc

    if not isinstance(data, dict):
        raise RuntimeConfigError(f"配置文件顶层必须是 JSON 对象：{path}")

    return data


def get_section(data: Mapping[str, Any], key: str) -> dict[str, Any]:
    """安全读取 JSON 子对象。"""

    value = data.get(key, {})
    if value in (None, ""):
        return {}
    if not isinstance(value, dict):
        raise RuntimeConfigError(f"配置项 {key} 必须是 JSON 对象。")
    return dict(value)
