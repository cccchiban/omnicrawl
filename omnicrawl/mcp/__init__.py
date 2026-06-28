"""MCP 子系统合并入口。

配置、能力注册、安全策略、审计和客户端管理集中在这里，减少代码文件数量。
模块别名会在导入时注册，兼容 omnicrawl.mcp.client/config 等旧路径。
server.py 保留为 python -m omnicrawl.mcp.server 的执行入口。
"""

from __future__ import annotations

import sys as _sys

_THIS_MODULE = _sys.modules[__name__]
_MCP_MODULE_ALIASES = (
    'registry',
    'config',
    'security',
    'audit',
    'client',
)
for _alias in _MCP_MODULE_ALIASES:
    _sys.modules[f"{__name__}.{_alias}"] = _THIS_MODULE
    globals()[_alias] = _THIS_MODULE

# --- former module: registry.py ---

import json
from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class MCPDiagnostic:
    """MCP 运行期诊断，用于 `/mcp` 和降级提示。"""

    severity: str
    code: str
    message: str
    server_name: str | None = None


@dataclass(frozen=True)
class MCPToolMeta:
    """已发现的 MCP Tool 元数据。"""

    logical_name: str
    server_name: str
    tool_name: str
    description: str
    input_schema: dict[str, Any] = field(default_factory=dict)
    requires_confirmation: bool = True
    risk_level: str = "restricted"

    @property
    def argument_schema(self) -> str:
        if not self.input_schema:
            return "{}"
        return json.dumps(self.input_schema, ensure_ascii=False, separators=(",", ":"))


@dataclass(frozen=True)
class MCPResourceMeta:
    """已发现的 MCP Resource 元数据。"""

    logical_uri: str
    server_name: str
    uri: str
    name: str
    description: str = ""
    mime_type: str = ""


@dataclass(frozen=True)
class MCPPromptMeta:
    """已发现的 MCP Prompt 元数据。"""

    logical_name: str
    server_name: str
    prompt_name: str
    description: str = ""
    arguments: list[dict[str, Any]] = field(default_factory=list)


class MCPCapabilityRegistry:
    """保存本轮会话中可用的 MCP Tool、Resource 和 Prompt。"""

    def __init__(self) -> None:
        self.tools: dict[str, MCPToolMeta] = {}
        self.resources: dict[str, MCPResourceMeta] = {}
        self.prompts: dict[str, MCPPromptMeta] = {}
        self.diagnostics: list[MCPDiagnostic] = []

    def add_diagnostic(
        self,
        severity: str,
        code: str,
        message: str,
        *,
        server_name: str | None = None,
    ) -> None:
        self.diagnostics.append(
            MCPDiagnostic(
                severity=severity,
                code=code,
                message=message,
                server_name=server_name,
            )
        )

    def add_tool(self, meta: MCPToolMeta) -> None:
        if meta.logical_name in self.tools:
            self.add_diagnostic(
                "warning",
                "CAPABILITY_DUPLICATED",
                f"重复的 MCP Tool 名称已跳过：{meta.logical_name}",
                server_name=meta.server_name,
            )
            return
        self.tools[meta.logical_name] = meta

    def add_resource(self, meta: MCPResourceMeta) -> None:
        if meta.logical_uri in self.resources:
            self.add_diagnostic(
                "warning",
                "CAPABILITY_DUPLICATED",
                f"重复的 MCP Resource URI 已跳过：{meta.logical_uri}",
                server_name=meta.server_name,
            )
            return
        self.resources[meta.logical_uri] = meta

    def add_prompt(self, meta: MCPPromptMeta) -> None:
        if meta.logical_name in self.prompts:
            self.add_diagnostic(
                "warning",
                "CAPABILITY_DUPLICATED",
                f"重复的 MCP Prompt 名称已跳过：{meta.logical_name}",
                server_name=meta.server_name,
            )
            return
        self.prompts[meta.logical_name] = meta


def namespace_capability_name(server_name: str, capability_name: str) -> str:
    """生成 Host 侧唯一名称，避免不同 Server 暴露同名能力。"""

    return f"{server_name}.{capability_name.strip()}"


# --- former module: config.py ---

import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping
from urllib.parse import urlparse

from ..config.runtime import RuntimeConfigError, get_section, load_config_data
from ..workspace.tools import MAX_COMMAND_TIMEOUT_SECONDS


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


# --- former module: security.py ---

import json
from typing import Any

from .config import (
    MCPPolicyConfig,
    MCP_RISK_EXTERNAL,
    MCP_RISK_RESTRICTED,
)
from .registry import MCPToolMeta


_SENSITIVE_KEY_PARTS = (
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "key",
    "password",
    "secret",
    "token",
)
_WRITE_KEYWORDS = (
    "append",
    "create",
    "edit",
    "modify",
    "patch",
    "replace",
    "save",
    "update",
    "write",
)
_COMMAND_KEYWORDS = (
    "command",
    "exec",
    "powershell",
    "run",
    "shell",
    "spawn",
    "terminal",
)
_DESTRUCTIVE_KEYWORDS = (
    "delete",
    "destroy",
    "drop",
    "force",
    "kill",
    "remove",
    "reset",
    "truncate",
)


