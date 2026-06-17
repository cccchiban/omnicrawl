from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable


@dataclass(frozen=True)
class ToolCall:
    """模型请求执行的一次工具调用。"""

    name: str
    arguments: dict[str, Any] = field(default_factory=dict)
    id: str = ""
    function_name: str = ""


@dataclass(frozen=True)
class ToolResult:
    """工具调用返回给模型的结构化结果。"""

    ok: bool
    output: str
    full_output: str = ""


@dataclass(frozen=True)
class AgentModelReply:
    """Chat Completions 一次回复的结构化结果。"""

    message: dict[str, Any]
    content: str
    tool_calls: list[ToolCall] = field(default_factory=list)
    reasoning: str = ""
    content_streamed: bool = False


@dataclass(frozen=True)
class ToolDefinition:
    """Agent 可用工具的说明与执行函数。"""

    name: str
    description: str
    argument_schema: str
    requires_confirmation: bool
    run: Callable[[dict[str, Any]], ToolResult]
