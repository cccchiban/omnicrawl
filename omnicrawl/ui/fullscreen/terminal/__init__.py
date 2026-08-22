"""terminal 类别包：终端协议适配与终端外观主题。

本文件只做再导出，不存放业务逻辑：
- Windows 终端驱动/事件监视器/``TerminalHandlingMixin`` 在 ``handling.py``；
- 颜色常量与 ``terminal_css`` 在 ``theme.py``。
"""

from .handling import (
    OmniCrawlWindowsDriver,
    OmniCrawlWindowsEventMonitor,
    TerminalHandlingMixin,
    _disable_terminal_mouse_reporting,
    _restore_windows_raw_input_mode_if_needed,
    _restore_windows_vt_input_mode_if_needed,
)
from .theme import (
    ACCENT_AMBER,
    ACCENT_BLUE,
    ACCENT_GREEN,
    ACCENT_PURPLE,
    ACCENT_RED,
    BORDER_MUTED,
    REASONING_BACKGROUND,
    REASONING_TEXT,
    TERMINAL_BACKGROUND,
    TERMINAL_FOREGROUND,
    TERMINAL_THEME,
    TEXT_FAINT,
    TEXT_MUTED,
    TEXT_PRIMARY,
    TEXT_SECONDARY,
    THEME_NAME,
    TOOL_TEXT,
    terminal_css,
    terminal_select_css,
)

__all__ = [
    "OmniCrawlWindowsDriver",
    "OmniCrawlWindowsEventMonitor",
    "TerminalHandlingMixin",
    "_disable_terminal_mouse_reporting",
    "_restore_windows_raw_input_mode_if_needed",
    "_restore_windows_vt_input_mode_if_needed",
    "ACCENT_AMBER",
    "ACCENT_BLUE",
    "ACCENT_GREEN",
    "ACCENT_PURPLE",
    "ACCENT_RED",
    "BORDER_MUTED",
    "REASONING_BACKGROUND",
    "REASONING_TEXT",
    "TERMINAL_BACKGROUND",
    "TERMINAL_FOREGROUND",
    "TERMINAL_THEME",
    "TEXT_FAINT",
    "TEXT_MUTED",
    "TEXT_PRIMARY",
    "TEXT_SECONDARY",
    "THEME_NAME",
    "TOOL_TEXT",
    "terminal_css",
    "terminal_select_css",
]