def mcp_tool_requires_confirmation(meta: MCPToolMeta, policy: MCPPolicyConfig) -> bool:
    """按 Host 策略判断 MCP Tool 是否需要进入现有审批门。

    Server 自称可信不代表免审；这里用名称关键词和 Server 风险等级做保守判断。
    外部或受限 Server 默认需要确认，trusted Server 的明显只读工具才自动放行。
    """

    lowered_name = meta.tool_name.lower()
    if any(keyword in lowered_name for keyword in _DESTRUCTIVE_KEYWORDS):
        return True
    if policy.require_confirmation_for_command and any(
        keyword in lowered_name for keyword in _COMMAND_KEYWORDS
    ):
        return True
    if policy.require_confirmation_for_write and any(
        keyword in lowered_name for keyword in _WRITE_KEYWORDS
    ):
        return True
    if meta.risk_level in {MCP_RISK_EXTERNAL, MCP_RISK_RESTRICTED}:
        return True
    return False


def validate_tool_arguments(
    arguments: dict[str, Any],
    input_schema: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """限制 MCP Tool 参数体积和基本形状。

    MCP Server 会用自己的 JSON Schema 再校验一次；Host 侧先挡掉明显异常的大对象，
    避免模型把超大内容或非 JSON 对象直接塞给外部进程。
    """

    if not isinstance(arguments, dict):
        raise ValueError("MCP Tool 参数必须是 JSON 对象。")
    for key in arguments:
        if not isinstance(key, str):
            raise ValueError("MCP Tool 参数键必须是字符串。")

    payload = json.dumps(arguments, ensure_ascii=False)
    if len(payload) > 100_000:
        raise ValueError("MCP Tool 参数超过 100000 字符。")
    if input_schema:
        _validate_schema_object(arguments, input_schema)
    return arguments


def _validate_schema_object(arguments: dict[str, Any], schema: dict[str, Any]) -> None:
    """执行轻量 JSON Schema 校验，覆盖 MCP Tool 常见入参边界。"""

    schema_type = schema.get("type")
    if schema_type not in (None, "object"):
        return

    required = schema.get("required", [])
    if isinstance(required, list):
        for key in required:
            if isinstance(key, str) and key not in arguments:
                raise ValueError(f"MCP Tool 参数缺少必填字段：{key}。")

    properties = schema.get("properties", {})
    if not isinstance(properties, dict):
        return
    for key, value in arguments.items():
        field_schema = properties.get(key)
        if isinstance(field_schema, dict):
            _validate_schema_value(key, value, field_schema)


def _validate_schema_value(key: str, value: Any, schema: dict[str, Any]) -> None:
    expected_type = schema.get("type")
    if isinstance(expected_type, list):
        expected_types = [item for item in expected_type if isinstance(item, str)]
    elif isinstance(expected_type, str):
        expected_types = [expected_type]
    else:
        expected_types = []

    if expected_types and not any(_matches_json_type(value, expected) for expected in expected_types):
        allowed = "/".join(expected_types)
        raise ValueError(f"MCP Tool 参数 {key} 类型应为 {allowed}。")

    if isinstance(value, str):
        max_length = _read_int(schema.get("maxLength"), default=20_000)
        min_length = _read_int(schema.get("minLength"), default=0)
        if len(value) > max_length:
            raise ValueError(f"MCP Tool 参数 {key} 超过 {max_length} 字符。")
        if len(value) < min_length:
            raise ValueError(f"MCP Tool 参数 {key} 少于 {min_length} 字符。")

    if isinstance(value, list):
        max_items = _read_int(schema.get("maxItems"), default=200)
        min_items = _read_int(schema.get("minItems"), default=0)
        if len(value) > max_items:
            raise ValueError(f"MCP Tool 参数 {key} 超过 {max_items} 项。")
        if len(value) < min_items:
            raise ValueError(f"MCP Tool 参数 {key} 少于 {min_items} 项。")

    if isinstance(value, (int, float)) and not isinstance(value, bool):
        maximum = schema.get("maximum")
        minimum = schema.get("minimum")
        if isinstance(maximum, (int, float)) and value > maximum:
            raise ValueError(f"MCP Tool 参数 {key} 不能大于 {maximum}。")
        if isinstance(minimum, (int, float)) and value < minimum:
            raise ValueError(f"MCP Tool 参数 {key} 不能小于 {minimum}。")


def _matches_json_type(value: Any, expected: str) -> bool:
    if expected == "string":
        return isinstance(value, str)
    if expected == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if expected == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if expected == "boolean":
        return isinstance(value, bool)
    if expected == "object":
        return isinstance(value, dict)
    if expected == "array":
        return isinstance(value, list)
    if expected == "null":
        return value is None
    return True


def _read_int(value: Any, *, default: int) -> int:
    if isinstance(value, bool):
        return default
    if isinstance(value, int):
        return value
    return default


def redact_sensitive_values(value: Any) -> Any:
    """递归脱敏常见密钥字段，供审计和错误输出使用。"""

    if isinstance(value, dict):
        redacted: dict[str, Any] = {}
        for key, item in value.items():
            if not isinstance(key, str):
                continue
            lowered = key.lower()
            if any(part in lowered for part in _SENSITIVE_KEY_PARTS):
                redacted[key] = "***"
            else:
                redacted[key] = redact_sensitive_values(item)
        return redacted
    if isinstance(value, list):
        return [redact_sensitive_values(item) for item in value[:100]]
    return value


# --- former module: audit.py ---

import json
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

from .security import redact_sensitive_values


DEFAULT_MCP_AUDIT_LOG_PATH = "logs/mcp-audit.jsonl"


@dataclass(frozen=True)
class MCPAuditEvent:
    """一条 MCP 调用审计记录。"""

    timestamp: str
    session_id: str
    audit_id: str
    server_name: str
    tool_name: str
    arguments_redacted: dict[str, Any]
    approval_mode: str
    approval_result: str
    duration_ms: int
    ok: bool
    error_code: str | None
    output_preview: str


class MCPAuditLogger:
    """把 MCP 工具调用写入本地 JSONL 审计日志。"""

    def __init__(
        self,
        workspace_root: Path,
        *,
        enabled: bool,
        relative_path: str = DEFAULT_MCP_AUDIT_LOG_PATH,
    ) -> None:
        self.enabled = enabled
        self.workspace_root = workspace_root.resolve()
        self.path = self._resolve_log_path(relative_path)

    def record_tool_call(
        self,
        *,
        session_id: str,
        audit_id: str,
        server_name: str,
        tool_name: str,
        arguments: dict[str, Any],
        approval_mode: str,
        approval_result: str,
        duration_ms: int,
        ok: bool,
        error_code: str | None,
        output: str,
    ) -> None:
        """记录一次 MCP Tool 调用。

        审计只保存参数脱敏版和输出预览，避免把密钥或大文件内容写入日志。
        """

        if not self.enabled:
            return

        event = MCPAuditEvent(
            timestamp=datetime.now().astimezone().isoformat(timespec="seconds"),
            session_id=session_id,
            audit_id=audit_id,
            server_name=server_name,
            tool_name=tool_name,
            arguments_redacted=redact_sensitive_values(arguments),
            approval_mode=approval_mode,
            approval_result=approval_result,
            duration_ms=duration_ms,
            ok=ok,
            error_code=error_code,
            output_preview=_preview(output),
        )
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with self.path.open("a", encoding="utf-8") as file:
                file.write(json.dumps(event.__dict__, ensure_ascii=False) + "\n")
        except OSError:
            # 审计日志不能影响主业务。调用方仍会在工具结果中看到真实执行状态。
            return

    def _resolve_log_path(self, relative_path: str) -> Path:
        cleaned = relative_path.strip().replace("\\", "/") or DEFAULT_MCP_AUDIT_LOG_PATH
        candidate = Path(cleaned)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        try:
            resolved.relative_to(self.workspace_root)
        except ValueError:
            return self.workspace_root / DEFAULT_MCP_AUDIT_LOG_PATH
        return resolved


def _preview(output: str, max_chars: int = 1000) -> str:
    text = output.strip()
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + "\n... 审计输出预览已截断。"


# --- former module: client.py ---

import json
import os
import queue
import shutil
import subprocess
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from .audit import MCPAuditLogger
from .config import (
    MCPConfig,
    MCPServerConfig,
    MCP_RISK_EXTERNAL,
    MCP_TRANSPORT_STDIO,
    load_mcp_config,
)
from .registry import (
    MCPCapabilityRegistry,
    MCPPromptMeta,
    MCPResourceMeta,
    MCPToolMeta,
    namespace_capability_name,
)
from .security import mcp_tool_requires_confirmation, validate_tool_arguments


MCP_PROTOCOL_VERSION = "2024-11-05"


class MCPClientError(RuntimeError):
    """MCP 连接、能力发现或工具调用失败。"""


@dataclass(frozen=True)
class MCPToolCallResult:
    """MCP Tool 调用返回给 Agent 的结构化结果。"""

    ok: bool
    server_name: str
    tool_name: str
    output: str
    error_code: str | None = None
    retryable: bool = False
    duration_ms: int = 0
    audit_id: str = ""


@dataclass(frozen=True)
class MCPResourceReadResult:
    """MCP Resource 读取结果。"""

    ok: bool
    server_name: str
    uri: str
    output: str
    error_code: str | None = None
    retryable: bool = False
    duration_ms: int = 0


@dataclass(frozen=True)
class MCPPromptReadResult:
    """MCP Prompt 获取结果。"""

    ok: bool
    server_name: str
    prompt_name: str
    output: str
    error_code: str | None = None
    retryable: bool = False
    duration_ms: int = 0


class MCPClientManager:
    """管理多个 MCP Server 的生命周期、能力注册和工具调用。"""

    def __init__(
        self,
        config: MCPConfig | None = None,
        *,
        workspace_root: Path | None = None,
        approval_mode_getter: Callable[[], str] | None = None,
    ) -> None:
        self.config = config or load_mcp_config()
        self.registry = MCPCapabilityRegistry()
        self._connections: dict[str, _StdioMCPConnection] = {}
        self._server_status: dict[str, str] = {
            name: "disabled" for name in self.config.servers
        }
        self._failure_counts: dict[str, int] = {name: 0 for name in self.config.servers}
        self._discovered = False
        self._session_id = f"session-{uuid.uuid4().hex[:12]}"
        self._approval_mode_getter = approval_mode_getter or (lambda: "")
        self._audit_logger = MCPAuditLogger(
            workspace_root or Path.cwd(),
            enabled=self.config.policy.audit_log_enabled,
        )

    @property
    def enabled(self) -> bool:
        return self.config.enabled

    @property
    def discovered(self) -> bool:
        """返回是否已经执行过能力发现，用于 Host 惰性加载 MCP 能力。"""

        return self._discovered

    def discover(self) -> None:
        """连接启用的 Server 并发现 Tool、Resource、Prompt。

        单个 Server 失败只记录诊断并降级，不影响其他 Server 或内置工具。
        """

        self._discovered = True
        if not self.config.enabled:
            self.registry.add_diagnostic("info", "MCP_DISABLED", "MCP 已关闭。")
            return

        enabled_servers = self.config.enabled_servers
        if not enabled_servers:
            self.registry.add_diagnostic(
                "warning",
                "NO_ENABLED_SERVERS",
                "MCP 已启用，但没有启用的 Server。",
            )
            return

        for server in enabled_servers:
            if server.transport != MCP_TRANSPORT_STDIO:
                self._server_status[server.name] = "unsupported"
                self.registry.add_diagnostic(
                    "warning",
                    "TRANSPORT_UNSUPPORTED",
                    f"暂不支持 {server.transport} 传输，已跳过 Server：{server.name}",
                    server_name=server.name,
                )
                continue

            connection = _StdioMCPConnection(server, self._audit_logger.workspace_root)
            try:
                capabilities = connection.discover()
            except Exception as exc:
                connection.close()
                self._server_status[server.name] = "degraded"
                self._failure_counts[server.name] = self._failure_counts.get(server.name, 0) + 1
                self.registry.add_diagnostic(
                    "error",
                    "SERVER_UNAVAILABLE",
                    f"MCP Server 连接或能力发现失败：{exc}",
                    server_name=server.name,
                )
                continue

            self._connections[server.name] = connection
            self._server_status[server.name] = "connected"
            self._register_capabilities(server, capabilities)

    def close(self) -> None:
        """关闭所有由 Host 启动的 stdio MCP Server。"""

        for connection in list(self._connections.values()):
            connection.close()
        self._connections.clear()

    def call_tool(self, logical_name: str, arguments: dict[str, Any]) -> MCPToolCallResult:
        """调用已注册的 MCP Tool，并把协议结果归一化为文本输出。"""

        started = time.perf_counter()
        audit_id = f"mcp-{uuid.uuid4().hex[:12]}"
        meta = self.registry.tools.get(logical_name)
        if meta is None:
            result = MCPToolCallResult(
                ok=False,
                server_name="",
                tool_name=logical_name,
                output=f"未知 MCP Tool：{logical_name}",
                error_code="TOOL_NOT_FOUND",
                retryable=False,
                audit_id=audit_id,
            )
            self._audit_tool_result(result, arguments, approval_result="not_found")
            return result

        try:
            safe_arguments = validate_tool_arguments(arguments, meta.input_schema)
        except ValueError as exc:
            result = MCPToolCallResult(
                ok=False,
                server_name=meta.server_name,
                tool_name=meta.tool_name,
                output=str(exc),
                error_code="SCHEMA_INVALID",
                retryable=False,
                audit_id=audit_id,
            )
            self._audit_tool_result(result, arguments, approval_result="schema_invalid")
            return result

        connection = self._connections.get(meta.server_name)
        if connection is None:
            self._record_tool_failure(meta.server_name)
            result = MCPToolCallResult(
                ok=False,
                server_name=meta.server_name,
                tool_name=meta.tool_name,
                output=f"MCP Server 不可用：{meta.server_name}",
                error_code="SERVER_UNAVAILABLE",
                retryable=True,
                audit_id=audit_id,
            )
            self._audit_tool_result(result, arguments, approval_result="server_unavailable")
            return result

        try:
            payload = connection.call_tool(meta.tool_name, safe_arguments)
            output = _stringify_tool_result_payload(payload)
            ok = payload.get("isError") is not True
            error_code = "TOOL_FAILED" if not ok else None
            retryable = False
        except TimeoutError as exc:
            ok = False
            output = str(exc)
            error_code = "TOOL_TIMEOUT"
            retryable = True
        except ValueError as exc:
            ok = False
            output = str(exc)
            error_code = "SCHEMA_INVALID"
            retryable = False
        except Exception as exc:
            ok = False
            output = str(exc)
            error_code = "TOOL_FAILED"
            retryable = True

        duration_ms = int((time.perf_counter() - started) * 1000)
        if not ok:
            self._record_tool_failure(meta.server_name)
        else:
            self._failure_counts[meta.server_name] = 0

        result = MCPToolCallResult(
            ok=ok,
            server_name=meta.server_name,
            tool_name=meta.tool_name,
            output=_truncate_output(output, self.config.max_tool_output_chars),
            error_code=error_code,
            retryable=retryable,
            duration_ms=duration_ms,
            audit_id=audit_id,
        )
        self._audit_tool_result(result, arguments, approval_result="approved")
        return result

    def record_denied_tool_call(
        self,
        logical_name: str,
        arguments: dict[str, Any],
        reason: str,
    ) -> None:
        """记录 Host 审批拒绝的 MCP Tool 调用。"""

        meta = self.registry.tools.get(logical_name)
        server_name = meta.server_name if meta is not None else ""
        tool_name = meta.tool_name if meta is not None else logical_name
        result = MCPToolCallResult(
            ok=False,
            server_name=server_name,
            tool_name=tool_name,
            output=reason,
            error_code="APPROVAL_DENIED",
            retryable=False,
            audit_id=f"mcp-{uuid.uuid4().hex[:12]}",
        )
        self._audit_tool_result(result, arguments, approval_result="denied")

    def read_resource(self, logical_uri: str) -> MCPResourceReadResult:
        """按注册表中的逻辑 URI 读取 MCP Resource。"""

        started = time.perf_counter()
        meta = self.registry.resources.get(logical_uri)
        if meta is None:
            return MCPResourceReadResult(
                ok=False,
                server_name="",
                uri=logical_uri,
                output=f"未知 MCP Resource：{logical_uri}",
                error_code="RESOURCE_NOT_FOUND",
            )

        connection = self._connections.get(meta.server_name)
        if connection is None:
            return MCPResourceReadResult(
                ok=False,
                server_name=meta.server_name,
                uri=meta.uri,
                output=f"MCP Server 不可用：{meta.server_name}",
                error_code="SERVER_UNAVAILABLE",
                retryable=True,
            )

        try:
            payload = connection.read_resource(meta.uri)
            output = _stringify_resource_payload(payload)
            ok = True
            error_code = None
            retryable = False
        except TimeoutError as exc:
            ok = False
            output = str(exc)
            error_code = "TOOL_TIMEOUT"
            retryable = True
        except Exception as exc:
            ok = False
            output = str(exc)
            error_code = "TOOL_FAILED"
            retryable = True

        return MCPResourceReadResult(
            ok=ok,
            server_name=meta.server_name,
            uri=meta.uri,
            output=_truncate_output(output, self.config.max_tool_output_chars),
            error_code=error_code,
            retryable=retryable,
            duration_ms=int((time.perf_counter() - started) * 1000),
        )

    def get_prompt(self, logical_name: str, arguments: dict[str, Any] | None = None) -> MCPPromptReadResult:
        """按注册表中的逻辑名称获取 MCP Prompt 内容。"""

        started = time.perf_counter()
        meta = self.registry.prompts.get(logical_name)
        if meta is None:
            return MCPPromptReadResult(
                ok=False,
                server_name="",
                prompt_name=logical_name,
                output=f"未知 MCP Prompt：{logical_name}",
                error_code="PROMPT_NOT_FOUND",
            )

        connection = self._connections.get(meta.server_name)
        if connection is None:
            return MCPPromptReadResult(
                ok=False,
                server_name=meta.server_name,
                prompt_name=meta.prompt_name,
                output=f"MCP Server 不可用：{meta.server_name}",
                error_code="SERVER_UNAVAILABLE",
                retryable=True,
            )

        try:
            payload = connection.get_prompt(meta.prompt_name, arguments or {})
            output = _stringify_prompt_payload(payload)
            ok = True
            error_code = None
            retryable = False
        except TimeoutError as exc:
            ok = False
            output = str(exc)
            error_code = "TOOL_TIMEOUT"
            retryable = True
        except Exception as exc:
            ok = False
            output = str(exc)
            error_code = "TOOL_FAILED"
            retryable = True

        return MCPPromptReadResult(
            ok=ok,
            server_name=meta.server_name,
            prompt_name=meta.prompt_name,
            output=_truncate_output(output, self.config.max_tool_output_chars),
            error_code=error_code,
            retryable=retryable,
            duration_ms=int((time.perf_counter() - started) * 1000),
        )

    def format_status(self) -> str:
        """返回适合终端展示的 MCP 状态摘要。"""

        if not self.config.enabled:
            return "MCP 已关闭。可在 config.json 的 mcp.enabled=true 开启。"

        if not self._discovered:
            self.discover()

        connected = sum(1 for status in self._server_status.values() if status == "connected")
        enabled_total = len(self.config.enabled_servers)
        lines = [
            f"MCP 已启用：{connected}/{enabled_total} 个 Server 已连接。",
            (
                f"已注册 Tool {len(self.registry.tools)} 个，"
                f"Resource {len(self.registry.resources)} 个，"
                f"Prompt {len(self.registry.prompts)} 个。"
            ),
        ]

        if self.config.servers:
            lines.append("Server：")
            for name, server in sorted(self.config.servers.items()):
                status = self._server_status.get(name, "unknown")
                enabled_label = "启用" if server.enabled else "禁用"
                failures = self._failure_counts.get(name, 0)
                suffix = f", 连续失败 {failures} 次" if failures else ""
                lines.append(f"  - {name}: {enabled_label}, {server.transport}, {status}{suffix}")

        if self.registry.tools:
            lines.append("Tool：")
            for name in sorted(self.registry.tools):
                meta = self.registry.tools[name]
                confirm_label = "需确认" if meta.requires_confirmation else "自动"
                lines.append(f"  - {name} ({confirm_label})")

        if self.registry.resources:
            lines.append("Resource：")
            for uri in sorted(self.registry.resources):
                meta = self.registry.resources[uri]
                lines.append(f"  - {uri} -> {meta.uri}")

        if self.registry.prompts:
            lines.append("Prompt：")
            for name in sorted(self.registry.prompts):
                meta = self.registry.prompts[name]
                lines.append(f"  - {name}: {meta.description or meta.prompt_name}")

        if self.registry.diagnostics:
            lines.append("诊断：")
            for diagnostic in self.registry.diagnostics[-10:]:
                prefix = f"{diagnostic.server_name}: " if diagnostic.server_name else ""
                lines.append(f"  - [{diagnostic.severity}] {prefix}{diagnostic.message}")

        return "\n".join(lines)

    def _register_capabilities(
        self,
        server: MCPServerConfig,
        capabilities: "_DiscoveredCapabilities",
    ) -> None:
        if server.risk_level == MCP_RISK_EXTERNAL and not self.config.policy.allow_external_network_tools:
            self.registry.add_diagnostic(
                "warning",
                "EXTERNAL_TOOLS_BLOCKED",
                f"外部 MCP Server 默认不暴露能力，已跳过：{server.name}",
                server_name=server.name,
            )
            return

        for raw_tool in capabilities.tools:
            name = raw_tool.get("name")
            if not isinstance(name, str) or not name.strip():
                self.registry.add_diagnostic(
                    "warning",
                    "CAPABILITY_INVALID",
                    "MCP Tool 缺少有效 name，已跳过。",
                    server_name=server.name,
                )
                continue

            input_schema = raw_tool.get("inputSchema") or raw_tool.get("input_schema") or {}
            if not isinstance(input_schema, dict):
                input_schema = {}
            description = raw_tool.get("description")
            if not isinstance(description, str) or not description.strip():
                description = f"来自 MCP Server {server.name} 的工具 {name}。"

            logical_name = namespace_capability_name(server.name, name)
            meta = MCPToolMeta(
                logical_name=logical_name,
                server_name=server.name,
                tool_name=name,
                description=description.strip(),
                input_schema=input_schema,
                requires_confirmation=True,
                risk_level=server.risk_level,
            )
            meta = MCPToolMeta(
                logical_name=meta.logical_name,
                server_name=meta.server_name,
                tool_name=meta.tool_name,
                description=meta.description,
                input_schema=meta.input_schema,
                requires_confirmation=mcp_tool_requires_confirmation(
                    meta,
                    self.config.policy,
                ),
                risk_level=meta.risk_level,
            )
            self.registry.add_tool(meta)

        for raw_resource in capabilities.resources:
            uri = raw_resource.get("uri")
            if not isinstance(uri, str) or not uri.strip():
                continue
            name = raw_resource.get("name")
            description = raw_resource.get("description")
            mime_type = raw_resource.get("mimeType") or raw_resource.get("mime_type")
            self.registry.add_resource(
                MCPResourceMeta(
                    logical_uri=f"{server.name}:{uri}",
                    server_name=server.name,
                    uri=uri,
                    name=name if isinstance(name, str) and name else uri,
                    description=description if isinstance(description, str) else "",
                    mime_type=mime_type if isinstance(mime_type, str) else "",
                )
            )

        for raw_prompt in capabilities.prompts:
            name = raw_prompt.get("name")
            if not isinstance(name, str) or not name.strip():
                continue
            description = raw_prompt.get("description")
            arguments = raw_prompt.get("arguments")
            self.registry.add_prompt(
                MCPPromptMeta(
                    logical_name=namespace_capability_name(server.name, name),
                    server_name=server.name,
                    prompt_name=name,
                    description=description if isinstance(description, str) else "",
                    arguments=arguments if isinstance(arguments, list) else [],
                )
            )

    def _record_tool_failure(self, server_name: str) -> None:
        count = self._failure_counts.get(server_name, 0) + 1
        self._failure_counts[server_name] = count
        if count >= 3:
            self._server_status[server_name] = "degraded"
            self.registry.add_diagnostic(
                "warning",
                "SERVER_DEGRADED",
                f"MCP Server 连续失败 {count} 次，已标记为 degraded：{server_name}",
                server_name=server_name,
            )

    def _audit_tool_result(
        self,
        result: MCPToolCallResult,
        arguments: dict[str, Any],
        *,
        approval_result: str,
    ) -> None:
        self._audit_logger.record_tool_call(
            session_id=self._session_id,
            audit_id=result.audit_id,
            server_name=result.server_name,
            tool_name=result.tool_name,
            arguments=arguments,
            approval_mode=str(self._approval_mode_getter()),
            approval_result=approval_result,
            duration_ms=result.duration_ms,
            ok=result.ok,
            error_code=result.error_code,
            output=result.output,
        )


@dataclass(frozen=True)
class _DiscoveredCapabilities:
    tools: list[dict[str, Any]]
    resources: list[dict[str, Any]]
    prompts: list[dict[str, Any]]


def _resolve_stdio_command(command: str) -> str:
    """解析 stdio Server 命令到真实可执行路径。

    Windows 的 CreateProcess 在 shell=False 且传入列表参数时，不总是按
    PATHEXT 找到 `npx.cmd` 这类 shim；先用 shutil.which 解析，可以保留
    非 shell 启动方式，同时兼容 Node/npm 等常见命令。
    """

    stripped = command.strip()
    if not stripped:
        raise MCPClientError("stdio MCP Server 缺少 command。")
    return shutil.which(stripped) or stripped


class _StdioMCPConnection:
    """最小 MCP stdio JSON-RPC 客户端。

    MCP stdio 使用 `Content-Length` 帧承载 JSON-RPC。这里实现初始化、
    能力发现和工具调用的最小闭环，避免第一阶段引入额外依赖。
    """

    def __init__(self, server: MCPServerConfig, workspace_root: Path) -> None:
        self.server = server
        self.workspace_root = workspace_root.resolve()
        self._process: subprocess.Popen[bytes] | None = None
        self._next_request_id = 1
        self._lock = threading.Lock()

    def discover(self) -> _DiscoveredCapabilities:
        self._start()
        self._request(
            "initialize",
            {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ai-voice-agent", "version": "0.1"},
            },
        )
        self._notify("notifications/initialized", {})
        return _DiscoveredCapabilities(
            tools=self._list_capability("tools/list", "tools"),
            resources=self._list_capability("resources/list", "resources"),
            prompts=self._list_capability("prompts/list", "prompts"),
        )

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        payload = self._request("tools/call", {"name": name, "arguments": arguments})
        if not isinstance(payload, dict):
            raise MCPClientError("MCP Tool 返回结果必须是 JSON 对象。")
        return payload

    def read_resource(self, uri: str) -> dict[str, Any]:
        payload = self._request("resources/read", {"uri": uri})
        if not isinstance(payload, dict):
            raise MCPClientError("MCP Resource 返回结果必须是 JSON 对象。")
        return payload

    def get_prompt(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        payload = self._request("prompts/get", {"name": name, "arguments": arguments})
        if not isinstance(payload, dict):
            raise MCPClientError("MCP Prompt 返回结果必须是 JSON 对象。")
        return payload

    def close(self) -> None:
        process = self._process
        self._process = None
        if process is None:
            return
        try:
            if process.stdin:
                process.stdin.close()
        except OSError:
            pass
        try:
            if process.stdout:
                process.stdout.close()
        except OSError:
            pass
        try:
            if process.stderr:
                process.stderr.close()
        except OSError:
            pass
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=2)

    def _start(self) -> None:
        if self._process is not None and self._process.poll() is None:
            return
        if not self.server.command:
            raise MCPClientError("stdio MCP Server 缺少 command。")

        command = [_resolve_stdio_command(self.server.command), *self.server.args]
        env = os.environ.copy()
        env.update(self.server.env)
        env.setdefault("MCP_WORKSPACE_ROOT", str(self.workspace_root))
        creationflags = 0
        if os.name == "nt" and hasattr(subprocess, "CREATE_NO_WINDOW"):
            creationflags = subprocess.CREATE_NO_WINDOW

        try:
            self._process = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=env,
                creationflags=creationflags,
            )
        except OSError as exc:
            raise MCPClientError(f"启动 MCP Server 失败：{exc}") from exc

    def _list_capability(self, method: str, result_key: str) -> list[dict[str, Any]]:
        try:
            payload = self._request(method, {})
        except Exception:
            return []
        if not isinstance(payload, dict):
            return []
        values = payload.get(result_key, [])
        if not isinstance(values, list):
            return []
        return [value for value in values if isinstance(value, dict)]

    def _request(self, method: str, params: dict[str, Any]) -> Any:
        with self._lock:
            request_id = self._next_request_id
            self._next_request_id += 1
            self._send_message(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": method,
                    "params": params,
                }
            )
            response = self._read_response_with_timeout(request_id)

        if "error" in response:
            error = response.get("error")
            if isinstance(error, dict):
                message = error.get("message") or json.dumps(error, ensure_ascii=False)
            else:
                message = str(error)
            raise MCPClientError(f"MCP 请求 {method} 失败：{message}")
        return response.get("result")

    def _notify(self, method: str, params: dict[str, Any]) -> None:
        self._send_message(
            {
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }
        )

    def _send_message(self, message: dict[str, Any]) -> None:
        process = self._require_process()
        if process.stdin is None:
            raise MCPClientError("MCP Server stdin 不可用。")
        body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        try:
            process.stdin.write(header + body)
            process.stdin.flush()
        except OSError as exc:
            raise MCPClientError(f"写入 MCP Server 失败：{exc}") from exc

    def _read_response_with_timeout(self, request_id: int) -> dict[str, Any]:
        result_queue: queue.Queue[dict[str, Any] | BaseException] = queue.Queue(maxsize=1)

        def read_worker() -> None:
            try:
                result_queue.put(self._read_response(request_id))
            except BaseException as exc:  # noqa: BLE001 - 需要跨线程传回原始异常。
                result_queue.put(exc)

        thread = threading.Thread(target=read_worker, daemon=True)
        thread.start()
        try:
            result = result_queue.get(timeout=self.server.timeout_seconds)
        except queue.Empty as exc:
            self.close()
            raise TimeoutError(
                f"MCP 请求超过 {self.server.timeout_seconds} 秒：{self.server.name}"
            ) from exc
        if isinstance(result, BaseException):
            raise result
        return result

    def _read_response(self, request_id: int) -> dict[str, Any]:
        while True:
            message = self._read_message()
            if message.get("id") != request_id:
                continue
            return message

    def _read_message(self) -> dict[str, Any]:
        process = self._require_process()
        if process.stdout is None:
            raise MCPClientError("MCP Server stdout 不可用。")

        header = bytearray()
        while not header.endswith(b"\r\n\r\n") and not header.endswith(b"\n\n"):
            chunk = process.stdout.read(1)
            if not chunk:
                stderr = self._read_stderr_preview(process)
                suffix = f" stderr: {stderr}" if stderr else ""
                raise MCPClientError(f"MCP Server 已退出或关闭 stdout。{suffix}")
            header.extend(chunk)
            if len(header) > 8192:
                raise MCPClientError("MCP 响应头超过 8192 字节。")

        content_length = _parse_content_length(bytes(header))
        body = process.stdout.read(content_length)
        if len(body) != content_length:
            raise MCPClientError("MCP 响应体长度不完整。")

        try:
            payload = json.loads(body.decode("utf-8"))
        except json.JSONDecodeError as exc:
            raise MCPClientError(f"MCP 响应不是合法 JSON：{exc}") from exc
        if not isinstance(payload, dict):
            raise MCPClientError("MCP 响应必须是 JSON 对象。")
        return payload

    def _require_process(self) -> subprocess.Popen[bytes]:
        process = self._process
        if process is None:
            raise MCPClientError("MCP Server 尚未启动。")
        if process.poll() is not None:
            raise MCPClientError(f"MCP Server 已退出，退出码：{process.returncode}")
        return process

    @staticmethod
    def _read_stderr_preview(process: subprocess.Popen[bytes]) -> str:
        if process.stderr is None:
            return ""
        if process.poll() is None:
            return ""
        try:
            return process.stderr.read(1000).decode("utf-8", errors="replace").strip()
        except OSError:
            return ""


