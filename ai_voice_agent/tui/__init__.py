"""TUI 包 — 重新导出所有公开 API，保持导入兼容。"""

from ._capabilities import TerminalCapabilities, detect_capabilities
from ._colors import (
    ANSI_BLACK,
    ANSI_BLUE,
    ANSI_BOLD,
    ANSI_BRIGHT_BLACK,
    ANSI_BRIGHT_BLUE,
    ANSI_BRIGHT_CYAN,
    ANSI_BRIGHT_GREEN,
    ANSI_BRIGHT_MAGENTA,
    ANSI_BRIGHT_RED,
    ANSI_BRIGHT_WHITE,
    ANSI_BRIGHT_YELLOW,
    ANSI_BLINK,
    ANSI_CLEAR_LINE,
    ANSI_CLEAR_TO_LINE_END,
    ANSI_CYAN,
    ANSI_DIM,
    ANSI_ERASE_TO_END,
    ANSI_GREEN,
    ANSI_ITALIC,
    ANSI_MAGENTA,
    ANSI_PREVIOUS_LINE,
    ANSI_RED,
    ANSI_RESET,
    ANSI_RESTORE_CURSOR,
    ANSI_SAVE_CURSOR,
    ANSI_UNDERLINE,
    ANSI_WHITE,
    ANSI_YELLOW,
    COLOR_ACCENT,
    COLOR_ERROR,
    COLOR_HEADING,
    COLOR_MUTED,
    COLOR_PRIMARY,
    COLOR_SECONDARY,
    COLOR_SUCCESS,
    COLOR_TEXT,
    COLOR_WARNING,
    AI_PREFIX,
    USER_PREFIX,
    ColorRole,
    color_text,
    get_color_sequence,
    strip_ansi,
)
from ._display import (
    _char_display_width,
    _contains_complex_display_width,
    _dialog_continuation_prefix,
    _display_width,
    _ellipsize_display_text,
    _normalize_terminal_text,
    _preview_display_rows,
    _split_display_rows,
    _take_display_width,
)
from ._markdown import (
    MarkdownSpan,
    MarkdownStreamState,
)
from ._tools import ToolDisplayState
from ._core import TerminalUI
from ._status import StatusLine
from ._spinner import InputBar, WaitingIndicator, SPINNER_FRAMES

__all__ = [
    # Capabilities
    "TerminalCapabilities",
    "detect_capabilities",
    # Colors
    "ANSI_RESET",
    "ANSI_BLACK", "ANSI_RED", "ANSI_GREEN", "ANSI_YELLOW",
    "ANSI_BLUE", "ANSI_MAGENTA", "ANSI_CYAN", "ANSI_WHITE",
    "ANSI_BRIGHT_BLACK", "ANSI_BRIGHT_RED", "ANSI_BRIGHT_GREEN", "ANSI_BRIGHT_YELLOW",
    "ANSI_BRIGHT_BLUE", "ANSI_BRIGHT_MAGENTA", "ANSI_BRIGHT_CYAN", "ANSI_BRIGHT_WHITE",
    "ANSI_BOLD", "ANSI_DIM", "ANSI_ITALIC", "ANSI_UNDERLINE", "ANSI_BLINK",
    "ANSI_CLEAR_LINE", "ANSI_CLEAR_TO_LINE_END", "ANSI_PREVIOUS_LINE",
    "ANSI_SAVE_CURSOR", "ANSI_RESTORE_CURSOR", "ANSI_ERASE_TO_END",
    "COLOR_PRIMARY", "COLOR_SECONDARY", "COLOR_SUCCESS", "COLOR_WARNING",
    "COLOR_ERROR", "COLOR_MUTED", "COLOR_TEXT", "COLOR_HEADING", "COLOR_ACCENT",
    "AI_PREFIX", "USER_PREFIX",
    "ColorRole", "color_text", "get_color_sequence", "strip_ansi",
    # Display
    "_char_display_width", "_contains_complex_display_width",
    "_dialog_continuation_prefix", "_display_width",
    "_ellipsize_display_text", "_normalize_terminal_text",
    "_preview_display_rows", "_split_display_rows", "_take_display_width",
    # Markdown
    "MarkdownSpan", "MarkdownStreamState",
    # Tools
    "ToolDisplayState",
    # Core
    "TerminalUI",
    # Status
    "StatusLine",
    # Spinner
    "InputBar", "WaitingIndicator", "SPINNER_FRAMES",
]
