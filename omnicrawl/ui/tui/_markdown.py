"""Markdown 流式解析与渲染。"""

from __future__ import annotations


import re
from dataclasses import dataclass, field

from ._colors import (
    ANSI_BOLD,
    COLOR_ACCENT,
    COLOR_MUTED,
    COLOR_SECONDARY,
    ColorRole,
)
from ._display import _display_width


# ── 数据类 ────────────────────────────────────────────────────

@dataclass
class MarkdownSpan:
    """Markdown 行内片段。

    渲染优先级：color_role（语义色路由）> style（ANSI SGR 前缀）。
    """

    text: str
    style: str | None = None
    color_role: ColorRole | None = None


@dataclass
class MarkdownStreamState:
    """记录流式 Markdown 渲染所需的跨行状态。"""

    in_code_block: bool = False
    rendered_lines: int = 0
    pending_line: str = ""
    preview_width: int = 0
    preview_visible: bool = False
    preview_needs_newline: bool = False
    passthrough_line: bool = False
    passthrough_printed_chars: int = 0
    content_column: int = 0
    table_header_candidate: str | None = None
    table_header_cells: list[str] | None = None
    table_alignments: list[str] | None = None
    table_rows: list[list[str]] = field(default_factory=list)


# ── 辅助函数 ──────────────────────────────────────────────────

def _style(*styles: str | None) -> str | None:
    return "".join(style for style in styles if style) or None


def _append_markdown_span(
    spans: list[MarkdownSpan],
    text: str,
    style: str | None,
    color_role: ColorRole | None = None,
) -> None:
    if not text:
        return
    if spans and spans[-1].style == style and spans[-1].color_role == color_role:
        spans[-1].text += text
    else:
        spans.append(MarkdownSpan(text, style, color_role))


def _parse_inline_markdown(text: str, default_style: str | None = None) -> list[MarkdownSpan]:
    """解析 TUI 中最常见的行内 Markdown 标记。"""

    pattern = re.compile(
        r"(`[^`\n]+`|\*\*[^*\n]+\*\*|__[^_\n]+__|\*[^*\n]+\*|_[^_\n]+_|\[[^\]]+\]\([^)]+\))"
    )
    spans: list[MarkdownSpan] = []
    cursor = 0
    for match in pattern.finditer(text):
        _append_markdown_span(spans, text[cursor:match.start()], default_style)
        token = match.group(0)

        if token.startswith("`") and token.endswith("`"):
            _append_markdown_span(spans, token[1:-1], _style(ANSI_BOLD, COLOR_ACCENT))
        elif token.startswith(("**", "__")) and token.endswith(("**", "__")):
            _append_markdown_span(spans, token[2:-2], ANSI_BOLD, color_role="primary")
        elif token.startswith("["):
            link_match = re.fullmatch(r"\[([^\]]+)\]\(([^)]+)\)", token)
            if link_match is not None:
                label, url = link_match.groups()
                _append_markdown_span(spans, f"{label} ({url})", default_style)
            else:
                _append_markdown_span(spans, token, default_style)
        else:
            _append_markdown_span(spans, token[1:-1], default_style)

        cursor = match.end()

    _append_markdown_span(spans, text[cursor:], default_style)
    return spans or [MarkdownSpan("", default_style)]


def _parse_final_inline_markdown(text: str, default_style: str | None = None) -> list[MarkdownSpan]:
    """最终落盘时解析行内 Markdown，并容错未闭合的常见标记。"""

    spans = _parse_inline_markdown(text, default_style)
    if len(spans) != 1 or spans[0].text != text or spans[0].style != default_style:
        return spans

    delimiter_positions = [
        (position, delimiter)
        for delimiter in ("**", "__", "`")
        if (position := text.find(delimiter)) >= 0
    ]
    if not delimiter_positions:
        return spans

    position, delimiter = min(delimiter_positions)
    marker_style = ANSI_BOLD
    marker_role: ColorRole | None = "primary" if delimiter in ("**", "__") else None
    return [
        MarkdownSpan(text[:position], default_style),
        MarkdownSpan(text[position + len(delimiter):], marker_style, marker_role),
    ]


