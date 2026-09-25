"""终端 UI 兼容导出（**已弃用**，保留作开发对照）。

产品工作台已改由 Rust TUI 承担（``omnicrawl-tui``）。本包内的 Textual 全屏工作台
（``omnicrawl.ui.fullscreen``）不再随发布产物分发，也不再是任何入口的启动路径：
Python 侧的入口（``main.py`` / ``omnicrawl/compat.py`` / console script）已随
「彻底脱离 Python 宿主」删除，本包只作为开发期的**样式/行为对照参照**保留
（用 ``D:/ProgramData/Anaconda3/python.exe`` 一类带 textual 的解释器直接跑仓库源码即可对照，
见 ``rust/docs/frozen-reference.md``）；功能缺口一律以 Rust 侧为准。

纯 Python TUI（``tui`` / ``chat_session`` / ``inline_input`` / ``stream_turn``）已删除，
仅保留 ``UIStartupError`` 供入口与旧测试引用。
"""

from __future__ import annotations

from .base import UIStartupError

__all__ = [
    "UIStartupError",
]
