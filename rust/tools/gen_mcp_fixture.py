#!/usr/bin/env python3
"""生成 MCP 对照数据集：把 Python 侧真实现的取值钉成数据，供 Rust `omnicrawl-mcp` 逐字段比对。

覆盖八组：

1. ``config``：`config.toml` 的 `[mcp]` 段与环境变量覆盖（含各类校验错误文案）；
2. ``registry``：能力登记的命名空间化、重复跳过与诊断顺序；
3. ``security``：MCP 参数体积与轻量 JSON Schema 校验、密钥脱敏；
4. ``audit``：审计行（固定时刻）与输出预览截断顺序；
5. ``protocol``：`Content-Length` 分帧、JSON-RPC 拆包、SSE 解析、能力分页与结果文本化；
6. ``server``：本地 stdio MCP Server 的响应（含内置文档与受保护路径）；
7. ``manager``：多 Server 管理器的发现、登记、状态、失败降级与调用结果（脚本化连接）；
8. ``http``：Streamable HTTP 客户端的请求形状与会话头推进（假 httpx 客户端）。

用法：``python rust/tools/gen_mcp_fixture.py``
输出：``rust/crates/omnicrawl-mcp/tests/fixtures/mcp_parity.json``
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import re
import sys
import tempfile
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-mcp/tests/fixtures/mcp_parity.json"
WORKSPACE_PLACEHOLDER = "{workspace}"

# 必须加载仓库源码：已安装的 omnicrawl 在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import omnicrawl.mcp.audit as audit_module  # noqa: E402
import omnicrawl.mcp.client as client_module  # noqa: E402
import omnicrawl.mcp.config as config_module  # noqa: E402
import omnicrawl.mcp.registry as registry_module  # noqa: E402
import omnicrawl.mcp.security as security_module  # noqa: E402
import omnicrawl.mcp.server as server_module  # noqa: E402
from omnicrawl.common import documentation  # noqa: E402

ID_PATTERN = re.compile(r"\b(?:session|mcp)-[0-9a-f]{12}\b")
FIXED_NOW = datetime(2026, 9, 20, 1, 54, 10, tzinfo=timezone(timedelta(hours=8)))


def assert_repo_source(module) -> None:
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit(f"加载到的不是仓库源码：{module.__file__}")


def normalize_ids(text: str) -> str:
    """把随机的会话/审计 ID 折成占位符：两侧的取值本来就不可复现。"""

    return ID_PATTERN.sub(lambda match: f"{match.group(0).split('-')[0]}-{{id}}", text)


def normalize_paths(text: str, workspace: Path) -> str:
    """把工作区绝对路径折成占位符：两侧的临时目录本来就不同（Windows 还有 8.3 短名）。"""

    for form in {str(workspace), str(workspace.resolve())}:
        text = text.replace(form, WORKSPACE_PLACEHOLDER)
        text = text.replace(form.replace("\\", "\\\\"), WORKSPACE_PLACEHOLDER)
    return text


def dump(value: Any) -> Any:
    if dataclasses.is_dataclass(value):
        return {field.name: dump(getattr(value, field.name)) for field in dataclasses.fields(value)}
    if isinstance(value, dict):
        return {key: dump(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [dump(item) for item in value]
    return value


# --------------------------------------------------------------------------- config


CONFIG_CASES: list[dict[str, Any]] = [
    {"name": "empty", "toml": "", "env": {}},
    {
        "name": "stdio_server",
        "toml": """
[mcp]
enabled = true
default_timeout_seconds = 45

[mcp.policy]
audit_log_enabled = false

[mcp.servers.files]
command = "npx"
args = ["-y", "server-files"]
env = { MCP_MODE = "readonly" }
timeout_seconds = 20
risk_level = "trusted"
""",
        "env": {},
    },
    {
        "name": "http_server",
        "toml": """
[mcp]
enabled = true

[mcp.servers.remote]
transport = "streamable-http"
url = "https://example.com/mcp"
headers = { Authorization = "Bearer token" }
""",
        "env": {},
    },
    {
        "name": "env_overrides",
        "toml": """
[mcp]
enabled = false
default_timeout_seconds = 10

[mcp.servers.files]
command = "npx"
""",
        "env": {
            "MCP_ENABLED": "YES",
            "MCP_DEFAULT_TIMEOUT_SECONDS": "120",
        },
    },
    {
        "name": "string_booleans",
        "toml": """
[mcp]
enabled = "启用"

[mcp.policy]
require_confirmation_for_write = "0"
require_confirmation_for_command = "关"
""",
        "env": {},
    },
    {"name": "bad_transport", "toml": """
[mcp]
enabled = true

[mcp.servers.files]
transport = "sse"
command = "npx"
""", "env": {}},
    {"name": "bad_risk", "toml": """
[mcp]
enabled = true

[mcp.servers.files]
command = "npx"
risk_level = "danger"
""", "env": {}},
    {"name": "bad_name", "toml": """
[mcp]
enabled = true

[mcp.servers."Files Server"]
command = "npx"
""", "env": {}},
    {"name": "missing_command", "toml": """
[mcp]
enabled = true

[mcp.servers.files]
args = ["-y"]
""", "env": {}},
    {"name": "disabled_server_without_command", "toml": """
[mcp]
enabled = true

[mcp.servers.files]
enabled = false
""", "env": {}},
    {"name": "missing_url", "toml": """
[mcp]
enabled = true

[mcp.servers.remote]
transport = "streamable_http"
""", "env": {}},
    {"name": "plain_http_public", "toml": """
[mcp]
enabled = true

[mcp.servers.remote]
transport = "streamable_http"
url = "http://example.com/mcp"
""", "env": {}},
    {"name": "plain_http_loopback", "toml": """
[mcp]
enabled = true

[mcp.servers.remote]
transport = "streamable_http"
url = "http://127.0.0.1:8765/mcp"
""", "env": {}},
    {"name": "bad_timeout", "toml": """
[mcp]
enabled = true
default_timeout_seconds = 999
""", "env": {}},
    {"name": "timeout_not_integer", "toml": """
[mcp]
enabled = true
default_timeout_seconds = "abc"
""", "env": {}},
    {"name": "bool_not_boolean", "toml": """
