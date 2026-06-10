from __future__ import annotations

import os
import random
import re
import shutil
import sys
import threading
import ctypes
import json
import unicodedata
from dataclasses import dataclass, field
from typing import Any


AI_PREFIX = "^"
USER_PREFIX = ">"
ANSI_CLEAR_LINE = "\033[2K"
ANSI_CLEAR_TO_LINE_END = "\033[K"
ANSI_PREVIOUS_LINE = "\033[1A"
ANSI_MUTED = "\033[2;90m"
ANSI_GRAY = "\033[90m"
ANSI_BRIGHT_WHITE = "\033[97m"
ANSI_LIGHT_BLUE = "\033[94m"
ANSI_BOLD = "\033[1m"
ANSI_BLINK = "\033[5m"
ANSI_MARKDOWN_STRONG = "\033[1;96m"
ANSI_DIM_YELLOW = "\033[2;33m"
ANSI_GREEN = "\033[32m"
ANSI_RED = "\033[31m"
ANSI_RESET = "\033[0m"
ANSI_SAVE_CURSOR = "\033[s"
ANSI_RESTORE_CURSOR = "\033[u"
ANSI_ERASE_TO_END = "\033[J"
WAITING_KAOMOJI = (
    "(^_^)",
    "(._.)",
    "(-_-)",
    "(o_o)",
)
WAITING_DOTS = ("", ".", "..", "...", "..", ".")
INLINE_INPUT_WINDOW_ROWS = 8
TOOL_DETAIL_MAX_ROWS = 3
TOOL_OUTPUT_MAX_ROWS = 6


def _dialog_continuation_prefix(prefix: str) -> str:
    return " " * _display_width(f"{prefix} ")


def _char_display_width(char: str) -> int:
    category = unicodedata.category(char)
    if unicodedata.combining(char) or category in {"Mn", "Me", "Cf"}:
        return 0
    if category.startswith("C"):
        return 0
    return 2 if unicodedata.east_asian_width(char) in {"F", "W"} else 1


def _display_width(text: str) -> int:
    return sum(_char_display_width(char) for char in text)


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


def _ellipsize_display_text(text: str, max_width: int) -> str:
    """按视觉宽度截断单行文本，并保留省略号空间。"""

    if max_width <= 0:
        return ""
    if _display_width(text) <= max_width:
        return text
    if max_width <= 3:
        return _take_display_width(text, max_width)
    return f"{_take_display_width(text, max_width - 3)}..."


def _contains_complex_display_width(text: str) -> bool:
    """判断文本是否含有不适合 ANSI 原地预览重绘的字符。

    Windows 终端、字体和代码页对 CJK、emoji、组合符号的列宽处理并不完全一致。
    对这些字符继续使用“光标左移 N 列后整行重绘”容易留下旧字符，表现为
    “获获取取”这类重复字。遇到复杂宽度字符时改用追加输出，避免依赖列宽回退。
    """

    return any(_char_display_width(char) != 1 for char in text)


def _normalize_terminal_text(text: str) -> str:
    """统一终端文本换行，避免 CRLF 在行数计算里被当成额外字符。"""

    return text.replace("\r\n", "\n").replace("\r", "\n")


def _compact_json(value: Any) -> str:
    """把工具参数压成单行 JSON，供 TUI 摘要展示。"""

    try:
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    except (TypeError, ValueError):
        return str(value)


def _split_display_rows(text: str, max_width: int) -> list[str]:
    """按终端视觉宽度拆分文本行，不包含输入提示符。"""

    max_width = max(1, max_width)
    rows: list[str] = []
    for raw_line in _normalize_terminal_text(text).split("\n"):
        remaining = raw_line
        if not remaining:
            rows.append("")
            continue
        while remaining:
            chunk = _take_display_width(remaining, max_width)
            if not chunk:
                # 极端情况下单个字符宽度大于可用宽度，仍要消费一个字符避免死循环。
                chunk = remaining[0]
            rows.append(chunk)
            remaining = remaining[len(chunk) :]
    return rows or [""]


def _preview_display_rows(
    text: str,
    max_width: int,
    max_rows: int,
    *,
    empty_text: str,
) -> list[str]:
    """生成适合工具记录展示的有限行预览。

    工具参数和命令输出可能很长；TUI 只展示前几行和省略计数，完整内容仍在
    Agent 的工具观察里交给模型使用，避免终端主线被大段 stdout 淹没。
    """

    normalized = _normalize_terminal_text(text).strip()
    if not normalized:
        return [empty_text]

    rows = _split_display_rows(normalized, max_width)
    if len(rows) <= max_rows:
        return rows
    visible_count = max(1, max_rows)
    hidden_count = len(rows) - visible_count
    return [*rows[:visible_count], f"... +{hidden_count} lines"]


