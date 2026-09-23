"""终端 UI 兼容导出（**已弃用**，保留作开发对照）。

产品工作台已改由 Rust TUI 承担（``omnicrawl-tui``）。本包内的 Textual 全屏工作台
（``omnicrawl.ui.fullscreen``）不再随发布产物分发，也不再是默认启动路径：进程入口
（``main.py`` / ``python -m omnicrawl`` / 控制台脚本）现在是 ``omnicrawl.compat`` 的薄垫片，
直接转发 Rust 宿主。仅在 ``OMNICRAWL_LEGACY_PYTHON_UI=1`` 或 ``--legacy-python`` 时才会
走到这里，用于开发期对照视觉与交互 parity；功能缺口一律以 Rust 侧为准。

纯 Python TUI（``tui`` / ``chat_session`` / ``inline_input`` / ``stream_turn``）已删除，
仅保留 ``UIStartupError`` 供入口与旧测试引用。
"""

from __future__ import annotations

from .base import UIStartupError

__all__ = [
    "UIStartupError",
]
