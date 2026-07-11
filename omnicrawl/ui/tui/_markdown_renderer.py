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
    _display_unit_width,
    _display_width,
    _iter_display_units,
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
        """追加模型分片，仅在完整逻辑行到达时提交 Markdown。

        终端无法可靠地按 Unicode 显示列回退重绘历史，尤其是 Windows Terminal
        遇到 CJK、emoji、自动换行或滚动时会留下残字。未结束的行先保存在
        ``pending_line``，收到换行或本轮结束后再稳定输出，保证终端历史只追加。
        """

        state.pending_line += delta
        while "\n" in state.pending_line:
            line, state.pending_line = state.pending_line.split("\n", 1)
            self._write_markdown_line(line, state)
            state.preview_needs_newline = True

    def flush_markdown(self, state: MarkdownStreamState) -> None:
        if state.pending_line:
            line = state.pending_line
            state.pending_line = ""
            self._write_markdown_line(line, state)

        self._write_pending_markdown_table(state)

    def _write_markdown_line(
        self,
        line: str,
        state: MarkdownStreamState,
    ) -> None:
        span_lines = _render_markdown_stream_lines(line, state, final=True)
        self._write_markdown_span_lines(
            span_lines,
            state,
            line_already_started=False,
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
        chunk_units: list[str] = []
        chunk_width = 0

        def flush_chunk() -> None:
            nonlocal chunk_units, chunk_width
            if not chunk_units:
                return
            chunk = "".join(chunk_units)
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
            chunk_units = []
            chunk_width = 0

        # 所有输出路径均以显示单元而非 Unicode 码点换行，保证 ZWJ、旗帜和
        # 肤色组合不会被续行前缀切开。
        for unit in _iter_display_units(text):
            if unit == "\n":
                flush_chunk()
                self._write_ai_continuation_prefix(state)
                continue

            unit_width = _display_unit_width(unit)
            if unit_width > 0 and state.content_column + chunk_width > 0:
                if state.content_column + chunk_width + unit_width > width_limit:
                    flush_chunk()
                    self._write_ai_continuation_prefix(state)

            chunk_units.append(unit)
            chunk_width += unit_width
        flush_chunk()

    @staticmethod
    def _ai_content_width() -> int:
        terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
        indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
        return max(1, terminal_width - indent_width - 1)
