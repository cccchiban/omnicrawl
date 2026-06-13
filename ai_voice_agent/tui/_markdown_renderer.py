"""TerminalUI 的 Markdown 流式渲染 Mixin。"""

from __future__ import annotations

import shutil
import sys
import threading

from ._colors import (
    ANSI_CLEAR_TO_LINE_END,
    ANSI_RESET,
    AI_PREFIX,
    ColorRole,
    color_text,
    get_color_sequence,
)
from ._display import (
    _char_display_width,
    _contains_complex_display_width,
    _dialog_continuation_prefix,
    _display_width,
)
from ._markdown import (
    MarkdownSpan,
    MarkdownStreamState,
    _flush_markdown_table_state,
    _looks_like_block_markdown_line,
    _parse_final_inline_markdown,
    _parse_inline_markdown,
    _render_basic_markdown_stream_line,
    _render_markdown_stream_lines,
    _split_markdown_table_row,
    _stable_inline_markdown_prefix_length,
)


class _MarkdownRendererMixin:
    """Markdown 流式渲染方法，供 TerminalUI 继承。"""

    # 以下属性由 TerminalUI 提供，类型标注便于 IDE 推断
    capabilities: "any"
    _lock: threading.Lock

    def write_markdown_delta(
        self,
        delta: str,
        state: MarkdownStreamState,
    ) -> None:
        state.pending_line += delta
        while "\n" in state.pending_line:
            line, state.pending_line = state.pending_line.split("\n", 1)
            if state.passthrough_line:
                self._finish_passthrough_line(line, state, newline=True, final=True)
            else:
                line_already_previewed = state.preview_visible
                self._clear_markdown_preview(state)
                self._write_markdown_line(
                    line,
                    state,
                    line_already_started=line_already_previewed,
                )
                state.preview_needs_newline = True
        self._write_markdown_preview(state)

    def flush_markdown(self, state: MarkdownStreamState) -> None:
        if state.pending_line:
            if state.passthrough_line:
                self._finish_passthrough_line(
                    state.pending_line,
                    state,
                    newline=False,
                    final=True,
                )
                state.pending_line = ""
                self._write_pending_markdown_table(state)
                return

            if state.preview_visible:
                self._clear_markdown_preview(state)
                line = state.pending_line
                state.pending_line = ""
                self._write_markdown_line(line, state, line_already_started=True)
                self._write_pending_markdown_table(state)
                return

            self._clear_markdown_preview(state)
            line = state.pending_line
            state.pending_line = ""
            self._write_markdown_line(line, state, line_already_started=False)
            self._write_pending_markdown_table(state)
            return

        self._write_pending_markdown_table(state)

    def _write_markdown_line(
        self,
        line: str,
        state: MarkdownStreamState,
        *,
        line_already_started: bool = False,
    ) -> None:
        span_lines = _render_markdown_stream_lines(line, state, final=True)
        self._write_markdown_span_lines(
            span_lines,
            state,
            line_already_started=line_already_started,
        )

    def _write_pending_markdown_table(self, state: MarkdownStreamState) -> None:
        span_lines = _flush_markdown_table_state(state, final=True)
        self._write_markdown_span_lines(span_lines, state, line_already_started=False)

    def _write_markdown_span_lines(
        self,
        span_lines: list[list[MarkdownSpan]],
        state: MarkdownStreamState,
        *,
        line_already_started: bool,
    ) -> None:
        if not span_lines:
            return

        with self._lock:
            for line_index, spans in enumerate(span_lines):
                if line_index > 0 or (state.rendered_lines > 0 and not line_already_started):
                    self._write_ai_continuation_prefix(state)
                self._write_markdown_spans(spans, state)
                state.rendered_lines += 1
            sys.stdout.flush()

    def _write_markdown_preview(self, state: MarkdownStreamState) -> None:
        if not state.pending_line or not self.capabilities.ansi:
            return

        if state.passthrough_line:
            self._write_passthrough_delta(state)
            return

        if (
            not state.in_code_block
            and (
                state.table_header_candidate is not None
                or state.table_alignments is not None
                or _split_markdown_table_row(state.pending_line) is not None
            )
        ):
            self._clear_markdown_preview(state)
            return

        if not state.in_code_block and _looks_like_block_markdown_line(state.pending_line):
            self._clear_markdown_preview(state)
            return

        if _contains_complex_display_width(state.pending_line):
            self._start_passthrough_line(state)
            return

        stable_preview_text = self._split_stable_inline_markdown(
            state.pending_line
        )[0]
        if not stable_preview_text:
            self._clear_markdown_preview(state)
            return

        preview_state = MarkdownStreamState(in_code_block=state.in_code_block)
        spans = _render_basic_markdown_stream_line(stable_preview_text, preview_state)
        if spans is None:
            return
        preview_width = sum(_display_width(span.text) for span in spans)
        if preview_width > self._markdown_preview_max_width():
            self._start_passthrough_line(state)
            return

        with self._lock:
            if state.preview_needs_newline:
                self._write_ai_continuation_prefix(state)
                state.preview_needs_newline = False
            elif state.preview_visible and state.preview_width > 0:
                print(f"\033[{state.preview_width}D", end="")
                state.content_column = 0
            self._write_markdown_spans(spans, state)
            print(ANSI_CLEAR_TO_LINE_END, end="", flush=True)
        state.preview_width = preview_width
        state.preview_visible = True

    def _write_markdown_spans(self, spans: list[MarkdownSpan], state: MarkdownStreamState) -> None:
        for span in spans:
            self._write_wrapped_ai_text(
                span.text, state, style=span.style, color_role=span.color_role,
            )

    def _write_ai_continuation_prefix(self, state: MarkdownStreamState) -> None:
        print()
        print(_dialog_continuation_prefix(AI_PREFIX), end="")
        state.content_column = 0

    def _write_wrapped_ai_text(
        self,
        text: str,
        state: MarkdownStreamState,
        *,
        style: str | None = None,
        color_role: ColorRole | None = None,
    ) -> None:
        if not text:
            return

        width_limit = self._ai_content_width()
        chunk_chars: list[str] = []
        chunk_width = 0

        def flush_chunk() -> None:
            nonlocal chunk_chars, chunk_width
            if not chunk_chars:
                return
            chunk = "".join(chunk_chars)
            if self.capabilities.ansi:
                prefix = style or ""
                if color_role is not None:
                    prefix += get_color_sequence(color_role, self.capabilities)
                if prefix:
                    print(f"{prefix}{chunk}{ANSI_RESET}", end="")
                else:
                    print(chunk, end="")
            else:
                print(chunk, end="")
            state.content_column += chunk_width
            chunk_chars = []
            chunk_width = 0

        for char in text:
            if char == "\n":
                flush_chunk()
                self._write_ai_continuation_prefix(state)
                continue

            char_width = _char_display_width(char)
            if char_width > 0 and state.content_column + chunk_width > 0:
                if state.content_column + chunk_width + char_width > width_limit:
                    flush_chunk()
                    self._write_ai_continuation_prefix(state)

            chunk_chars.append(char)
            chunk_width += char_width
        flush_chunk()

    @staticmethod
    def _ai_content_width() -> int:
        terminal_width = shutil.get_terminal_size((100, 30)).columns
        indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
        return max(20, terminal_width - indent_width - 1)

    @staticmethod
    def _markdown_preview_max_width() -> int:
        terminal_width = shutil.get_terminal_size((100, 30)).columns
        return max(20, terminal_width - 4)

    def _start_passthrough_line(self, state: MarkdownStreamState) -> None:
        self._clear_markdown_preview(state)
        if state.preview_needs_newline:
            with self._lock:
                self._write_ai_continuation_prefix(state)
            state.preview_needs_newline = False
        stable_text, remaining_text = self._split_stable_inline_markdown(state.pending_line)
        with self._lock:
            self._write_passthrough_text(stable_text, state)
            sys.stdout.flush()
        state.passthrough_line = True
        state.pending_line = remaining_text
        state.passthrough_printed_chars = 0

    def _write_passthrough_delta(self, state: MarkdownStreamState) -> None:
        stable_text, remaining_text = self._split_stable_inline_markdown(
            state.pending_line[state.passthrough_printed_chars:]
        )
        if not stable_text:
            return
        with self._lock:
            self._write_passthrough_text(stable_text, state)
            sys.stdout.flush()
        state.pending_line = remaining_text
        state.passthrough_printed_chars = 0

    def _finish_passthrough_line(
        self,
        line: str,
        state: MarkdownStreamState,
        *,
        newline: bool,
        final: bool = False,
    ) -> None:
        unprinted = line[state.passthrough_printed_chars:]
        stable_length = _stable_inline_markdown_prefix_length(unprinted, final=final)
        stable_text = unprinted[:stable_length]
        remaining_text = unprinted[stable_length:]
        with self._lock:
            if stable_text:
                self._write_passthrough_text_with_mode(stable_text, state, final=final)
            if newline:
                self._write_ai_continuation_prefix(state)
            sys.stdout.flush()

        _render_basic_markdown_stream_line(line, state)
        state.rendered_lines += 1
        state.passthrough_line = False
        state.passthrough_printed_chars = 0
        if not newline and not final:
            state.pending_line = remaining_text
        state.preview_needs_newline = False

    @staticmethod
    def _split_stable_inline_markdown(text: str) -> tuple[str, str]:
        stable_length = _stable_inline_markdown_prefix_length(text)
        return text[:stable_length], text[stable_length:]

    def _write_passthrough_text(self, text: str, state: MarkdownStreamState) -> None:
        self._write_passthrough_text_with_mode(text, state, final=False)

    def _write_passthrough_text_with_mode(
        self,
        text: str,
        state: MarkdownStreamState,
        *,
        final: bool,
    ) -> None:
        if not text:
            return
        if state.in_code_block:
            self._write_wrapped_ai_text(text, state)
            return
        parser = _parse_final_inline_markdown if final else _parse_inline_markdown
        self._write_markdown_spans(parser(text), state)

    def _clear_markdown_preview(self, state: MarkdownStreamState) -> None:
        if not state.preview_visible or not self.capabilities.ansi:
            return
        with self._lock:
            if state.preview_width > 0:
                print(f"\033[{state.preview_width}D", end="")
            print(ANSI_CLEAR_TO_LINE_END, end="", flush=True)
        state.preview_width = 0
        state.preview_visible = False
        state.content_column = 0
