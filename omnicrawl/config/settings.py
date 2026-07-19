"""交互式设置使用的通用功能开关读写。"""

from __future__ import annotations

from dataclasses import replace
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


def save_context_window_tokens(
    tokens: int,
    *,
    model_source: str,
    catalog_key: str = "",
    config_path: str | Path | None = None,
    models_path: str | Path | None = None,
) -> Path:
    """持久化当前模型的上下文窗口，单位由调用方转换为 Token。"""

    if isinstance(tokens, bool) or not isinstance(tokens, int) or tokens <= 0:
        raise SettingsConfigError("上下文长度必须是正整数 Token。")

    if model_source == "custom":
        key = catalog_key.strip()
        if not key:
            raise SettingsConfigError("当前自定义模型缺少 catalog key，无法保存上下文长度。")
        from .model_store import ModelStore, ModelStoreError, load_model_store, save_model_store

        try:
            store = load_model_store(models_path)
            current = store.by_key().get(key)
            if current is None:
                raise SettingsConfigError(f"models.yaml 中不存在当前模型：{key}。")
            updated = replace(
                current,
                context_window_tokens=tokens,
                capabilities=replace(current.capabilities, context_window_tokens=tokens),
            )
            next_models = tuple(updated if item.key == key else item for item in store.models)
            return save_model_store(
                ModelStore(version=store.version, models=next_models, path=store.path),
                models_path,
            )
        except ModelStoreError as exc:
            raise SettingsConfigError(str(exc)) from exc

    try:
        data = load_config_data(config_path)
        section = get_section(data, "llm")
        from .llm_multi import is_multi_model_section

        if is_multi_model_section(section):
            defaults = section.get("defaults")
            if not isinstance(defaults, dict):
                defaults = {}
            defaults = dict(defaults)
            defaults["context_window_tokens"] = tokens
            section["defaults"] = defaults
        else:
            section["context_window_tokens"] = tokens
        data["llm"] = section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc


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


__all__ = [
    "SettingsConfigError",
    "load_feature_enabled",
    "save_context_window_tokens",
    "save_feature_enabled",
]
