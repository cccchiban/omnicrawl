"""Textual 全屏工作台（门面入口）。

P4 归类重构（2026-08-21）：``ui/fullscreen`` 下所有业务逻辑均已按职责归入
分类子包，各子包 ``__init__.py`` 只做再导出：

- ``app/`` 装配（core/startup/runner）、``input/`` 输入区（composer/editing/menu）
- ``turn/`` 回合执行（execution）、``screens/`` 设置与管理面板（settings/
  model_picker/channel_manager/… 各 Screen + navigation）
- ``rendering/`` 渲染管线与对话区组件（pipeline/widgets/tool_diff/latex/welcome_logo）
- ``status/`` HUD（indicators/hud 纯格式化）、``terminal/`` 终端协议与外观
  （handling/theme）、``support/`` 非 Textual 支持层（turns/commands/monitor）
- ``conversation/`` 会话视图（view）

本文件**只做再导出，不存放业务逻辑**；进程入口 ``run_fullscreen_tui``
位于 ``app/runner.py``。这里保留的模块级名称是既有测试/扩展的
monkeypatch 契约：``sys``（``omnicrawl.ui.fullscreen.sys.__stdout__``）、
``OmniCrawlApp``、``ModelPickerScreen`` 与 ``commands.slash`` 委托函数。
"""

from __future__ import annotations

import sys

from ...commands.slash import (
    build_slash_command_options,
    format_memory_clean_result,
    format_mcp_status,
    format_plugins_status,
    format_skills_list,
    format_tool_confirmation,
    handle_advisor_command,
    handle_approval_command,
    handle_mode_command,
    handle_model_command,
    handle_reasoning_command,
    handle_review_command,
    handle_session_command,
    handle_subagent_task_command,
)
from .app import OmniCrawlApp
from .app.runner import run_fullscreen_tui
from .app.startup import FullscreenStartup
from .input.composer import Composer
from .rendering.widgets import (
    AssistantMessage,
    ConfirmationScreen,
    ReasoningDisclosure,
    SubAgentProgressTree,
    TodoPlan,
    ToolDisclosure,
)
from .screens.model_picker import ModelPickerResult, ModelPickerScreen
from .support.turns import AgentTurnCallbacks, AgentTurnController
from .terminal.handling import (
    OmniCrawlWindowsDriver,
    OmniCrawlWindowsEventMonitor,
    TerminalHandlingMixin,
    _disable_terminal_mouse_reporting,
    _restore_windows_raw_input_mode_if_needed,
    _restore_windows_vt_input_mode_if_needed,
)

__all__ = [
    "AgentTurnCallbacks",
    "AgentTurnController",
    "ConfirmationScreen",
    "FullscreenStartup",
    "ModelPickerResult",
    "ModelPickerScreen",
    "OmniCrawlApp",
    "ReasoningDisclosure",
    "ToolDisclosure",
    "run_fullscreen_tui",
]