[mcp]
enabled = 7
""", "env": {}},
    {"name": "args_not_list", "toml": """
[mcp]
enabled = true

[mcp.servers.files]
command = "npx"
args = "oops"
""", "env": {}},
    {"name": "servers_not_table", "toml": """
[mcp]
enabled = true
servers = "oops"
""", "env": {}},
    {"name": "policy_not_table", "toml": """
[mcp]
enabled = true
policy = 3
""", "env": {}},
    {"name": "mcp_not_table", "toml": 'mcp = "oops"\n', "env": {}},
    {
        "name": "duplicate_server_name",
        "toml": """
[mcp]
enabled = true

[mcp.servers."files "]
command = "first"

[mcp.servers.files]
command = "second"
""",
        "env": {},
    },
]


def config_cases(workspace: Path) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    config_path = workspace / "config.toml"
    saved_env = {name: os.environ.get(name) for name in ("MCP_ENABLED", "MCP_DEFAULT_TIMEOUT_SECONDS")}
    try:
        for case in CONFIG_CASES:
            config_path.write_text(case["toml"], encoding="utf-8")
            for name in ("MCP_ENABLED", "MCP_DEFAULT_TIMEOUT_SECONDS"):
                os.environ.pop(name, None)
            os.environ.update(case["env"])
            common_fields = {"name": case["name"], "toml": case["toml"], "env": case["env"]}
            try:
                config = config_module.load_mcp_config(config_path)
            except config_module.MCPConfigError as exc:
                cases.append({**common_fields, "error": str(exc)})
                continue
            except Exception as exc:  # noqa: BLE001 - 非 MCPConfigError 也如实记录（含类型）
                cases.append({**common_fields, "error": str(exc), "error_kind": type(exc).__name__})
                continue
            cases.append({**common_fields, "config": dump(config)})
    finally:
        for name, value in saved_env.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
    return cases


# ------------------------------------------------------------------------- registry


REGISTRY_CASES: list[dict[str, Any]] = [
    {
        "name": "tools_and_duplicates",
        "steps": [
            {"kind": "tool", "logical_name": "files.read", "server_name": "files", "tool_name": "read", "description": "读", "input_schema": {"type": "object"}},
            {"kind": "tool", "logical_name": "files.read", "server_name": "other", "tool_name": "read", "description": "重复", "input_schema": {}},
            {"kind": "resource", "logical_uri": "files:file:///a", "server_name": "files", "uri": "file:///a", "name": "a"},
            {"kind": "resource", "logical_uri": "files:file:///a", "server_name": "other", "uri": "file:///a", "name": "a"},
            {"kind": "prompt", "logical_name": "files.review", "server_name": "files", "prompt_name": "review", "description": "审查"},
            {"kind": "prompt", "logical_name": "files.review", "server_name": "other", "prompt_name": "review"},
        ],
    },
    {"name": "empty_schema_renders_braces", "steps": [
        {"kind": "tool", "logical_name": "a.echo", "server_name": "a", "tool_name": "echo", "description": "回声", "input_schema": {}},
        {"kind": "tool", "logical_name": "a.complex", "server_name": "a", "tool_name": "complex", "description": "复杂", "input_schema": {"type": "object", "properties": {"文本": {"type": "string"}}, "required": ["文本"]}},
    ]},
]


def registry_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for case in REGISTRY_CASES:
        registry = registry_module.MCPCapabilityRegistry()
        for step in case["steps"]:
            if step["kind"] == "tool":
                registry.add_tool(
                    registry_module.MCPToolMeta(
                        logical_name=step["logical_name"],
                        server_name=step["server_name"],
                        tool_name=step["tool_name"],
                        description=step["description"],
                        input_schema=step.get("input_schema") or {},
                        requires_confirmation=True,
                        risk_level="restricted",
                    )
                )
            elif step["kind"] == "resource":
                registry.add_resource(
                    registry_module.MCPResourceMeta(
                        logical_uri=step["logical_uri"],
                        server_name=step["server_name"],
                        uri=step["uri"],
                        name=step["name"],
                        description=step.get("description", ""),
                        mime_type=step.get("mime_type", ""),
                    )
                )
            else:
                registry.add_prompt(
                    registry_module.MCPPromptMeta(
                        logical_name=step["logical_name"],
                        server_name=step["server_name"],
                        prompt_name=step["prompt_name"],
                        description=step.get("description", ""),
                        arguments=step.get("arguments") or [],
                    )
                )
        cases.append(
            {
                "name": case["name"],
                "steps": case["steps"],
                "tools": [
                    {
                        "logical_name": meta.logical_name,
                        "server_name": meta.server_name,
                        "tool_name": meta.tool_name,
                        "description": meta.description,
                        "requires_confirmation": meta.requires_confirmation,
                        "risk_level": meta.risk_level,
                        "argument_schema": meta.argument_schema,
                    }
                    for meta in registry.tools.values()
                ],
                "resources": [
                    {
                        "logical_uri": meta.logical_uri,
                        "server_name": meta.server_name,
                        "uri": meta.uri,
                        "name": meta.name,
                        "description": meta.description,
                        "mime_type": meta.mime_type,
                    }
                    for meta in registry.resources.values()
                ],
                "prompts": [
                    {
                        "logical_name": meta.logical_name,
                        "server_name": meta.server_name,
                        "prompt_name": meta.prompt_name,
                        "description": meta.description,
                        "arguments": meta.arguments,
                    }
                    for meta in registry.prompts.values()
                ],
                "diagnostics": [dump(item) for item in registry.diagnostics],
            }
        )
    return cases


# ------------------------------------------------------------------------- security


ARGUMENT_CASES: list[dict[str, Any]] = [
    {"name": "plain_ok", "arguments": {"path": "/tmp/a"}, "schema": None},
    {"name": "missing_required", "arguments": {}, "schema": {"type": "object", "required": ["path"]}},
    {"name": "wrong_type", "arguments": {"path": 7}, "schema": {"type": "object", "properties": {"path": {"type": "string"}}}},
    {"name": "union_type", "arguments": {"value": True}, "schema": {"type": "object", "properties": {"value": {"type": ["string", "boolean"]}}}},
    {"name": "type_conflict_message", "arguments": {"value": 1}, "schema": {"type": "object", "properties": {"value": {"type": ["string", "boolean"]}}}},
    {"name": "string_too_long", "arguments": {"text": "a" * 12}, "schema": {"type": "object", "properties": {"text": {"type": "string", "maxLength": 5}}}},
    {"name": "string_too_short", "arguments": {"text": "a"}, "schema": {"type": "object", "properties": {"text": {"type": "string", "minLength": 3}}}},
    {"name": "string_default_length", "arguments": {"text": "a" * 20_010}, "schema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "array_too_many", "arguments": {"items": [1, 2, 3]}, "schema": {"type": "object", "properties": {"items": {"type": "array", "maxItems": 2}}}},
    {"name": "array_too_few", "arguments": {"items": []}, "schema": {"type": "object", "properties": {"items": {"type": "array", "minItems": 1}}}},
    {"name": "array_default_limit", "arguments": {"items": list(range(201))}, "schema": {"type": "object", "properties": {"items": {"type": "array"}}}},
    {"name": "above_maximum", "arguments": {"count": 11}, "schema": {"type": "object", "properties": {"count": {"type": "integer", "maximum": 10}}}},
    {"name": "below_minimum", "arguments": {"count": 1.5}, "schema": {"type": "object", "properties": {"count": {"type": "number", "minimum": 2}}}},
    {"name": "non_object_schema_skips", "arguments": {"a": 1}, "schema": {"type": "array"}},
    {"name": "unknown_property_is_allowed", "arguments": {"b": 1}, "schema": {"type": "object", "properties": {"a": {"type": "string"}}}},
    {"name": "string_length_uses_characters", "arguments": {"text": "汉字八个字符"}, "schema": {"type": "object", "properties": {"text": {"type": "string", "maxLength": 6}}}},
    {"name": "bad_max_length_falls_back", "arguments": {"text": "a" * 20_010}, "schema": {"type": "object", "properties": {"text": {"type": "string", "maxLength": "5"}}}},
    {"name": "payload_over_limit", "arguments": {"text": "a" * 100_050}, "schema": None},
    {"name": "boolean_maximum", "arguments": {"count": 2}, "schema": {"type": "object", "properties": {"count": {"type": "integer", "maximum": True}}}},
]

REDACTION_CASES: list[dict[str, Any]] = [
    {"name": "flat_keys", "value": {"api_key": "abc", "normal": "keep", "X-Api-Key": "abc", "x-auth": "abc"}},
    {"name": "nested", "value": {"outer": {"cookie": "c", "list": [{"token": "t"}, "text"]}}},
    {"name": "list_truncated", "value": {"items": list(range(120))}},
    {"name": "assignment_text", "value": "api_key=sk-0123456789abcdefghijklmn password: hunter2"},
    {"name": "bearer_text", "value": "Authorization: Bearer abcdefgh token=abcdefgh"},
    {"name": "provider_secret", "value": "sk-0123456789abcdefghijklmn and ak-abcdefghijklmnopqrstuvwx"},
    {"name": "short_secret_kept", "value": "token=\"\" secret= short"},
]


def security_cases() -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    arguments: list[dict[str, Any]] = []
    for case in ARGUMENT_CASES:
        record: dict[str, Any] = {
            "name": case["name"],
            "arguments": case["arguments"],
            "schema": case["schema"],
        }
        try:
            security_module.validate_tool_arguments(case["arguments"], case["schema"])
        except ValueError as exc:
            record["error"] = str(exc)
        else:
            record["ok"] = True
        arguments.append(record)

    redaction: list[dict[str, Any]] = []
    for case in REDACTION_CASES:
        value = case["value"]
        redaction.append(
            {
                "name": case["name"],
                "value": value,
                "redacted": security_module.redact_sensitive_values(value),
            }
        )
    return arguments, redaction


# ------------------------------------------------------------------------- audit


def audit_cases(workspace: Path) -> list[dict[str, Any]]:
    original_datetime = audit_module.datetime

    class FixedDateTime:
        @staticmethod
        def now(tz=None):  # noqa: ANN001 - 与 datetime.now 同形
            return FIXED_NOW

    audit_module.datetime = FixedDateTime  # type: ignore[assignment]
    cases: list[dict[str, Any]] = []
    try:
        logger = audit_module.MCPAuditLogger(workspace, enabled=True)
        records = [
            {
                "name": "approved",
                "arguments": {"path": "/tmp/a", "api_key": "sk-0123456789abcdefghijklmn"},
                "output": "  done  ",
                "ok": True,
                "error_code": None,
                "approval_result": "approved",
            },
            {
                "name": "long_output",
                "arguments": {},
                "output": "token=sk-0123456789abcdefghijklmn " + "x" * 3000,
                "ok": False,
                "error_code": "TOOL_FAILED",
                "approval_result": "denied",
            },
        ]
        for record in records:
            logger.record_tool_call(
                session_id="session-0123456789ab",
                audit_id="mcp-0123456789ab",
                server_name="files",
                tool_name="read",
                arguments=record["arguments"],
                approval_mode="manual",
                approval_result=record["approval_result"],
                duration_ms=12,
                ok=record["ok"],
                error_code=record["error_code"],
                output=record["output"],
            )
        lines = logger.path.read_text(encoding="utf-8").splitlines()
        for record, line in zip(records, lines):
            cases.append({"name": record["name"], "line": line})

        disabled = audit_module.MCPAuditLogger(workspace / "disabled", enabled=False)
        disabled.record_tool_call(
            session_id="s",
            audit_id="mcp-0123456789ab",
            server_name="",
            tool_name="read",
            arguments={},
            approval_mode="",
            approval_result="not_found",
            duration_ms=0,
            ok=False,
            error_code="TOOL_NOT_FOUND",
            output="",
        )
        cases.append(
            {
                "name": "disabled",
                "line": None,
                "wrote_file": disabled.path.exists() and disabled.path.stat().st_size > 0,
            }
        )
        escaped = audit_module.MCPAuditLogger(workspace, enabled=True, relative_path="../escape.jsonl")
        cases.append({"name": "escape_path", "tail": "/".join(list(escaped.path.parts)[-3:])})
    finally:
        audit_module.datetime = original_datetime  # type: ignore[assignment]
    return cases


# ------------------------------------------------------------------------- protocol


def protocol_cases() -> dict[str, Any]:
    header_cases = [
        {"name": "ok", "header": "Content-Length: 12\r\n", "kind": "response"},
        {"name": "ok_lowercase_key", "header": "content-length: 12\r\n", "kind": "response"},
        {"name": "ok_with_extra", "header": "Content-Type: application/json\r\nContent-Length: 3\r\n", "kind": "response"},
        {"name": "missing_client", "header": "Content-Type: application/json\r\n", "kind": "response"},
        {"name": "missing_server", "header": "Content-Type: application/json\r\n", "kind": "request"},
        {"name": "not_integer", "header": "Content-Length: abc\r\n", "kind": "response"},
        {"name": "not_integer_server", "header": "Content-Length: abc\r\n", "kind": "request"},
        {"name": "negative", "header": "Content-Length: -1\r\n", "kind": "response"},
        {"name": "too_large", "header": "Content-Length: 50000001\r\n", "kind": "response"},
        {"name": "huge_number", "header": "Content-Length: 99999999999999999999\r\n", "kind": "response"},
        {"name": "zero", "header": "Content-Length: 0\r\n", "kind": "response"},
    ]
    headers: list[dict[str, Any]] = []
    for case in header_cases:
        parser = (
            client_module._parse_content_length
            if case["kind"] == "response"
            else server_module._parse_content_length
        )
        record: dict[str, Any] = {"name": case["name"], "kind": case["kind"], "header": case["header"]}
        try:
            record["length"] = parser(case["header"].encode("ascii"))
        except Exception as exc:  # noqa: BLE001 - 两侧的异常类型都要如实记录
            record["error"] = str(exc)
            record["error_kind"] = type(exc).__name__
        headers.append(record)

    sse_texts = [
        "event: message\ndata: {\"a\": 1}\n\n",
        "data: {\"a\": 1}\ndata: {\"b\": 2}\n\n",
        "data: {\"a\":\ndata: 1}\n\n",
        "data: not json\n\n",
        "data: [1, 2]\n\n",
        "data:\n\n",
        ": comment\n\n",
        "data: {\"a\": 1}",
        "data: {\"a\": 1}\r\n\r\n",
    ]

    stringify_cases = [
        {"kind": "tool", "payload": {"content": [{"type": "text", "text": "hi"}, {"type": "image", "data": "x"}]}},
        {"kind": "tool", "payload": {"content": [{"type": "text", "text": 7}]}},
        {"kind": "tool", "payload": {"content": []}},
        {"kind": "tool", "payload": {"isError": True}},
        {"kind": "tool", "payload": {"content": [{"type": "text", "text": "第一行\n第二行"}]}},
        {"kind": "resource", "payload": {"contents": [{"uri": "file:///a", "text": "正文"}]}},
        {"kind": "resource", "payload": {"contents": [{"blob": "YWJj"}]}},
        {"kind": "resource", "payload": {"contents": [{"uri": "file:///a", "blob": "YWJj"}, {"uri": "file:///b", "text": "b"}]}},
        {"kind": "resource", "payload": {}},
        {"kind": "prompt", "payload": {"messages": [{"role": "user", "content": {"type": "text", "text": "你好"}}]}},
        {"kind": "prompt", "payload": {"messages": [{"role": "user", "content": {"type": "image", "data": "x"}}]}},
        {"kind": "prompt", "payload": {"messages": [{"role": 7, "content": "纯文本"}, {"content": {"text": "无角色"}}]}},
        {"kind": "prompt", "payload": {"messages": []}},
    ]


    capability_cases = [
        {"name": "declared", "init": {"capabilities": {"tools": {}, "resources": {}}}, "capability": "tools"},
        {"name": "not_declared", "init": {"capabilities": {"tools": {}}}, "capability": "resources"},
        {"name": "missing_capabilities", "init": {}, "capability": "prompts"},
        {"name": "capabilities_not_object", "init": {"capabilities": []}, "capability": "prompts"},
    ]

    pagination_scripts = [
        {"name": "two_pages", "pages": [{"items": [{"name": "a"}], "next": "c1"}, {"items": [{"name": "b"}]}]},
        {"name": "repeated_cursor", "pages": [{"items": [{"name": "a"}], "next": "c1"}, {"items": [{"name": "b"}], "next": "c1"}]},
        {"name": "cursor_empty", "pages": [{"items": [{"name": "a"}], "next": ""}]},
        {"name": "weird_items", "pages": [{"items": [{"name": "a"}, 7, "x"]}]},
        {"name": "request_fails", "pages": [{"items": [{"name": "a"}], "next": "c1"}, {"error": "boom"}]},
    ]
    pagination: list[dict[str, Any]] = []
    for script in pagination_scripts:
        calls: list[Any] = []

        def request(method, params, _script=script):  # noqa: ANN001
            calls.append({"method": method, "params": params})
            page = _script["pages"][len(calls) - 1] if len(calls) - 1 < len(_script["pages"]) else {}
            if "error" in page:
                raise RuntimeError(page["error"])
            payload: dict[str, Any] = {"tools": page.get("items", [])}
            if page.get("next"):
                payload["nextCursor"] = page["next"]
            return payload

        items = client_module._list_capability_pages(request, "tools/list", "tools")
        pagination.append({"name": script["name"], "items": items, "calls": calls, "pages": script["pages"]})

    return {
        "headers": headers,
        "sse": [{"text": text, "payloads": client_module._parse_sse_json_payloads(text)} for text in sse_texts],
        "stringify": [
            {
                "kind": case["kind"],
                "payload": case["payload"],
                "text": {
                    "tool": client_module._stringify_tool_result_payload,
                    "resource": client_module._stringify_resource_payload,
                    "prompt": client_module._stringify_prompt_payload,
                }[case["kind"]](case["payload"]),
            }
            for case in stringify_cases
        ],
        "unwrap": unwrap_cases(),
        "capability": [
            {
                "name": case["name"],
                "init": case["init"],
                "capability": case["capability"],
                "declared": client_module._capability_declared(case["init"], case["capability"]),
            }
            for case in capability_cases
        ],
        "pagination": pagination,
    }


def unwrap_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for case in [
        {"name": "result_object", "payload": {"jsonrpc": "2.0", "id": 1, "result": {"tools": []}}, "method": "tools/list"},
        {"name": "result_missing", "payload": {"jsonrpc": "2.0", "id": 1}, "method": "tools/list"},
        {"name": "result_not_object", "payload": {"jsonrpc": "2.0", "id": 1, "result": [1]}, "method": "tools/list"},
        {"name": "error_dict", "payload": {"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "未知方法"}}, "method": "tools/call"},
        {"name": "error_without_message", "payload": {"jsonrpc": "2.0", "id": 1, "error": {"code": -32601}}, "method": "tools/call"},
        {"name": "error_empty_message", "payload": {"jsonrpc": "2.0", "id": 1, "error": {"code": 1, "message": ""}}, "method": "tools/call"},
        {"name": "error_string", "payload": {"jsonrpc": "2.0", "id": 1, "error": "坏了"}, "method": "tools/call"},
        {"name": "error_number_message", "payload": {"jsonrpc": "2.0", "id": 1, "error": {"message": 5}}, "method": "tools/call"},
    ]:
        record: dict[str, Any] = {"name": case["name"], "method": case["method"], "payload": case["payload"]}
        try:
            record["result"] = client_module._unwrap_json_rpc_response(case["payload"], case["method"])
        except client_module.MCPClientError as exc:
            record["error"] = str(exc)
        cases.append(record)
    return cases


# ------------------------------------------------------------------------- server


SERVER_REQUESTS: list[dict[str, Any]] = [
    {"name": "initialize", "request": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}},
    {"name": "initialized_notification", "request": {"jsonrpc": "2.0", "method": "notifications/initialized"}},
    {"name": "tools_list", "request": {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}},
    {"name": "tools_call_no_name", "request": {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": ""}}},
    {"name": "tools_call_bad_arguments", "request": {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "read", "arguments": 3}}},
    {"name": "tools_call_unknown", "request": {"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "read"}}},
    {"name": "resources_list", "request": {"jsonrpc": "2.0", "id": 6, "method": "resources/list"}},
    {"name": "resources_read_health", "request": {"jsonrpc": "2.0", "id": 7, "method": "resources/read", "params": {"uri": "server://local_project/health"}}},
    {"name": "resources_read_agents", "request": {"jsonrpc": "2.0", "id": 8, "method": "resources/read", "params": {"uri": "project://agents-instructions"}}},
    {"name": "resources_read_readme", "request": {"jsonrpc": "2.0", "id": 9, "method": "resources/read", "params": {"uri": "project://README.md"}}},
    {"name": "resources_read_protected", "request": {"jsonrpc": "2.0", "id": 10, "method": "resources/read", "params": {"uri": "project://config.toml"}}},
    {"name": "resources_read_missing", "request": {"jsonrpc": "2.0", "id": 11, "method": "resources/read", "params": {"uri": "project://missing.md"}}},
    {"name": "resources_read_bundled", "request": {"jsonrpc": "2.0", "id": 12, "method": "resources/read", "params": {"uri": "omnicrawl://docs/API.md"}}},
    {"name": "resources_read_unknown", "request": {"jsonrpc": "2.0", "id": 13, "method": "resources/read", "params": {"uri": "file:///etc/passwd"}}},
    {"name": "resources_read_no_uri", "request": {"jsonrpc": "2.0", "id": 14, "method": "resources/read", "params": {"uri": "  "}}},
    {"name": "prompts_list", "request": {"jsonrpc": "2.0", "id": 15, "method": "prompts/list"}},
    {"name": "prompts_get", "request": {"jsonrpc": "2.0", "id": 16, "method": "prompts/get", "params": {"name": "debug_triage", "arguments": {"error": "崩了"}}}},
    {"name": "prompts_get_doc_writer", "request": {"jsonrpc": "2.0", "id": 17, "method": "prompts/get", "params": {"name": "project_doc_writer", "arguments": {"target": "接口"}}}},
    {"name": "prompts_get_unknown", "request": {"jsonrpc": "2.0", "id": 18, "method": "prompts/get", "params": {"name": "nope"}}},
    {"name": "prompts_get_bad_arguments", "request": {"jsonrpc": "2.0", "id": 19, "method": "prompts/get", "params": {"name": "code_review", "arguments": "oops"}}},
    {"name": "unknown_method", "request": {"jsonrpc": "2.0", "id": 20, "method": "does/not/exist"}},
    {"name": "missing_method", "request": {"jsonrpc": "2.0", "id": 21}},
    {"name": "params_not_object", "request": {"jsonrpc": "2.0", "id": 22, "method": "resources/read", "params": 5}},
                        {"name": "no_id_error", "request": {"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "read"}}},
]


def server_cases(workspace: Path) -> list[dict[str, Any]]:
    server = server_module.LocalMCPServer(workspace)
    cases: list[dict[str, Any]] = []
    for case in SERVER_REQUESTS:
        response = server.handle_message(case["request"])
        record: dict[str, Any] = {"name": case["name"], "request": case["request"]}
        if response is None:
            record["response"] = None
        else:
            text = json.dumps(response, ensure_ascii=False)
            record["response"] = json.loads(normalize_paths(text, workspace))
        cases.append(record)
    return cases


# ------------------------------------------------------------------------- manager


class ScriptedConnection:
    """脚本化连接：把「发现结果 / 调用结果」写成表，用来驱动真管理器。"""

    def __init__(self, script: dict[str, Any], server) -> None:  # noqa: ANN001
        self.script = script
        self.server = server
        self.closed = False

    def discover(self):
        if "discover_error" in self.script:
            raise RuntimeError(self.script["discover_error"])
        return client_module._DiscoveredCapabilities(
            tools=self.script.get("tools", []),
            resources=self.script.get("resources", []),
            prompts=self.script.get("prompts", []),
        )

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        return self._outcome("tool_result")

    def read_resource(self, uri: str) -> dict[str, Any]:
        return self._outcome("resource_result")

    def get_prompt(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        return self._outcome("prompt_result")

    def _outcome(self, key: str) -> dict[str, Any]:
        outcome = self.script.get(key)
        if outcome is None:
            return {}
        if "raise" in outcome:
            raise TimeoutError(outcome["raise"])
        if "raise_client_error" in outcome:
            raise client_module.MCPClientError(outcome["raise_client_error"])
        return outcome.get("payload", {})

    def close(self) -> None:
        self.closed = True


def manager_cases(workspace: Path) -> list[dict[str, Any]]:
    original_datetime = audit_module.datetime

    class FixedDateTime:
        @staticmethod
        def now(tz=None):  # noqa: ANN001
            return FIXED_NOW

    cases: list[dict[str, Any]] = []
    scenarios = manager_scenarios()
    audit_module.datetime = FixedDateTime  # type: ignore[assignment]
    original_stdio = client_module._StdioMCPConnection
    original_http = client_module._StreamableHTTPMCPConnection
    try:
        for scenario in scenarios:
            case_dir = workspace / scenario["name"]
            case_dir.mkdir(parents=True, exist_ok=True)
            config = config_module.MCPConfig(
                enabled=scenario.get("enabled", True),
                default_timeout_seconds=30,
                servers={
                    server["name"]: config_module.MCPServerConfig(
                        name=server["name"],
                        enabled=server.get("enabled", True),
                        transport=server.get("transport", "stdio"),
                        command=server.get("command", "npx"),
                        risk_level=server.get("risk_level", "restricted"),
                    )
                    for server in scenario.get("servers", [])
                },
                policy=config_module.MCPPolicyConfig(
                    allow_external_network_tools=scenario.get("allow_external", False)
                ),
            )

            script = scenario.get("script", {})
            made: list[ScriptedConnection] = []

            def factory(servers, *args, _script=script, _made=made):  # noqa: ANN001
                # Python 侧 stdio 会传第二个位置参数（工作区根），HTTP 只传 Server。
                connection = ScriptedConnection(_script.get(servers.name, {}), servers)
                _made.append(connection)
                return connection

            client_module._StdioMCPConnection = factory  # type: ignore[assignment]
            client_module._StreamableHTTPMCPConnection = factory  # type: ignore[assignment]
            manager = client_module.MCPClientManager(
                config,
                workspace_root=case_dir,
                approval_mode_getter=lambda: "manual",
            )
            record: dict[str, Any] = {
                "name": scenario["name"],
                "config": dump(config),
                "script": dump(script),
                "operations": [],
            }
            manager.discover()
            for operation in scenario.get("operations", []):
                record["operations"].append(run_manager_operation(manager, operation))
            record["tools"] = [
                {
                    "logical_name": meta.logical_name,
                    "server_name": meta.server_name,
                    "tool_name": meta.tool_name,
                    "argument_schema": meta.argument_schema,
                    "requires_confirmation": meta.requires_confirmation,
                }
                for meta in manager.registry.tools.values()
            ]
            record["resources"] = list(manager.registry.resources.keys())
            record["prompts"] = list(manager.registry.prompts.keys())
            record["diagnostics"] = [dump(item) for item in manager.registry.diagnostics]
            record["status"] = normalize_paths(manager.format_status(), case_dir)
            audit_path = case_dir / audit_module.DEFAULT_MCP_AUDIT_LOG_PATH
            record["audit"] = (
                normalize_paths(normalize_ids(audit_path.read_text(encoding="utf-8")), case_dir).splitlines()
                if audit_path.exists()
                else []
            )
            if scenario.get("close"):
                manager.close()
                record["closed_connections"] = sum(1 for item in made if item.closed)
            cases.append(record)
    finally:
        audit_module.datetime = original_datetime  # type: ignore[assignment]
        client_module._StdioMCPConnection = original_stdio  # type: ignore[assignment]
        client_module._StreamableHTTPMCPConnection = original_http  # type: ignore[assignment]
    return cases


def run_manager_operation(manager, operation: dict[str, Any]) -> dict[str, Any]:  # noqa: ANN001
    kind = operation["op"]
    if kind == "call_tool":
        result = manager.call_tool(operation["name"], operation.get("arguments", {}))
    elif kind == "read_resource":
        result = manager.read_resource(operation["name"])
    elif kind == "get_prompt":
        result = manager.get_prompt(operation["name"], operation.get("arguments"))
    elif kind == "denied":
        manager.record_denied_tool_call(
            operation["name"], operation.get("arguments", {}), operation.get("reason", "用户取消。")
        )
        return {"op": kind, "input": operation, "name": operation["name"]}
    else:
        raise SystemExit(f"未知操作：{kind}")
    payload = dump(result)
    payload.pop("duration_ms", None)
    if "audit_id" in payload:
        # 只有 Tool 调用带审计 ID；Resource/Prompt 读取不进审计。
        payload["audit_id"] = "{id}"
    return {"op": kind, "input": operation, **payload}


def manager_scenarios() -> list[dict[str, Any]]:
    tool_script = {
        "tools": [
            {
                "name": "read",
                "description": "读一个文件",
                "inputSchema": {"type": "object", "required": ["path"]},
            },
            {"name": "  ", "description": "空名字"},
            {"description": "缺名字"},
            {
                "name": "silent",
                "input_schema": {"type": "object"},
            },
        ],
        "resources": [
            {"uri": "file:///a", "name": "a", "mimeType": "text/plain"},
            {"uri": "  "},
            {"name": "缺 uri"},
        ],
        "prompts": [
            {"name": "review", "description": "审查", "arguments": [{"name": "path"}]},
            {"description": "缺名字"},
        ],
    }
    return [
        {
            "name": "stdio_ok",
            "servers": [{"name": "files"}],
            "script": {
                "files": {
                    **tool_script,
                    "tool_result": {"payload": {"content": [{"type": "text", "text": "内容"}]}},
                    "resource_result": {"payload": {"contents": [{"uri": "file:///a", "text": "正文"}]}},
                    "prompt_result": {
                        "payload": {"messages": [{"role": "user", "content": {"type": "text", "text": "模板"}}]}
                    },
                }
            },
            "operations": [
                {"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}},
                {"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a", "api_key": "sk-0123456789abcdefghijklmn"}},
                {"op": "call_tool", "name": "files.read", "arguments": {}},
                {"op": "call_tool", "name": "files.silent"},
                {"op": "call_tool", "name": "files.missing"},
                {"op": "read_resource", "name": "files:file:///a"},
                {"op": "read_resource", "name": "files:missing"},
                {"op": "get_prompt", "name": "files.review", "arguments": {"path": "a"}},
                {"op": "get_prompt", "name": "files.missing"},
                {"op": "denied", "name": "files.read", "arguments": {"path": "/tmp/a"}},
            ],
        },
        {
            "name": "tool_error_flag",
            "servers": [{"name": "files"}],
            "script": {"files": {**tool_script, "tool_result": {"payload": {"isError": True, "content": [{"type": "text", "text": "失败"}]}}}},
            "operations": [{"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}}],
        },
        {
            "name": "timeout_and_degrade",
            "servers": [{"name": "files"}],
            "script": {"files": {**tool_script, "tool_result": {"raise": "等超时了"}}},
            "operations": [
                {"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}},
                {"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}},
                {"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}},
                {"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}},
            ],
        },
        {
            "name": "client_error",
            "servers": [{"name": "files"}],
            "script": {"files": {**tool_script, "tool_result": {"raise_client_error": "连接断了"}}},
            "operations": [{"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}}],
        },
        {
            "name": "discover_failure",
            "servers": [{"name": "files"}],
            "script": {"files": {"discover_error": "连不上"}},
            "operations": [{"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}}],
        },
        {
            "name": "external_blocked",
            "servers": [{"name": "remote", "risk_level": "external"}],
            "script": {"remote": tool_script},
            "operations": [],
        },
        {
            "name": "external_allowed",
            "servers": [{"name": "remote", "risk_level": "external"}],
            "allow_external": True,
            "script": {"remote": tool_script},
            "operations": [],
        },
        {
            "name": "unsupported_transport",
            "servers": [{"name": "legacy", "transport": "sse"}],
            "script": {"legacy": tool_script},
            "operations": [],
        },
        {
            "name": "disabled_server",
            "servers": [{"name": "files", "enabled": False}],
            "script": {"files": tool_script},
            "operations": [],
        },
        {
            "name": "no_enabled_servers",
            "servers": [],
            "operations": [],
        },
        {
            "name": "mcp_disabled",
            "enabled": False,
            "servers": [{"name": "files"}],
            "script": {"files": tool_script},
            "operations": [],
        },
        {
            "name": "two_servers_order",
            "servers": [{"name": "alpha"}, {"name": "beta"}],
            "script": {
                "alpha": {**tool_script, "tools": [{"name": "read", "description": "A"}]},
                "beta": {**tool_script, "tools": [{"name": "read", "description": "B"}]},
            },
            "operations": [],
        },
        {
            "name": "closed_then_call",
            "servers": [{"name": "files"}],
            "script": {"files": {**tool_script, "tool_result": {"payload": {}}}},
            "operations": [{"op": "call_tool", "name": "files.read", "arguments": {"path": "/tmp/a"}}],
            "close": True,
        },
    ]


# ------------------------------------------------------------------------- http


class FakeHttpResponse:
    def __init__(self, status: int, headers: dict[str, str], body: str, json_error: bool = False) -> None:
        self.status_code = status
        self.headers = headers
        self._body = body
        self._json_error = json_error

    @property
    def is_error(self) -> bool:
        return self.status_code >= 400

    @property
    def content(self) -> bytes:
        return self._body.encode("utf-8")

    @property
    def text(self) -> str:
        return self._body

    def json(self):
        if self._json_error:
            raise ValueError("bad json")
        return json.loads(self._body)


class FakeHttpClient:
    """假 httpx.Client：按脚本回放响应，并记录每次请求的形状。"""

    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = responses
        self.requests: list[dict[str, Any]] = []
        self.closed = False

    def post(self, url, headers=None, json=None):  # noqa: ANN001, A002
        self.requests.append(
            {"url": url, "headers": dict(headers or {}), "body": json}
        )
        response = self.responses[len(self.requests) - 1]
        if "raise_timeout" in response:
            import httpx

            raise httpx.TimeoutException("timeout")
        if "raise_http_error" in response:
            import httpx

            raise httpx.ConnectError("boom")
        return FakeHttpResponse(
            response.get("status", 200),
            response.get("headers", {}),
            response.get("body", ""),
            response.get("json_error", False),
        )

    def close(self) -> None:
        self.closed = True


def http_cases() -> list[dict[str, Any]]:
    import httpx

    scenarios = http_scenarios()
    original_client = httpx.Client
    cases: list[dict[str, Any]] = []
    try:
        for scenario in scenarios:
            fake = FakeHttpClient(scenario["responses"])
            httpx.Client = lambda **kwargs: fake  # type: ignore[assignment]
            server = config_module.MCPServerConfig(
                name="remote",
                transport="streamable_http",
                url=scenario.get("url", "https://example.com/mcp"),
                headers=scenario.get("headers", {}),
                timeout_seconds=scenario.get("timeout_seconds", 30),
            )
            record: dict[str, Any] = {
                "name": scenario["name"],
                "steps": [],
                "responses": scenario["responses"],
                "timeout_seconds": scenario.get("timeout_seconds", 30),
                "headers": scenario.get("headers", {}),
            }
            try:
                connection = client_module._StreamableHTTPMCPConnection(server)
            except Exception as exc:  # noqa: BLE001
                record["error"] = f"{type(exc).__name__}: {exc}"
                cases.append(record)
                continue
            for step in scenario["steps"]:
                entry: dict[str, Any] = {"method": step["method"]}
                try:
                    result = connection._request(
                        step["method"], step.get("params", {}), notification=step.get("notification", False)
                    )
                except TimeoutError as exc:
                    entry["error"] = str(exc)
                    entry["error_kind"] = "timeout"
                except Exception as exc:  # noqa: BLE001
                    entry["error"] = str(exc)
                    entry["error_kind"] = type(exc).__name__
                else:
                    entry["result"] = result
                record["steps"].append(entry)
                record.setdefault("requests", []).append(
                    {
                        "url": fake.requests[-1]["url"],
                        "body": fake.requests[-1]["body"],
                        "headers": {
                            key.lower(): value
                            for key, value in fake.requests[-1]["headers"].items()
                        },
                    }
                )
                if len(fake.requests) > len(record["requests"]):
                    # 某一步发了多次请求（理论上不会发生）：把多出来的也记上。
                    record["requests"].extend(
                        {
                            "url": extra["url"],
                            "body": extra["body"],
                            "headers": {key.lower(): value for key, value in extra["headers"].items()},
                        }
                        for extra in fake.requests[len(record["requests"]) :]
                    )
            cases.append(record)
    finally:
        httpx.Client = original_client  # type: ignore[assignment]
    return cases


def http_scenarios() -> list[dict[str, Any]]:
    def json_response(payload: dict[str, Any], status: int = 200, headers: dict[str, str] | None = None) -> dict[str, Any]:
        return {
            "status": status,
            "headers": {"content-type": "application/json", **(headers or {})},
            "body": json.dumps(payload, ensure_ascii=False),
        }

    initialized = json_response(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}},
        },
        headers={"Mcp-Session-Id": "session-abc"},
    )
    return [
        {
            "name": "json_flow",
            "steps": [
                {"method": "initialize", "params": {"protocolVersion": "2025-03-26"}},
                {"method": "notifications/initialized", "notification": True},
                {"method": "tools/list", "params": {}},
            ],
            "responses": [
                initialized,
                {"status": 202, "headers": {}, "body": ""},
                json_response({"jsonrpc": "2.0", "id": 2, "result": {"tools": [{"name": "read"}]}}),
            ],
        },
        {
            "name": "sse_flow",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [
                {
                    "status": 200,
                    "headers": {"content-type": "text/event-stream"},
                    "body": 'event: message\ndata: {"jsonrpc": "2.0", "id": 1, "result": {"tools": [{"name": "sse"}]}}\n\n',
                }
            ],
        },
        {
            "name": "sse_without_matching_id",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [
                {
                    "status": 200,
                    "headers": {"content-type": "text/event-stream"},
                    "body": 'data: {"jsonrpc": "2.0", "id": 99, "result": {"tools": [{"name": "first"}]}}\n\n',
                }
            ],
        },
        {
            "name": "sse_empty",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [{"status": 200, "headers": {"content-type": "text/event-stream"}, "body": ""}],
        },
        {
            "name": "empty_body",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [{"status": 200, "headers": {}, "body": ""}],
        },
        {
            "name": "http_error",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [{"status": 500, "headers": {}, "body": "boom"}],
        },
        {
            "name": "bad_json",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [{"status": 200, "headers": {"content-type": "application/json"}, "body": "{oops"}],
        },
        {
            "name": "non_object_json",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [{"status": 200, "headers": {"content-type": "application/json"}, "body": "[1, 2]"}],
        },
        {
            "name": "transport_timeout",
            "steps": [{"method": "tools/list", "params": {}}],
            "timeout_seconds": 1,
            "responses": [{"raise_timeout": True}],
        },
        {
            "name": "transport_error",
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [{"raise_http_error": True}],
        },
        {
            "name": "json_rpc_error",
            "steps": [{"method": "tools/call", "params": {"name": "read"}}],
            "responses": [json_response({"jsonrpc": "2.0", "id": 1, "error": {"code": -32000, "message": "坏了"}})],
        },
        {
            "name": "custom_headers_and_managed_override",
            "headers": {"Authorization": "Bearer t", "accept": "text/plain", "X-Trace": "1"},
            "steps": [{"method": "tools/list", "params": {}}],
            "responses": [json_response({"jsonrpc": "2.0", "id": 1, "result": {}})],
        },
        {
            "name": "session_and_protocol_headers",
            "steps": [
                {"method": "initialize", "params": {}},
                {"method": "notifications/initialized", "notification": True},
                {"method": "tools/list", "params": {}},
            ],
            "responses": [
                initialized,
                {"status": 202, "headers": {}, "body": ""},
                json_response({"jsonrpc": "2.0", "id": 2, "result": {}}),
            ],
        },
        {
            "name": "notification_200_with_body",
            "steps": [{"method": "notifications/initialized", "notification": True}],
            "responses": [json_response({"jsonrpc": "2.0", "result": {"ignored": True}})],
        },
    ]


# ------------------------------------------------------------------------- bundled docs


def bundled_doc_cases() -> dict[str, Any]:
    names = documentation.bundled_doc_names()
    hashes = {
        name: hashlib.sha256(
            (documentation.bundled_docs_dir() / name).read_bytes()
        ).hexdigest()
        for name in names
    }
    return {"names": names, "hashes": hashes}


def main() -> None:
    for module in (audit_module, client_module, config_module, registry_module, security_module, server_module):
        assert_repo_source(module)

    workspace = Path(tempfile.mkdtemp(prefix="omnicrawl-mcp-fixture-"))
    docs_dir = workspace / "docs"
    docs_dir.mkdir(parents=True, exist_ok=True)
    (workspace / "README.md").write_text("# 项目\n", encoding="utf-8")
    (workspace / "AGENTS.md").write_text("协作规范\n", encoding="utf-8")
    (docs_dir / "API.md").write_text("接口\n", encoding="utf-8")
    (workspace / "config.toml").write_text("secret=1\n", encoding="utf-8")

    protocol = protocol_cases()
    protocol["unwrap"] = unwrap_cases()

    fixture = {
        "generated_from": "omnicrawl/mcp/",
        "config": config_cases(workspace),
        "registry": registry_cases(),
        "security": {"arguments": security_cases()[0], "redaction": security_cases()[1]},
        "audit": audit_cases(workspace),
        "protocol": protocol,
        "server": server_cases(workspace),
        "manager": manager_cases(workspace),
        "http": http_cases(),
        "bundled_docs": bundled_doc_cases(),
    }

    text = normalize_ids(json.dumps(fixture, ensure_ascii=False, indent=2)) + "\n"
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(text, encoding="utf-8")
    print(f"已写入 {FIXTURE_PATH}（{len(text)} 字符）")


if __name__ == "__main__":
    main()
