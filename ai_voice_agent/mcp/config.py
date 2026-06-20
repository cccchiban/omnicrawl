from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping
from urllib.parse import urlparse

from ..runtime_config import RuntimeConfigError, get_section, load_config_data
from ..workspace_tools import MAX_COMMAND_TIMEOUT_SECONDS


MCP_TRANSPORT_STDIO = "stdio"
MCP_TRANSPORT_STREAMABLE_HTTP = "streamable_http"
VALID_MCP_TRANSPORTS = {MCP_TRANSPORT_STDIO, MCP_TRANSPORT_STREAMABLE_HTTP}

MCP_RISK_TRUSTED = "trusted"
MCP_RISK_RESTRICTED = "restricted"
MCP_RISK_EXTERNAL = "external"
VALID_MCP_RISK_LEVELS = {MCP_RISK_TRUSTED, MCP_RISK_RESTRICTED, MCP_RISK_EXTERNAL}

_SERVER_NAME_PATTERN = re.compile(r"^[a-z0-9_-]+$")
_BOOL_TRUE_VALUES = {"1", "true", "yes", "on", "enabled", "启用", "是"}
_BOOL_FALSE_VALUES = {"0", "false", "no", "off", "disabled", "禁用", "否"}


class MCPConfigError(RuntimeConfigError):
    """MCP 配置读取或校验失败。"""


@dataclass(frozen=True)
class MCPPolicyConfig:
    """Host 侧 MCP 安全策略。

    这些配置只决定默认策略；实际工具调用仍会在 Host 侧按工具名称、
    Server 风险等级和审批模式再次判断，避免完全信任外部 Server 声明。
    """

    require_confirmation_for_write: bool = True
    require_confirmation_for_command: bool = True
    allow_external_network_tools: bool = False
    audit_log_enabled: bool = True


@dataclass(frozen=True)
class MCPServerConfig:
    """单个 MCP Server 的连接配置。"""

    name: str
    enabled: bool = True
    transport: str = MCP_TRANSPORT_STDIO
    command: str | None = None
    args: list[str] = field(default_factory=list)
    url: str | None = None
    env: dict[str, str] = field(default_factory=dict)
    timeout_seconds: int = 30
    risk_level: str = MCP_RISK_RESTRICTED


@dataclass(frozen=True)
class MCPConfig:
    """MCP 子系统总配置。"""

    enabled: bool = False
    default_timeout_seconds: int = 30
    max_tool_output_chars: int = 6000
    servers: dict[str, MCPServerConfig] = field(default_factory=dict)
    policy: MCPPolicyConfig = field(default_factory=MCPPolicyConfig)

    @property
    def enabled_servers(self) -> list[MCPServerConfig]:
        return [server for server in self.servers.values() if server.enabled]


def load_mcp_config(config_path: str | Path | None = None) -> MCPConfig:
    """从 `config.json` 和环境变量读取 MCP 配置。

    环境变量只覆盖全局开关和通用阈值，Server 列表仍放在 JSON 中，
    这样可以避免把复杂命令、参数和环境变量拆散到多个临时配置来源。
    """

    try:
        data = load_config_data(config_path)
        section = get_section(data, "mcp")
    except RuntimeConfigError as exc:
        raise MCPConfigError(str(exc)) from exc

    enabled = _read_bool_field(section, "enabled", default=False, config_key="mcp.enabled")
    env_enabled = _read_bool_env("MCP_ENABLED")
    if env_enabled is not None:
        enabled = env_enabled

    default_timeout = _read_int_field(
        section,
        "default_timeout_seconds",
        default=30,
        min_value=1,
        max_value=MAX_COMMAND_TIMEOUT_SECONDS,
        config_key="mcp.default_timeout_seconds",
    )
    default_timeout = _read_int_env(
        "MCP_DEFAULT_TIMEOUT_SECONDS",
        default_timeout,
        min_value=1,
        max_value=MAX_COMMAND_TIMEOUT_SECONDS,
    )

    max_tool_output_chars = _read_int_field(
        section,
        "max_tool_output_chars",
        default=6000,
        min_value=100,
        max_value=200_000,
        config_key="mcp.max_tool_output_chars",
    )
    max_tool_output_chars = _read_int_env(
        "MCP_MAX_TOOL_OUTPUT_CHARS",
        max_tool_output_chars,
        min_value=100,
        max_value=200_000,
    )

    policy = _load_policy_config(get_section(section, "policy"))
    servers = _load_server_configs(section, default_timeout_seconds=default_timeout)

    return MCPConfig(
        enabled=enabled,
        default_timeout_seconds=default_timeout,
        max_tool_output_chars=max_tool_output_chars,
        servers=servers,
        policy=policy,
    )