@dataclass(frozen=True)
class TerminalCapabilities:
    """当前终端可用能力。

    终端样式本质由终端模拟器决定。这里仅判断是否适合输出 ANSI 控制序列，
    不尝试模拟真正的小字体或复杂 TUI。
    """

    ansi: bool


@dataclass
class MarkdownSpan:
    """Markdown 行内片段，style 为 ANSI SGR 前缀。"""

    text: str
    style: str | None = None


@dataclass
class MarkdownStreamState:
    """记录流式 Markdown 渲染所需的跨行状态。

    TUI 中的 AI 回复是按增量返回的，当前行会反复重绘成 Markdown 预览；
    代码块围栏会跨多行生效，因此需要把状态保存在播放器实例里。
    """

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


@dataclass
class ToolDisplayState:
    """记录一次工具执行块，供执行完成后把运行态标记更新为完成态。"""

    step: int
    tool_name: str
    line_count: int


def _style(*styles: str | None) -> str | None:
    return "".join(style for style in styles if style) or None


def _markdown_strong_style(default_style: str | None = None) -> str | None:
    """返回 Markdown 加粗样式。

    真实终端对 SGR 1 的支持不稳定，尤其中文字体可能看不出明显字重变化。
    这里叠加高亮色，让 `**重点**` 即使在不支持粗体字重的终端里也能被识别。
    """

    return _style(default_style, ANSI_MARKDOWN_STRONG)


def _append_markdown_span(
    spans: list[MarkdownSpan],
    text: str,
    style: str | None,
) -> None:
    if not text:
        return
    if spans and spans[-1].style == style:
        spans[-1].text += text
    else:
        spans.append(MarkdownSpan(text, style))


def _parse_inline_markdown(text: str, default_style: str | None = None) -> list[MarkdownSpan]:
    """解析 TUI 中最常见的行内 Markdown 标记。

    这里保持轻量，不引入 rich 等新依赖；目标是让 AI 常输出的标题、列表、
    加粗、代码和链接不再以原始 Markdown 标记裸露在终端里。
    """

    pattern = re.compile(
        r"(`[^`\n]+`|\*\*[^*\n]+\*\*|__[^_\n]+__|\*[^*\n]+\*|_[^_\n]+_|\[[^\]]+\]\([^)]+\))"
    )
    spans: list[MarkdownSpan] = []
    cursor = 0
    for match in pattern.finditer(text):
        _append_markdown_span(spans, text[cursor : match.start()], default_style)
        token = match.group(0)

        if token.startswith("`") and token.endswith("`"):
            _append_markdown_span(spans, token[1:-1], _style(ANSI_BOLD, default_style))
        elif token.startswith(("**", "__")) and token.endswith(("**", "__")):
            _append_markdown_span(spans, token[2:-2], _markdown_strong_style(default_style))
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
    marker_style = _markdown_strong_style(default_style)
    return [
        MarkdownSpan(text[:position], default_style),
        MarkdownSpan(text[position + len(delimiter) :], marker_style),
    ]


def _stable_inline_markdown_prefix_length(text: str, *, final: bool = False) -> int:
    """返回可安全渲染的行内 Markdown 前缀长度。

    AI 回复是逐字/逐片段流式到达的，`**结论**` 可能先到达 `**结`。
    如果此时立即渲染，用户会短暂看到裸露的 Markdown 标记；这里会把未闭合
    的加粗或行内代码标记留在缓冲区，等闭合标记到达后再一次性按样式输出。
    """

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


def _spans_display_width(spans: list[MarkdownSpan]) -> int:
    return sum(_display_width(span.text) for span in spans)


def _split_markdown_table_row(raw_line: str) -> list[str] | None:
    """按未转义的管道符拆分 Markdown 表格行。

    这里仅识别标准管道表格的文本边界，不做完整 Markdown 语法解析。单元格内的
    `\|` 会还原成普通竖线，避免把常见转义内容误切成新列。
    """

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
) -> None:
    cell_spans = _parse_inline_markdown(cell, style)
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
        _append_markdown_span(spans, span.text, span.style)
    _append_markdown_span(spans, " " * right_padding, style)


def _render_markdown_table_separator(widths: list[int]) -> list[MarkdownSpan]:
    spans: list[MarkdownSpan] = []
    for index, width in enumerate(widths):
        if index > 0:
            _append_markdown_span(spans, "─┼─", ANSI_MUTED)
        _append_markdown_span(spans, "─" * width, ANSI_MUTED)
    return spans


