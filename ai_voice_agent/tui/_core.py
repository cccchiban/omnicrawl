"""TerminalUI 核心类。"""

from __future__ import annotations

import shutil
import threading
from typing import Any

from ._capabilities import TerminalCapabilities, detect_capabilities
from ._colors import (
    ANSI_CLEAR_LINE,
    ANSI_DIM,
    ANSI_ERASE_TO_END,
    ANSI_ITALIC,
    ANSI_PREVIOUS_LINE,
    ANSI_RESET,
    ANSI_SAVE_CURSOR,
    ANSI_RESTORE_CURSOR,
    AI_PREFIX,
    USER_PREFIX,
    ColorRole,
    color_text,
    get_color_sequence,
)
from ._display import (
    _dialog_continuation_prefix,
    _display_width,
    _normalize_terminal_text,
    _split_display_rows,
    _take_display_width,
)
from ._markdown_renderer import _MarkdownRendererMixin
from ._tools import ToolDisplayState

# 常量
INLINE_INPUT_WINDOW_ROWS = 8


class TerminalUI(_MarkdownRendererMixin):
    """集中管理终端输出样式，避免多个调用点各自拼 ANSI。"""

    def __init__(
        self,
        capabilities: TerminalCapabilities | None = None,
        *,
        model_label: str | None = None,
    ) -> None:
        self.capabilities = capabilities or detect_capabilities()
        self.model_label = model_label
        self._lock = threading.Lock()
        self._input_tokens = 0
        self._output_tokens = 0
        self._cached_input_tokens = 0

    # ── 颜色/样式快捷方法 ──────────────────────────────────────

    def muted(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "muted", self.capabilities)

    def muted_italic(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        seq = get_color_sequence("muted", self.capabilities)
        return f"{ANSI_DIM}{ANSI_ITALIC}{seq}{text}{ANSI_RESET}"

    def result_text(self, ok: bool, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "success" if ok else "error", self.capabilities)

    def accent(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "accent", self.capabilities)

    def bright(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "text", self.capabilities)

    def primary(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "primary", self.capabilities)

    def secondary(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "secondary", self.capabilities)

    def success(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "success", self.capabilities)

    def warning(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "warning", self.capabilities)

    def error(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "error", self.capabilities)

    def heading(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return color_text(text, "heading", self.capabilities)

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

    # ── 工具调用显示 ──────────────────────────────────────────

    def print_tool_call_start(
        self,
        step: int,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        leading_blank: bool = True,
    ) -> ToolDisplayState:
        from ._tools import print_tool_call_start as _impl
        return _impl(
            self, step, tool_name, arguments,
            caps=self.capabilities, lock=self._lock,
            leading_blank=leading_blank,
        )

    def print_tool_result_record(
        self,
        ok: bool,
        output: str | None = None,
        *,
        tool_name: str = "",
        display_state: ToolDisplayState | None = None,
    ) -> None:
        from ._tools import print_tool_result_record as _impl
        _impl(
            ok, output,
            tool_name=tool_name, display_state=display_state,
            caps=self.capabilities, lock=self._lock,
        )

    # ── 启动面板 ──────────────────────────────────────────────

    def print_startup_panel(self, title: str, lines: list[str]) -> None:
        from ._panels import print_startup_panel as _impl
        _impl(title, lines, caps=self.capabilities, lock=self._lock)

    # ── 临时输出管理 ──────────────────────────────────────────

    def mark_transient_output_start(self) -> bool:
        if not self.capabilities.ansi:
            return False
        with self._lock:
            print(ANSI_SAVE_CURSOR, end="", flush=True)
        return True

    def clear_transient_output(self) -> None:
        if not self.capabilities.ansi:
            return
        with self._lock:
            print(f"{ANSI_RESTORE_CURSOR}{ANSI_ERASE_TO_END}", end="", flush=True)

    # ── 提示符 ────────────────────────────────────────────────

    def prompt(self) -> str:
        return f"\n{color_text('▸', 'primary', self.capabilities)} "

    def prompt_width(self) -> int:
        return _display_width("▸ ")

    # ── 提示状态行 ─────────────────────────────────────────────

    def print_prompt_status(self, cursor_column: int = 0) -> None:
        if not self.model_label or not self.capabilities.ansi:
            return
        line = self.prompt_status_line()
        cursor_target = max(1, self.prompt_width() + cursor_column + 1)
        with self._lock:
            print(
                f"\n{ANSI_CLEAR_LINE}{line}"
                f"{ANSI_PREVIOUS_LINE}\033[{cursor_target}G",
                end="", flush=True,
            )

    def clear_prompt_status(self) -> None:
        if not self.model_label or not self.capabilities.ansi:
            return
        with self._lock:
            print(f"\n{ANSI_CLEAR_LINE}{ANSI_PREVIOUS_LINE}", end="", flush=True)

    def prompt_status_line(self) -> str:
        from ._status import prompt_status_line as _impl
        return _impl(
            self.model_label, self._input_tokens,
            self._output_tokens, self._cached_input_tokens,
            caps=self.capabilities,
        )

    # ── 输入状态覆盖 ───────────────────────────────────────────

    def replace_current_input_with_status(self, message: str) -> None:
        status_text = self._single_line_status_text(message)
        with self._lock:
            if self.capabilities.ansi:
                print(
                    f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}{self.muted(status_text)}\n",
                    end="", flush=True,
                )
            else:
                print(self.muted(status_text), flush=True)

    @staticmethod
    def _single_line_status_text(message: str) -> str:
        text = " ".join(str(message).splitlines())
        width = shutil.get_terminal_size((100, 30)).columns
        max_width = max(1, width)
        if _display_width(text) <= max_width:
            return text
        if max_width <= 3:
            return _take_display_width(text, max_width)
        return f"{_take_display_width(text, max_width - 3)}..."

    def clear_current_input_status(self) -> None:
        if not self.capabilities.ansi:
            return
        with self._lock:
            print(f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}", end="", flush=True)

    # ── 用户输入行 ────────────────────────────────────────────

    def inline_turn_base(self, user_text: str) -> str:
        if not self.capabilities.ansi:
            return ""
        text = _normalize_terminal_text(user_text)
        prompt_width = self.prompt_width()
        terminal_width, terminal_height = shutil.get_terminal_size((100, 30))
        content_width = max(1, max(40, terminal_width) - prompt_width - 1)
        all_rows = _split_display_rows(text, content_width)
        visible_rows = min(
            len(all_rows),
            max(3, min(INLINE_INPUT_WINDOW_ROWS, max(14, terminal_height) - 8)),
        )
        continuation_prefix = " " * prompt_width
        lines = [
            f"{color_text(USER_PREFIX, 'primary', self.capabilities)} {row}" if index == 0
            else f"{continuation_prefix}{row}"
            for index, row in enumerate(all_rows)
        ]
        with self._lock:
            print(f"\033[{visible_rows}A", end="")
            for index in range(visible_rows):
                print(f"\r{ANSI_CLEAR_LINE}", end="")
                if index < visible_rows - 1:
                    print("\033[1B", end="")
            if visible_rows > 1:
                print(f"\033[{visible_rows - 1}A", end="")
            print("\n".join(lines), flush=True)
        return ""

    def print_ai_prefix(self) -> None:
        with self._lock:
            print(f"{color_text(AI_PREFIX, 'primary', self.capabilities)} ", end="", flush=True)

    def write(self, text: str) -> None:
        with self._lock:
            print(text, end="", flush=True)

    def newline(self) -> None:
        with self._lock:
            print()

    def status(self, message: str, *, leading_blank: bool = True, italic: bool = False) -> None:
        with self._lock:
            prefix = "\n" if leading_blank else ""
            indent = _dialog_continuation_prefix(AI_PREFIX)
            style = self.muted_italic if italic else self.muted
            print(f"{prefix}{indent}{style(f'[{message}]')}", flush=True)

    def notice(self, message: str) -> None:
        with self._lock:
            print(self.muted(message), flush=True)

    # ── 确认对话框 ────────────────────────────────────────────

    def prompt_yes_no(self, prompt: str, confirmed_label: str = "") -> bool:
        from ._prompt import prompt_yes_no as _impl
        return _impl(prompt, confirmed_label, caps=self.capabilities, lock=self._lock)