def _load_policy_config(section: Mapping[str, Any]) -> MCPPolicyConfig:
    return MCPPolicyConfig(
        require_confirmation_for_write=_read_bool_field(
            section,
            "require_confirmation_for_write",
            default=True,
            config_key="mcp.policy.require_confirmation_for_write",
        ),
        require_confirmation_for_command=_read_bool_field(
            section,
            "require_confirmation_for_command",
            default=True,
            config_key="mcp.policy.require_confirmation_for_command",
        ),
        allow_external_network_tools=_read_bool_field(
            section,
            "allow_external_network_tools",
            default=False,
            config_key="mcp.policy.allow_external_network_tools",
        ),
        audit_log_enabled=_read_bool_field(
            section,
            "audit_log_enabled",
            default=True,
            config_key="mcp.policy.audit_log_enabled",
        ),
    )


def _load_server_configs(
    section: Mapping[str, Any],
    *,
    default_timeout_seconds: int,
) -> dict[str, MCPServerConfig]:
    raw_servers = section.get("servers", {})
    if raw_servers in (None, ""):
        return {}
    if not isinstance(raw_servers, Mapping):
        raise MCPConfigError("配置项 mcp.servers 必须是 JSON 对象。")

    servers: dict[str, MCPServerConfig] = {}
    for raw_name, raw_config in raw_servers.items():
        if not isinstance(raw_name, str):
            raise MCPConfigError("mcp.servers 的 Server 名称必须是字符串。")
        if not isinstance(raw_config, Mapping):
            raise MCPConfigError(f"配置项 mcp.servers.{raw_name} 必须是 JSON 对象。")

        server = _load_server_config(
            raw_name,
            raw_config,
            default_timeout_seconds=default_timeout_seconds,
        )
        servers[server.name] = server
    return servers


def _load_server_config(
    name: str,
    section: Mapping[str, Any],
    *,
    default_timeout_seconds: int,
) -> MCPServerConfig:
    normalized_name = name.strip()
    if not _SERVER_NAME_PATTERN.match(normalized_name):
        raise MCPConfigError(
            f"mcp.servers.{name} 名称只能包含小写字母、数字、下划线和连字符。"
        )

    enabled = _read_bool_field(
        section,
        "enabled",
        default=True,
        config_key=f"mcp.servers.{normalized_name}.enabled",
    )
    transport = _read_text_field(
        section,
        "transport",
        default=MCP_TRANSPORT_STDIO,
        config_key=f"mcp.servers.{normalized_name}.transport",
    )
    if transport not in VALID_MCP_TRANSPORTS:
        allowed = ", ".join(sorted(VALID_MCP_TRANSPORTS))
        raise MCPConfigError(
            f"mcp.servers.{normalized_name}.transport 仅支持 {allowed}，当前值：{transport}。"
        )

    command = _read_optional_text_field(
        section,
        "command",
        config_key=f"mcp.servers.{normalized_name}.command",
    )
    url = _read_optional_text_field(
        section,
        "url",
        config_key=f"mcp.servers.{normalized_name}.url",
    )
    args = _read_text_list_field(
        section,
        "args",
        config_key=f"mcp.servers.{normalized_name}.args",
    )
    env = _read_text_map_field(
        section,
        "env",
        config_key=f"mcp.servers.{normalized_name}.env",
    )
    timeout_seconds = _read_int_field(
        section,
        "timeout_seconds",
        default=default_timeout_seconds,
        min_value=1,
        max_value=MAX_COMMAND_TIMEOUT_SECONDS,
        config_key=f"mcp.servers.{normalized_name}.timeout_seconds",
    )
    risk_level = _read_text_field(
        section,
        "risk_level",
        default=MCP_RISK_RESTRICTED,
        config_key=f"mcp.servers.{normalized_name}.risk_level",
    )
    if risk_level not in VALID_MCP_RISK_LEVELS:
        allowed = ", ".join(sorted(VALID_MCP_RISK_LEVELS))
        raise MCPConfigError(
            f"mcp.servers.{normalized_name}.risk_level 仅支持 {allowed}，当前值：{risk_level}。"
        )

    if enabled and transport == MCP_TRANSPORT_STDIO and not command:
        raise MCPConfigError(f"mcp.servers.{normalized_name}.command 不能为空。")
    if enabled and transport == MCP_TRANSPORT_STREAMABLE_HTTP:
        if not url:
            raise MCPConfigError(f"mcp.servers.{normalized_name}.url 不能为空。")
        _validate_streamable_http_url(url, normalized_name)

    return MCPServerConfig(
        name=normalized_name,
        enabled=enabled,
        transport=transport,
        command=command,
        args=args,
        url=url,
        env=env,
        timeout_seconds=timeout_seconds,
        risk_level=risk_level,
    )


