"""UI 启动异常定义。

纯 Python TUI（``omnicrawl.ui.tui`` / ``chat_session`` / ``inline_input`` /
``stream_turn``）已移除，默认启动路径只有 Textual 全屏 TUI。此处仅保留
``UIStartupError`` 供入口与测试引用。
"""

from __future__ import annotations


class UIStartupError(RuntimeError):
    """前端界面启动失败时抛出，通常由缺少 GUI 依赖或系统图形环境异常引起。"""
