"""交互式设置使用的通用功能开关读写。"""

from __future__ import annotations

from dataclasses import replace
from pathlib import Path
from typing import Any

from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data
from .subagents import validate_subagent_advanced_setting


class SettingsConfigError(RuntimeConfigError):
    """设置文件读取、类型校验或写回失败。"""


def _serialize_mcp_config(config: Any) -> dict[str, Any]:
    """把 MCP 不可变配置转换为可写回 YAML 的普通对象。"""

    servers: dict[str, Any] = {}
    for name, server in config.servers.items():
        servers[name] = {
            "enabled": server.enabled,
            "transport": server.transport,
            "command": server.command,
            "args": list(server.args),
            "url": server.url,
            "env": dict(server.env),
            "headers": dict(server.headers),
            "timeout_seconds": server.timeout_seconds,
            "risk_level": server.risk_level,
        }
    return {
        "enabled": config.enabled,
        "default_timeout_seconds": config.default_timeout_seconds,
        "max_tool_output_chars": config.max_tool_output_chars,
        "servers": servers,
        "policy": {
            "require_confirmation_for_write": config.policy.require_confirmation_for_write,
            "require_confirmation_for_command": config.policy.require_confirmation_for_command,
            "allow_external_network_tools": config.policy.allow_external_network_tools,
            "audit_log_enabled": config.policy.audit_log_enabled,
        },
    }


def save_mcp_config(config: Any, config_path: str | Path | None = None) -> Path:
    """保留其他配置段，只更新完整的 MCP 配置并原子写回。"""

    if not hasattr(config, "servers") or not hasattr(config, "policy"):
        raise SettingsConfigError("MCP 配置对象无效。")
    try:
        data: dict[str, Any] = load_config_data(config_path)
        data["mcp"] = _serialize_mcp_config(config)
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc


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


def save_subagent_setting(
    name: str,
    value: Any,
    config_path: str | Path | None = None,
) -> Path:
    """保留 ``subagents`` 其他配置，只更新面板允许的资源参数。"""

    try:
        normalized = validate_subagent_advanced_setting(name, value)
        data: dict[str, Any] = load_config_data(config_path)
        section = get_section(data, "subagents")
        section[name] = normalized
        data["subagents"] = section
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
    "save_mcp_config",
    "save_subagent_setting",
]
