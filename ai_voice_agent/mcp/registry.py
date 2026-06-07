from __future__ import annotations

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
