"""UI 子系统合并入口。

TUI/Qt 会话外壳、配置、启动器和基础 UI 类型集中到这里，减少代码文件数量。
模块别名会在导入时注册，兼容 omnicrawl.ui.config/chat_session 等旧路径。
Qt 具体窗口实现和 TUI 具体渲染实现仍保留在子包中。
"""

from __future__ import annotations

import sys as _sys

_THIS_MODULE = _sys.modules[__name__]
_UI_MODULE_ALIASES = (
    'base',
    'terminal',
    'config',
    'windows_launcher',
    'inline_input',
    'chat_session',
    'qt_chat_session',
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
    ) -> Any:
        """返回实现相关的 display_state 对象。"""
        ...

    @abc.abstractmethod
    def print_tool_result_record(
        self,
        ok: bool,
        output: str | None = None,
        *,
        tool_name: str = "",
        display_state: Any | None = None,
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

    @abc.abstractmethod
    def prompt_status_line(self) -> str: ...

    @abc.abstractmethod
    def print_prompt_status(self, cursor_column: int = 0) -> None: ...

    @abc.abstractmethod
    def clear_prompt_status(self) -> None: ...


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


# --- former module: __init__.py ---
"""UI 前端工厂 — 根据 config 中的 frontend.type 创建对应的 UI 实例。"""


from .base import BaseUI, UIStartupError

__all__ = ["BaseUI", "UIStartupError", "create_ui"]


def create_ui(
    *,
    model_label: str | None = None,
    frontend_type: str = "tui",
) -> BaseUI:
    """根据 frontend_type 创建 UI 实例。

    Parameters
    ----------
    model_label : str | None
        模型名称标签，显示在状态行。
    frontend_type : str
        "tui" — 终端 UI（默认）；"qt" — Fluent Design 桌面 GUI。
    """
    if frontend_type == "qt":
        try:
            from .qt import QtUI
        except ModuleNotFoundError as exc:
            if _is_missing_qt_dependency(exc):
                raise UIStartupError(_qt_dependency_error_message()) from exc
            raise

        return QtUI(model_label=model_label)

    if frontend_type == "tui":
        from .terminal import TerminalUI

        return TerminalUI(model_label=model_label)

    raise ValueError(f"未知前端类型：{frontend_type!r}，可选值：tui、qt")


def _is_missing_qt_dependency(exc: ModuleNotFoundError) -> bool:
    """识别 Qt GUI 依赖缺失，避免把内部业务模块导入错误误报成安装问题。"""

    missing_name = getattr(exc, "name", "") or ""
    message = str(exc)
    return (
        missing_name.startswith("PyQt5")
        or missing_name.startswith("PyQtWebEngine")
        or "PyQt5" in message
        or "PyQtWebEngine" in message
    )


def _qt_dependency_error_message() -> str:
    """返回用户可直接执行的 Qt 依赖修复说明。"""

    return (
        "Qt GUI 依赖未安装完整。请在当前 Python 环境执行："
        "python -m pip install -r requirements.txt。"
        "如果只补本次缺失包，请执行：python -m pip install \"PyQtWebEngine>=5.15.0\"。"
        "在 PowerShell 中版本约束必须加引号，否则 >= 会被当作重定向符号。"
    )


# --- former module: config.py ---
"""前端 UI 配置加载。"""


from dataclasses import dataclass

from ..config.runtime import RuntimeConfigError, get_section, load_config_data


@dataclass
class FrontendConfig:
    """前端 UI 配置。"""

    type: str = "tui"


def load_frontend_config() -> FrontendConfig:
    """从 config.json 加载前端配置。"""
    data = load_config_data()
    section = get_section(data, "frontend")

    frontend_type = section.get("type", "tui")
    if frontend_type not in ("tui", "qt"):
        raise RuntimeConfigError(
            f"frontend.type 可选值为 tui 或 qt，当前值：{frontend_type!r}"
        )

    return FrontendConfig(type=frontend_type)


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
                    chunk = remaining[0]
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
    command_width = max(8, terminal_width - 8)
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
        if not lines and self._ui is not None and self._ui.model_label:
            lines = [self._ui.prompt_status_line()]

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
            terminal_width=max(40, terminal_width),
            terminal_height=max(14, terminal_height),
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
        terminal_width = max(40, terminal_width)
        terminal_height = max(14, terminal_height)
        rendered_lines, cursor_row, cursor_col = format_inline_input_render(
            text,
            cursor,
            prompt_text=prompt_text,
            prompt_width=prompt_width,
            terminal_width=terminal_width,
            terminal_height=terminal_height,
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
        cursor = max(0, min(len(text), cursor + delta))
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
        text = text[:cursor] + normalized + text[cursor:]
        cursor += len(normalized)
        _redraw_input()
        _update_matches()

    def _delete_previous_char() -> None:
        nonlocal text, cursor
        if cursor <= 0:
            return
        history_browser.reset()
        text = text[: cursor - 1] + text[cursor:]
        cursor -= 1
        _redraw_input()
        _update_matches()

    def _delete_current_char() -> None:
        nonlocal text
        if cursor >= len(text):
            return
        history_browser.reset()
        text = text[:cursor] + text[cursor + 1 :]
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

        tool_display_state = None

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
            nonlocal tool_display_state
            had_output = _has_display_output
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            tool_display_state = ui.print_tool_call_start(
                step,
                tool_call.name,
                tool_call.arguments,
                leading_blank=had_output,
            )
            input_bar.pop_down()

        def handle_tool_result(_tool_call, result) -> None:
            """工具执行完成时先收起等待动画，再输出执行摘要。"""
            nonlocal tool_display_state
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.print_tool_result_record(
                result.ok,
                result.output,
                tool_name=_tool_call.name,
                display_state=tool_display_state,
            )
            input_bar.pop_down()
            tool_display_state = None

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


# --- former module: qt_chat_session.py ---
"""Qt GUI 对话主循环 — 完整复刻 TUI 全部功能。"""


import os
import subprocess
import threading
from pathlib import Path
from typing import Callable

from ..agent import AgentError, LocalToolAgent
from ..state.project import ProjectEntry
from ..state.session import COMPACT_SUMMARY_PREFIX, SessionEvent, SessionIndexEntry
from ..commands.slash import (
    build_slash_command_options,
    format_memory_clean_result,
    format_mcp_status,
    format_skills_list,
    format_tool_confirmation,
    handle_approval_command,
    handle_model_command,
    handle_reasoning_command,
    handle_session_command,
)
from ..config.model_catalog import (
    ModelCatalogError,
    detect_model_options,
    ensure_current_model_option,
    model_options_to_ui,
)
from .qt import QtUI


EXIT_WORDS = {"退出", "结束", "再见"}
NEW_CHAT_COMMAND = "/new"


class _QtChatStopped(RuntimeError):
    """Qt 窗口关闭后用于中断本轮后台对话的内部信号。"""


class _QtChatCancelled(RuntimeError):
    """用户点击停止按钮后用于中断当前生成的内部信号。"""


def open_project_in_file_manager(project_path: str) -> None:
    """用当前系统的文件管理器打开项目目录。"""

    path = Path(project_path).expanduser().resolve(strict=False)
    if not path.exists():
        raise AgentError(f"路径不存在：{project_path}")
    if not path.is_dir():
        raise AgentError(f"路径不是目录：{project_path}")
    if os.name == "nt":
        os.startfile(str(path))  # type: ignore[attr-defined]
        return
    command = ["open", str(path)] if os.sys.platform == "darwin" else ["xdg-open", str(path)]
    try:
        subprocess.Popen(command)
    except OSError as exc:
        raise AgentError(str(exc)) from exc


def _session_entry_to_ui(entry: SessionIndexEntry, current_session_id: str) -> dict[str, object]:
    """把会话索引转换为 Qt Web 前端使用的轻量 JSON。

    Qt 前端只需要渲染列表和高亮当前会话，不直接读取 `index.json`。
    后端在这里裁剪字段，可以避免把本地完整路径等不必要细节暴露给 UI。
    """

    return {
        "id": entry.session_id,
        "title": entry.title or "未命名会话",
        "updatedAt": entry.updated_at.astimezone().strftime("%Y-%m-%d %H:%M"),
        "messageCount": entry.message_count,
        "current": entry.session_id == current_session_id,
    }


def _project_session_entry_to_ui(entry: SessionIndexEntry, current_session_id: str) -> dict[str, object]:
    """把会话索引转换为项目侧栏中的嵌套会话条目。"""

    return {
        "id": entry.session_id,
        "title": entry.title or "未命名会话",
        "timeAgo": entry.updated_at.astimezone().strftime("%Y-%m-%d %H:%M"),
        "messageCount": entry.message_count,
        "current": entry.session_id == current_session_id,
    }


def _project_entry_to_ui(
    agent: LocalToolAgent,
    entry: ProjectEntry,
    current_session_id: str,
) -> dict[str, object]:
    """把项目记录转换为 Qt Web 前端项目侧栏使用的 JSON。

    项目记录来自 `.agent_sessions/projects.json`，嵌套会话实时按项目
    路径从 SessionStore 过滤，避免项目列表和会话索引保存两份归属关系。
    """

    sessions = agent.list_sessions(limit=20, project_path=entry.path)
    return {
        "name": entry.name,
        "path": entry.path,
        "pinned": entry.pinned,
        "current": Path(entry.path).resolve() == agent.workspace_root.resolve(),
        "sessions": [
            _project_session_entry_to_ui(session, current_session_id)
            for session in sessions
        ],
    }


def _session_events_to_ui(
    events: list[SessionEvent],
    read_html_artifact: Callable[[str, str], str] | None = None,
) -> list[dict[str, object]]:
    """把 JSONL 事件流转换为 Qt 可回放的消息列表。"""

    messages: list[dict[str, object]] = []
    tool_step = 1
    for event in events:
        payload = event.payload
        created_at = event.created_at.astimezone().strftime("%H:%M")
        if event.type == "user_message":
            content = payload.get("content", "")
            if isinstance(content, str) and content.strip():
                messages.append({"type": "user", "content": content, "time": created_at})
        elif event.type == "assistant_message":
            content = payload.get("content", "")
            if isinstance(content, str) and content.strip():
                messages.append({"type": "assistant", "content": content, "time": created_at})
        elif event.type == "compact_summary":
            content = payload.get("content", "")
            if isinstance(content, str) and content.strip():
                messages.append(
                    {
                        "type": "assistant",
                        "content": f"{COMPACT_SUMMARY_PREFIX}{content}",
                        "time": created_at,
                    }
                )
        elif event.type == "tool_call_requested":
            tool = payload.get("tool", "")
            arguments = payload.get("arguments", {})
            if isinstance(tool, str) and tool.strip():
                messages.append(
                    {
                        "type": "tool_start",
                        "step": tool_step,
                        "tool": tool,
                        "arguments": arguments if isinstance(arguments, dict) else {},
                    }
                )
                tool_step += 1
        elif event.type == "tool_result":
            tool = payload.get("tool", "")
            output = payload.get("model_output")
            if not isinstance(output, str) or not output.strip():
                output = payload.get("output_preview")
            if not isinstance(output, str) or not output.strip():
                output = payload.get("output", "")
            artifact_path = payload.get("artifact_path", "")
            if isinstance(artifact_path, str) and artifact_path.strip():
                artifact_hint = f"\n完整输出 artifact：{artifact_path.strip()}"
                output = f"{output}{artifact_hint}" if isinstance(output, str) else artifact_hint.strip()
            ui_artifact = payload.get("ui_artifact", {})
            if isinstance(ui_artifact, dict):
                ui_artifact = _hydrate_html_ui_artifact(
                    event.session_id,
                    ui_artifact,
                    read_html_artifact,
                )
            ok = payload.get("ok", False)
            if isinstance(tool, str) and isinstance(output, str):
                messages.append(
                    {
                        "type": "tool_result",
                        "tool": tool,
                        "ok": bool(ok),
                        "output": output,
                        "uiArtifact": ui_artifact if isinstance(ui_artifact, dict) else {},
                    }
                )
    return messages


def _hydrate_html_ui_artifact(
    session_id: str,
    ui_artifact: dict[str, object],
    read_html_artifact: Callable[[str, str], str] | None,
) -> dict[str, object]:
    """为历史回放补回 HTML artifact 原文。

    新生成时前端会直接拿到 `html`，但写入 JSONL 时为了避免单行过大只保留
    `artifact_path`。恢复历史会话时需要重新读取该文件，否则右侧显示区只能
    显示路径提示，用户还要手动打开 artifact。
    """

    if ui_artifact.get("type") != "html" or isinstance(ui_artifact.get("html"), str):
        return ui_artifact
    artifact_path = ui_artifact.get("artifact_path")
    if not isinstance(artifact_path, str) or not artifact_path.strip() or read_html_artifact is None:
        return ui_artifact
    try:
        html = read_html_artifact(session_id, artifact_path.strip())
    except Exception:
        return ui_artifact
    if not html.strip():
        return ui_artifact
    hydrated = dict(ui_artifact)
    hydrated["html"] = html
    return hydrated


def run_qt_chat(
    agent: LocalToolAgent,
    ui: QtUI,
    stop_event: threading.Event | None = None,
    cancel_event: threading.Event | None = None,
) -> None:
    """运行 Qt GUI 对话循环，完整复刻 TUI 功能。"""

    from .terminal import MarkdownStreamState

    stop_event = stop_event or threading.Event()
    cancel_event = cancel_event or threading.Event()

    def stop_requested() -> bool:
        return stop_event.is_set() or ui.is_closed()

    def cancel_requested() -> bool:
        return cancel_event.is_set() or stop_requested()

    def raise_if_stopped() -> None:
        if stop_requested():
            raise _QtChatStopped()
        if cancel_event.is_set():
            raise _QtChatCancelled()

    def raise_if_cancelled() -> None:
        if cancel_event.is_set():
            raise _QtChatCancelled()

    def confirm_tool_call(tool_name, arguments) -> bool:
        raise_if_stopped()
        approved = ui.prompt_yes_no(
            format_tool_confirmation(tool_name, arguments),
            confirmed_label="",
        )
        raise_if_stopped()
        return approved

    agent.set_confirm_handler(confirm_tool_call)

    pending_user_text: str | None = None

    def refresh_session_list() -> None:
        """刷新 Qt 侧边栏会话列表和当前会话标题。"""

        try:
            entries = agent.list_sessions(limit=20)
        except AgentError as exc:
            ui.show_session_list_error(str(exc))
            return

        current_id = agent.current_session_id
        ui.update_session_list([_session_entry_to_ui(entry, current_id) for entry in entries])
        current_entry = next((entry for entry in entries if entry.session_id == current_id), None)
        if current_entry is not None:
            ui.set_current_session(current_entry.session_id, current_entry.title or "未命名会话")

    def refresh_project_list() -> None:
        """刷新 Qt 项目列表，并把会话按项目路径分组。"""

        try:
            projects = agent.list_projects()
            current_id = agent.current_session_id
            ui.update_project_list(
                [_project_entry_to_ui(agent, project, current_id) for project in projects]
            )
            ui.set_current_project(str(agent.workspace_root.resolve()))
        except AgentError as exc:
            ui.notice(f"项目列表刷新失败：{exc}")

    def render_session(session_id: str) -> None:
        """从 JSONL 事件流重建 Qt 消息区。"""

        events = agent.load_session_events(session_id)
        ui.render_session_messages(
            _session_events_to_ui(
                events,
                read_html_artifact=agent.read_session_artifact_text,
            )
        )

    def resume_session_for_qt(session_id: str) -> None:
        """恢复会话并同步 Qt 消息列表、标题和侧边栏高亮。"""

        state = agent.resume_session(session_id)
        render_session(state.session_id)
        ui.set_current_session(state.session_id, state.title or "未命名会话")
        refresh_session_list()
        refresh_project_list()
        ui.notice(f"已恢复会话：{state.title or state.session_id}")

    def parse_project_command_payload(text: str) -> tuple[str, str]:
        """解析窗口层传来的 `left|right` 控制参数。"""

        payload = text.split(None, 1)[1] if " " in text else ""
        left, separator, right = payload.partition("|")
        if not separator:
            return payload.strip(), ""
        return left.strip(), right.strip()

    def handle_session_control_text(user_text: str) -> bool:
        """处理 Qt 会话控制指令，避免它们进入模型请求。"""

        text = user_text.strip()
        normalized = text.lower()
        if text == "__REFRESH_SESSIONS__":
            refresh_session_list()
            return True

        if text == "__REFRESH_PROJECTS__":
            refresh_project_list()
            return True

        if text.startswith("__CREATE_PROJECT__ "):
            name, path = parse_project_command_payload(text)
            try:
                project = agent.create_project(name, path)
            except AgentError as exc:
                ui.notice(f"项目创建失败：{exc}")
                return True
            refresh_project_list()
            ui.notice(f"已创建项目：{project.name}")
            return True

        if text.startswith("__IMPORT_PROJECT__ "):
            name, path = parse_project_command_payload(text)
            try:
                project = agent.import_project(name, path)
            except AgentError as exc:
                ui.notice(f"项目导入失败：{exc}")
                return True
            refresh_project_list()
            ui.notice(f"已导入项目：{project.name}")
            return True

        if text.startswith("__PIN_PROJECT__ "):
            project_path = text.split(None, 1)[1].strip()
            try:
                project = agent.toggle_project_pin(project_path)
            except AgentError as exc:
                ui.notice(f"项目置顶状态更新失败：{exc}")
                return True
            refresh_project_list()
            action = "已置顶" if project.pinned else "已取消置顶"
            ui.notice(f"{action}项目：{project.name}")
            return True

        if text.startswith("__RENAME_PROJECT__ "):
            project_path, name = parse_project_command_payload(text)
            try:
                project = agent.rename_project(project_path, name)
            except AgentError as exc:
                ui.notice(f"项目重命名失败：{exc}")
                return True
            refresh_project_list()
            ui.notice(f"项目已重命名为：{project.name}")
            return True

        if text.startswith("__REMOVE_PROJECT__ "):
            project_path = text.split(None, 1)[1].strip()
            try:
                agent.remove_project(project_path)
            except AgentError as exc:
                ui.notice(f"项目移除失败：{exc}")
                return True
            refresh_project_list()
            ui.notice("已从列表移除项目。")
            return True

        if text.startswith("__SWITCH_PROJECT__ "):
            project_path = text.split(None, 1)[1].strip()
            try:
                new_root = agent.switch_workspace(project_path)
            except AgentError as exc:
                ui.notice(f"工作区切换失败：{exc}")
                return True
            ui.notice(f"已切换到工作区：{new_root}")
            ui.render_session_messages([])
            ui.status("")
            ui.set_input_enabled(True)
            ui.set_input_placeholder("输入消息，Enter 发送 · 退出词结束对话")
            refresh_session_list()
            refresh_project_list()
            ui.update_slash_commands(build_slash_command_options(agent))
            return True
        if text.startswith("__OPEN_PROJECT__ "):
            project_path = text.split(None, 1)[1].strip()
            ui.notice(f"项目已在列表中：{project_path}。如需切换工作区请使用项目侧栏。")
            return True

        if text.startswith("__OPEN_IN_EXPLORER__ "):
            project_path = text.split(None, 1)[1].strip()
            try:
                open_project_in_file_manager(project_path)
            except AgentError as exc:
                ui.notice(f"打开项目目录失败：{exc}")
                return True
            ui.notice(f"已打开项目目录：{project_path}")
            return True

        if text.startswith("__RESUME_SESSION__ "):
            session_id = text.split(None, 1)[1].strip()
            try:
                resume_session_for_qt(session_id)
            except AgentError as exc:
                ui.show_session_list_error(str(exc))
            return True

        if text.startswith("__RENAME_SESSION__ "):
            title = text.split(None, 1)[1].strip()
            try:
                state = agent.rename_current_session(title)
            except AgentError as exc:
                ui.notice(f"会话重命名失败：{exc}")
                return True
            ui.set_current_session(state.session_id, state.title or "未命名会话")
            refresh_session_list()
            refresh_project_list()
            ui.notice(f"当前会话已重命名为：{state.title}")
            return True

        if text.startswith("__DELETE_SESSION__ "):
            session_id = text.split(None, 1)[1].strip()
            try:
                agent.delete_session(session_id)
            except AgentError as exc:
                ui.notice(f"会话删除失败：{exc}")
                return True
            refresh_session_list()
            refresh_project_list()
            ui.notice(f"已删除会话：{session_id}")
            return True

        if normalized == NEW_CHAT_COMMAND:
            agent.reset_conversation()
            ui.render_session_messages([])
            refresh_session_list()
            refresh_project_list()
            ui.notice("已开启新对话。")
            return True

        if normalized == "/resume" or normalized.startswith("/resume "):
            parts = text.split(None, 1)
            if len(parts) == 1 or not parts[1].strip():
                ui.notice("用法：/resume <session_id>。可先用 /sessions 查看最近会话。")
                return True
            try:
                resume_session_for_qt(parts[1].strip())
            except AgentError as exc:
                ui.notice(f"会话恢复失败：{exc}")
            return True

        if normalized == "/rename" or normalized.startswith("/rename "):
            message = handle_session_command(agent, text)
            if message is None:
                return False
            refresh_session_list()
            refresh_project_list()
            ui.notice(message)
            return True

        if normalized == "/compact":
            message = handle_session_command(agent, text)
            if message is None:
                return False
            refresh_session_list()
            refresh_project_list()
            ui.notice(message)
            return True

        if normalized == "/sessions":
            refresh_session_list()
            message = handle_session_command(agent, text)
            if message is not None:
                ui.write(message)
                ui.flush_markdown(None)
            return True

        if normalized == "/archive" or normalized == "/archives":
            message = handle_session_command(agent, text)
            if message is not None:
                if normalized == "/archive":
                    ui.render_session_messages([])
                    refresh_session_list()
                    refresh_project_list()
                    ui.notice(message)
                else:
                    ui.write(message)
                    ui.flush_markdown(None)
            return True

        return False

    def handle_model_control_text(user_text: str) -> bool:
        """处理 Qt 前端产生的模型控制指令，避免它们进入聊天请求。"""

        if user_text.strip() == "__REFRESH_MODELS__":
            try:
                model_options = ensure_current_model_option(
                    detect_model_options(agent.config.llm),
                    agent.current_model,
                )
                ui.update_model_list(model_options_to_ui(model_options), agent.current_model)
            except ModelCatalogError as exc:
                ui.show_model_list_error(str(exc))
            return True

        reasoning_message = handle_reasoning_command(agent, user_text)
        if reasoning_message is not None:
            ui.notice(reasoning_message)
            return True

        before_model = agent.current_model
        model_message = handle_model_command(agent, user_text)
        if model_message is None:
            return False

        if agent.current_model != before_model:
            ui.set_model_label(agent.current_model)
            ui.set_current_model(agent.current_model)
        else:
            ui.set_current_model(agent.current_model)
        ui.notice(model_message)
        return True

    def handle_export_request(markdown_text: str) -> None:
        """保存前端导出的 Markdown 对话，并给用户明确反馈。"""

        try:
            path = agent.export_current_session_markdown(markdown_text)
        except AgentError as exc:
            ui.notice(f"导出失败：{exc}")
            return
        refresh_session_list()
        refresh_project_list()
        ui.notice(f"当前会话已导出：{path}")

    ui.export_requested.connect(handle_export_request)
    ui.update_slash_commands(build_slash_command_options(agent))
    refresh_session_list()
    refresh_project_list()

    # 清除 main.py 设置的"正在初始化"状态，表示 Agent 已就绪
    ui.status("")

    while True:
        if stop_requested():
            break

        try:
            if pending_user_text is not None:
                user_text = pending_user_text
                pending_user_text = None
            else:
                # 启用输入框，等待用户输入
                ui.update_slash_commands(build_slash_command_options(agent))
                ui.set_input_enabled(True)
                ui.set_input_placeholder("输入消息，Enter 发送 · 退出词结束对话")
                user_text = None
                while user_text is None and not stop_requested():
                    user_text = ui.wait_for_input(timeout=0.1)
        except Exception:
            continue

        if user_text is None:
            continue

        # 窗口关闭信号
        if user_text == "__WINDOW_CLOSED__":
            stop_event.set()
            break

        if not user_text:
            continue

        if user_text.strip().lower() in EXIT_WORDS:
            ui.notice("对话结束。")
            break

        if handle_session_control_text(user_text):
            continue

        if user_text.strip() == "/skills":
            ui.write(format_skills_list(agent))
            ui.flush_markdown(None)
            continue

        if user_text.strip() == "/memory:clean":
            ui.write(format_memory_clean_result(agent))
            ui.flush_markdown(None)
            continue

        if user_text.strip() == "/mcp":
            ui.write(format_mcp_status(agent))
            ui.flush_markdown(None)
            continue

        session_message = handle_session_command(agent, user_text)
        if session_message is not None:
            ui.write(session_message)
            ui.flush_markdown(None)
            refresh_session_list()
            continue

        if handle_model_control_text(user_text):
            continue

        approval_message = handle_approval_command(agent, user_text)
        if approval_message is not None:
            ui.notice(approval_message)
            continue

        # 禁用输入框，防止在模型响应期间重复发送
        ui.set_input_enabled(False)
        ui.set_input_placeholder("等待响应中...")

        ui.inline_turn_base(user_text)
        ui.status("正在思考")
        ui.set_generating(True)

        _markdown_state = MarkdownStreamState()

        def handle_delta(delta: str) -> None:
            raise_if_cancelled()
            ui.write_markdown_delta(delta, _markdown_state)

        def _flush_display() -> None:
            ui.flush_markdown(_markdown_state)

        def handle_agent_status(message: str) -> None:
            raise_if_stopped()
            if message:
                _flush_display()
                ui.status(message)
            else:
                _flush_display()
                nonlocal _markdown_state
                _markdown_state = MarkdownStreamState()
                ui.status("正在思考")

        def handle_retry_status(message: str) -> None:
            raise_if_stopped()
            _flush_display()
            ui.status(message, italic=True)

        def handle_tool_start(step: int, tool_call) -> None:
            raise_if_stopped()
            _flush_display()
            ui.print_tool_call_start(
                step,
                tool_call.name,
                tool_call.arguments,
            )

        def handle_tool_result(_tool_call, result) -> None:
            raise_if_stopped()
            _flush_display()
            if result.ui_artifact.get("type") == "html":
                ui.show_html(
                    str(result.ui_artifact.get("title") or "HTML 预览"),
                    str(result.ui_artifact.get("html") or ""),
                )
            ui.print_tool_result_record(
                result.ok,
                result.output,
                tool_name=_tool_call.name,
            )

        def handle_protocol_wait() -> None:
            _flush_display()
            ui.status("正在继续")

        try:
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
            _flush_display()
            ui.newline()
            refresh_session_list()
            refresh_project_list()

        except _QtChatStopped:
            break
        except _QtChatCancelled:
            cancel_event.clear()
            ui.notice("已取消当前生成。")
            continue
        except KeyboardInterrupt:
            ui.notice("已取消当前操作。")
            continue
        except AgentError as exc:
            message = str(exc).strip() or "Agent 请求失败，请检查配置或稍后重试。"
            ui.notice(message if message.startswith("Agent ") else f"Agent 请求失败：{message}")
            continue
        finally:
            # 清除等待状态
            ui.set_generating(False)
            ui.status("")
            if not stop_requested():
                ui.set_input_enabled(True)
                ui.set_input_placeholder("输入消息，Enter 发送 · 退出词结束对话")
