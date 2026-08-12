"""终端 UI 兼容导出。

默认启动路径为 Textual 全屏工作台（``omnicrawl.ui.fullscreen``）。纯 Python
TUI（``tui`` / ``chat_session`` / ``inline_input`` / ``stream_turn``）已删除，
仅保留 ``UIStartupError`` 供入口与旧测试引用。
"""

from __future__ import annotations

from .base import UIStartupError

__all__ = [
    "UIStartupError",
]