def _stable_inline_markdown_prefix_length(text: str, *, final: bool = False) -> int:
    """返回可安全渲染的行内 Markdown 前缀长度。"""

    if final:
        return len(text)

    pending_indexes: list[int] = []
    for delimiter in ("**", "__", "`"):
        unmatched_index: int | None = None
        index = 0
        while True:
            delimiter_index = text.find(delimiter, index)
            if delimiter_index < 0:
                break
            if unmatched_index is None:
                unmatched_index = delimiter_index
            else:
                unmatched_index = None
            index = delimiter_index + len(delimiter)

        if unmatched_index is not None:
            pending_indexes.append(unmatched_index)

    for delimiter in ("*", "_"):
        if text.endswith(delimiter) and not text.endswith(delimiter * 2):
            pending_indexes.append(len(text) - 1)

    return min(pending_indexes) if pending_indexes else len(text)


def _looks_like_block_markdown_line(text: str) -> bool:
    """判断当前行是否应等到换行后按块级 Markdown 渲染。"""

    if not text.strip():
        return False
    return any(
        re.match(pattern, text) is not None
        for pattern in (
            r"^\s{0,3}#{1,6}(?:\s|$)",
            r"^\s{0,3}>\s?",
            r"^\s{0,3}```",
            r"^\s*[-*+]\s+(?:\[[ xX]\]\s+)?",
            r"^\s*\d+[.)]\s+",
            r"^\s{0,3}([-*_]\s*){3,}$",
        )
    )


def _spans_display_width(spans: list[MarkdownSpan]) -> int:
    return sum(_display_width(span.text) for span in spans)


# ── 表格渲染 ──────────────────────────────────────────────────

def _split_markdown_table_row(raw_line: str) -> list[str] | None:
    """按未转义的管道符拆分 Markdown 表格行。"""

    line = raw_line.strip()
    if "|" not in line:
        return None

    cells: list[str] = []
    chars: list[str] = []
    saw_pipe = False
    escaped = False
    for char in line:
        if escaped:
            if char == "|":
                chars.append("|")
            else:
                chars.append("\\")
                chars.append(char)
            escaped = False
            continue

        if char == "\\":
            escaped = True
            continue
        if char == "|":
            saw_pipe = True
            cells.append("".join(chars).strip())
            chars = []
            continue
        chars.append(char)

    if escaped:
        chars.append("\\")
    cells.append("".join(chars).strip())

    if not saw_pipe:
        return None
    if cells and cells[0] == "" and line.startswith("|"):
        cells = cells[1:]
    if cells and cells[-1] == "" and line.endswith("|"):
        cells = cells[:-1]
    if len(cells) < 2:
        return None
    return cells


def _parse_markdown_table_delimiter(raw_line: str) -> list[str] | None:
    cells = _split_markdown_table_row(raw_line)
    if cells is None:
        return None

    alignments: list[str] = []
    for cell in cells:
        marker = re.sub(r"\s+", "", cell)
        if re.fullmatch(r":?-{3,}:?", marker) is None:
            return None
        if marker.startswith(":") and marker.endswith(":"):
            alignments.append("center")
        elif marker.endswith(":"):
            alignments.append("right")
        else:
            alignments.append("left")
    return alignments


def _append_table_cell(
    spans: list[MarkdownSpan],
    cell: str,
    width: int,
    alignment: str,
    style: str | None,
    color_role: ColorRole | None = None,
) -> None:
    cell_spans = _parse_inline_markdown(cell, style)
    # 为加粗 span 补充语义色角色
    if color_role is not None:
        for i, span in enumerate(cell_spans):
            if span.style == ANSI_BOLD and span.color_role is None:
                cell_spans[i] = MarkdownSpan(span.text, span.style, color_role)
            elif span.style is None:
                cell_spans[i] = MarkdownSpan(span.text, style, color_role)
    cell_width = _spans_display_width(cell_spans)
    padding = max(0, width - cell_width)
    if alignment == "right":
        left_padding = padding
        right_padding = 0
    elif alignment == "center":
        left_padding = padding // 2
        right_padding = padding - left_padding
    else:
        left_padding = 0
        right_padding = padding

    _append_markdown_span(spans, " " * left_padding, style)
    for span in cell_spans:
        _append_markdown_span(spans, span.text, span.style, span.color_role)
    _append_markdown_span(spans, " " * right_padding, style)


