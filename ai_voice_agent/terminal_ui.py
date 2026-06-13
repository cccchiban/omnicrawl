"""兼容 shim — 重新导出 tui 包的所有公开 API。"""

from .tui import *  # noqa: F401,F403
from .tui import (  # noqa: F401 — 显式导出以帮助 IDE 补全
    TerminalCapabilities,
    TerminalUI,
    InputBar,
    StatusLine,
    WaitingIndicator,
    MarkdownSpan,
    MarkdownStreamState,
    ToolDisplayState,
    AI_PREFIX,
    USER_PREFIX,
    SPINNER_FRAMES,
    _display_width,
    _char_display_width,
    _contains_complex_display_width,
    _dialog_continuation_prefix,
    _ellipsize_display_text,
    _normalize_terminal_text,
    _preview_display_rows,
    _split_display_rows,
    _take_display_width,
)
