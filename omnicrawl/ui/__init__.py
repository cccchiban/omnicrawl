"""TUI 子系统合并入口。

终端会话外壳、启动器和基础 UI 类型集中到这里，并注册旧模块别名以保持兼容。
"""

from __future__ import annotations

import sys as _sys

_THIS_MODULE = _sys.modules[__name__]
_UI_MODULE_ALIASES = (
    'base',
    'terminal',
    'windows_launcher',
    'inline_input',
    'chat_session',
)
for _alias in _UI_MODULE_ALIASES:
    _sys.modules[f"{__name__}.{_alias}"] = _THIS_MODULE
    globals()[_alias] = _THIS_MODULE

# --- former module: base.py ---
"""UI 抽象基类 — 定义前端接口契约，TUI / Web 等实现均继承此类。"""


import abc
import threading
from typing import Any


class UIStartupError(RuntimeError):
    """前端界面启动失败时抛出，通常由缺少 GUI 依赖或系统图形环境异常引起。"""


class BaseUI(abc.ABC):
    """前端 UI 的抽象接口。

    所有前端实现（TUI、Web 等）必须提供这些方法，供 chat_session /
    speech_playback 等模块调用。方法分为三类：

    1. 样式快捷方法 — 返回带样式标记的文本（实现可忽略样式）
    2. 输出方法 — 向用户展示信息
    3. 交互方法 — 获取用户输入 / 确认
    """

    def __init__(self, *, model_label: str | None = None) -> None:
        self.model_label = model_label
        self._lock = threading.Lock()
        self._input_tokens = 0
        self._output_tokens = 0
        self._cached_input_tokens = 0

    # ── Token 统计 ───────────────────────────────────────────

    def update_token_usage(
        self,
        input_tokens: int,
        output_tokens: int,
        cached_input_tokens: int = 0,
    ) -> None:
        with self._lock:
            self._input_tokens = max(0, int(input_tokens))
            self._output_tokens = max(0, int(output_tokens))
            self._cached_input_tokens = max(0, int(cached_input_tokens))

    def set_model_label(self, text: str) -> None:
        """更新当前模型标签；具体 UI 可覆盖此方法同步刷新界面。"""

        with self._lock:
            self.model_label = text.strip()

    def show_html(self, title: str, html: str) -> None:
        """在支持的图形界面中显示 HTML；终端界面默认忽略。"""

        return None

    # ── 样式快捷方法（默认无样式透传文本）──────────────────────

    def muted(self, text: str) -> str:
        return text

    def muted_italic(self, text: str) -> str:
        return text

    def result_text(self, ok: bool, text: str) -> str:
        return text

    def accent(self, text: str) -> str:
        return text

    def bright(self, text: str) -> str:
        return text

    def primary(self, text: str) -> str:
        return text

    def secondary(self, text: str) -> str:
        return text

    def success(self, text: str) -> str:
        return text

    def warning(self, text: str) -> str:
        return text

    def error(self, text: str) -> str:
        return text

    def heading(self, text: str) -> str:
        return text

    # ── 输出方法 ─────────────────────────────────────────────

    @abc.abstractmethod
    def print_startup_panel(self, title: str, lines: list[str]) -> None: ...

    @abc.abstractmethod
    def print_tool_call_start(
        self,
        step: int,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        leading_blank: bool = True,
    ) -> None: ...

    @abc.abstractmethod
    def print_tool_result_record(
        self,
        ok: bool,
        output: str | None = None,
        *,
        tool_name: str = "",
    ) -> None: ...

    @abc.abstractmethod
    def write_markdown_delta(self, delta: str, state: Any) -> None: ...

    @abc.abstractmethod
    def flush_markdown(self, state: Any) -> None: ...

    @abc.abstractmethod
    def print_ai_prefix(self) -> None: ...

    @abc.abstractmethod
    def write(self, text: str) -> None: ...

    @abc.abstractmethod
    def newline(self) -> None: ...

    @abc.abstractmethod
    def status(self, message: str, *, leading_blank: bool = True, italic: bool = False) -> None: ...

    @abc.abstractmethod
    def notice(self, message: str) -> None: ...

    @abc.abstractmethod
    def mark_transient_output_start(self) -> bool: ...

    @abc.abstractmethod
    def clear_transient_output(self) -> None: ...

    # ── 输入 / 交互方法 ───────────────────────────────────────

    @abc.abstractmethod
    def prompt(self) -> str: ...

    @abc.abstractmethod
    def prompt_yes_no(self, prompt: str, confirmed_label: str = "") -> bool: ...

    @abc.abstractmethod
    def inline_turn_base(self, user_text: str) -> str: ...

    @abc.abstractmethod
    def replace_current_input_with_status(self, message: str) -> None: ...

    @abc.abstractmethod
    def clear_current_input_status(self) -> None: ...


# --- former module: terminal.py ---
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
from .tui import _combine_surrogate_pair as _terminal_combine_surrogate_pair


# --- former module: __init__.py ---
"""终端 UI 工厂。"""


from .base import BaseUI, UIStartupError

__all__ = ["BaseUI", "UIStartupError", "create_ui"]


def create_ui(
    *,
    model_label: str | None = None,
) -> BaseUI:
    """创建终端 UI 实例。

    Parameters
    ----------
    model_label : str | None
        模型名称标签，显示在状态行。
    """
    from .terminal import TerminalUI

    return TerminalUI(model_label=model_label)


# --- former module: windows_launcher.py ---

import os
import subprocess
import sys
from pathlib import Path