def _parse_content_length(header: bytes) -> int:
    text = header.decode("ascii", errors="replace")
    for line in text.splitlines():
        key, separator, value = line.partition(":")
        if separator and key.strip().lower() == "content-length":
            try:
                length = int(value.strip())
            except ValueError as exc:
                raise MCPClientError("MCP Content-Length 不是整数。") from exc
            if length < 0 or length > 50_000_000:
                raise MCPClientError("MCP Content-Length 超出允许范围。")
            return length
    raise MCPClientError("MCP 响应缺少 Content-Length。")


def _stringify_tool_result_payload(payload: dict[str, Any]) -> str:
    content = payload.get("content")
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if not isinstance(item, dict):
                continue
            item_type = item.get("type")
            if item_type == "text" and isinstance(item.get("text"), str):
                parts.append(item["text"])
            elif item_type:
                parts.append(json.dumps(item, ensure_ascii=False))
        if parts:
            return "\n".join(parts)
    return json.dumps(payload, ensure_ascii=False, indent=2)


def _stringify_resource_payload(payload: dict[str, Any]) -> str:
    contents = payload.get("contents")
    if isinstance(contents, list):
        parts: list[str] = []
        for item in contents:
            if not isinstance(item, dict):
                continue
            uri = item.get("uri")
            text = item.get("text")
            blob = item.get("blob")
            title = f"Resource {uri}:" if isinstance(uri, str) else "Resource:"
            if isinstance(text, str):
                parts.append(f"{title}\n{text}")
            elif isinstance(blob, str):
                parts.append(f"{title}\n<base64 blob，字符数 {len(blob)}>")
        if parts:
            return "\n\n".join(parts)
    return json.dumps(payload, ensure_ascii=False, indent=2)