def _render_markdown_table_row(
    cells: list[str],
    widths: list[int],
    alignments: list[str],
    style: str | None,
) -> list[MarkdownSpan]:
    spans: list[MarkdownSpan] = []
    for index, width in enumerate(widths):
        if index > 0:
            _append_markdown_span(spans, " │ ", ANSI_MUTED)
        _append_table_cell(spans, cells[index], width, alignments[index], style)
    return spans or [MarkdownSpan("", style)]


def _render_markdown_table(
    header_cells: list[str],
    alignments: list[str],
    rows: list[list[str]],
    default_style: str | None,
) -> list[list[MarkdownSpan]]:
    column_count = len(header_cells)
    table_rows = [row for row in rows if len(row) == column_count]
    header_style = _markdown_strong_style(default_style)
    widths: list[int] = []
    for column_index in range(column_count):
        column_cells = [header_cells[column_index], *(row[column_index] for row in table_rows)]
        cell_width = max(
            (_spans_display_width(_parse_inline_markdown(cell, default_style)) for cell in column_cells),
            default=0,
        )
        widths.append(max(3, cell_width))

    return [
        _render_markdown_table_row(header_cells, widths, alignments, header_style),
        _render_markdown_table_separator(widths),
        *(
            _render_markdown_table_row(row, widths, alignments, default_style)
            for row in table_rows
        ),
    ]


def _clear_markdown_table_state(state: MarkdownStreamState) -> None:
    state.table_header_candidate = None
    state.table_header_cells = None
    state.table_alignments = None
    state.table_rows.clear()


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
        return [MarkdownSpan(f"    {raw_line}", default_style)]

    if not stripped:
        return None

    inline_parser = _parse_final_inline_markdown if final else _parse_inline_markdown

    heading_match = re.match(r"^\s{0,3}(#{1,6})\s+(.+?)\s*#*\s*$", raw_line)
    if heading_match is not None:
        return inline_parser(heading_match.group(2), _markdown_strong_style(default_style))

    if re.match(r"^\s{0,3}([-*_]\s*){3,}$", raw_line):
        return [MarkdownSpan("─" * 20, ANSI_MUTED)]

    quote_match = re.match(r"^\s{0,3}>\s?(.*)$", raw_line)
    if quote_match is not None:
        spans = [MarkdownSpan("│ ", ANSI_LIGHT_BLUE)]
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
        normalized_marker = f"{marker} " if marker[0].isdigit() else "- "
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
    """渲染一条完整 Markdown 输入行。

    管道表格必须看到"表头行 + 分隔行"才能确认。普通流式输出会提前逐行写入，
    因此这里为疑似表头保留一行缓冲；确认进入表格后，再等到表格块结束时统一
    输出等宽列，避免用户看到先打印原始表头、再补画表格的抖动。
    """

    rendered: list[list[MarkdownSpan]] = []

    if state.table_alignments is not None:
        cells = _split_markdown_table_row(raw_line)
        if cells is not None and len(cells) == len(state.table_alignments):
            state.table_rows.append(cells)
            return rendered
        rendered.extend(_flush_markdown_table_state(state, default_style, final=final))
    elif state.table_header_candidate is not None:
        if not raw_line.strip():
            # 模型常会在表头和分隔行之间插入空行。标准 Markdown 不允许这样写，
            # 但在终端里直接打印 `|---|---|` 更糟；这里忽略这些空行，继续等待
            # 分隔行，以便把整张表按对齐后的 TUI 表格输出。
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


def detect_capabilities() -> TerminalCapabilities:
    """根据环境判断是否启用 ANSI 样式和行重绘。"""

    if os.getenv("NO_COLOR"):
        return TerminalCapabilities(ansi=False)
    if os.name == "nt":
        return TerminalCapabilities(
            ansi=bool(
                os.getenv("WT_SESSION")
                or os.getenv("TERM_PROGRAM")
                or os.getenv("ANSICON")
                or os.getenv("ConEmuANSI") == "ON"
                or "xterm" in os.getenv("TERM", "").lower()
                or _enable_windows_virtual_terminal()
            )
        )

    return TerminalCapabilities(ansi=sys.stdout.isatty() and os.getenv("TERM") != "dumb")