from ..workspace.context import LAUNCH_CWD_ENV

POWERSHELL_CHILD_ENV = "AI_VOICE_CHAT_IN_POWERSHELL"


def configure_console_encoding() -> None:
    """尽量使用 UTF-8 输出，减少 Windows 命令行中文乱码概率。"""

    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")


def _running_in_powershell_child() -> bool:
    """判断当前进程是否已经是弹出窗口中的真实对话进程。"""

    return os.getenv(POWERSHELL_CHILD_ENV) == "1"


def launch_in_powershell_window(script_path: Path, argv: list[str] | None = None) -> bool:
    """从 IDE 或测试窗口启动时，弹出独立 PowerShell 运行本脚本。"""

    if os.name != "nt" or _running_in_powershell_child():
        return False

    script_path = script_path.resolve()
    launch_cwd = Path.cwd().resolve()
    args = list(sys.argv[1:] if argv is None else argv)
    script_args = "".join(f" {_powershell_single_quoted(argument)}" for argument in args)
    command = (
        f"$env:{POWERSHELL_CHILD_ENV}='1'; "
        f"$env:{LAUNCH_CWD_ENV}={_powershell_single_quoted(str(launch_cwd))}; "
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; "
        f"& {_powershell_single_quoted(sys.executable)} {_powershell_single_quoted(str(script_path))}{script_args}; "
        "Write-Host ''; "
        "Read-Host '对话已结束，按 Enter 关闭窗口'"
    )

    try:
        subprocess.Popen(
            [
                "powershell.exe",
                "-NoExit",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=str(launch_cwd),
            creationflags=subprocess.CREATE_NEW_CONSOLE,
        )
    except OSError as exc:
        print(f"弹出 PowerShell 窗口失败，将在当前窗口继续运行：{exc}")
        return False

    print("已弹出独立 PowerShell 窗口，请在新窗口中进行语音对话。")
    return True


def _powershell_single_quoted(value: str) -> str:
    """生成 PowerShell 单引号字符串，避免路径中的空格或特殊字符破坏启动命令。"""

    return "'" + value.replace("'", "''") + "'"


# --- former module: inline_input.py ---

import shutil
import time

from .terminal import (
    TerminalUI,
    _display_width as _terminal_display_width,
    _iter_display_units as _terminal_display_units,
    _take_display_width as _terminal_take_display_width,
)


INLINE_COMPLETION_LIMIT = 8
INLINE_PASTE_SEQUENCE_TIMEOUT_SECONDS = 0.5
INLINE_PASTE_BURST_QUIET_SECONDS = 0.03
INLINE_BRACKETED_PASTE_ON = "\033[?2004h"
INLINE_BRACKETED_PASTE_OFF = "\033[?2004l"
INLINE_CLEAR_TO_LINE_END = "\033[K"
INLINE_INPUT_WINDOW_ROWS = 8
INLINE_INPUT_HISTORY_LIMIT = 100
_INLINE_INPUT_HISTORY: list[str] = []


def _prompt_visible_width(prompt: str, ui: TerminalUI | None) -> int:
    """返回当前输入提示符在终端里实际占用的列数。"""

    if ui is not None:
        return ui.prompt_width()
    return _terminal_display_width(prompt.rsplit("\n", 1)[-1])


def _truncate_menu_text(text: str, max_width: int) -> str:
    """按字符近似截断菜单项，避免长命令把终端行撑乱。"""

    if max_width <= 3:
        return _terminal_take_display_width(text, max(0, max_width))
    if _terminal_display_width(text) <= max_width:
        return text
    return f"{_terminal_take_display_width(text, max_width - 3)}..."


def _normalize_inline_pasted_text(text: str) -> str:
    """统一粘贴文本换行，保留代码缩进和首尾空白。"""

    return text.replace("\r\n", "\n").replace("\r", "\n")


def _display_unit_boundaries(text: str) -> list[tuple[int, int]]:
    """返回文本中每个可见显示单元的 Python 字符串偏移范围。"""

    boundaries: list[tuple[int, int]] = []
    offset = 0
    for unit in _terminal_display_units(text):
        end = offset + len(unit)
        boundaries.append((offset, end))
        offset = end
    return boundaries


def _previous_display_unit_offset(text: str, cursor: int) -> int:
    """将光标移至前一个完整显示单元的起点。"""

    cursor = max(0, min(cursor, len(text)))
    for start, end in _display_unit_boundaries(text):
        if cursor <= end:
            return start
    return len(text)


def _next_display_unit_offset(text: str, cursor: int) -> int:
    """将光标移至下一个完整显示单元的终点。"""

    cursor = max(0, min(cursor, len(text)))
    for _start, end in _display_unit_boundaries(text):
        if cursor < end:
            return end
    return len(text)


def _display_unit_at_or_after(text: str, cursor: int) -> tuple[int, int] | None:
    """返回光标所在或紧随其后的完整显示单元范围。"""

    cursor = max(0, min(cursor, len(text)))
    for start, end in _display_unit_boundaries(text):
        if start <= cursor < end or cursor <= start:
            return start, end
    return None


def _delete_display_unit_before(text: str, cursor: int) -> str:
    if cursor <= 0:
        return text
    start = _previous_display_unit_offset(text, cursor)
    end = _next_display_unit_offset(text, start)
    return text[:start] + text[end:]


def _delete_display_unit_after(text: str, cursor: int) -> str:
    unit = _display_unit_at_or_after(text, cursor)
    if unit is None:
        return text
    start, end = unit
    return text[:start] + text[end:]


def _split_inline_input_rows_with_offsets(
    text: str,
    max_width: int,
) -> list[tuple[str, int, int]]:
    """把输入按视觉行拆分，并保留每一行在原文本中的偏移。"""

    if max_width <= 0:
        return [("", 0, 0)]

    normalized = _normalize_inline_pasted_text(text)
    parts = normalized.split("\n")
    rows: list[tuple[str, int, int]] = []
    position = 0
    for part_index, raw_line in enumerate(parts):
        line_start = position
        remaining = raw_line
        consumed = 0
        if not remaining:
            rows.append(("", line_start, line_start))
        else:
            while remaining:
                chunk = _terminal_take_display_width(remaining, max_width)
                if not chunk:
                    # 与 TUI 输出共用字素簇边界，避免光标进入 ZWJ/旗帜等组合字符内部。
                    chunk = next(_terminal_display_units(remaining))
                chunk_start = line_start + consumed
                consumed += len(chunk)
                rows.append((chunk, chunk_start, chunk_start + len(chunk)))
                remaining = remaining[len(chunk) :]
        position += len(raw_line)
        if part_index < len(parts) - 1:
            position += 1
    return rows or [("", 0, 0)]


def format_inline_input_render(
    text: str,
    cursor: int,
    *,
    prompt_text: str,
    prompt_width: int,
    terminal_width: int,
    terminal_height: int,
) -> tuple[list[str], int, int]:
    """生成真实多行输入块，并返回光标所在的可见行和终端列。"""

    content_width = max(1, terminal_width - prompt_width - 1)
    window_height = max(3, min(INLINE_INPUT_WINDOW_ROWS, max(3, terminal_height - 8)))
    cursor = max(0, min(cursor, len(text)))

    rows = _split_inline_input_rows_with_offsets(text, content_width)
    cursor_rows = _split_inline_input_rows_with_offsets(text[:cursor], content_width)
    cursor_row = max(0, len(cursor_rows) - 1)
    cursor_text = cursor_rows[-1][0] if cursor_rows else ""
    cursor_content_width = _terminal_display_width(cursor_text)

    if len(rows) <= window_height:
        start = 0
    else:
        start = max(0, cursor_row - window_height + 1)
        start = min(start, len(rows) - window_height)

    visible_rows = rows[start : start + window_height]
    continuation_prefix = " " * prompt_width
    rendered_lines = [
        (prompt_text if start + offset == 0 else continuation_prefix) + row_text
        for offset, (row_text, _row_start, _row_end) in enumerate(visible_rows)
    ]
    if not rendered_lines:
        rendered_lines = [prompt_text]

    cursor_row_offset = max(0, cursor_row - start)
    cursor_column = min(prompt_width + cursor_content_width + 1, max(1, terminal_width))
    return rendered_lines, cursor_row_offset, cursor_column


def _slash_command_matches(text: str, commands: list[str]) -> list[str]:
    """只在整行以 / 开头时启用命令候选，避免普通文本中误触发。"""

    if not text.startswith("/"):
        return []
    return [command for command in commands if command.startswith(text)]


def _visible_completion_window(
    matches: list[str],
    selected_index: int,
    *,
    limit: int = INLINE_COMPLETION_LIMIT,
) -> tuple[int, list[str]]:
    """返回当前选中项附近的一段候选，保证选中项始终在可见窗口内。"""

    if not matches:
        return 0, []

    selected_index = max(0, min(selected_index, len(matches) - 1))
    limit = max(1, limit)
    start = 0
    if selected_index >= limit:
        start = selected_index - limit + 1
    return start, matches[start : start + limit]


def _format_completion_menu_lines(
    matches: list[str],
    selected_index: int,
    *,
    terminal_width: int,
) -> list[str]:
    """把候选命令格式化成稳定的菜单行，供行内 TUI 重绘。"""

    if not matches:
        return []

    start, visible = _visible_completion_window(matches, selected_index)
    command_width = max(1, terminal_width - 8)
    lines: list[str] = []
    for offset, command in enumerate(visible):
        actual_index = start + offset
        marker = "> " if actual_index == selected_index else "  "
        lines.append(f"  {marker}{_truncate_menu_text(command, command_width)}")
    return lines


def _append_inline_input_history(
    history: list[str],
    text: str,
    *,
    limit: int = INLINE_INPUT_HISTORY_LIMIT,
) -> None:
    """记录已提交输入，跳过空输入和连续重复项，避免历史里塞满噪声。"""

    if not text.strip():
        return
    if history and history[-1] == text:
        return

    history.append(text)
    overflow = len(history) - max(1, limit)
    if overflow > 0:
        del history[:overflow]


class _InlineInputHistoryBrowser:
    """维护单次输入编辑中的历史浏览状态。

    `history` 列表跨多次读取复用；浏览器只保存当前输入框的临时位置和草稿。
    第一次按上键时保存正在编辑的草稿，按下键越过最新历史后会恢复这份草稿。
    """

    def __init__(self, history: list[str]) -> None:
        self._history = history
        self._index: int | None = None
        self._draft = ""

    @property
    def is_browsing(self) -> bool:
        return self._index is not None

    def reset(self) -> None:
        self._index = None
        self._draft = ""

    def previous(self, current_text: str) -> str | None:
        if not self._history:
            return None

        if self._index is None:
            self._draft = current_text
            self._index = len(self._history) - 1
        else:
            self._index = max(0, self._index - 1)
        return self._history[self._index]

    def next(self) -> str | None:
        if self._index is None:
            return None

        if self._index < len(self._history) - 1:
            self._index += 1
            return self._history[self._index]

        draft = self._draft
        self.reset()
        return draft


def _is_inline_escape_sequence_complete(sequence: str) -> bool:
    """判断行内输入读取到的 ESC 序列是否完整。"""

    if not sequence:
        return False
    if sequence[0] not in {"[", "O"}:
        return True
    if sequence[0] == "O":
        return len(sequence) >= 2
    return len(sequence) >= 2 and 0x40 <= ord(sequence[-1]) <= 0x7E


def _read_inline_bracketed_paste(
    *,
    timeout_seconds: float = INLINE_PASTE_SEQUENCE_TIMEOUT_SECONDS,
) -> str:
    """读取终端括号粘贴 ESC[200~ 和 ESC[201~ 之间的原始文本。"""

    import msvcrt
    import time as _time

    chars: list[str] = []
    deadline = _time.monotonic() + timeout_seconds
    while _time.monotonic() < deadline:
        if not msvcrt.kbhit():
            _time.sleep(0.001)
            continue

        char = msvcrt.getwch()
        deadline = _time.monotonic() + timeout_seconds
        if char != "\x1b":
            chars.append(char)
            continue

        seq_parts: list[str] = []
        sequence_deadline = _time.monotonic() + timeout_seconds
        while _time.monotonic() < sequence_deadline:
            if msvcrt.kbhit():
                seq_parts.append(msvcrt.getwch())
                if _is_inline_escape_sequence_complete("".join(seq_parts)):
                    break
            else:
                _time.sleep(0.001)
        sequence = "".join(seq_parts)
        if sequence == "[201~":
            break
        chars.append("\x1b" + sequence)

    return "".join(chars)


class _InlineCompletionMenu:
    """管理输入行下方的临时候选区域。"""

    def __init__(self, ui: TerminalUI | None) -> None:
        self._ui = ui
        self._rendered_lines = 0
        self._allocated_lines = 0

    @property
    def _ansi_enabled(self) -> bool:
        return self._ui is not None and self._ui.capabilities.ansi

    def render(
        self,
        *,
        matches: list[str],
        selected_index: int,
        cursor_column: int,
        rows_below_cursor: int = 0,
    ) -> None:
        if not self._ansi_enabled:
            return

        terminal_width = shutil.get_terminal_size((100, 30)).columns
        lines = _format_completion_menu_lines(
            matches,
            selected_index,
            terminal_width=terminal_width,
        )
        self._replace_lines(lines, cursor_column, rows_below_cursor)

    def clear(self, *, cursor_column: int, rows_below_cursor: int = 0) -> None:
        if not self._ansi_enabled:
            return
        self._replace_lines([], cursor_column, rows_below_cursor)

    def _replace_lines(
        self,
        lines: list[str],
        cursor_column: int,
        rows_below_cursor: int = 0,
    ) -> None:
        assert self._ui is not None

        rows_below_cursor = max(0, rows_below_cursor)
        if rows_below_cursor:
            with self._ui._lock:
                print(f"\033[{rows_below_cursor}B", end="", flush=True)

        self._ensure_allocated(len(lines), cursor_column)
        lines_to_clear = max(self._rendered_lines, len(lines))
        if lines_to_clear == 0:
            if rows_below_cursor:
                with self._ui._lock:
                    print(f"\033[{rows_below_cursor}A", end="", flush=True)
            return

        parts: list[str] = []
        for index in range(lines_to_clear):
            parts.append("\033[1B\r\033[2K")
            if index < len(lines):
                parts.append(lines[index])
        parts.append(f"\033[{lines_to_clear}A")
        parts.append(f"\033[{max(1, cursor_column)}G")

        with self._ui._lock:
            print("".join(parts), end="", flush=True)
        self._rendered_lines = len(lines)
        if rows_below_cursor:
            with self._ui._lock:
                print(f"\033[{rows_below_cursor}A", end="", flush=True)

    def _ensure_allocated(self, line_count: int, cursor_column: int) -> None:
        assert self._ui is not None

        missing_lines = max(0, line_count - self._allocated_lines)
        if missing_lines == 0:
            return

        # 在输入行下面预留真实终端行，避免光标已经在窗口底部时 CSI 向下移动失败。
        with self._ui._lock:
            print("\n" * missing_lines, end="")
            print(f"\033[{missing_lines}A\033[{max(1, cursor_column)}G", end="", flush=True)
        self._allocated_lines = line_count


def read_line_autocomplete(
    prompt: str,
    commands: list[str],
    ui: TerminalUI | None = None,
    history: list[str] | None = None,
) -> str:
    """逐字符读取输入，在输入 / 时实时显示匹配命令，并支持上下键浏览历史。"""

    import msvcrt

    if ui is None or not ui.capabilities.ansi:
        return input(prompt).strip()

    input_history = _INLINE_INPUT_HISTORY if history is None else history
    history_browser = _InlineInputHistoryBrowser(input_history)
    prompt_text = prompt.rsplit("\n", 1)[-1]
    prompt_width = _prompt_visible_width(prompt, ui)
    menu = _InlineCompletionMenu(ui)
    print(f"{INLINE_BRACKETED_PASTE_ON}{prompt}", end="", flush=True)
    text = ""
    cursor = 0
    matches: list[str] = []
    match_index = 0
    paste_burst_until = 0.0
    skip_next_lf_after_cr_paste = False
    rendered_input_lines = 1
    rendered_cursor_row = 0
    pending_high_surrogate = ""

    def _input_layout() -> tuple[
        list[tuple[str, int, int]],
        list[tuple[str, int, int]],
        int,
        int,
        int,
        int,
        int,
        int,
        int,
    ]:
        terminal_width, terminal_height = shutil.get_terminal_size((100, 30))
        terminal_height = max(14, terminal_height)
        content_width = max(1, terminal_width - prompt_width - 1)
        window_height = max(3, min(INLINE_INPUT_WINDOW_ROWS, terminal_height - 8))
        rows = _split_inline_input_rows_with_offsets(text, content_width)
        cursor_rows = _split_inline_input_rows_with_offsets(text[:cursor], content_width)
        cursor_row = max(0, len(cursor_rows) - 1)
        cursor_col = _terminal_display_width(cursor_rows[-1][0]) if cursor_rows else 0
        if len(rows) <= window_height:
            start = 0
        else:
            start = max(0, cursor_row - window_height + 1)
            start = min(start, len(rows) - window_height)
        visible_rows = rows[start : start + window_height]
        return (
            rows,
            visible_rows,
            start,
            cursor_row,
            cursor_row - start,
            cursor_col,
            window_height,
            terminal_width,
            terminal_height,
        )

    def _cursor_column() -> int:
        terminal_width, terminal_height = shutil.get_terminal_size((100, 30))
        _lines, _cursor_row, cursor_column = format_inline_input_render(
            text,
            cursor,
            prompt_text=prompt_text,
            prompt_width=prompt_width,
            terminal_width=max(1, terminal_width),
            terminal_height=max(3, terminal_height),
        )
        return cursor_column

    def _supports_completion_menu() -> bool:
        terminal_width = shutil.get_terminal_size((100, 30)).columns
        content_width = max(1, terminal_width - prompt_width - 1)
        return "\n" not in text and len(_split_inline_input_rows_with_offsets(text, content_width)) == 1

    def _completion_menu_active() -> bool:
        return _supports_completion_menu() and bool(matches)

    def _redraw_input() -> None:
        nonlocal rendered_input_lines, rendered_cursor_row

        old_rows_below_cursor = rendered_input_lines - 1 - rendered_cursor_row
        menu.clear(
            cursor_column=_cursor_column(),
            rows_below_cursor=old_rows_below_cursor,
        )
        terminal_width, terminal_height = shutil.get_terminal_size((100, 30))
        rendered_lines, cursor_row, cursor_col = format_inline_input_render(
            text,
            cursor,
            prompt_text=prompt_text,
            prompt_width=prompt_width,
            terminal_width=max(1, terminal_width),
            terminal_height=max(3, terminal_height),
        )
        parts: list[str] = []
        line_delta = len(rendered_lines) - rendered_input_lines
        if line_delta > 0:
            # 输入块变高时先在底部下面插入真实终端行，避免新行覆盖模型状态行。
            rows_to_after_old_block = rendered_input_lines - rendered_cursor_row
            if rows_to_after_old_block > 0:
                parts.append(f"\033[{rows_to_after_old_block}B")
            parts.append(f"\033[{line_delta}L")
            if rows_to_after_old_block > 0:
                parts.append(f"\033[{rows_to_after_old_block}A")
        if rendered_cursor_row > 0:
            parts.append(f"\033[{rendered_cursor_row}A")
        for index in range(rendered_input_lines):
            parts.append("\r\033[2K")
            if index < rendered_input_lines - 1:
                parts.append("\033[1B")
        if rendered_input_lines > 1:
            parts.append(f"\033[{rendered_input_lines - 1}A")
        for index, line in enumerate(rendered_lines):
            if index > 0:
                parts.append("\n")
            parts.append(line)
            # 删除字符后新行可能比旧行短；显式清掉行尾，避免终端没及时擦除
            # 旧字符，表现成“删不掉，继续输入才覆盖”。
            parts.append(INLINE_CLEAR_TO_LINE_END)
        rows_below_cursor = len(rendered_lines) - 1 - cursor_row
        if rows_below_cursor > 0:
            parts.append(f"\033[{rows_below_cursor}A")
        if line_delta < 0:
            rows_to_after_new_block = len(rendered_lines) - cursor_row
            if rows_to_after_new_block > 0:
                parts.append(f"\033[{rows_to_after_new_block}B")
            parts.append(f"\033[{-line_delta}M")
            if rows_to_after_new_block > 0:
                parts.append(f"\033[{rows_to_after_new_block}A")
        parts.append(f"\033[{max(1, cursor_col)}G")
        with ui._lock:
            print("".join(parts), end="", flush=True)
        rendered_input_lines = len(rendered_lines)
        rendered_cursor_row = cursor_row

    def _clear_input_area_for_submit() -> None:
        rows_below_cursor = rendered_input_lines - 1 - rendered_cursor_row
        if rows_below_cursor > 0:
            print(f"\033[{rows_below_cursor}B", end="", flush=True)
        print()

    def _render_menu() -> None:
        rows_below_cursor = rendered_input_lines - 1 - rendered_cursor_row
        if not _supports_completion_menu():
            menu.render(
                matches=[],
                selected_index=0,
                cursor_column=_cursor_column(),
                rows_below_cursor=rows_below_cursor,
            )
            return
        menu.render(
            matches=matches,
            selected_index=match_index,
            cursor_column=_cursor_column(),
            rows_below_cursor=rows_below_cursor,
        )

    def _update_matches(*, reset_selection: bool = True) -> None:
        nonlocal matches, match_index
        if not _supports_completion_menu():
            matches = []
            match_index = 0
            _render_menu()
            return
        matches = _slash_command_matches(text, commands)
        if reset_selection:
            match_index = 0
        elif matches:
            match_index = max(0, min(match_index, len(matches) - 1))
        else:
            match_index = 0
        _render_menu()

    def _hide_matches() -> None:
        nonlocal matches, match_index
        matches = []
        match_index = 0
        _render_menu()

    def _move_selection(delta: int) -> None:
        nonlocal match_index
        if not matches:
            return
        match_index = (match_index + delta) % len(matches)
        _render_menu()

    def _move_cursor(delta: int) -> None:
        nonlocal cursor
        if delta < 0:
            for _ in range(-delta):
                cursor = _previous_display_unit_offset(text, cursor)
        else:
            for _ in range(delta):
                cursor = _next_display_unit_offset(text, cursor)
        _redraw_input()
        _render_menu()

    def _move_cursor_vertical(delta: int) -> bool:
        nonlocal cursor
        (
            rows,
            _visible_rows,
            _start,
            current_row,
            _visible_cursor_row,
            current_col,
            _window_height,
            _terminal_width,
            _terminal_height,
        ) = _input_layout()
        target_row = max(0, min(len(rows) - 1, current_row + delta))
        if target_row == current_row:
            return False
        target_text, target_start, _target_end = rows[target_row]
        target_prefix = _terminal_take_display_width(target_text, current_col)
        cursor = target_start + len(target_prefix)
        _redraw_input()
        _render_menu()
        return True

    def _replace_input_text(next_text: str) -> None:
        nonlocal text, cursor
        text = next_text
        cursor = len(text)
        _redraw_input()
        _update_matches()

    def _show_previous_history() -> None:
        previous_text = history_browser.previous(text)
        if previous_text is not None:
            _replace_input_text(previous_text)

    def _show_next_history() -> None:
        next_text = history_browser.next()
        if next_text is not None:
            _replace_input_text(next_text)

    def _handle_up_key() -> None:
        if _completion_menu_active():
            _move_selection(-1)
            return
        if history_browser.is_browsing:
            _show_previous_history()
            return
        if not _move_cursor_vertical(-1):
            _show_previous_history()

    def _handle_down_key() -> None:
        if _completion_menu_active():
            _move_selection(1)
            return
        if history_browser.is_browsing:
            _show_next_history()
            return
        _move_cursor_vertical(1)

    def _handle_navigation_key(key: str) -> None:
        nonlocal text, cursor
        if key == "H":
            _handle_up_key()
            return
        if key == "P":
            _handle_down_key()
            return
        if key == "K":
            _move_cursor(-1)
            return
        if key == "M":
            _move_cursor(1)
            return
        if key == "G":
            cursor = 0
            _redraw_input()
            _render_menu()
            return
        if key == "O":
            cursor = len(text)
            _redraw_input()
            _render_menu()
            return
        if key == "S":
            _delete_current_char()

    def _insert_text(inserted: str) -> None:
        nonlocal text, cursor
        if not inserted:
            return
        history_browser.reset()
        normalized = _normalize_inline_pasted_text(inserted)
        # 正常导航不会落入组合字符内部；这里仍保守归一化，避免外部调用或
        # 历史文本导致插入把一个显示单元拆成两半。
        unit = _display_unit_at_or_after(text, cursor)
        if unit is not None and unit[0] < cursor < unit[1]:
            cursor = unit[0]
        text = text[:cursor] + normalized + text[cursor:]
        cursor += len(normalized)
        _redraw_input()
        _update_matches()

    def _delete_previous_char() -> None:
        nonlocal text, cursor
        if cursor <= 0:
            return
        history_browser.reset()
        start = _previous_display_unit_offset(text, cursor)
        text = _delete_display_unit_before(text, cursor)
        cursor = start
        _redraw_input()
        _update_matches()

    def _delete_current_char() -> None:
        nonlocal text, cursor
        if cursor >= len(text):
            return
        history_browser.reset()
        unit = _display_unit_at_or_after(text, cursor)
        if unit is None:
            return
        cursor = unit[0]
        text = _delete_display_unit_after(text, cursor)
        _redraw_input()
        _update_matches()

    def _has_queued_input(timeout_seconds: float = 0.0) -> bool:
        deadline = time.monotonic() + max(0.0, timeout_seconds)
        while True:
            if msvcrt.kbhit():
                return True
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.001)

    def _is_paste_burst_active() -> bool:
        return time.monotonic() <= paste_burst_until

    def _mark_paste_burst_if_more_input() -> None:
        nonlocal paste_burst_until
        if _has_queued_input() or _is_paste_burst_active():
            paste_burst_until = time.monotonic() + INLINE_PASTE_BURST_QUIET_SECONDS

    def _extend_active_paste_burst() -> None:
        nonlocal paste_burst_until
        if _is_paste_burst_active():
            paste_burst_until = time.monotonic() + INLINE_PASTE_BURST_QUIET_SECONDS

    def _consume_pasted_newline(char: str) -> bool:
        nonlocal skip_next_lf_after_cr_paste
        if char == "\n" and skip_next_lf_after_cr_paste:
            skip_next_lf_after_cr_paste = False
            return True

        if not _is_paste_burst_active() and not _has_queued_input(
            INLINE_PASTE_BURST_QUIET_SECONDS
        ):
            return False

        _insert_text("\n")
        skip_next_lf_after_cr_paste = char == "\r"
        _mark_paste_burst_if_more_input()
        return True

    def _restore_bracketed_paste() -> None:
        print(INLINE_BRACKETED_PASTE_OFF, end="", flush=True)

    _render_menu()

    while True:
        # 输入等待期间不再做定时状态行刷新；所有重绘都由真实按键事件触发。
        # 这样删除、光标移动和状态行不会在同一时间竞争终端光标位置。
        char = msvcrt.getwch()

        if pending_high_surrogate:
            combined = _terminal_combine_surrogate_pair(pending_high_surrogate, char)
            if combined is not None:
                pending_high_surrogate = ""
                _insert_text(combined)
                _extend_active_paste_burst()
                continue
            _insert_text("�")
            pending_high_surrogate = ""
        if 0xD800 <= ord(char) <= 0xDBFF:
            pending_high_surrogate = char
            continue

        if char in {"\r", "\n"} and _consume_pasted_newline(char):
            continue

        if char in {"\r", "\n"}:
            menu.clear(
                cursor_column=_cursor_column(),
                rows_below_cursor=rendered_input_lines - 1 - rendered_cursor_row,
            )
            _restore_bracketed_paste()
            _clear_input_area_for_submit()
            _append_inline_input_history(input_history, text)
            return text if text.strip() else ""

        if char == "\t":
            if _supports_completion_menu() and matches:
                history_browser.reset()
                text = matches[match_index]
                cursor = len(text)
                _redraw_input()
                _hide_matches()
            else:
                _insert_text("    ")
            continue

        if char == "\x1b":
            seq_parts: list[str] = []
            import time as _time

            deadline = _time.monotonic() + 0.02
            while _time.monotonic() < deadline:
                if msvcrt.kbhit():
                    seq_parts.append(msvcrt.getwch())
                    if _is_inline_escape_sequence_complete("".join(seq_parts)):
                        break
                else:
                    _time.sleep(0.001)
            sequence = "".join(seq_parts)

            if not sequence:
                _hide_matches()
                continue
            if sequence in {"[A", "OA"}:
                _handle_up_key()
                continue
            if sequence in {"[B", "OB"}:
                _handle_down_key()
                continue
            if sequence in {"[D", "OD"}:
                _move_cursor(-1)
                continue
            if sequence in {"[C", "OC"}:
                _move_cursor(1)
                continue
            if sequence in {"[H", "[1~"}:
                cursor = 0
                _redraw_input()
                _render_menu()
                continue
            if sequence in {"[F", "[4~"}:
                cursor = len(text)
                _redraw_input()
                _render_menu()
                continue
            if sequence == "[3~":
                _delete_current_char()
                continue
            if sequence == "[200~":
                _insert_text(_read_inline_bracketed_paste())
                continue
            continue

        if char in {"\x00", "\xe0"}:
            key_code = msvcrt.getwch()
            if key_code == "H":
                _handle_up_key()
                continue
            if key_code == "P":
                _handle_down_key()
                continue
            _handle_navigation_key(key_code)
            continue

        if char == "\x03":
            _restore_bracketed_paste()
            raise KeyboardInterrupt

        if char in {"\b", "\x7f"}:
            # Windows 控制台通常把退格返回为 \b；部分终端/键盘映射会返回 DEL。
            # Delete 键仍通过 ESC[3~ 或扩展键 S 走“删除光标处字符”的分支。
            _delete_previous_char()
            continue

        if char.isprintable() or char.isspace():
            _insert_text(char)
            _extend_active_paste_burst()


