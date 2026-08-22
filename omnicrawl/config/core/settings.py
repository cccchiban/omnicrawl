"""交互式设置使用的通用功能开关读写。"""

from __future__ import annotations

from dataclasses import replace
from pathlib import Path
from typing import Any

from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data
from ..features.subagents import validate_subagent_advanced_setting


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
    subagents_path: str | Path | None = None,
) -> bool:
    """读取 ``<section>.enabled``，缺省时保持既有默认行为。

    ``subagents`` 段已从 ``config.toml`` 迁移到独立 ``subagents.toml``：
    该段开关从子代理设置文件读取，不再回退读取 ``config.toml``。
    """

    try:
        if section_name == "subagents":
            from .runtime import resolve_subagents_path

            section = get_section(
                load_config_data(resolve_subagents_path(subagents_path)),
                section_name,
            )
        else:
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
        from ..models.model_store import ModelStore, ModelStoreError, load_model_store, save_model_store

        try:
            store = load_model_store(models_path)
            current = store.by_key().get(key)
            if current is None:
                raise SettingsConfigError(f"models.toml 中不存在当前模型：{key}。")
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
        from ..models.llm_multi import is_multi_model_section

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


def save_context_compaction_trigger_percent(
    percent: int,
    *,
    context_window_tokens: int,
    config_path: str | Path | None = None,
) -> Path:
    """按当前上下文窗口的百分比换算触发阈值并写回 ``config.toml``。

    换算公式：``trigger_context_tokens = context_window_tokens * percent // 100``。
    只更新 ``context_compaction.trigger_context_tokens``，保留该段其余字段。
    """

    if isinstance(percent, bool) or not isinstance(percent, int) or percent <= 0:
        raise SettingsConfigError("上下文压缩阈值百分比必须是正整数。")
    if (
        isinstance(context_window_tokens, bool)
        or not isinstance(context_window_tokens, int)
        or context_window_tokens <= 0
    ):
        raise SettingsConfigError("上下文长度必须是正整数 Token。")
    tokens = max(1, context_window_tokens * percent // 100)
    try:
        data = load_config_data(config_path)
        section = get_section(data, "context_compaction")
        section["trigger_context_tokens"] = tokens
        section["trigger_context_percent"] = percent
        data["context_compaction"] = section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc


def save_subagent_setting(
    name: str,
    value: Any,
    subagents_path: str | Path | None = None,
) -> Path:
    """保留 ``subagents`` 其他配置，只更新面板允许的资源参数。

    子代理设置已从 ``config.toml`` 完全迁移到独立的 ``subagents.toml``；
    只写回子代理设置文件，不再回退写回 ``config.toml`` 的 ``[subagents]`` 段。
    目标文件不存在时会自动创建。
    """

    try:
        normalized = validate_subagent_advanced_setting(name, value)
        from .runtime import resolve_subagents_write_path

        target = resolve_subagents_write_path(subagents_path)
        data: dict[str, Any] = load_config_data(target)
        section = get_section(data, "subagents")
        section[name] = normalized
        data["subagents"] = section
        return save_config_data(data, target)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc


def save_feature_enabled(
    section_name: str,
    enabled: bool,
    config_path: str | Path | None = None,
    subagents_path: str | Path | None = None,
) -> Path:
    """保留功能段其余字段，只更新 ``enabled`` 并原子写回。

    ``subagents`` 段已从 ``config.toml`` 完全迁移到独立的 ``subagents.toml``：
    直接写回子代理设置文件（不存在时自动创建），不再回退写回 ``config.toml``。
    """

    if not isinstance(enabled, bool):
        raise SettingsConfigError(f"配置项 {section_name}.enabled 必须是布尔值。")
    try:
        if section_name == "subagents":
            from .runtime import resolve_subagents_write_path

            target = resolve_subagents_write_path(subagents_path)
            data: dict[str, Any] = load_config_data(target)
            section = get_section(data, "subagents")
            section["enabled"] = enabled
            data["subagents"] = section
            return save_config_data(data, target)
        data: dict[str, Any] = load_config_data(config_path)
        section = get_section(data, section_name)
        section["enabled"] = enabled
        data[section_name] = section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc


def load_show_thinking(config_path: str | Path | None = None) -> bool:
    """读取 ``ui.show_thinking``，缺省时默认开启。

    该开关只控制对话区是否渲染思考块（Markdown 渲染）；模型仍照常产生并
    接收思考内容，不显示不影响推理链路本身。
    """

    try:
        section = get_section(load_config_data(config_path), "ui")
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc
    value = section.get("show_thinking", True)
    if not isinstance(value, bool):
        raise SettingsConfigError("配置项 ui.show_thinking 必须是布尔值。")
    return value


def save_show_thinking(
    enabled: bool,
    config_path: str | Path | None = None,
) -> Path:
    """保留 ``ui`` 段其余字段，只更新 ``show_thinking`` 并原子写回。"""

    if not isinstance(enabled, bool):
        raise SettingsConfigError("配置项 ui.show_thinking 必须是布尔值。")
    try:
        data: dict[str, Any] = load_config_data(config_path)
        section = get_section(data, "ui")
        section["show_thinking"] = enabled
        data["ui"] = section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise SettingsConfigError(str(exc)) from exc


__all__ = [
    "SettingsConfigError",
    "load_feature_enabled",
    "load_show_thinking",
    "save_context_compaction_trigger_percent",
    "save_context_window_tokens",
    "save_feature_enabled",
    "save_mcp_config",
    "save_show_thinking",
    "save_subagent_setting",
]
