"""终端能力检测。"""

from __future__ import annotations

import ctypes
import os
import sys
from dataclasses import dataclass


@dataclass(frozen=True)
class TerminalCapabilities:
    """当前终端可用能力。

    终端样式本质由终端模拟器决定。这里仅判断是否适合输出 ANSI 控制序列，
    不尝试模拟真正的小字体或复杂 TUI。
    """

    ansi: bool
    truecolor: bool = False
    color256: bool = False


def detect_capabilities() -> TerminalCapabilities:
    """根据环境判断是否启用 ANSI 样式和行重绘。"""

    if os.getenv("NO_COLOR"):
        return TerminalCapabilities(ansi=False)

    if os.name == "nt":
        ansi = bool(
            os.getenv("WT_SESSION")
            or os.getenv("TERM_PROGRAM")
            or os.getenv("ANSICON")
            or os.getenv("ConEmuANSI") == "ON"
            or "xterm" in os.getenv("TERM", "").lower()
            or _enable_windows_virtual_terminal()
        )
    else:
        ansi = sys.stdout.isatty() and os.getenv("TERM") != "dumb"

    if not ansi:
        return TerminalCapabilities(ansi=False)

    # 真彩色检测：COLORTERM 含 truecolor/24bit，或 Windows Terminal
    truecolor = bool(
        os.getenv("WT_SESSION")
        or "truecolor" in os.getenv("COLORTERM", "").lower()
        or "24bit" in os.getenv("COLORTERM", "").lower()
    )

    # 256 色检测
    color256 = bool(
        os.getenv("WT_SESSION")
        or "256color" in os.getenv("TERM", "").lower()
        or os.getenv("ANSICON")
        or os.getenv("ConEmuANSI") == "ON"
    )

    return TerminalCapabilities(ansi=True, truecolor=truecolor, color256=color256)


def _enable_windows_virtual_terminal() -> bool:
    """在 Windows 控制台中开启 ANSI/VT 控制序列支持。"""

    if not sys.stdout.isatty():
        return False

    kernel32 = ctypes.windll.kernel32
    handle = kernel32.GetStdHandle(-11)
    if handle == -1:
        return False

    mode = ctypes.c_uint32()
    if not kernel32.GetConsoleMode(handle, ctypes.byref(mode)):
        return False

    ENABLE_VIRTUAL_TERMINAL_PROCESSING = 0x0004
    updated_mode = mode.value | ENABLE_VIRTUAL_TERMINAL_PROCESSING
    if not kernel32.SetConsoleMode(handle, updated_mode):
        return False

    return True