def _stringify_prompt_payload(payload: dict[str, Any]) -> str:
    messages = payload.get("messages")
    if isinstance(messages, list):
        parts: list[str] = []
        for item in messages:
            if not isinstance(item, dict):
                continue
            role = item.get("role", "unknown")
            content = item.get("content")
            if isinstance(content, dict):
                if isinstance(content.get("text"), str):
                    parts.append(f"{role}: {content['text']}")
                else:
                    parts.append(f"{role}: {json.dumps(content, ensure_ascii=False)}")
            else:
                parts.append(f"{role}: {content}")
        if parts:
            return "\n".join(parts)
    return json.dumps(payload, ensure_ascii=False, indent=2)


def _truncate_output(output: str, max_chars: int) -> str:
    if len(output) <= max_chars:
        return output
    return output[:max_chars] + "\n... MCP 工具输出已截断。"


__all__ = [
    'MCPCapabilityRegistry',
    'MCPClientError',
    'MCPClientManager',
    'MCPConfig',
    'MCPConfigError',
    'MCPDiagnostic',
    'MCPPolicyConfig',
    'MCPPromptMeta',
    'MCPPromptReadResult',
    'MCPResourceMeta',
    'MCPResourceReadResult',
    'MCPServerConfig',
    'MCPToolCallResult',
    'MCPToolMeta',
    'load_mcp_config',
]
