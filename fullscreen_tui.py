from __future__ import annotations

import os
import random
import re
import shutil
import sys
import threading
import time
import unicodedata
from dataclasses import dataclass

from terminal_ui import AI_PREFIX, USER_PREFIX, WAITING_DOTS, WAITING_KAOMOJI, detect_capabilities


CURSOR_HOME = "\033[H"
CURSOR_SHOW = "\033[?25h"
CURSOR_HIDE = "\033[?25l"
CURSOR_BLOCK = "\033[1 q"
CURSOR_DEFAULT = "\033[0 q"
MOUSE_TRACKING_RESET = "\033[?1003l\033[?1002l\033[?1000l\033[?1015l\033[?1006l\033[?1005l"
MOUSE_TRACKING_ON = f"{MOUSE_TRACKING_RESET}\033[?1006h\033[?1000h"
MOUSE_TRACKING_OFF = MOUSE_TRACKING_RESET
ERASE_LINE = "\033[K"
RESET = "\033[0m"
BOLD = "\033[1m"
MUTED = "\033[2;90m"
GRAY = "\033[90m"
LIGHT_BLUE = "\033[94m"
WHITE = "\033[37m"
SCROLL_LINES_PER_WHEEL = 4
ESCAPE_SEQUENCE_TIMEOUT_SECONDS = 0.08
ESCAPE_SEQUENCE_MAX_CHARS = 64
WINDOWS_EXTENDED_KEY_PENDING_SECONDS = 0.75
MOUSE_FRAGMENT_SUPPRESSION_SECONDS = 0.25
MOUSE_FRAGMENT_CHARS = frozenset("0123456789[]<>;MmHhPpKkSs")


@dataclass
class TUIMessage:
    role: str
    text: str


@dataclass
class TUIRenderLine:
    text: str
    style: str | None
    spans: list[MarkdownSpan] | None = None


@dataclass
class MarkdownSpan:
    """Markdown 行内片段，style 为 ANSI SGR 前缀。"""

    text: str
    style: str | None = None


@dataclass
class MarkdownLine:
    """终端可渲染的 Markdown 逻辑行。"""

    spans: list[MarkdownSpan]
    style: str | None = None


def supports_fullscreen_tui() -> bool:
    """判断当前终端是否适合启用 ANSI TUI。"""

    if os.getenv("AI_DISABLE_FULLSCREEN_TUI") == "1":
        return False
    return os.name == "nt" and detect_capabilities().ansi and sys.stdin.isatty()


def _char_display_width(char: str) -> int:
    if unicodedata.combining(char):
        return 0
    return 2 if unicodedata.east_asian_width(char) in {"F", "W"} else 1


def _display_width(text: str) -> int:
    width = 0
    for char in text:
        width += _char_display_width(char)
    return width


def _take_display_width(text: str, max_width: int) -> str:
    if max_width <= 0:
        return ""

    width = 0
    chars: list[str] = []
    for char in text:
        char_width = _char_display_width(char)
        if width + char_width > max_width:
            break
        chars.append(char)
        width += char_width
    return "".join(chars)


def _take_tail_display_width(text: str, max_width: int) -> str:
    if max_width <= 0:
        return ""

    width = 0
    chars: list[str] = []
    for char in reversed(text):
        char_width = _char_display_width(char)
        if width + char_width > max_width:
            break
        chars.append(char)
        width += char_width
    return "".join(reversed(chars))


def _wrap_display(text: str, max_width: int) -> list[str]:
    if max_width <= 0:
        return [""]

    lines: list[str] = []
    for raw_line in text.splitlines() or [""]:
        remaining = raw_line
        if not remaining:
            lines.append("")
            continue

        while remaining:
            chunk = _take_display_width(remaining, max_width)
            if not chunk:
                break
            lines.append(chunk)
            remaining = remaining[len(chunk) :]

    return lines or [""]


def _style(*styles: str | None) -> str | None:
    return "".join(style for style in styles if style) or None


def _plain_text(spans: list[MarkdownSpan]) -> str:
    return "".join(span.text for span in spans)


def _append_span(spans: list[MarkdownSpan], text: str, style: str | None) -> None:
    if not text:
        return
    if spans and spans[-1].style == style:
        spans[-1].text += text
    else:
        spans.append(MarkdownSpan(text, style))


def _take_spans_display_width(spans: list[MarkdownSpan], max_width: int) -> list[MarkdownSpan]:
    if max_width <= 0:
        return []

    width = 0
    truncated: list[MarkdownSpan] = []
    for span in spans:
        for char in span.text:
            char_width = _char_display_width(char)
            if width + char_width > max_width:
                return truncated
            _append_span(truncated, char, span.style)
            width += char_width
    return truncated


def _wrap_prefixed_spans(
    spans: list[MarkdownSpan],
    *,
    first_width: int,
    continuation_width: int,
) -> list[list[MarkdownSpan]]:
    """按首行和续行不同宽度折行，避免前缀把内容挤出屏幕。"""

    first_width = max(1, first_width)
    continuation_width = max(1, continuation_width)
    if not spans or not _plain_text(spans):
        return [[MarkdownSpan("")]]

    chunks: list[list[MarkdownSpan]] = []
    current: list[MarkdownSpan] = []
    width = 0
    max_width = first_width

    for span in spans:
        for char in span.text:
            char_width = _char_display_width(char)
            if width > 0 and width + char_width > max_width:
                chunks.append(current)
                current = []
                width = 0
                max_width = continuation_width

            _append_span(current, char, span.style)
            width += char_width

            if width >= max_width:
                chunks.append(current)
                current = []
                width = 0
                max_width = continuation_width

    if current:
        chunks.append(current)
    return chunks or [[MarkdownSpan("")]]