# --- former module: chat_session.py ---

import os

from ..agent import AgentError, LocalToolAgent
from .inline_input import read_line_autocomplete
from ..commands.slash import (
    build_slash_commands,
    format_tool_confirmation,
    handle_model_command,
    handle_approval_command,
    handle_reasoning_command,
    handle_session_command,
    print_memory_clean_result,
    print_mcp_status,
    print_skills_list,
)
from .terminal import USER_PREFIX, InputBar, StatusLine, TerminalUI, WaitingIndicator


EXIT_WORDS = {"退出", "结束", "再见"}
NEW_CHAT_COMMAND = "/new"


def _get_user_text(
    ui: TerminalUI,
    slash_commands: list[str] | None = None,
    history: list[str] | None = None,
) -> str:
    """读取一轮用户输入，支持斜杠命令 Tab 补全（Windows 下）。"""

    if os.name == "nt" and slash_commands:
        try:
            import msvcrt
        except ImportError:
            pass
        else:
            return read_line_autocomplete(
                ui.prompt(),
                slash_commands,
                ui,
                history=history,
            ).strip()
    return input(ui.prompt()).strip()


def run_inline_chat(
    agent: LocalToolAgent,
    ui: TerminalUI,
) -> None:
    """运行默认的普通终端内联 UI。"""

    agent.set_confirm_handler(
        lambda tool_name, arguments: ui.prompt_yes_no(
            format_tool_confirmation(tool_name, arguments),
            confirmed_label="",
        )
    )
    pending_user_text: str | None = None
    input_interrupt_count = 0
    try:
        input_history = agent.prompt_history_texts(limit=100)
    except AgentError:
        input_history = []
    while True:
        try:
            if pending_user_text is not None:
                user_text = pending_user_text
                pending_user_text = None
            else:
                user_text = _get_user_text(
                    ui,
                    build_slash_commands(agent),
                    history=input_history,
                )
        except KeyboardInterrupt:
            input_interrupt_count += 1
            if input_interrupt_count >= 2:
                print("\n对话结束。")
                break
            ui.notice("\n已取消输入，再按一次 Ctrl+C 退出。")
            continue

        input_interrupt_count = 0
        if not user_text:
            continue

        if user_text.strip().lower() in EXIT_WORDS:
            print("对话结束。")
            break

        if user_text.strip() == NEW_CHAT_COMMAND:
            agent.reset_conversation()
            ui.notice("已开启新对话。")
            continue

        if user_text.strip() == "/skills":
            print_skills_list(agent)
            continue

        if user_text.strip() == "/memory:clean":
            print_memory_clean_result(agent)
            continue

        if user_text.strip() == "/workspace" or user_text.strip().startswith("/workspace "):
            parts = user_text.strip().split(None, 1)
            if len(parts) == 1 or not parts[1].strip():
                print(f"当前工作区：{agent.workspace_root}")
                print("用法：/workspace <新工作区路径>")
                continue
            try:
                new_root = agent.switch_workspace(parts[1].strip())
                print(f"已切换到工作区：{new_root}")
            except AgentError as exc:
                print(f"工作区切换失败：{exc}")
            continue
        if user_text.strip() == "/mcp":
            print_mcp_status(agent)
            continue

        session_message = handle_session_command(agent, user_text)
        if session_message is not None:
            print(session_message)
            continue

        before_model = agent.current_model
        model_message = handle_model_command(agent, user_text)
        if model_message is not None:
            if agent.current_model != before_model:
                ui.set_model_label(agent.current_model)
            ui.notice(model_message)
            continue

        approval_message = handle_approval_command(agent, user_text)
        if approval_message is not None:
            ui.notice(approval_message)
            continue

        reasoning_message = handle_reasoning_command(agent, user_text)
        if reasoning_message is not None:
            ui.notice(reasoning_message)
            continue

        ui.inline_turn_base(user_text)
        status_line = StatusLine(ui)
        input_bar = InputBar(ui)
        waiting_indicator = WaitingIndicator(status_line, input_bar=input_bar)

        # 流式输出状态：管理 markdown 增量渲染和首次输出标记
        _has_display_output = False
        from .terminal import MarkdownStreamState
        _markdown_state = MarkdownStreamState()

        def _collect_pre_input() -> None:
            """停止 spinner 并收集预输入到 pending_user_text。"""
            nonlocal pending_user_text
            pre = waiting_indicator.stop()
            if pre and pending_user_text is None:
                pending_user_text = pre

        def handle_delta(delta: str) -> None:
            """流式增量文本渲染回调，直接写入终端 Markdown。"""
            nonlocal _has_display_output
            if not _has_display_output:
                _collect_pre_input()
                status_line.clear()
                input_bar.push_up()
                ui.newline()
                ui.print_ai_prefix()
                _has_display_output = True
            input_bar.push_up()
            ui.write_markdown_delta(delta, _markdown_state)
            input_bar.pop_down()

        def _flush_display() -> None:
            """提交当前 Markdown 预览到终端，不触发额外操作。"""
            input_bar.push_up()
            ui.flush_markdown(_markdown_state)
            input_bar.pop_down()

        def handle_agent_status(message: str) -> None:
            if message:
                _flush_display()
                _collect_pre_input()
                input_bar.push_up()
                ui.status(message, leading_blank=_has_display_output)
                input_bar.pop_down()
            else:
                if _has_display_output:
                    _flush_display()
                    _markdown_state = MarkdownStreamState()
                    _has_display_output = False
                waiting_indicator.start()

        def handle_retry_status(message: str) -> None:
            """流式连接可恢复中断时，用弱提示说明自动重试。"""
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.status(message, leading_blank=_has_display_output, italic=True)
            input_bar.pop_down()

        def handle_tool_start(step: int, tool_call) -> None:
            """工具开始执行时立即展示调用详情和运行态标记。"""
            had_output = _has_display_output
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.print_tool_call_start(
                step,
                tool_call.name,
                tool_call.arguments,
                leading_blank=had_output,
            )
            input_bar.pop_down()

        def handle_tool_result(_tool_call, result) -> None:
            """工具执行完成时先收起等待动画，再输出执行摘要。"""
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.print_tool_result_record(
                result.ok,
                result.output,
                tool_name=_tool_call.name,
            )
            input_bar.pop_down()

        def handle_protocol_wait() -> None:
            """模型已显示进度、正在继续输出隐藏工具协议时恢复等待动画。"""
            _flush_display()
            waiting_indicator.start()

        try:
            waiting_indicator.start()
            agent.run_stream(
                user_text,
                handle_delta,
                on_status=handle_agent_status,
                on_tool_start=handle_tool_start,
                on_tool_result=handle_tool_result,
                on_token_usage=ui.update_token_usage,
                on_protocol_wait=handle_protocol_wait,
                on_retry_status=handle_retry_status,
            )
            _collect_pre_input()
            input_bar.push_up()
            _flush_display()
            ui.newline()
            input_bar.pop_down()
            input_bar.clear()
        except KeyboardInterrupt:
            _collect_pre_input()
            input_bar.push_up()
            ui.newline()
            input_bar.pop_down()
            input_bar.clear()
            ui.notice("已取消当前操作。")
            continue
        except AgentError as exc:
            _collect_pre_input()
            input_bar.push_up()
            message = str(exc).strip() or "Agent 请求失败，请检查配置或稍后重试。"
            print(message if message.startswith("Agent ") else f"Agent 请求失败：{message}")
            input_bar.pop_down()
            input_bar.clear()
            continue
