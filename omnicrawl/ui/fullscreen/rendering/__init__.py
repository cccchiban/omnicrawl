"""rendering 类别包：渲染管线与对话区展示组件。

本文件只做再导出，不存放业务逻辑：
- ``RenderingMixin``（事件聚合/流式渲染管线）在 ``pipeline.py``；
- 对话区组件（消息/思考块/工具卡/子代理树/计划区）在 ``widgets.py``；
- 工具卡 diff 渲染在 ``tool_diff.py``，LaTeX 文本化在 ``latex.py``，
  欢迎 Logo 在 ``welcome_logo.py``。
"""

from .pipeline import RenderingMixin
from .welcome_logo import welcome_logo_lines, welcome_logo_text
from .widgets import (
    AssistantMessage,
    ConfirmationScreen,
    ReasoningDisclosure,
    SubAgentConversation,
    SubAgentProgressTree,
    TodoPlan,
    ToolDisclosure,
)

__all__ = [
    "RenderingMixin",
    "AssistantMessage",
    "ConfirmationScreen",
    "ReasoningDisclosure",
    "SubAgentConversation",
    "SubAgentProgressTree",
    "TodoPlan",
    "ToolDisclosure",
    "welcome_logo_lines",
    "welcome_logo_text",
]