def _enable_windows_virtual_terminal() -> bool:
    """在 Windows 控制台中开启 ANSI/VT 控制序列支持。"""

    if not sys.stdout.isatty():
        return False

    kernel32 = ctypes.windll.kernel32
    handle = kernel32.GetStdHandle(-11)
    if handle == -1:
        return False

    mode = ctypes.c_uint32()
    if not kernel32.GetConsoleMode(handle, ctypes.byref(mode)):
        return False

    ENABLE_VIRTUAL_TERMINAL_PROCESSING = 0x0004
    updated_mode = mode.value | ENABLE_VIRTUAL_TERMINAL_PROCESSING
    if not kernel32.SetConsoleMode(handle, updated_mode):
        return False

    return True


class TerminalUI:
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

    def muted(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return f"{ANSI_MUTED}{text}{ANSI_RESET}"

    def result_text(self, ok: bool, text: str) -> str:
        """按工具执行结果给单个结果词着色，避免整段记录被误读成模型回复。"""

        if not self.capabilities.ansi:
            return text
        color = ANSI_GREEN if ok else ANSI_RED
        return f"{color}{text}{ANSI_RESET}"

    def accent(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return f"{ANSI_LIGHT_BLUE}{text}{ANSI_RESET}"

    def bright(self, text: str) -> str:
        if not self.capabilities.ansi:
            return text
        return f"{ANSI_BRIGHT_WHITE}{text}{ANSI_RESET}"

    def update_token_usage(self, input_tokens: int, output_tokens: int) -> None:
        """更新最近一次模型 token 统计；输入框下方状态行在下次按键时即时重绘。"""

        with self._lock:
            self._input_tokens = max(0, int(input_tokens))
            self._output_tokens = max(0, int(output_tokens))

    def print_tool_call_start(
        self,
        step: int,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        leading_blank: bool = True,
    ) -> ToolDisplayState:
        """打印工具开始执行的结构化记录。

        首行保留“步骤”语义，前置闪烁星号表示当前工具正在运行；后续行只展示
        命令或参数摘要，完整输出仍通过工具结果传给模型，避免 TUI 被长日志淹没。
        """

        indent = _dialog_continuation_prefix(AI_PREFIX)
        marker = f"{ANSI_BLINK}*{ANSI_RESET}" if self.capabilities.ansi else "*"
        line_width = shutil.get_terminal_size((100, 30)).columns
        tool_width = max(
            1,
            line_width
            - _display_width(indent)
            - _display_width("* 步骤 ")
            - _display_width(str(max(1, step)))
            - _display_width(" — 请求 ")
            - 1,
        )
        visible_tool_name = _ellipsize_display_text(tool_name, tool_width)
        header = (
            f"{indent}{marker} {self.muted('步骤 ')}"
            f"{self.result_text(True, str(max(1, step)))}"
            f"{self.muted(' — 请求 ')}{self.accent(visible_tool_name)}"
        )
        detail_rows = self._tool_call_detail_rows(tool_name, arguments)
        prefix = "\n" if leading_blank or step > 1 else ""
        line_count = 1 + len(detail_rows)

        with self._lock:
            print(f"{prefix}{header}")
            for row in detail_rows:
                print(f"{indent}{row}")
            sys.stdout.flush()
        return ToolDisplayState(step=max(1, step), tool_name=tool_name, line_count=line_count)

    def print_tool_result_record(
        self,
        ok: bool,
        output: str | None = None,
        *,
        tool_name: str = "",
        display_state: ToolDisplayState | None = None,
    ) -> None:
        """打印工具执行结果摘要，并用有限行展示 stdout/结果预览。"""

        result = "成功" if ok else "失败"
        indent = _dialog_continuation_prefix(AI_PREFIX)
        suffix = self._tool_result_suffix(tool_name, output or "")
        output_rows = self._tool_result_output_rows(tool_name, output or "")

        with self._lock:
            if display_state is not None:
                self._refresh_tool_call_header(display_state, ok)
            print(
                f"{indent}{self.muted('执行记录：')}"
                f"{self.result_text(ok, result)}{suffix}",
            )
            for row in output_rows:
                print(f"{indent}{row}")
            sys.stdout.flush()

    def _refresh_tool_call_header(self, display_state: ToolDisplayState, ok: bool) -> None:
        """把正在运行的闪烁星号改成完成态标记。

        只有 ANSI 终端才能可靠回到已打印的工具块首行重绘；普通终端保持原样，
        仍能通过随后的“执行记录”看出工具已经结束。
        """

        if not self.capabilities.ansi or display_state.line_count <= 0:
            return

        indent = _dialog_continuation_prefix(AI_PREFIX)
        marker = self.result_text(ok, "✓" if ok else "✗")
        line_width = shutil.get_terminal_size((100, 30)).columns
        tool_width = max(
            1,
            line_width
            - _display_width(indent)
            - _display_width("✓ 步骤 ")
            - _display_width(str(display_state.step))
            - _display_width(" — 请求 ")
            - 1,
        )
        visible_tool_name = _ellipsize_display_text(display_state.tool_name, tool_width)
        label = (
            f"{indent}{marker} {self.muted('步骤 ')}"
            f"{self.result_text(True, str(display_state.step))}"
            f"{self.muted(' — 请求 ')}{self.accent(visible_tool_name)}"
        )
        print(
            f"\033[{display_state.line_count}A"
            f"\r{ANSI_CLEAR_LINE}{label}"
            f"\033[{display_state.line_count}B"
            f"\r",
            end="",
        )

    def _tool_call_detail_rows(
        self,
        tool_name: str,
        arguments: dict[str, Any],
    ) -> list[str]:
        width = self._tool_detail_width()
        if tool_name == "run_command":
            command = str(arguments.get("command") or "")
            rows = _preview_display_rows(
                command,
                max(1, width - _display_width("Ran ")),
                TOOL_DETAIL_MAX_ROWS,
                empty_text="(empty command)",
            )
            first, *rest = rows
            rendered = [f"{self.muted('Ran ')}{self.bright(first)}"]
            rendered.extend(f"{self.muted('│ ')}{self.bright(row)}" for row in rest)
            return rendered

        rows = _preview_display_rows(
            _compact_json(arguments),
            width,
            TOOL_DETAIL_MAX_ROWS,
            empty_text="{}",
        )
        rendered = [f"{self.muted('│ ')}{self.bright(row)}" for row in rows]
        return rendered

    def _tool_result_suffix(self, tool_name: str, output: str) -> str:
        if tool_name != "run_command":
            return ""
        match = re.search(r"^退出码：(-?\d+)", output)
        if match is None:
            return ""
        return f"{self.muted('（退出码 ')}{self.bright(match.group(1))}{self.muted('）')}"

    def _tool_result_output_rows(self, tool_name: str, output: str) -> list[str]:
        width = self._tool_detail_width()
        if tool_name == "run_command":
            rows = _preview_display_rows(
                self._extract_command_visible_output(output),
                width,
                TOOL_OUTPUT_MAX_ROWS,
                empty_text="(no output)",
            )
        else:
            rows = _preview_display_rows(
                output,
                width,
                TOOL_OUTPUT_MAX_ROWS,
                empty_text="(no output)",
            )
        return self._render_branch_rows(rows)

    @staticmethod
    def _extract_command_visible_output(output: str) -> str:
        stdout_match = re.search(
            r"(?:^|\n\n)stdout:\n(.*?)(?=\n\nstderr:\n|\Z)",
            output,
            re.DOTALL,
        )
        stderr_match = re.search(r"(?:^|\n\n)stderr:\n(.*)\Z", output, re.DOTALL)
        visible_parts: list[str] = []
        if stdout_match is not None and stdout_match.group(1).strip():
            visible_parts.append(stdout_match.group(1).strip())
        if stderr_match is not None and stderr_match.group(1).strip():
            stderr_text = stderr_match.group(1).strip()
            visible_parts.append(f"stderr:\n{stderr_text}")
        return "\n".join(visible_parts)

    def _render_branch_rows(self, rows: list[str]) -> list[str]:
        rendered: list[str] = []
        last_index = len(rows) - 1
        for index, row in enumerate(rows):
            branch = "└ " if index == last_index else "│ "
            style = self.muted(branch)
            rendered.append(f"{style}{self.bright(row)}")
        return rendered

    @staticmethod
    def _tool_detail_width() -> int:
        terminal_width = shutil.get_terminal_size((100, 30)).columns
        indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
        return max(20, terminal_width - indent_width - 4)

    def print_startup_panel(
        self,
        title: str,
        lines: list[str],
    ) -> None:
        """打印普通终端内的启动面板。

        这里不接管屏幕缓冲区，只输出一次带灰色边框的配置摘要，保持终端历史可滚动；
        面板宽度会按终端宽度收缩，避免长配置路径把右侧边框挤出可视范围。
        """

        terminal_width = shutil.get_terminal_size((100, 30)).columns
        max_box_width = max(24, terminal_width - 2)
        desired_content_width = max(_display_width(title), *(_display_width(line) for line in lines), 36)
        content_width = min(max_box_width - 4, desired_content_width)

        def render_row(text: str) -> str:
            content = _take_display_width(text, content_width)
            padding = " " * max(0, content_width - _display_width(content))
            if not self.capabilities.ansi:
                return f"| {content}{padding} |"
            return (
                f"{ANSI_GRAY}│ {ANSI_RESET}"
                f"{ANSI_LIGHT_BLUE}{content}{ANSI_RESET}"
                f"{padding}"
                f"{ANSI_GRAY} │{ANSI_RESET}"
            )

        horizontal = "─" * (content_width + 2)
        if self.capabilities.ansi:
            top = f"{ANSI_GRAY}┌{horizontal}┐{ANSI_RESET}"
            bottom = f"{ANSI_GRAY}└{horizontal}┘{ANSI_RESET}"
        else:
            top = f"+{'-' * (content_width + 2)}+"
            bottom = top

        visible_lines = [line for line in lines if line.strip()]
        with self._lock:
            print(top)
            for line in visible_lines:
                print(render_row(line))
            print(bottom)

    def mark_transient_output_start(self) -> bool:
        """标记临时启动输出起点，便于初始化完成后清空麦克风选择等信息。"""

        if not self.capabilities.ansi:
            return False

        with self._lock:
            print(ANSI_SAVE_CURSOR, end="", flush=True)
        return True

    def clear_transient_output(self) -> None:
        """清空从最近一次标记起点到当前光标之间的临时启动输出。"""

        if not self.capabilities.ansi:
            return

        with self._lock:
            print(f"{ANSI_RESTORE_CURSOR}{ANSI_ERASE_TO_END}", end="", flush=True)

    def prompt(self) -> str:
        return f"\n{USER_PREFIX} "

    def prompt_width(self) -> int:
        return _display_width(f"{USER_PREFIX} ")

    def print_prompt_status(self, cursor_column: int = 0) -> None:
        """在当前输入行下方显示模型状态，并把光标放回输入行。"""

        if not self.model_label or not self.capabilities.ansi:
            return

        line = self.prompt_status_line()
        cursor_target = max(1, self.prompt_width() + cursor_column + 1)

        with self._lock:
            print(
                f"\n{ANSI_CLEAR_LINE}{line}"
                f"{ANSI_PREVIOUS_LINE}\033[{cursor_target}G",
                end="",
                flush=True,
            )

    def clear_prompt_status(self) -> None:
        """清空输入行下方的模型状态行，保持光标回到输入行。"""

        if not self.model_label or not self.capabilities.ansi:
            return

        with self._lock:
            print(f"\n{ANSI_CLEAR_LINE}{ANSI_PREVIOUS_LINE}", end="", flush=True)

    def prompt_status_line(self) -> str:
        """返回输入框下方的模型和 token 状态行。"""

        label = self.model_label or ""
        token_text = f"[Input Token: {self._input_tokens} Output Token: {self._output_tokens}]"
        if not self.capabilities.ansi:
            return f"- {label} {token_text}".strip()

        model = f"{ANSI_BRIGHT_WHITE}- {label}{ANSI_RESET}" if label else ""
        token = f"{ANSI_MUTED}{token_text}{ANSI_RESET}"
        return f"{model} {token}".strip()

    def replace_current_input_with_status(self, message: str) -> None:
        """用灰色弱提示覆盖当前输入行，用于录音过程中的临时状态。

        用户空回车触发录音后，光标已经停在下一行；这里先回到上一行清空原来的
        `> ` 输入提示，再写入最新状态。后续状态继续覆盖同一行，避免把麦克风、
        校准和开始说话提示刷成多行日志。
        """

        status_text = self._single_line_status_text(message)
        with self._lock:
            if self.capabilities.ansi:
                print(
                    f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}{self.muted(status_text)}\n",
                    end="",
                    flush=True,
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
        """清空录音状态占用的临时输入行。"""

        if not self.capabilities.ansi:
            return

        with self._lock:
            print(f"{ANSI_PREVIOUS_LINE}\r{ANSI_CLEAR_LINE}", end="", flush=True)

    def inline_turn_base(self, user_text: str) -> str:
        """把已提交的用户输入固定成独立对话行。

        输入编辑态可能只显示一个滚动窗口，尤其粘贴多行长文本时，提交后直接依赖
        编辑态残留会丢失前几行或留下旧字符。这里统一回到输入块顶部并重绘完整
        用户消息，让后续等待动画和 AI 回复都有稳定的起点。
        """

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
            f"{USER_PREFIX} {row}" if index == 0 else f"{continuation_prefix}{row}"
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
            print(f"{AI_PREFIX} ", end="", flush=True)

    def write(self, text: str) -> None:
        with self._lock:
            print(text, end="", flush=True)

    def write_markdown_delta(
        self,
        delta: str,
        state: MarkdownStreamState,
    ) -> None:
        """流式写入 AI 回复，并实时重绘当前行的轻量 Markdown 预览。"""

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
        """回复结束时渲染最后一个没有换行结尾的 Markdown 片段。"""

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

        if _contains_complex_display_width(state.pending_line):
            self._start_passthrough_line(state)
            return

        stable_preview_text, _remaining_text = self._split_stable_inline_markdown(
            state.pending_line
        )
        if not stable_preview_text:
            self._clear_markdown_preview(state)
            return

        preview_state = MarkdownStreamState(in_code_block=state.in_code_block)
        spans = _render_basic_markdown_stream_line(stable_preview_text, preview_state)
        if spans is None:
            return
        if _spans_display_width(spans) > self._markdown_preview_max_width():
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
        state.preview_width = _spans_display_width(spans)
        state.preview_visible = True

    def _write_markdown_spans(self, spans: list[MarkdownSpan], state: MarkdownStreamState) -> None:
        for span in spans:
            self._write_wrapped_ai_text(span.text, state, style=span.style)

    def _write_ai_continuation_prefix(self, state: MarkdownStreamState) -> None:
        """换到 AI 回复的下一行，并保持整段文本与 `^ ` 后方对齐。"""

        print()
        print(_dialog_continuation_prefix(AI_PREFIX), end="")
        state.content_column = 0

    def _write_wrapped_ai_text(
        self,
        text: str,
        state: MarkdownStreamState,
        *,
        style: str | None = None,
    ) -> None:
        """按终端宽度手动换行，避免长中文行触发终端自动折行后丢失缩进。

        这里把 `content_column` 当作 AI 正文区内的列号，而不是整行终端列号。
        首行的 `^ ` 由调用方先打印，续行则由 `_write_ai_continuation_prefix`
        打印等宽空白，因此所有视觉行都能与正文起点对齐。
        """

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
            if self.capabilities.ansi and style:
                print(f"{style}{chunk}{ANSI_RESET}", end="")
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
        # 留 1 列余量，避开不同 Windows 终端在最后一列触发自动换行的差异。
        return max(20, terminal_width - indent_width - 1)

    @staticmethod
    def _markdown_preview_max_width() -> int:
        terminal_width = shutil.get_terminal_size((100, 30)).columns
        # 只对单个视觉行做原地重绘。长行交给直写流式输出，避免终端自动换行后
        # 光标回退只能回到当前视觉行，导致上一轮预览残留并反复叠加。
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
            state.pending_line[state.passthrough_printed_chars :]
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
        unprinted = line[state.passthrough_printed_chars :]
        stable_length = _stable_inline_markdown_prefix_length(unprinted, final=final)
        stable_text = unprinted[:stable_length]
        remaining_text = unprinted[stable_length:]
        with self._lock:
            if stable_text:
                self._write_passthrough_text_with_mode(stable_text, state, final=final)
            if newline:
                self._write_ai_continuation_prefix(state)
            sys.stdout.flush()

        # 直写长行时不再重排 Markdown，但仍让围栏状态随完整行推进，
        # 避免长代码行之后的代码块状态错乱。
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
        """写入长行直通文本；普通文本解析行内 Markdown，代码块保持原样。"""

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

    def newline(self) -> None:
        with self._lock:
            print()

    def status(self, message: str, *, leading_blank: bool = True) -> None:
        with self._lock:
            prefix = "\n" if leading_blank else ""
            indent = _dialog_continuation_prefix(AI_PREFIX)
            print(f"{prefix}{indent}{self.muted(f'[{message}]')}", flush=True)

    def notice(self, message: str) -> None:
        with self._lock:
            print(self.muted(message), flush=True)

    def prompt_yes_no(self, prompt: str, confirmed_label: str = "") -> bool:
        """以默认 YES 的方式确认一次高风险操作。

        显示编号选项 + ❯ 指针。支持上下/左右方向键切换选项，Enter 确认当前选中项。
        确认后若提供了 confirmed_label，则用单行缩略替换整个确认块。
        """

        pointer = " ❯ " if self.capabilities.ansi else " > "
        selected_yes = True
        # 下方用 `print(f"\n{prompt}")` 让确认框和上一个状态块保持间隔。
        # 收折确认框时必须把这个前置空行也算进去；否则 ANSI 光标上移会少一行，
        # 正好留下确认提示的第一行（例如“Agent 想要执行命令。”）。
        _prompt_lines = prompt.count("\n") + 2

        with self._lock:
            print(f"\n{prompt}")

        def _print_options() -> None:
            with self._lock:
                print()
                if selected_yes:
                    print(f"{pointer}1. Yes{ANSI_CLEAR_TO_LINE_END}")
                    print(f"    2. No{ANSI_CLEAR_TO_LINE_END}")
                else:
                    print(f"    1. Yes{ANSI_CLEAR_TO_LINE_END}")
                    print(f"{pointer}2. No{ANSI_CLEAR_TO_LINE_END}")
                print()
                print(self.muted("↑↓/←→ 选择  Enter 确认  N 取消"), flush=True)

        _print_options()

        try:
            import msvcrt
        except ImportError:
            answer = input("确认？[Enter=YES / n=NO] ").strip().lower()
            return answer not in {"n", "no", "否", "false", "2"}

        while True:
            char = msvcrt.getwch()
            if char in {"\r", "\n"}:
                self._collapse_and_label(confirmed_label, _prompt_lines, confirmed=selected_yes)
                return selected_yes
            if char in {"1", "y", "Y"}:
                self._collapse_and_label(confirmed_label, _prompt_lines, confirmed=True)
                return True
            if char in {"n", "N", "2"}:
                self._collapse_and_label(confirmed_label, _prompt_lines, confirmed=False)
                return False
            if char == "\x03":
                self._collapse_and_label("", _prompt_lines, confirmed=False)
                raise KeyboardInterrupt
            if char in {"\x00", "\xe0"}:
                key = msvcrt.getwch()
                next_selected_yes = self._selection_from_key(key, selected_yes)
                if next_selected_yes != selected_yes:
                    selected_yes = next_selected_yes
                    self._redraw_options(_print_options)
                continue
            # 任意其他键忽略，继续等待

    @staticmethod
    def _selection_from_key(key: str, selected_yes: bool) -> bool:
        if key in {"H", "K"}:
            return True
        if key in {"P", "M"}:
            return False
        return selected_yes

    @staticmethod
    def _redraw_options(print_fn) -> None:
        """回到选项区起始位置，用当前选中状态重绘 5 行。"""
        print("\033[5A", end="")  # 上移 5 行
        print_fn()

    def _collapse_and_label(self, label: str, prompt_lines: int, confirmed: bool = True) -> None:
        """选中后把完整确认块收折为单行缩略。"""
        if not self.capabilities.ansi:
            return

        symbol = "✓" if confirmed else "✗"
        total_lines = prompt_lines + 5  # prompt + 选项区 5 行
        with self._lock:
            # 回到 prompt 起始行并清除确认块；工具结果会在真实执行完成后单独打印。
            print(f"\033[{total_lines}A\033[J", end="")
            if label:
                print(f"{ANSI_GRAY}  {symbol} {label}{ANSI_RESET}", flush=True)
            else:
                print("", flush=True)


class StatusLine:
    """当前行上的弱提示和等待动画。

    同一行重绘只在支持 ANSI 时启用；否则每次 show 都退化为普通状态行，
    避免把转义字符显示给用户。
    """

    def __init__(self, ui: TerminalUI) -> None:
        self._ui = ui
        self._visible = False

    def show(self, text: str) -> None:
        indent = " " * self._ui.prompt_width()
        rendered = f"{indent}{text}"
        with self._ui._lock:
            if self._ui.capabilities.ansi:
                prefix = "\n" if not self._visible else "\r"
                print(
                    f"{prefix}{ANSI_CLEAR_LINE}{self._ui.bright(rendered)}",
                    end="",
                    flush=True,
                )
                self._visible = True
            elif not self._visible:
                print(f"\n{rendered}", flush=True)
                self._visible = True

    def clear(self) -> None:
        with self._ui._lock:
            if not self._visible:
                return
            if self._ui.capabilities.ansi:
                print(f"\r{ANSI_CLEAR_LINE}", end="", flush=True)
            self._visible = False

    def new_line_for_input(self, prefix: str = USER_PREFIX) -> None:
        with self._ui._lock:
            if self._ui.capabilities.ansi:
                print(f"\r{ANSI_CLEAR_LINE}", end="", flush=True)
            print(f"{prefix} ", end="", flush=True)
            self._visible = False


class WaitingIndicator:
    """模型返回前的轻量等待动画。"""

    def __init__(self, status_line: StatusLine) -> None:
        self._status_line = status_line
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None
        self._status_line.clear()

    def _run(self) -> None:
        kaomoji = random.choice(WAITING_KAOMOJI)
        dot_index = 0
        while not self._stop.is_set():
            dots = WAITING_DOTS[dot_index % len(WAITING_DOTS)]
            self._status_line.show(f"处理中  {kaomoji}{dots}")
            dot_index += 1
            self._stop.wait(0.35)
