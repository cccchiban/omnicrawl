from __future__ import annotations

import shutil
import time

from .terminal_ui import (
    TerminalUI,
    _display_width as _terminal_display_width,
    _take_display_width as _terminal_take_display_width,
)


INLINE_COMPLETION_LIMIT = 8
INLINE_PASTE_SEQUENCE_TIMEOUT_SECONDS = 0.5
INLINE_PASTE_BURST_QUIET_SECONDS = 0.03
INLINE_BRACKETED_PASTE_ON = "\033[?2004h"
INLINE_BRACKETED_PASTE_OFF = "\033[?2004l"
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
            lines = [self._ui.muted(f"- {self._ui.model_label}")]

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
        if key == "S" and cursor < len(text):
            history_browser.reset()
            text = text[:cursor] + text[cursor + 1 :]
            _redraw_input()
            _update_matches()

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
            if sequence == "[3~" and cursor < len(text):
                history_browser.reset()
                text = text[:cursor] + text[cursor + 1 :]
                _redraw_input()
                _update_matches()
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

        if char == "\b":
            if cursor > 0:
                history_browser.reset()
                text = text[:cursor - 1] + text[cursor:]
                cursor -= 1
                _redraw_input()
                _update_matches()
            continue

        if char == "\x7f":
            if cursor < len(text):
                history_browser.reset()
                text = text[:cursor] + text[cursor + 1:]
                _redraw_input()
                _update_matches()
            continue

        if char.isprintable() or char.isspace():
            _insert_text(char)
            _extend_active_paste_burst()