def _parse_inline_markdown(text: str, default_style: str | None) -> list[MarkdownSpan]:
    """解析常见行内 Markdown，保留终端中最有价值的视觉层级。"""

    pattern = re.compile(
        r"(`[^`]+`|\*\*[^*]+\*\*|__[^_]+__|\*[^*\n]+\*|_[^_\n]+_|\[[^\]]+\]\([^)]+\))"
    )
    spans: list[MarkdownSpan] = []
    cursor = 0
    for match in pattern.finditer(text):
        _append_span(spans, text[cursor : match.start()], default_style)
        token = match.group(0)

        if token.startswith("`") and token.endswith("`"):
            _append_span(spans, token[1:-1], _style(BOLD, default_style))
        elif token.startswith(("**", "__")) and token.endswith(("**", "__")):
            _append_span(spans, token[2:-2], _style(BOLD, default_style))
        elif token.startswith("["):
            link_match = re.fullmatch(r"\[([^\]]+)\]\(([^)]+)\)", token)
            if link_match:
                label, url = link_match.groups()
                _append_span(spans, f"{label} ({url})", default_style)
            else:
                _append_span(spans, token, default_style)
        else:
            _append_span(spans, token[1:-1], default_style)

        cursor = match.end()

    _append_span(spans, text[cursor:], default_style)
    return spans or [MarkdownSpan("", default_style)]


def _render_markdown(text: str, default_style: str | None) -> list[MarkdownLine]:
    """把 Markdown 转成适合 ANSI TUI 的逻辑行，不引入第三方依赖。"""

    lines: list[MarkdownLine] = []
    in_code_block = False

    for raw_line in text.splitlines() or [""]:
        stripped = raw_line.strip()
        if stripped.startswith("```"):
            in_code_block = not in_code_block
            continue

        if in_code_block:
            lines.append(MarkdownLine([MarkdownSpan(f"    {raw_line}", default_style)], default_style))
            continue

        if not stripped:
            lines.append(MarkdownLine([MarkdownSpan("", default_style)], default_style))
            continue

        heading_match = re.match(r"^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$", raw_line)
        if heading_match:
            heading_style = _style(BOLD, default_style)
            lines.append(
                MarkdownLine(
                    _parse_inline_markdown(heading_match.group(2), heading_style),
                    heading_style,
                )
            )
            continue

        if re.match(r"^\s{0,3}([-*_]\s*){3,}$", raw_line):
            lines.append(MarkdownLine([MarkdownSpan("─" * 20, MUTED)], MUTED))
            continue

        quote_match = re.match(r"^\s{0,3}>\s?(.*)$", raw_line)
        if quote_match:
            quote_spans = [MarkdownSpan("│ ", LIGHT_BLUE)]
            quote_spans.extend(_parse_inline_markdown(quote_match.group(1), default_style))
            lines.append(MarkdownLine(quote_spans, default_style))
            continue

        task_match = re.match(r"^(\s*)[-*+]\s+\[([ xX])\]\s+(.*)$", raw_line)
        if task_match:
            indent, checked, body = task_match.groups()
            marker = "[x] " if checked.lower() == "x" else "[ ] "
            spans = [MarkdownSpan(indent + marker, default_style)]
            spans.extend(_parse_inline_markdown(body, default_style))
            lines.append(MarkdownLine(spans, default_style))
            continue

        list_match = re.match(r"^(\s*)([-*+]|\d+[.)])\s+(.*)$", raw_line)
        if list_match:
            indent, marker, body = list_match.groups()
            normalized_marker = f"{marker} " if marker[0].isdigit() else "- "
            spans = [MarkdownSpan(indent + normalized_marker, default_style)]
            spans.extend(_parse_inline_markdown(body, default_style))
            lines.append(MarkdownLine(spans, default_style))
            continue

        lines.append(MarkdownLine(_parse_inline_markdown(raw_line, default_style), default_style))

    return lines or [MarkdownLine([MarkdownSpan("", default_style)], default_style)]