def _validate_streamable_http_url(url: str, server_name: str) -> None:
    parsed = urlparse(url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise MCPConfigError(f"mcp.servers.{server_name}.url 必须是 http(s) URL。")

    hostname = (parsed.hostname or "").lower()
    if parsed.scheme == "http" and hostname not in {"localhost", "127.0.0.1", "::1"}:
        raise MCPConfigError(
            f"mcp.servers.{server_name}.url 默认不允许明文公网 HTTP 地址：{url}"
        )


def _read_bool_env(name: str) -> bool | None:
    value = os.getenv(name)
    if value is None or not value.strip():
        return None
    return _parse_bool(value, config_key=name)


def _read_int_env(
    name: str,
    default: int,
    *,
    min_value: int,
    max_value: int,
) -> int:
    value = os.getenv(name)
    if value is None or not value.strip():
        return default
    return _parse_int(value, min_value=min_value, max_value=max_value, config_key=name)


def _read_bool_field(
    section: Mapping[str, Any],
    key: str,
    *,
    default: bool,
    config_key: str,
) -> bool:
    if key not in section or section[key] is None:
        return default
    return _parse_bool(section[key], config_key=config_key)


def _parse_bool(value: Any, *, config_key: str) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in _BOOL_TRUE_VALUES:
            return True
        if normalized in _BOOL_FALSE_VALUES:
            return False
    raise MCPConfigError(f"配置项 {config_key} 必须是布尔值。")


def _read_int_field(
    section: Mapping[str, Any],
    key: str,
    *,
    default: int,
    min_value: int,
    max_value: int,
    config_key: str,
) -> int:
    if key not in section or section[key] is None:
        return default
    return _parse_int(
        section[key],
        min_value=min_value,
        max_value=max_value,
        config_key=config_key,
    )


def _parse_int(
    value: Any,
    *,
    min_value: int,
    max_value: int,
    config_key: str,
) -> int:
    if isinstance(value, bool):
        raise MCPConfigError(f"配置项 {config_key} 必须是 {min_value} 到 {max_value} 的整数。")
    try:
        parsed = int(value)
    except (TypeError, ValueError) as exc:
        raise MCPConfigError(
            f"配置项 {config_key} 必须是 {min_value} 到 {max_value} 的整数。"
        ) from exc
    if parsed < min_value or parsed > max_value:
        raise MCPConfigError(
            f"配置项 {config_key} 必须是 {min_value} 到 {max_value} 的整数，当前值：{parsed}。"
        )
    return parsed


def _read_text_field(
    section: Mapping[str, Any],
    key: str,
    *,
    default: str,
    config_key: str,
) -> str:
    if key not in section or section[key] is None:
        return default
    value = section[key]
    if not isinstance(value, str):
        raise MCPConfigError(f"配置项 {config_key} 必须是字符串。")
    return value.strip() or default


def _read_optional_text_field(
    section: Mapping[str, Any],
    key: str,
    *,
    config_key: str,
) -> str | None:
    if key not in section or section[key] is None:
        return None
    value = section[key]
    if not isinstance(value, str):
        raise MCPConfigError(f"配置项 {config_key} 必须是字符串。")
    stripped = value.strip()
    return stripped or None


def _read_text_list_field(
    section: Mapping[str, Any],
    key: str,
    *,
    config_key: str,
) -> list[str]:
    value = section.get(key, [])
    if value is None:
        return []
    if not isinstance(value, list):
        raise MCPConfigError(f"配置项 {config_key} 必须是字符串列表。")
    result: list[str] = []
    for index, item in enumerate(value, start=1):
        if not isinstance(item, str):
            raise MCPConfigError(f"配置项 {config_key}[{index}] 必须是字符串。")
        result.append(item)
    return result


def _read_text_map_field(
    section: Mapping[str, Any],
    key: str,
    *,
    config_key: str,
) -> dict[str, str]:
    value = section.get(key, {})
    if value is None:
        return {}
    if not isinstance(value, Mapping):
        raise MCPConfigError(f"配置项 {config_key} 必须是字符串键值对象。")

    result: dict[str, str] = {}
    for raw_key, raw_value in value.items():
        if not isinstance(raw_key, str) or not isinstance(raw_value, str):
            raise MCPConfigError(f"配置项 {config_key} 必须是字符串键值对象。")
        result[raw_key] = raw_value
    return result
