"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

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
class ToolImageAttachment:
    """仅在当前 Agent 工具循环中发送给视觉模型的图片。

    图片数据不会进入 Session 事件或长期历史；截图文件由工具另行保存到受控的
    Agent 临时目录，避免 Base64 使会话文件和恢复上下文持续膨胀。
    """

    media_type: str
    data_base64: str
    filename: str = ""
    detail: str = "auto"


@dataclass(frozen=True)
class ToolResult:
    """工具调用返回给模型的结构化结果。"""

    ok: bool
    output: str
    full_output: str = ""
    ui_artifact: dict[str, Any] = field(default_factory=dict)
    model_images: tuple[ToolImageAttachment, ...] = ()


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
    # 仅供 Host 内建、已自行执行模型 Token 预算的工具使用；模型参数不能设置。
    model_output_is_bounded: bool = False