class FullScreenTUI:
    """基于 ANSI 光标控制的终端界面。

    该实现刻意保持轻量：只负责固定布局、状态栏、消息区和单行输入。
    复杂输入编辑、历史补全和多光标体验应交给后续的 prompt_toolkit 版本。
    """

    def __init__(
        self,
        *,
        model: str,
        thinking_type: str,
        reasoning_effort: str,
        config_label: str,
        slash_commands: list[str] | None = None,
    ) -> None:
        self.model = model
        self.thinking_type = thinking_type
        self.reasoning_effort = reasoning_effort
        self.config_label = config_label
        if self.reasoning_effort:
            thinking_display = f"thinking=enabled (reasoning={self.reasoning_effort})"
        else:
            thinking_display = f"thinking={self.thinking_type}"
        self._header_message = TUIMessage(
            "header",
            f"AI 语音 Agent\nmodel={self.model}  {thinking_display}\nconfig: {self.config_label}",
        )
        self._messages: list[TUIMessage] = [self._header_message]
        self._status = "Enter 发送，空 Enter 录音，Ctrl+C 或输入“退出”结束"
        self._input_text = ""
        self._input_cursor = 0
        self._active_assistant_index: int | None = None
        self._assistant_prefix_visible = True
        self._thinking_indicator_visible = False
        self._scroll_offset = 0
        self._confirm_prompt: str | None = None
        self._confirm_selection_yes = True
        self._stdin_handle: int | None = None
        self._stdin_mode: int | None = None
        self._pending_windows_extended_key_until = 0.0
        self._suppress_mouse_fragments_until = 0.0
        self._lock = threading.RLock()
        self._active = False
        self._input_active = False       # True 时 stdin 由 read_line/confirm 独占
        self._scroll_poll_thread: threading.Thread | None = None
        # 自动补全
        self._slash_commands: list[str] = sorted(slash_commands or [])
        self._autocomplete_visible = False
        self._autocomplete_matches: list[str] = []
        self._autocomplete_index = 0

    def __enter__(self) -> FullScreenTUI:
        self._active = True
        self._enable_virtual_terminal_input()
        self._start_scroll_poller()
        sys.stdout.write(f"{MOUSE_TRACKING_ON}{CURSOR_SHOW}{CURSOR_BLOCK}{CURSOR_HOME}")
        sys.stdout.flush()
        self.render()
        return self

    def __exit__(self, _exc_type: object, _exc: object, _traceback: object) -> None:
        self._active = False
        self._scroll_poll_thread = None
        sys.stdout.write(f"{RESET}{CURSOR_SHOW}{CURSOR_DEFAULT}{MOUSE_TRACKING_OFF}")
        sys.stdout.flush()
        self._restore_console_input_mode()

    def add_user_message(self, text: str) -> None:
        with self._lock:
            self._messages.append(TUIMessage("user", text))
            self._active_assistant_index = None
            self._scroll_offset = 0
            self.render_locked()

    def add_system_message(self, text: str) -> None:
        with self._lock:
            self._messages.append(TUIMessage("system", text))
            self._scroll_offset = 0
            self.render_locked()

    def add_status_message(self, text: str) -> None:
        with self._lock:
            self._messages.append(TUIMessage("status", text))
            self._scroll_offset = 0
            self.render_locked()

    def start_assistant_message(self) -> None:
        with self._lock:
            self._thinking_indicator_visible = False
            self._messages.append(TUIMessage("assistant", ""))
            self._active_assistant_index = len(self._messages) - 1
            self._scroll_offset = 0
            self.render_locked()

    def append_assistant(self, delta: str) -> None:
        with self._lock:
            if self._active_assistant_index is None:
                self._thinking_indicator_visible = False
                self._messages.append(TUIMessage("assistant", ""))
                self._active_assistant_index = len(self._messages) - 1
                self._scroll_offset = 0
            self._messages[self._active_assistant_index].text += delta
            self.render_locked()

    def finish_assistant_message(self) -> None:
        with self._lock:
            self._active_assistant_index = None
            self._assistant_prefix_visible = True
            self._thinking_indicator_visible = False
            self.render_locked()

    def set_assistant_prefix_visible(self, visible: bool) -> None:
        with self._lock:
            self._assistant_prefix_visible = visible
            self.render_locked()

    def show_thinking_indicator(self) -> None:
        """模型尚未返回正文时，在消息区显示可闪烁的 AI 前缀。"""

        with self._lock:
            self._thinking_indicator_visible = True
            self._assistant_prefix_visible = True
            self._scroll_offset = 0
            self.render_locked()

    def hide_thinking_indicator(self) -> None:
        with self._lock:
            if not self._thinking_indicator_visible:
                return
            self._thinking_indicator_visible = False
            self.render_locked()

    def set_status(self, text: str) -> None:
        with self._lock:
            self._status = text
            self.render_locked()

    def set_input(self, text: str) -> None:
        with self._lock:
            self._input_text = text
            self._input_cursor = len(text)
            self.render_locked()

    def _set_input_state(self, text: str, cursor: int) -> None:
        with self._lock:
            self._input_text = text
            self._input_cursor = max(0, min(cursor, len(text)))
            self.render_locked()

    def confirm_yes_no(self, prompt: str) -> bool:
        """显示默认 YES 的确认弹窗。

        左右箭头切换选项；Enter 提交当前选项；Y/N 可直接确认或取消。
        """

        with self._lock:
            self._messages.append(TUIMessage("status", prompt))
            self._scroll_offset = 0
            self._confirm_prompt = prompt
            self._confirm_selection_yes = True
            self.render_locked()

        try:
            import msvcrt
        except ImportError:
            answer = input("确认？[Enter=YES / n=NO] ").strip().lower()
            return answer not in {"n", "no", "否", "false"}

        self._input_active = True
        try:
            while True:
                char = msvcrt.getwch()
                if self._consume_pending_confirm_extended_key(char):
                    continue
                if char in {"\r", "\n", "y", "Y"}:
                    return True if char in {"y", "Y"} else self._confirm_selection_yes
                if char in {"n", "N"}:
                    self._set_confirm_selection(False)
                    return False
                if char == "\x1b":
                    sequence = self._read_pending_escape_sequence()
                    if self._is_left_arrow_sequence(sequence):
                        self._set_confirm_selection(True)
                        continue
                    if self._is_right_arrow_sequence(sequence):
                        self._set_confirm_selection(False)
                        continue
                    self._handle_mouse_sequence(sequence)
                    continue
                if char in {"\x00", "\xe0"}:
                    key_code = self._read_windows_extended_key()
                    if key_code == "K":
                        self._set_confirm_selection(True)
                        continue
                    if key_code == "M":
                        self._set_confirm_selection(False)
                        continue
                    if not key_code or key_code in {"\x00", "\xe0"}:
                        self._mark_pending_windows_extended_key()
                    continue
        finally:
            self._input_active = False
            with self._lock:
                self._confirm_prompt = None
                self._confirm_selection_yes = True
                self.render_locked()

    def _set_confirm_selection(self, yes_selected: bool) -> None:
        with self._lock:
            self._confirm_selection_yes = yes_selected
            self.render_locked()

    def _consume_pending_confirm_extended_key(self, char: str) -> bool:
        if time.monotonic() > self._pending_windows_extended_key_until:
            return False

        if char in {"\x00", "\xe0"}:
            self._mark_pending_windows_extended_key()
            return True

        self._pending_windows_extended_key_until = 0.0
        if char == "K":
            self._set_confirm_selection(True)
            return True
        if char == "M":
            self._set_confirm_selection(False)
            return True
        return False

    def _enable_virtual_terminal_input(self) -> None:
        """让 Windows 控制台把鼠标滚轮作为 VT 转义序列交给输入循环。"""

        if os.name != "nt" or not sys.stdin.isatty():
            return

        try:
            import ctypes

            kernel32 = ctypes.windll.kernel32
            handle = kernel32.GetStdHandle(-10)
            mode = ctypes.c_uint32()
            if handle == -1 or not kernel32.GetConsoleMode(handle, ctypes.byref(mode)):
                return

            ENABLE_VIRTUAL_TERMINAL_INPUT = 0x0200
            ENABLE_QUICK_EDIT_MODE = 0x0040
            ENABLE_EXTENDED_FLAGS = 0x0080
            updated_mode = (
                mode.value | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS
            ) & ~ENABLE_QUICK_EDIT_MODE
            if not kernel32.SetConsoleMode(handle, updated_mode):
                return

            self._stdin_handle = handle
            self._stdin_mode = mode.value
        except Exception:
            return

    def _restore_console_input_mode(self) -> None:
        if self._stdin_handle is None or self._stdin_mode is None:
            return

        try:
            import ctypes

            ctypes.windll.kernel32.SetConsoleMode(self._stdin_handle, self._stdin_mode)
        except Exception:
            pass
        finally:
            self._stdin_handle = None
            self._stdin_mode = None

    def _start_scroll_poller(self) -> None:
        """启动后台线程，在 TUI 空闲时处理鼠标滚轮等输入事件。"""
        if os.name != "nt":
            return
        try:
            import msvcrt  # noqa: F811
        except ImportError:
            return

        def _poll() -> None:
            while self._active and self._scroll_poll_thread is not None:
                if not self._input_active:
                    try:
                        while msvcrt.kbhit():
                            char = msvcrt.getwch()
                            if char == "\x1b":
                                sequence = self._read_pending_escape_sequence()
                                self._handle_mouse_sequence(sequence)
                            elif char in {"\x00", "\xe0"}:
                                self._handle_windows_extended_key()
                            elif self._consume_mouse_fragment(char):
                                continue
                            else:
                                self._consume_pending_windows_extended_key(char)
                    except Exception:
                        pass
                time.sleep(0.08)

        self._scroll_poll_thread = threading.Thread(target=_poll, daemon=True)
        self._scroll_poll_thread.start()

    def read_line(self) -> str:
        """读取底部输入栏的一行文本。"""

        try:
            import msvcrt
        except ImportError:
            return input(f"{USER_PREFIX}").strip()

        self._input_active = True
        try:
            self.set_input("")
            while True:
                char = msvcrt.getwch()
                submitted = self._handle_input_char(char)
                if submitted is not None:
                    return submitted
        finally:
            self._input_active = False

    def poll_submitted_line(self, buffer: list[str]) -> str | None:
        """非阻塞读取底部输入栏，适合朗读期间打断或输入下一句。"""

        try:
            import msvcrt
        except ImportError:
            return None

        self._input_active = True
        try:
            while msvcrt.kbhit():
                submitted = self._handle_input_char(msvcrt.getwch())
                buffer[:] = list(self._input_text)
                if submitted is not None:
                    return submitted
            return None
        finally:
            self._input_active = False

    def _handle_input_char(self, char: str) -> str | None:
        if char == "\x1b":
            # 如果自动补全可见，Escape 优先关闭菜单
            if self._autocomplete_visible:
                sequence = self._read_pending_escape_sequence()
                if not sequence:
                    self._autocomplete_dismiss()
                    return None
                # 否则当作普通转义序列处理
                return self._handle_escape_with_sequence(sequence)
            return self._handle_escape_sequence()
        if self._consume_mouse_fragment(char):
            return None
        if self._consume_pending_windows_extended_key(char):
            return None
        if char == "\t":
            self._autocomplete_complete()
            return None
        if char in {"\r", "\n"}:
            text = self._input_text.strip()
            self.set_input("")
            self._autocomplete_visible = False
            self._autocomplete_matches.clear()
            return text
        if char in {"\x00", "\xe0"}:
            self._handle_windows_extended_key()
            return None
        if char == "\x03":
            raise KeyboardInterrupt
        if char == "\b":
            if self._input_cursor > 0:
                updated = self._input_text[: self._input_cursor - 1] + self._input_text[self._input_cursor :]
                self._update_autocomplete(updated)
                self._set_input_state(updated, self._input_cursor - 1)
            return None
        if char == "\x7f":
            if self._input_cursor < len(self._input_text):
                updated = self._input_text[: self._input_cursor] + self._input_text[self._input_cursor + 1 :]
                self._update_autocomplete(updated)
                self._set_input_state(updated, self._input_cursor)
            elif self._input_cursor > 0:
                updated = self._input_text[: self._input_cursor - 1] + self._input_text[self._input_cursor :]
                self._update_autocomplete(updated)
                self._set_input_state(updated, self._input_cursor - 1)
            return None
        if char.isprintable() or char.isspace():
            updated = self._input_text[: self._input_cursor] + char + self._input_text[self._input_cursor :]
            self._update_autocomplete(updated)
            self._set_input_state(updated, self._input_cursor + 1)
        return None

    def _handle_windows_extended_key(self) -> None:
        key_code = self._read_windows_extended_key()
        if not key_code or key_code in {"\x00", "\xe0"}:
            self._mark_pending_windows_extended_key()
            return
        self._handle_windows_extended_key_code(key_code)

    def _handle_windows_extended_key_code(self, key_code: str) -> bool:
        if key_code == "H":
            self.scroll_messages(SCROLL_LINES_PER_WHEEL)
            return True
        if key_code == "P":
            self.scroll_messages(-SCROLL_LINES_PER_WHEEL)
            return True
        if key_code == "K":
            self._move_input_cursor(-1)
            return True
        if key_code == "M":
            self._move_input_cursor(1)
            return True
        if key_code == "S":
            if self._input_cursor < len(self._input_text):
                updated = self._input_text[: self._input_cursor] + self._input_text[self._input_cursor + 1 :]
                self._update_autocomplete(updated)
                self._set_input_state(updated, self._input_cursor)
            return True
        return False

    def _mark_pending_windows_extended_key(self) -> None:
        self._pending_windows_extended_key_until = (
            time.monotonic() + WINDOWS_EXTENDED_KEY_PENDING_SECONDS
        )

    def _consume_pending_windows_extended_key(self, char: str) -> bool:
        if time.monotonic() > self._pending_windows_extended_key_until:
            self._pending_windows_extended_key_until = 0.0
            return False

        if char in {"\x00", "\xe0"}:
            self._mark_pending_windows_extended_key()
            return True

        self._pending_windows_extended_key_until = 0.0
        return self._handle_windows_extended_key_code(char)

    def _move_input_cursor(self, delta: int) -> None:
        with self._lock:
            self._input_cursor = max(0, min(len(self._input_text), self._input_cursor + delta))
            self.render_locked()

    # ── 自动补全 ──────────────────────────────────────

    def _update_autocomplete(self, text: str) -> None:
        """根据当前输入更新自动补全匹配列表（仅更新状态，不触发渲染）。"""
        if text.startswith("/"):
            matches = [cmd for cmd in self._slash_commands if cmd.startswith(text)]
            if matches:
                self._autocomplete_visible = True
                self._autocomplete_matches = matches
                self._autocomplete_index = 0
                return
        self._autocomplete_visible = False
        self._autocomplete_matches.clear()
        self._autocomplete_index = 0

    def _autocomplete_complete(self) -> None:
        """Tab 键：用当前选中匹配项补全输入。"""
        if not self._autocomplete_visible or not self._autocomplete_matches:
            return
        selected = self._autocomplete_matches[self._autocomplete_index]
        with self._lock:
            self._input_text = selected
            self._input_cursor = len(selected)
            self._autocomplete_visible = False
            self._autocomplete_matches.clear()
            self._autocomplete_index = 0
            self.render_locked()

    def _autocomplete_next(self) -> None:
        """选择下一个匹配项。"""
        if not self._autocomplete_matches:
            return
        self._autocomplete_index = (self._autocomplete_index + 1) % len(self._autocomplete_matches)
        self.render_locked()

    def _autocomplete_prev(self) -> None:
        """选择上一个匹配项。"""
        if not self._autocomplete_matches:
            return
        self._autocomplete_index = (self._autocomplete_index - 1) % len(self._autocomplete_matches)
        self.render_locked()

    def _autocomplete_dismiss(self) -> None:
        """关闭自动补全菜单。"""
        with self._lock:
            self._autocomplete_visible = False
            self._autocomplete_matches.clear()
            self._autocomplete_index = 0
            self.render_locked()

    @staticmethod
    def _read_windows_extended_key(timeout_seconds: float = 0.08) -> str:
        try:
            import msvcrt
        except ImportError:
            return ""

        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            if msvcrt.kbhit():
                return msvcrt.getwch()
            time.sleep(0.001)
        return ""

    def _handle_escape_sequence(self) -> str | None:
        sequence = self._read_pending_escape_sequence()
        return self._handle_escape_with_sequence(sequence)

    def _handle_escape_with_sequence(self, sequence: str) -> str | None:
        if self._handle_mouse_sequence(sequence):
            return None
        if sequence.startswith("[<") or sequence.startswith("[M"):
            self._mark_mouse_fragment_suppression()
            return None
        if self._handle_arrow_sequence(sequence):
            return None
        return None

    def _handle_arrow_sequence(self, sequence: str) -> bool:
        if self._autocomplete_visible:
            if sequence in {"[A", "OA"}:
                self._autocomplete_prev()
                return True
            if sequence in {"[B", "OB"}:
                self._autocomplete_next()
                return True
        else:
            if sequence in {"[A", "OA"}:
                self.scroll_messages(SCROLL_LINES_PER_WHEEL)
                return True
            if sequence in {"[B", "OB"}:
                self.scroll_messages(-SCROLL_LINES_PER_WHEEL)
                return True
        if sequence in {"[D", "OD"}:
            self._move_input_cursor(-1)
            return True
        if sequence in {"[C", "OC"}:
            self._move_input_cursor(1)
            return True
        if sequence == "[3~":
            if self._input_cursor < len(self._input_text):
                updated = self._input_text[: self._input_cursor] + self._input_text[self._input_cursor + 1 :]
                self._set_input_state(updated, self._input_cursor)
            return True
        if sequence in {"[H", "[1~"}:
            self._set_input_state(self._input_text, 0)
            return True
        if sequence in {"[F", "[4~"}:
            self._set_input_state(self._input_text, len(self._input_text))
            return True
        return False

    @staticmethod
    def _read_pending_escape_sequence(timeout_seconds: float = ESCAPE_SEQUENCE_TIMEOUT_SECONDS) -> str:
        try:
            import msvcrt
        except ImportError:
            return ""

        deadline = time.monotonic() + timeout_seconds
        chars: list[str] = []
        while time.monotonic() < deadline and len(chars) < ESCAPE_SEQUENCE_MAX_CHARS:
            if not msvcrt.kbhit():
                time.sleep(0.001)
                continue

            chars.append(msvcrt.getwch())

            sequence = "".join(chars)
            if FullScreenTUI._is_escape_sequence_complete(sequence):
                break

            # VT 鼠标序列可能包含多位坐标，例如 ESC [ < 35 ; 120 ; 40 M。
            # 每读到一个字符后刷新等待窗口，避免长序列被 20ms 总超时截断，
            # 否则剩余的数字、分号或 M 会被后续输入循环当作用户文本插入。
            deadline = time.monotonic() + timeout_seconds

        return "".join(chars)

    @staticmethod
    def _is_escape_sequence_complete(sequence: str) -> bool:
        if not sequence:
            return False

        if sequence[0] not in {"[", "O"}:
            return True

        if sequence.startswith("[M"):
            return len(sequence) >= 5

        if sequence[0] == "O":
            return len(sequence) >= 2

        return len(sequence) >= 2 and FullScreenTUI._is_csi_final_char(sequence[-1])

    @staticmethod
    def _is_csi_final_char(char: str) -> bool:
        return bool(char) and 0x40 <= ord(char) <= 0x7E

    def _handle_mouse_sequence(self, sequence: str) -> bool:
        match = re.fullmatch(r"\[<(\d+);(\d+);(\d+)([mM])", sequence)
        if match is not None:
            button_code = int(match.group(1))
            self._mark_mouse_fragment_suppression()
            return self._handle_mouse_button(button_code)

        if len(sequence) >= 5 and sequence.startswith("[M"):
            button_code = max(0, ord(sequence[2]) - 32)
            self._mark_mouse_fragment_suppression()
            return self._handle_mouse_button(button_code)

        return False

    def _mark_mouse_fragment_suppression(self) -> None:
        self._suppress_mouse_fragments_until = (
            time.monotonic() + MOUSE_FRAGMENT_SUPPRESSION_SECONDS
        )

    def _consume_mouse_fragment(self, char: str) -> bool:
        if time.monotonic() > self._suppress_mouse_fragments_until:
            self._suppress_mouse_fragments_until = 0.0
            return False

        if char == "\x1b":
            self._mark_mouse_fragment_suppression()
            return True

        if char in MOUSE_FRAGMENT_CHARS:
            self._mark_mouse_fragment_suppression()
            return True

        self._suppress_mouse_fragments_until = 0.0
        return False

    @staticmethod
    def _is_right_arrow_sequence(sequence: str) -> bool:
        return sequence in {"[C", "OC"}

    @staticmethod
    def _is_left_arrow_sequence(sequence: str) -> bool:
        return sequence in {"[D", "OD"}

    def _handle_mouse_button(self, button_code: int) -> bool:
        if button_code == 64:
            self.scroll_messages(SCROLL_LINES_PER_WHEEL)
            return True
        if button_code == 65:
            self.scroll_messages(-SCROLL_LINES_PER_WHEEL)
            return True
        return True

    def scroll_messages(self, delta_lines: int) -> None:
        """滚动消息区；正数看更早内容，负数回到底部。"""

        with self._lock:
            max_offset = self._max_scroll_offset_locked()
            self._scroll_offset = max(0, min(max_offset, self._scroll_offset + delta_lines))
            self.render_locked()

    def _max_scroll_offset_locked(self) -> int:
        width, height = shutil.get_terminal_size((100, 30))
        height = max(height, 14)
        width = max(width, 40)
        message_height = self._message_height(height)
        return max(0, len(self._build_message_lines(width)) - message_height)

    @staticmethod
    def _message_height(height: int) -> int:
        status_row = height - 2
        status_separator_row = status_row - 1
        message_top = 1
        message_bottom = max(message_top, status_separator_row - 1)
        return max(1, message_bottom - message_top + 1)

    def render(self) -> None:
        with self._lock:
            self.render_locked()

    def render_locked(self) -> None:
        if not self._active:
            return

        width, height = shutil.get_terminal_size((100, 30))
        height = max(height, 14)
        width = max(width, 40)
        status_row = height - 2
        input_row = height
        status_separator_row = status_row - 1
        input_separator_row = input_row - 1
        message_top = 1
        message_height = self._message_height(height)

        rows: list[tuple[int, str, str | None, list[MarkdownSpan] | None]] = []

        message_lines = self._build_message_lines(width)
        max_scroll_offset = max(0, len(message_lines) - message_height)
        self._scroll_offset = max(0, min(max_scroll_offset, self._scroll_offset))
        if self._scroll_offset:
            visible_end = len(message_lines) - self._scroll_offset
            visible_start = max(0, visible_end - message_height)
            visible_message_lines = message_lines[visible_start:visible_end]
        else:
            visible_message_lines = message_lines[-message_height:]
        for offset, line in enumerate(visible_message_lines):
            rows.append((message_top + offset, line.text, line.style, line.spans))

        rows.append((status_separator_row, "─" * width, MUTED, None))
        rows.append((status_row, self._status, MUTED, None))
        rows.append((input_separator_row, "─" * width, MUTED, None))
        rows.append((input_row, self._input_line(width), WHITE, None))
        if self._autocomplete_visible:
            autocomplete_rows = self._autocomplete_overlay_rows(width, status_row)
            rows.extend(autocomplete_rows)
        if self._confirm_prompt is not None:
            rows.extend(self._confirmation_overlay_rows(width, height))

        sys.stdout.write(f"{CURSOR_HIDE}{CURSOR_HOME}")
        occupied_rows = {row for row, _text, _style, _spans in rows}
        for row in range(1, height + 1):
            sys.stdout.write(f"\033[{row};1H")
            matching = [item for item in rows if item[0] == row]
            if matching:
                _row, text, style, spans = matching[-1]
                self._write_row(text, width, style, spans)
            elif row not in occupied_rows:
                sys.stdout.write(ERASE_LINE)

        if self._should_show_input_cursor():
            cursor_col = self._input_cursor_column(width)
            sys.stdout.write(f"\033[{input_row};{max(1, cursor_col)}H{CURSOR_SHOW}")
        sys.stdout.flush()

    def _should_show_input_cursor(self) -> bool:
        return (
            self._confirm_prompt is None
            and not self._thinking_indicator_visible
            and self._active_assistant_index is None
        )

    def _build_message_lines(self, width: int) -> list[TUIRenderLine]:
        lines: list[TUIRenderLine] = []
        for message_index, message in enumerate(self._messages):
            if message_index > 0:
                lines.append(TUIRenderLine("", None))

            if message.role == "header":
                lines.extend(self._build_header_box_lines(width))
                continue

            prefix = self._message_prefix(message.role, message_index)
            style = self._message_style(message.role)
            first_prefix = prefix
            continuation_prefix = "  "
            markdown_lines = _render_markdown(message.text or " ", style)
            is_first_rendered_line = True

            for markdown_line in markdown_lines:
                active_prefix = first_prefix if is_first_rendered_line else continuation_prefix
                wrapped_spans = _wrap_prefixed_spans(
                    markdown_line.spans,
                    first_width=max(1, width - _display_width(active_prefix)),
                    continuation_width=max(1, width - _display_width(continuation_prefix)),
                )
                for chunk_index, chunk_spans in enumerate(wrapped_spans):
                    line_prefix = active_prefix if chunk_index == 0 else continuation_prefix
                    line_spans = [MarkdownSpan(line_prefix, style), *chunk_spans]
                    lines.append(
                        TUIRenderLine(_plain_text(line_spans), style, line_spans)
                    )
                    is_first_rendered_line = False

        if self._thinking_indicator_visible:
            if lines:
                lines.append(TUIRenderLine("", None))
            prefix = f"{AI_PREFIX if self._assistant_prefix_visible else ' '} "
            text = "AI 正在思考"
            line_spans = [MarkdownSpan(prefix + text, WHITE)]
            lines.append(TUIRenderLine(prefix + text, WHITE, line_spans))
        return lines or [TUIRenderLine(" ", None)]

    def _message_prefix(self, role: str, message_index: int) -> str:
        if role == "user":
            return f"{USER_PREFIX} "
        if role == "assistant":
            visible = message_index != self._active_assistant_index or self._assistant_prefix_visible
            return f"{AI_PREFIX if visible else ' '} "
        if role == "status":
            return "· "
        return "! "

    @staticmethod
    def _message_style(role: str) -> str | None:
        if role in {"user", "assistant"}:
            return WHITE
        if role == "status":
            return MUTED
        if role == "header":
            return LIGHT_BLUE
        if role == "system":
            return WHITE
        return None

    def _input_line(self, width: int) -> str:
        prompt = f"{USER_PREFIX} "
        max_text_width = max(1, width - _display_width(prompt))
        return prompt + self._visible_input_text(max_text_width)

    def _visible_input_text(self, max_width: int) -> str:
        if max_width <= 0:
            return ""

        before_cursor = self._input_text[: self._input_cursor]
        visible_before = _take_tail_display_width(before_cursor, max_width)
        remaining_width = max_width - _display_width(visible_before)
        visible_after = _take_display_width(self._input_text[self._input_cursor :], remaining_width)
        return visible_before + visible_after

    def _input_cursor_column(self, width: int) -> int:
        prompt = f"{USER_PREFIX} "
        max_text_width = max(1, width - _display_width(prompt))
        visible_before = _take_tail_display_width(self._input_text[: self._input_cursor], max_text_width)
        return min(_display_width(prompt + visible_before) + 1, width + 1)

    def _write_row(
        self,
        text: str,
        width: int,
        style: str | None = None,
        spans: list[MarkdownSpan] | None = None,
    ) -> None:
        if spans is not None:
            truncated_spans = _take_spans_display_width(spans, width)
            for span in truncated_spans:
                span_style = span.style or style
                if span_style:
                    sys.stdout.write(f"{span_style}{span.text}{RESET}")
                else:
                    sys.stdout.write(span.text)
            sys.stdout.write(ERASE_LINE)
            return

        truncated = _take_display_width(text, width)
        if style:
            sys.stdout.write(f"{style}{truncated}{RESET}{ERASE_LINE}")
        else:
            sys.stdout.write(f"{truncated}{ERASE_LINE}")

    def _build_header_box_lines(self, width: int) -> list[TUIRenderLine]:
        box_width = max(4, width - 2)
        inner_width = max(1, box_width - 4)
        top_spans = [MarkdownSpan("┌" + "─" * (box_width - 2) + "┐", GRAY)]
        lines = [TUIRenderLine(_plain_text(top_spans), None, top_spans)]
        for raw_line in self._header_message.text.splitlines():
            wrapped = _wrap_display(raw_line, inner_width)
            for line in wrapped:
                row_spans = self._header_box_row_spans(line, box_width)
                lines.append(TUIRenderLine(_plain_text(row_spans), None, row_spans))
        bottom_spans = [MarkdownSpan("└" + "─" * (box_width - 2) + "┘", GRAY)]
        lines.append(TUIRenderLine(_plain_text(bottom_spans), None, bottom_spans))
        return lines

    @staticmethod
    def _header_box_row_spans(text: str, width: int) -> list[MarkdownSpan]:
        inner_width = max(0, width - 4)
        content = _take_display_width(text, inner_width)
        padding = max(0, inner_width - _display_width(content))
        return [
            MarkdownSpan("│ ", GRAY),
            MarkdownSpan(content, LIGHT_BLUE),
            MarkdownSpan(" " * padding, None),
            MarkdownSpan(" │", GRAY),
        ]

    @staticmethod
    def _box_row(text: str, width: int) -> str:
        inner_width = max(0, width - 4)
        content = _take_display_width(text, inner_width)
        padding = max(0, inner_width - _display_width(content))
        return f"│ {content}{' ' * padding} │"

    def _autocomplete_overlay_rows(
        self,
        width: int,
        above_row: int,
    ) -> list[tuple[int, str, str | None, list[MarkdownSpan] | None]]:
        """生成自动补全菜单行，显示在 above_row 上方，不覆盖该行。"""
        if not self._autocomplete_visible or not self._autocomplete_matches:
            return []

        max_items = min(len(self._autocomplete_matches), 8)
        selected = self._autocomplete_index
        start = 0
        if selected >= max_items:
            start = selected - max_items + 1
        visible = self._autocomplete_matches[start:start + max_items]

        box_width = min(max(20, max((len(m) for m in visible), default=0) + 4), width - 4)
        inner_width = max(1, box_width - 4)

        lines: list[str] = []
        lines.append("┌─ " + "补全" + " " + "─" * max(0, box_width - _display_width("┌─ 补全 ") - 1) + "┐")
        for i, match in enumerate(visible):
            actual_index = start + i
            prefix = "> " if actual_index == selected else "  "
            content = _take_display_width(prefix + match, inner_width)
            pad = max(0, inner_width - _display_width(content))
            lines.append(f"│ {content}{' ' * pad} │")
        lines.append("└" + "─" * (box_width - 2) + "┘")

        top = max(1, above_row - len(lines))
        left_padding = " " * max(0, (width - box_width) // 2)
        result: list[tuple[int, str, str | None, list[MarkdownSpan] | None]] = []
        for index, line in enumerate(lines):
            result.append((top + index, left_padding + line, LIGHT_BLUE, None))
        return result

    def _confirmation_overlay_rows(
        self,
        width: int,
        height: int,
    ) -> list[tuple[int, str, str | None, list[MarkdownSpan] | None]]:
        box_width = min(max(40, width - 8), 88)
        inner_width = max(1, box_width - 4)
        prompt = self._confirm_prompt or ""
        prompt_lines: list[str] = []
        for raw_line in prompt.splitlines() or [""]:
            prompt_lines.extend(_wrap_display(raw_line, inner_width))

        max_prompt_lines = max(3, min(10, height - 8))
        if len(prompt_lines) > max_prompt_lines:
            prompt_lines = prompt_lines[: max_prompt_lines - 1] + ["... 内容已截断，请看消息区参数。"]

        yes_label = "[ YES ]" if self._confirm_selection_yes else "  YES  "
        no_label = "  NO  " if self._confirm_selection_yes else "[ NO ]"
        button_line = f"{yes_label}    {no_label}"
        help_line = "左右箭头选择；Enter 确认；Y/N 快捷"
        box_lines = [
            "┌" + "─" * (box_width - 2) + "┐",
            self._box_row("操作确认", box_width),
            self._box_row("", box_width),
            *(self._box_row(line, box_width) for line in prompt_lines),
            self._box_row("", box_width),
            self._box_row(button_line, box_width),
            self._box_row(help_line, box_width),
            "└" + "─" * (box_width - 2) + "┘",
        ]

        top = max(1, (height - len(box_lines)) // 2)
        left_padding = " " * max(0, (width - box_width) // 2)
        return [
            (top + index, left_padding + line, LIGHT_BLUE, None)
            for index, line in enumerate(box_lines)
        ]


class FullScreenWaitingIndicator:
    """ANSI TUI 状态栏等待动画。"""

    def __init__(self, tui: FullScreenTUI, base_text: str) -> None:
        self._tui = tui
        self._base_text = base_text
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        if self._thread is not None and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self, final_status: str = "") -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None
        if final_status:
            self._tui.set_status(final_status)

    def _run(self) -> None:
        kaomoji = random.choice(WAITING_KAOMOJI)
        dot_index = 0
        while not self._stop.is_set():
            dots = WAITING_DOTS[dot_index % len(WAITING_DOTS)]
            self._tui.set_status(f"{self._base_text}  {kaomoji}{dots}")
            dot_index += 1
            self._stop.wait(0.35)


class AssistantPrefixBlinker:
    """AI 回复生成期间闪烁当前助手消息前缀。"""

    def __init__(self, tui: FullScreenTUI, interval_seconds: float = 0.45) -> None:
        self._tui = tui
        self._interval_seconds = interval_seconds
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        if self._thread is not None and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None
        self._tui.set_assistant_prefix_visible(True)

    def _run(self) -> None:
        visible = True
        while not self._stop.is_set():
            self._tui.set_assistant_prefix_visible(visible)
            visible = not visible
            self._stop.wait(self._interval_seconds)
