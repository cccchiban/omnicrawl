"""终端 UI 工厂与兼容导出。"""

from __future__ import annotations

from .base import BaseUI, UIStartupError
from .chat_session import run_inline_chat
from .inline_input import read_line_autocomplete
from .terminal import TerminalUI
# 兼容早期测试和调用方从包根目录打补丁；实际实现位于 chat_session.py。
from .chat_session import _get_user_text

__all__ = [
    "BaseUI",
    "UIStartupError",
    "TerminalUI",
    "create_ui",
    "read_line_autocomplete",
    "run_inline_chat",
]


def create_ui(*, model_label: str | None = None) -> BaseUI:
    """创建兼容旧调用方的 ANSI 终端 UI 实例。

    默认启动路径已经改为 Textual 全屏工作台；此工厂保留给测试、重定向输出
    和仍依赖 ``BaseUI`` 的旧代码，不承担交互式主界面职责。
    """

    from .terminal import TerminalUI

    return TerminalUI(model_label=model_label)