def _render_markdown_table_top_border(widths: list[int]) -> list[MarkdownSpan]:
    """渲染表格上边框：╭───┬───╮

    ┬ 的位置与数据行 │ 对齐（预留 │ 两侧的空格）。
    """
    spans: list[MarkdownSpan] = []
    _append_markdown_span(spans, "╭", COLOR_MUTED)
    for index, width in enumerate(widths):
        if index > 0:
            _append_markdown_span(spans, "─┬─", COLOR_MUTED)
        else:
            _append_markdown_span(spans, "─", COLOR_MUTED)
        _append_markdown_span(spans, "─" * width, COLOR_MUTED)
    _append_markdown_span(spans, "─╮", COLOR_MUTED)
    return spans


def _render_markdown_table_bottom_border(widths: list[int]) -> list[MarkdownSpan]:
    """渲染表格下边框：╰───┴───╯

    ┴ 的位置与数据行 │ 对齐。
    """
    spans: list[MarkdownSpan] = []
    _append_markdown_span(spans, "╰", COLOR_MUTED)
    for index, width in enumerate(widths):
        if index > 0:
            _append_markdown_span(spans, "─┴─", COLOR_MUTED)
        else:
            _append_markdown_span(spans, "─", COLOR_MUTED)
        _append_markdown_span(spans, "─" * width, COLOR_MUTED)
    _append_markdown_span(spans, "─╯", COLOR_MUTED)
    return spans


def _render_markdown_table_separator(widths: list[int]) -> list[MarkdownSpan]:
    """渲染表格分隔线（header 下方）：├─...─┼─...─┤"""
    spans: list[MarkdownSpan] = []
    _append_markdown_span(spans, "├", COLOR_MUTED)
    for index, width in enumerate(widths):
        if index > 0:
            _append_markdown_span(spans, "─┼─", COLOR_MUTED)
        else:
            _append_markdown_span(spans, "─", COLOR_MUTED)
        _append_markdown_span(spans, "─" * width, COLOR_MUTED)
    _append_markdown_span(spans, "─┤", COLOR_MUTED)
    return spans


def _render_markdown_table_row(
    cells: list[str],
    widths: list[int],
    alignments: list[str],
    style: str | None,
    header_role: ColorRole | None = None,
) -> list[MarkdownSpan]:
    spans: list[MarkdownSpan] = []
    _append_markdown_span(spans, "│ ", COLOR_MUTED)
    for index, width in enumerate(widths):
        if index > 0:
            _append_markdown_span(spans, " │ ", COLOR_MUTED)
        _append_table_cell(spans, cells[index], width, alignments[index], style, color_role=header_role)
    _append_markdown_span(spans, " │", COLOR_MUTED)
    return spans or [MarkdownSpan("", style)]


def _render_markdown_table(
    header_cells: list[str],
    alignments: list[str],
    rows: list[list[str]],
    default_style: str | None,
) -> list[list[MarkdownSpan]]:
    column_count = len(header_cells)
    table_rows = [row for row in rows if len(row) == column_count]
    header_style = ANSI_BOLD
    widths: list[int] = []
    for column_index in range(column_count):
        column_cells = [header_cells[column_index], *(row[column_index] for row in table_rows)]
        cell_width = max(
            (_spans_display_width(_parse_inline_markdown(cell, default_style)) for cell in column_cells),
            default=0,
        )
        widths.append(max(3, cell_width))

    # 渲染上边框
    top_border = _render_markdown_table_top_border(widths)

    return [
        top_border,
        _render_markdown_table_row(
            header_cells, widths, alignments, header_style, header_role="primary",
        ),
        _render_markdown_table_separator(widths),
        *(
            _render_markdown_table_row(row, widths, alignments, default_style)
            for row in table_rows
        ),
        _render_markdown_table_bottom_border(widths),
    ]


def _clear_markdown_table_state(state: MarkdownStreamState) -> None:
    state.table_header_candidate = None
    state.table_header_cells = None
    state.table_alignments = None
    state.table_rows.clear()


# ── 行级渲染 ──────────────────────────────────────────────────

