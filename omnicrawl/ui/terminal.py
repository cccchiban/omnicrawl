"""兼容 shim — 重新导出 tui 包的所有公开 API。"""

from __future__ import annotations

from .tui import *  # noqa: F401,F403
from .tui import (  # noqa: F401 — 显式导出以帮助 IDE 补全
    TerminalCapabilities,
    TerminalUI,
    InputBar,
    StatusLine,
    WaitingIndicator,
    MarkdownSpan,
    MarkdownStreamState,
    AI_PREFIX,
    USER_PREFIX,
    SPINNER_FRAMES,
    _display_width,
    _char_display_width,
    _contains_complex_display_width,
    _dialog_continuation_prefix,
    _ellipsize_display_text,
    _iter_display_units,
    _normalize_terminal_text,
    _preview_display_rows,
    _split_display_rows,
    _take_display_width,
)
from .tui import _combine_surrogate_pair as _terminal_combine_surrogate_pair
