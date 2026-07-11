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
from ..base import BaseUI
from ._markdown_renderer import _MarkdownRendererMixin

# 常量
INLINE_INPUT_WINDOW_ROWS = 8


class TerminalUI(_MarkdownRendererMixin, BaseUI):
    """集中管理终端输出样式，避免多个调用点各自拼 ANSI。"""

    def __init__(
        self,
        capabilities: TerminalCapabilities | None = None,
        *,
        model_label: str | None = None,
    ) -> None:
        super().__init__(model_label=model_label)
        self.capabilities = capabilities or detect_capabilities()
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

    def set_model_label(self, text: str) -> None:
        super().set_model_label(text)

    # ── 工具调用显示 ──────────────────────────────────────────

    def print_tool_call_start(
        self,
        step: int,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        leading_blank: bool = True,
    ) -> None:
        from ._tools import print_tool_call_start as _impl
        _impl(
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
    ) -> None:
        from ._tools import print_tool_result_record as _impl
        _impl(
            ok, output,
            tool_name=tool_name,
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
        """将已提交输入视为终端历史，不再回跳改写。

        行内输入编辑器和标准 ``input`` 已经把用户文本写入终端。此前为了更换
        前缀而根据估算行数回跳重绘，会在窗口缩放、自动滚动或复杂字符下覆盖历史。
        保持原始输入可回读，优先保证对话流稳定。
        """

        del user_text
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


__all__ = [
    'TerminalCapabilities',
    'detect_capabilities',
    'ANSI_RESET',
    'ANSI_BLACK',
    'ANSI_RED',
    'ANSI_GREEN',
    'ANSI_YELLOW',
    'ANSI_BLUE',
    'ANSI_MAGENTA',
    'ANSI_CYAN',
    'ANSI_WHITE',
    'ANSI_BRIGHT_BLACK',
    'ANSI_BRIGHT_RED',
    'ANSI_BRIGHT_GREEN',
    'ANSI_BRIGHT_YELLOW',
    'ANSI_BRIGHT_BLUE',
    'ANSI_BRIGHT_MAGENTA',
    'ANSI_BRIGHT_CYAN',
    'ANSI_BRIGHT_WHITE',
    'ANSI_BOLD',
    'ANSI_DIM',
    'ANSI_ITALIC',
    'ANSI_UNDERLINE',
    'ANSI_CLEAR_LINE',
    'ANSI_CLEAR_TO_LINE_END',
    'ANSI_PREVIOUS_LINE',
    'ANSI_SAVE_CURSOR',
    'ANSI_RESTORE_CURSOR',
    'ANSI_ERASE_TO_END',
    'COLOR_PRIMARY',
    'COLOR_SECONDARY',
    'COLOR_SUCCESS',
    'COLOR_WARNING',
    'COLOR_ERROR',
    'COLOR_MUTED',
    'COLOR_TEXT',
    'COLOR_HEADING',
    'COLOR_ACCENT',
    'AI_PREFIX',
    'USER_PREFIX',
    'ColorRole',
    'color_text',
    'get_color_sequence',
    'strip_ansi',
    '_char_display_width',
    '_iter_display_units',
    '_contains_complex_display_width',
    '_dialog_continuation_prefix',
    '_display_width',
    '_delete_last_display_unit',
    '_combine_surrogate_pair',
    '_ellipsize_display_text',
    '_normalize_terminal_text',
    '_preview_display_rows',
    '_split_display_rows',
    '_take_display_width',
    'MarkdownSpan',
    'MarkdownStreamState',
    'TerminalUI',
    'StatusLine',
    'InputBar',
    'WaitingIndicator',
    'SPINNER_FRAMES',
]
