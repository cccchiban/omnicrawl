from __future__ import annotations

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