def _render_basic_markdown_stream_line(
    raw_line: str,
    state: MarkdownStreamState,
    default_style: str | None = None,
    *,
    final: bool = False,
) -> list[MarkdownSpan] | None:
    """把一行 Markdown 转成终端可写的片段。

    返回 None 表示这一行只是代码块围栏，不应单独显示。
    """

    stripped = raw_line.strip()
    if stripped.startswith("```"):
        state.in_code_block = not state.in_code_block
        return None

    if state.in_code_block:
        return [MarkdownSpan(f"▎ {raw_line}", default_style)]

    if not stripped:
        return None

    inline_parser = _parse_final_inline_markdown if final else _parse_inline_markdown

    heading_match = re.match(r"^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$", raw_line)
    if heading_match is not None:
        # 标题增加左侧色条前缀
        spans = [MarkdownSpan("█ ", COLOR_SECONDARY)]
        heading_spans = inline_parser(heading_match.group(2), ANSI_BOLD)
        # 标题文字统一用 primary 语义色
        for i, span in enumerate(heading_spans):
            if span.color_role is None:
                heading_spans[i] = MarkdownSpan(span.text, span.style, "primary")
        spans.extend(heading_spans)
        return spans

    if re.match(r"^\s{0,3}([-*_]\s*){3,}$", raw_line):
        return [MarkdownSpan("─" * 20, COLOR_MUTED)]

    quote_match = re.match(r"^\s{0,3}>\s?(.*)$", raw_line)
    if quote_match is not None:
        spans = [MarkdownSpan("║ ", COLOR_SECONDARY)]
        spans.extend(inline_parser(quote_match.group(1), default_style))
        return spans

    task_match = re.match(r"^(\s*)[-*+]\s+\[([ xX])\]\s+(.*)$", raw_line)
    if task_match is not None:
        indent, checked, body = task_match.groups()
        marker = "[x] " if checked.lower() == "x" else "[ ] "
        spans = [MarkdownSpan(indent + marker, default_style)]
        spans.extend(inline_parser(body, default_style))
        return spans

    list_match = re.match(r"^(\s*)([-*+]|\d+[.)])\s+(.*)$", raw_line)
    if list_match is not None:
        indent, marker, body = list_match.groups()
        if indent and marker in {"-", "*", "+"}:
            # 子列表使用 ◦ 标记
            normalized_marker = "◦ "
        elif marker in {"-", "*", "+"}:
            # 顶级列表使用 • 标记（避免与用户输入前缀 $ 冲突）
            normalized_marker = "• "
        else:
            normalized_marker = f"{marker} "
        spans = [MarkdownSpan(indent + normalized_marker, default_style)]
        spans.extend(inline_parser(body, default_style))
        return spans

    return inline_parser(raw_line, default_style)


def _flush_markdown_table_state(
    state: MarkdownStreamState,
    default_style: str | None = None,
    *,
    final: bool = False,
) -> list[list[MarkdownSpan]]:
    if state.table_alignments is not None and state.table_header_cells is not None:
        rendered = _render_markdown_table(
            state.table_header_cells,
            state.table_alignments,
            state.table_rows,
            default_style,
        )
        _clear_markdown_table_state(state)
        return rendered

    if state.table_header_candidate is not None:
        raw_line = state.table_header_candidate
        _clear_markdown_table_state(state)
        spans = _render_basic_markdown_stream_line(
            raw_line,
            state,
            default_style,
            final=final,
        )
        return [] if spans is None else [spans]

    return []


def _render_markdown_stream_lines(
    raw_line: str,
    state: MarkdownStreamState,
    default_style: str | None = None,
    *,
    final: bool = False,
) -> list[list[MarkdownSpan]]:
    """渲染一条完整 Markdown 输入行。"""

    rendered: list[list[MarkdownSpan]] = []

    if state.table_alignments is not None:
        cells = _split_markdown_table_row(raw_line)
        if cells is not None and len(cells) == len(state.table_alignments):
            state.table_rows.append(cells)
            return rendered
        rendered.extend(_flush_markdown_table_state(state, default_style, final=final))
    elif state.table_header_candidate is not None:
        if not raw_line.strip():
            return rendered

        alignments = _parse_markdown_table_delimiter(raw_line)
        if (
            alignments is not None
            and state.table_header_cells is not None
            and len(alignments) == len(state.table_header_cells)
        ):
            state.table_alignments = alignments
            return rendered
        rendered.extend(_flush_markdown_table_state(state, default_style, final=final))

    if not state.in_code_block:
        cells = _split_markdown_table_row(raw_line)
        if cells is not None and _parse_markdown_table_delimiter(raw_line) is None:
            state.table_header_candidate = raw_line
            state.table_header_cells = cells
            return rendered

    spans = _render_basic_markdown_stream_line(
        raw_line,
        state,
        default_style,
        final=final,
    )
    if spans is not None:
        rendered.append(spans)
    return rendered
