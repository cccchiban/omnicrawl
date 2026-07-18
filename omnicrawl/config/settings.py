"""交互式设置使用的通用功能开关读写。"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data


class SettingsConfigError(RuntimeConfigError):
    """设置文件读取、类型校验或写回失败。"""


def load_feature_enabled(
    section_name: str,
    *,
    default: bool,
    config_path: str | Path | None = None,
) -> bool:
    """读取 ``<section>.enabled``，缺省时保持既有默认行为。"""

    try:
        section = get_section(load_config_data(config_path), section_name)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc
    value = section.get("enabled", default)
    if not isinstance(value, bool):
        raise SettingsConfigError(f"配置项 {section_name}.enabled 必须是布尔值。")
    return value


def save_feature_enabled(
    section_name: str,
    enabled: bool,
    config_path: str | Path | None = None,
) -> Path:
    """保留功能段其余字段，只更新 ``enabled`` 并原子写回。"""

    if not isinstance(enabled, bool):
        raise SettingsConfigError(f"配置项 {section_name}.enabled 必须是布尔值。")
    try:
        data: dict[str, Any] = load_config_data(config_path)
        section = get_section(data, section_name)
        section["enabled"] = enabled
        data[section_name] = section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc


__all__ = ["SettingsConfigError", "load_feature_enabled", "save_feature_enabled"]
