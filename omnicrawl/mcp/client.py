"""MCP 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

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
from typing import Any, Callable, Protocol

import httpx

from .audit import MCPAuditLogger
from .config import (
    MCPConfig,
    MCPServerConfig,
    MCP_RISK_EXTERNAL,
    MCP_TRANSPORT_STDIO,
    MCP_TRANSPORT_STREAMABLE_HTTP,
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
MCP_STREAMABLE_HTTP_PROTOCOL_VERSION = "2025-03-26"


class MCPClientError(RuntimeError):
    """MCP 连接、能力发现或工具调用失败。"""


@dataclass(frozen=True)
class MCPToolCallResult:
    """MCP Tool 调用返回给 Agent 的结构化结果。"""

    ok: bool
    server_name: str
    tool_name: str
    output: str
    full_output: str = ""
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
    full_output: str = ""
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
    full_output: str = ""
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
        self._connections: dict[str, _MCPConnection] = {}
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
            if server.transport == MCP_TRANSPORT_STDIO:
                connection: _MCPConnection = _StdioMCPConnection(
                    server,
                    self._audit_logger.workspace_root,
                )
            elif server.transport == MCP_TRANSPORT_STREAMABLE_HTTP:
                connection = _StreamableHTTPMCPConnection(server)
            else:
                self._server_status[server.name] = "unsupported"
                self.registry.add_diagnostic(
                    "warning",
                    "TRANSPORT_UNSUPPORTED",
                    f"暂不支持 {server.transport} 传输，已跳过 Server：{server.name}",
                    server_name=server.name,
                )
                continue

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
        """关闭所有由 Host 启动的 MCP 连接。"""

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
            output=output,
            full_output=output,
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
            output=output,
            full_output=output,
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
            output=output,
            full_output=output,
            error_code=error_code,
            retryable=retryable,
            duration_ms=int((time.perf_counter() - started) * 1000),
        )

    def format_status(self) -> str:
        """返回适合终端展示的 MCP 状态摘要。"""

        if not self.config.enabled:
            return "MCP 已关闭。可在 config.toml 的 mcp.enabled=true 开启。"

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
            output=result.full_output or result.output,
        )


@dataclass(frozen=True)
class _DiscoveredCapabilities:
    tools: list[dict[str, Any]]
    resources: list[dict[str, Any]]
    prompts: list[dict[str, Any]]


class _MCPConnection(Protocol):
    def discover(self) -> _DiscoveredCapabilities: ...

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]: ...

    def read_resource(self, uri: str) -> dict[str, Any]: ...

    def get_prompt(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]: ...

    def close(self) -> None: ...


class _StreamableHTTPMCPConnection:
    """MCP Streamable HTTP 客户端。

    每个 JSON-RPC 请求通过 HTTP POST 发送到同一端点。服务端可以返回
    ``application/json`` 或 ``text/event-stream``；初始化响应中的
    ``Mcp-Session-Id`` 会附加到后续请求。
    """

    def __init__(self, server: MCPServerConfig) -> None:
        self.server = server
        self._client = httpx.Client(timeout=server.timeout_seconds)
        self._session_id: str | None = None
        self._initialized = False
        self._next_request_id = 1
        self._lock = threading.Lock()

    def discover(self) -> _DiscoveredCapabilities:
        self._request(
            "initialize",
            {
                "protocolVersion": MCP_STREAMABLE_HTTP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ai-voice-agent", "version": "0.1"},
            },
        )
        self._initialized = True
        self._request("notifications/initialized", {}, notification=True)
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
        self._client.close()

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

    def _request(
        self,
        method: str,
        params: dict[str, Any],
        *,
        notification: bool = False,
    ) -> dict[str, Any]:
        with self._lock:
            request_id: int | None = None
            message: dict[str, Any] = {"jsonrpc": "2.0", "method": method, "params": params}
            if not notification:
                request_id = self._next_request_id
                self._next_request_id += 1
                message["id"] = request_id

            # 认证头等用户自定义 Header 只属于远程 HTTP 连接；协议头由
            # Client 维护，并覆盖同名（大小写不敏感）配置，避免配置破坏会话。
            headers = dict(self.server.headers)
            managed_headers = {
                "Accept": "application/json, text/event-stream",
                "Content-Type": "application/json",
            }
            if self._session_id:
                managed_headers["Mcp-Session-Id"] = self._session_id
            if self._initialized:
                managed_headers["MCP-Protocol-Version"] = MCP_STREAMABLE_HTTP_PROTOCOL_VERSION
            for name, value in managed_headers.items():
                for existing_name in list(headers):
                    if existing_name.lower() == name.lower():
                        del headers[existing_name]
                headers[name] = value

            if not self.server.url:
                raise MCPClientError("streamable_http MCP Server 缺少 url。")
            try:
                response = self._client.post(self.server.url, headers=headers, json=message)
            except httpx.TimeoutException as exc:
                raise TimeoutError(
                    f"MCP 请求超过 {self.server.timeout_seconds} 秒：{self.server.name}"
                ) from exc
            except httpx.HTTPError as exc:
                raise MCPClientError(f"MCP Server HTTP 请求失败：{exc}") from exc

            session_id = response.headers.get("Mcp-Session-Id")
            if session_id:
                self._session_id = session_id

            if notification and response.status_code == 202:
                return {}
            if response.is_error:
                raise MCPClientError(
                    f"MCP Server HTTP 请求失败：HTTP {response.status_code}"
                )
            if not response.content:
                return {}

            content_type = response.headers.get("content-type", "").lower()
            if "text/event-stream" in content_type:
                payloads = _parse_sse_json_payloads(response.text)
                if request_id is not None:
                    for payload in payloads:
                        if payload.get("id") == request_id:
                            return _unwrap_json_rpc_response(payload, method)
                if payloads:
                    return _unwrap_json_rpc_response(payloads[0], method)
                raise MCPClientError("MCP Streamable HTTP 返回空 SSE 事件流。")

            try:
                payload = response.json()
            except ValueError as exc:
                raise MCPClientError("MCP Streamable HTTP 返回的不是合法 JSON。") from exc
            if not isinstance(payload, dict):
                raise MCPClientError("MCP Streamable HTTP 响应必须是 JSON 对象。")
            return _unwrap_json_rpc_response(payload, method)


def _parse_sse_json_payloads(text: str) -> list[dict[str, Any]]:
    """解析 Streamable HTTP 的 SSE data 事件，忽略非 JSON 事件。"""

    payloads: list[dict[str, Any]] = []
    data_lines: list[str] = []
    for line in text.splitlines() + [""]:
        if line.startswith("data:"):
            data_lines.append(line[5:].lstrip())
            continue
        if line.strip() or not data_lines:
            continue
        raw_data = "\n".join(data_lines).strip()
        data_lines.clear()
        if not raw_data:
            continue
        try:
            payload = json.loads(raw_data)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            payloads.append(payload)
    return payloads


def _unwrap_json_rpc_response(payload: dict[str, Any], method: str) -> dict[str, Any]:
    if "error" in payload:
        error = payload.get("error")
        if isinstance(error, dict):
            message = error.get("message") or json.dumps(error, ensure_ascii=False)
        else:
            message = str(error)
        raise MCPClientError(f"MCP 请求 {method} 失败：{message}")
    result = payload.get("result", {})
    if not isinstance(result, dict):
        raise MCPClientError(f"MCP 请求 {method} 返回结果必须是 JSON 对象。")
    return result


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

