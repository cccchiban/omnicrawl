"""TUI 子系统合并入口。

原先拆在 ui/tui/_*.py 的终端 UI 实现集中到这里，减少代码文件数量。
模块别名会在导入时注册，兼容 omnicrawl.ui.tui._core/_spinner 等旧路径。
"""

from __future__ import annotations

import sys as _sys

_THIS_MODULE = _sys.modules[__name__]
_TUI_MODULE_ALIASES = (
    '_capabilities',
    '_colors',
    '_display',
    '_markdown',
    '_markdown_renderer',
    '_tools',
    '_status',
    '_panels',
    '_prompt',
    '_spinner',
    '_core',
)
for _alias in _TUI_MODULE_ALIASES:
    _sys.modules[f"{__name__}.{_alias}"] = _THIS_MODULE
    globals()[_alias] = _THIS_MODULE

# --- former module: _capabilities.py ---
"""终端能力检测。"""


import ctypes
import os
import sys
from dataclasses import dataclass


@dataclass(frozen=True)
class TerminalCapabilities:
    """当前终端可用能力。

    终端样式本质由终端模拟器决定。这里仅判断是否适合输出 ANSI 控制序列，
    不尝试模拟真正的小字体或复杂 TUI。
    """

    ansi: bool
    truecolor: bool = False
    color256: bool = False


def detect_capabilities() -> TerminalCapabilities:
    """根据环境判断是否启用 ANSI 样式和行重绘。"""

    if os.getenv("NO_COLOR"):
        return TerminalCapabilities(ansi=False)

    if os.name == "nt":
        ansi = bool(
            os.getenv("WT_SESSION")
            or os.getenv("TERM_PROGRAM")
            or os.getenv("ANSICON")
            or os.getenv("ConEmuANSI") == "ON"
            or "xterm" in os.getenv("TERM", "").lower()
            or _enable_windows_virtual_terminal()
        )
    else:
        ansi = sys.stdout.isatty() and os.getenv("TERM") != "dumb"

    if not ansi:
        return TerminalCapabilities(ansi=False)

    # 真彩色检测：COLORTERM 含 truecolor/24bit，或 Windows Terminal
    truecolor = bool(
        os.getenv("WT_SESSION")
        or "truecolor" in os.getenv("COLORTERM", "").lower()
        or "24bit" in os.getenv("COLORTERM", "").lower()
    )

    # 256 色检测
    color256 = bool(
        os.getenv("WT_SESSION")
        or "256color" in os.getenv("TERM", "").lower()
        or os.getenv("ANSICON")
        or os.getenv("ConEmuANSI") == "ON"
    )

    return TerminalCapabilities(ansi=True, truecolor=truecolor, color256=color256)


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


# --- former module: _colors.py ---
"""ANSI 颜色常量、真彩色配色表、256 色映射与色彩路由。"""


import re
from typing import Literal

from ._capabilities import TerminalCapabilities

# ── ANSI 基础常量 ─────────────────────────────────────────────

ANSI_RESET = "\033[0m"

# 基础色
ANSI_BLACK = "\033[30m"
ANSI_RED = "\033[31m"
ANSI_GREEN = "\033[32m"
ANSI_YELLOW = "\033[33m"
ANSI_BLUE = "\033[34m"
ANSI_MAGENTA = "\033[35m"
ANSI_CYAN = "\033[36m"
ANSI_WHITE = "\033[37m"

# 亮色
ANSI_BRIGHT_BLACK = "\033[90m"
ANSI_BRIGHT_RED = "\033[91m"
ANSI_BRIGHT_GREEN = "\033[92m"
ANSI_BRIGHT_YELLOW = "\033[93m"
ANSI_BRIGHT_BLUE = "\033[94m"
ANSI_BRIGHT_MAGENTA = "\033[95m"
ANSI_BRIGHT_CYAN = "\033[96m"
ANSI_BRIGHT_WHITE = "\033[97m"

# 样式
ANSI_BOLD = "\033[1m"
ANSI_DIM = "\033[2m"
ANSI_ITALIC = "\033[3m"
ANSI_UNDERLINE = "\033[4m"
ANSI_BLINK = "\033[5m"

# 控制序列
ANSI_CLEAR_LINE = "\033[2K"
ANSI_CLEAR_TO_LINE_END = "\033[K"
ANSI_PREVIOUS_LINE = "\033[1A"
ANSI_SAVE_CURSOR = "\033[s"
ANSI_RESTORE_CURSOR = "\033[u"
ANSI_ERASE_TO_END = "\033[J"

# ── 语义色角色 ────────────────────────────────────────────────

ColorRole = Literal[
    "primary", "secondary", "success", "warning", "error",
    "muted", "text", "heading", "accent", "surface",
]

# ── Tokyo Night 配色表 ────────────────────────────────────────

_RGB_COLORS: dict[ColorRole, tuple[int, int, int]] = {
    "primary":    (0x7A, 0xA2, 0xF7),   # #7AA2F7 淡蓝
    "secondary":  (0x7D, 0xCF, 0xFF),   # #7DCFFF 天蓝
    "success":    (0x9E, 0xCE, 0x6A),   # #9ECE6A 草绿
    "warning":    (0xE0, 0xAF, 0x68),   # #E0AF68 暖黄
    "error":      (0xF7, 0x76, 0x8F),   # #F7768F 玫红
    "muted":      (0x56, 0x5F, 0x89),   # #565F89 灰蓝
    "text":       (0xC0, 0xCA, 0xF5),   # #C0CAF5 亮灰
    "heading":    (0xC0, 0xCA, 0xF5),   # #C0CAF5 亮灰（+BOLD）
    "accent":     (0xBB, 0x9A, 0xF7),   # #BB9AF7 淡紫
    "surface":    (0x1A, 0x1B, 0x26),   # #1A1B26 暗面
}

_256_COLORS: dict[ColorRole, int] = {
    "primary":    111,
    "secondary":  117,
    "success":    150,
    "warning":    179,
    "error":      210,
    "muted":      60,
    "text":       189,
    "heading":    189,
    "accent":     183,
    "surface":    234,
}

_16_COLORS: dict[ColorRole, str] = {
    "primary":    ANSI_BRIGHT_CYAN,
    "secondary":  ANSI_BRIGHT_BLUE,
    "success":    ANSI_BRIGHT_GREEN,
    "warning":    ANSI_BRIGHT_YELLOW,
    "error":      ANSI_BRIGHT_RED,
    "muted":      ANSI_BRIGHT_BLACK,
    "text":       ANSI_WHITE,
    "heading":    ANSI_BOLD + ANSI_BRIGHT_WHITE,
    "accent":     ANSI_BRIGHT_MAGENTA,
    "surface":    ANSI_BLACK,
}

# 兼容旧名称（供迁移期使用）
COLOR_PRIMARY = _16_COLORS["primary"]
COLOR_SECONDARY = _16_COLORS["secondary"]
COLOR_SUCCESS = _16_COLORS["success"]
COLOR_WARNING = _16_COLORS["warning"]
COLOR_ERROR = _16_COLORS["error"]
COLOR_MUTED = _16_COLORS["muted"]
COLOR_TEXT = _16_COLORS["text"]
COLOR_HEADING = _16_COLORS["heading"]
COLOR_ACCENT = _16_COLORS["accent"]

# 对话前缀
AI_PREFIX = "◆"
USER_PREFIX = "▸"

# ── 256 色映射 ────────────────────────────────────────────────

# 标准 256 色调色板：0-15 基础色，16-231 6×6×6 色立方，232-255 灰度
def _rgb_to_256(r: int, g: int, b: int) -> int:
    """将 RGB 映射到最近的 256 色索引。"""

    # 先尝试灰度匹配（232-255）
    if r == g == b:
        if r < 8:
            return 16
        if r > 248:
            return 231
        return 232 + round((r - 8) / 10)

    # 6×6×6 色立方
    def _scale(v: int) -> int:
        if v < 48:
            return 0
        if v < 115:
            return 1
        return round((v - 35) / 40)

    return 16 + 36 * _scale(r) + 6 * _scale(g) + _scale(b)


# ── 色彩路由 ──────────────────────────────────────────────────

def get_color_sequence(role: ColorRole, caps: TerminalCapabilities) -> str:
    """根据终端能力返回对应色阶的 ANSI 前缀序列。"""

    if caps.truecolor:
        r, g, b = _RGB_COLORS[role]
        return f"\033[38;2;{r};{g};{b}m"

    if caps.color256:
        idx = _256_COLORS[role]
        return f"\033[38;5;{idx}m"

    return _16_COLORS[role]


def color_text(text: str, role: ColorRole, caps: TerminalCapabilities) -> str:
    """用语义色角色着色文本，根据终端能力自动选择色阶。"""

    if not caps.ansi:
        return text
    seq = get_color_sequence(role, caps)
    return f"{seq}{text}{ANSI_RESET}"


# ── ANSI 剥离 ────────────────────────────────────────────────

_ANSI_PATTERN = re.compile(r"\033\[[0-9;]*m")


def strip_ansi(text: str) -> str:
    """剥离文本中的所有 ANSI 转义序列。"""
    return _ANSI_PATTERN.sub("", text)


# --- former module: _display.py ---
"""终端文本显示宽度计算与截断。"""


import unicodedata


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
    对这些字符继续使用"光标左移 N 列后整行重绘"容易留下旧字符，表现为
    "获获取取"这类重复字。遇到复杂宽度字符时改用追加输出，避免依赖列宽回退。
    """

    return any(_char_display_width(char) != 1 for char in text)


def _normalize_terminal_text(text: str) -> str:
    """统一终端文本换行，避免 CRLF 在行数计算里被当成额外字符。"""

    return text.replace("\r\n", "\n").replace("\r", "\n")


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
                chunk = remaining[0]
            rows.append(chunk)
            remaining = remaining[len(chunk):]
    return rows or [""]


def _preview_display_rows(
    text: str,
    max_width: int,
    max_rows: int,
    *,
    empty_text: str,
) -> list[str]:
    """生成适合工具记录展示的有限行预览。"""

    normalized = _normalize_terminal_text(text).strip()
    if not normalized:
        return [empty_text]

    rows = _split_display_rows(normalized, max_width)
    if len(rows) <= max_rows:
        return rows
    visible_count = max(1, max_rows)
    hidden_count = len(rows) - visible_count
    return [*rows[:visible_count], f"... +{hidden_count} lines"]


# --- former module: _markdown.py ---
"""Markdown 流式解析与渲染。"""


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
            # 顶级列表使用 • 标记（避免与用户输入前缀 ▸ 冲突）
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


# --- former module: _markdown_renderer.py ---
"""TerminalUI 的 Markdown 流式渲染 Mixin。"""


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


# --- former module: _tools.py ---
"""工具调用显示。"""


import json
import re
import shutil
import sys
import threading
from dataclasses import dataclass
from typing import Any

from ._colors import (
    ANSI_BLINK,
    ANSI_CLEAR_LINE,
    ANSI_RESET,
    color_text,
)
from ._capabilities import TerminalCapabilities
from ._display import (
    _dialog_continuation_prefix,
    _display_width,
    _ellipsize_display_text,
    _preview_display_rows,
)

AI_PREFIX = "◆"

TOOL_DETAIL_MAX_ROWS = 3
TOOL_OUTPUT_MAX_ROWS = 6


@dataclass
class ToolDisplayState:
    """记录一次工具执行块，供执行完成后把运行态标记更新为完成态。"""

    step: int
    tool_name: str
    line_count: int


def _compact_json(value: Any) -> str:
    """把工具参数压成单行 JSON，供 TUI 摘要展示。"""

    try:
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    except (TypeError, ValueError):
        return str(value)


def print_tool_call_start(
    ui: Any,
    step: int,
    tool_name: str,
    arguments: dict[str, Any],
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
    leading_blank: bool = True,
) -> ToolDisplayState:
    """打印工具开始执行的结构化记录。"""

    indent = _dialog_continuation_prefix(AI_PREFIX)
    marker = f"{ANSI_BLINK}◌{ANSI_RESET}" if caps.ansi else "◌"
    line_width = shutil.get_terminal_size((100, 30)).columns
    # header 格式：◌ ╭─ 步骤 N · tool_name ─ ─ ─
    marker_prefix_width = _display_width("◌ ╭─ 步骤 ")
    tool_width = max(
        1,
        line_width
        - _display_width(indent)
        - marker_prefix_width
        - _display_width(str(max(1, step)))
        - _display_width(" · ")
        - 1,
    )
    visible_tool_name = _ellipsize_display_text(tool_name, tool_width)

    # 框线头部（含闪烁运行标记）
    header = (
        f"{indent}{marker} "
        f"{color_text('╭─', 'muted', caps)} "
        f"{color_text('步骤', 'muted', caps)} "
        f"{color_text(str(max(1, step)), 'primary', caps)}"
        f"{color_text(' · ', 'muted', caps)}"
        f"{color_text(visible_tool_name, 'accent', caps)}"
        f"{color_text(' ─' * 3, 'muted', caps)}"
    )
    detail_rows = _tool_call_detail_rows(tool_name, arguments, caps)
    prefix = "\n" if leading_blank or step > 1 else ""
    line_count = 1 + len(detail_rows)

    with lock:
        print(f"{prefix}{header}")
        for row in detail_rows:
            print(f"{indent}{row}")
        sys.stdout.flush()
    return ToolDisplayState(step=max(1, step), tool_name=tool_name, line_count=line_count)


def print_tool_result_record(
    ok: bool,
    output: str | None = None,
    *,
    tool_name: str = "",
    display_state: ToolDisplayState | None = None,
    caps: TerminalCapabilities = TerminalCapabilities(ansi=False),
    lock: threading.Lock = threading.Lock(),  # noqa: B008 — 模块级默认值，仅作回退
) -> None:
    """打印工具执行结果摘要。"""

    result_label = color_text("成功", "success", caps) if ok else color_text("失败", "error", caps)
    indent = _dialog_continuation_prefix(AI_PREFIX)
    suffix = _tool_result_suffix(tool_name, output or "", caps)
    output_rows = _tool_result_output_rows(tool_name, output or "", caps)

    with lock:
        if display_state is not None:
            _refresh_tool_call_header(display_state, ok, caps)
        result_marker = color_text("✓" if ok else "✗", "success" if ok else "error", caps)
        bracket_result = f"[{result_label}]" if caps.ansi else ("成功" if ok else "失败")
        if output_rows:
            # 有输出行：结果行用 │ 继续，输出行最后一行用 ╰─ 关闭
            print(
                f"{indent}{color_text('│ ', 'muted', caps)}{result_marker} {bracket_result}{suffix}",
            )
            for row in output_rows:
                print(f"{indent}{row}")
        else:
            # 无输出行：结果行本身就是最后一行，用 ╰─ 关闭
            print(
                f"{indent}{color_text('╰─ ', 'muted', caps)}{result_marker} {bracket_result}{suffix}",
            )
        sys.stdout.flush()


def _refresh_tool_call_header(
    display_state: ToolDisplayState,
    ok: bool,
    caps: TerminalCapabilities,
) -> None:
    """把正在运行的闪烁标记改成完成态。"""

    if not caps.ansi or display_state.line_count <= 0:
        return

    indent = _dialog_continuation_prefix(AI_PREFIX)
    # 完成态 marker：✓ 或 ✗
    marker = color_text("✓" if ok else "✗", "success" if ok else "error", caps)
    line_width = shutil.get_terminal_size((100, 30)).columns
    # 重新计算宽度——完成态 marker 是 ✓（宽1），与运行态 ◌（宽1）一致
    marker_prefix_width = _display_width("✓ ╭─ 步骤 ")
    tool_width = max(
        1,
        line_width
        - _display_width(indent)
        - marker_prefix_width
        - _display_width(str(display_state.step))
        - _display_width(" · ")
        - 1,
    )
    visible_tool_name = _ellipsize_display_text(display_state.tool_name, tool_width)
    label = (
        f"{indent}{marker} "
        f"{color_text('╭─', 'muted', caps)} "
        f"{color_text('步骤', 'muted', caps)} "
        f"{color_text(str(display_state.step), 'primary', caps)}"
        f"{color_text(' · ', 'muted', caps)}"
        f"{color_text(visible_tool_name, 'accent', caps)}"
        f"{color_text(' ─' * 3, 'muted', caps)}"
    )
    print(
        f"\033[{display_state.line_count}A"
        f"\r{ANSI_CLEAR_LINE}{label}"
        f"\033[{display_state.line_count}B"
        f"\r",
        end="",
    )


def _tool_call_detail_rows(
    tool_name: str,
    arguments: dict[str, Any],
    caps: TerminalCapabilities,
) -> list[str]:
    width = _tool_detail_width()
    if tool_name == "run_command":
        command = str(arguments.get("command") or "")
        rows = _preview_display_rows(
            command,
            max(1, width - _display_width("▸ command  ")),
            TOOL_DETAIL_MAX_ROWS,
            empty_text="(empty command)",
        )
        first, *rest = rows
        rendered = [
            f"{color_text('│ ', 'muted', caps)} "
            f"{color_text('▸ command', 'secondary', caps)}  "
            f"{color_text(first, 'text', caps)}"
        ]
        rendered.extend(
            f"{color_text('│ ', 'muted', caps)} "
            f"{color_text(row, 'text', caps)}"
            for row in rest
        )
        return rendered

    rows = _preview_display_rows(
        _compact_json(arguments),
        width,
        TOOL_DETAIL_MAX_ROWS,
        empty_text="{}",
    )
    return [
        f"{color_text('│ ', 'muted', caps)} {color_text(row, 'text', caps)}"
        for row in rows
    ]


def _tool_result_suffix(tool_name: str, output: str, caps: TerminalCapabilities) -> str:
    if tool_name != "run_command":
        return ""
    match = re.search(r"^退出码：(-?\d+)", output)
    if match is None:
        return ""
    return (
        f"{color_text(' · 退出码 ', 'muted', caps)}"
        f"{color_text(match.group(1), 'text', caps)}"
    )


def _tool_result_output_rows(tool_name: str, output: str, caps: TerminalCapabilities) -> list[str]:
    width = _tool_detail_width()
    if tool_name == "run_command":
        rows = _preview_display_rows(
            _extract_command_visible_output(output),
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
    return _render_branch_rows(rows, caps)


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


def _render_branch_rows(rows: list[str], caps: TerminalCapabilities) -> list[str]:
    """渲染输出行，与圆角框风格统一：中间行用 │，最后一行用 ╰─ 关闭。"""
    rendered: list[str] = []
    last_index = len(rows) - 1
    for index, row in enumerate(rows):
        if index == last_index:
            branch = "╰─ "
        else:
            branch = "│ "
        rendered.append(
            f"{color_text(branch, 'muted', caps)}{color_text(row, 'text', caps)}"
        )
    return rendered


def _tool_detail_width() -> int:
    terminal_width = shutil.get_terminal_size((100, 30)).columns
    indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
    return max(20, terminal_width - indent_width - 4)


# --- former module: _status.py ---
"""状态行与提示状态。"""


import shutil
import sys
import threading

from ._colors import (
    ANSI_CLEAR_LINE,
    ANSI_PREVIOUS_LINE,
    ANSI_RESET,
    ColorRole,
    color_text,
    get_color_sequence,
)
from ._capabilities import TerminalCapabilities
from ._display import _display_width

USER_PREFIX = "▸"


class StatusLine:
    """当前行上的弱提示和等待动画。"""

    def __init__(self, ui: Any) -> None:
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
            print(f"{color_text(prefix, 'primary', self._ui.capabilities)} ", end="", flush=True)
            self._visible = False


def prompt_status_line(
    model_label: str | None,
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
    *,
    caps: TerminalCapabilities,
) -> str:
    """返回输入框下方的模型和 token 状态行（语义化标签独立着色）。"""

    if not caps.ansi:
        token_text = f"in:{input_tokens} cache:{cached_input_tokens} out:{output_tokens}"
        label = model_label or ""
        return f"- {label} {token_text}".strip()

    parts: list[str] = []
    if model_label:
        parts.append(color_text(f"- {model_label}", "muted", caps))

    # 语义化指标标签
    parts.append(color_text("in:", "primary", caps) + color_text(str(input_tokens), "text", caps))
    parts.append(color_text("cache:", "success", caps) + color_text(str(cached_input_tokens), "text", caps))
    parts.append(color_text("out:", "secondary", caps) + color_text(str(output_tokens), "text", caps))

    return " ".join(parts)


from typing import Any


# --- former module: _panels.py ---
"""启动面板。"""


import re
import shutil
import sys
import threading

from ._colors import color_text
from ._capabilities import TerminalCapabilities
from ._display import _display_width, _take_display_width


def print_startup_panel(
    title: str,
    lines: list[str],
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> None:
    """打印现代无框卡片风格的启动面板。

    左侧色条 + 分组展示 + 状态徽章 + 底部帮助行。
    """

    terminal_width = shutil.get_terminal_size((100, 30)).columns
    max_width = max(40, terminal_width - 4)
    content_width = min(max_width, max(_display_width(title) + 4, 52))

    groups = _parse_panel_groups(lines)

    with lock:
        # 标题行：左侧色条 + 标题
        title_text = _take_display_width(title, content_width)
        print(f"  {color_text('█', 'primary', caps)} {color_text(title_text, 'heading', caps)}")
        print(f"  {color_text('─' * (content_width - 2), 'muted', caps)}")

        for group_name, group_lines in groups:
            if group_name:
                print(f"  {color_text(group_name, 'secondary', caps)}")
            for line in group_lines:
                rendered = _render_panel_line(line, caps, content_width)
                print(f"  {rendered}")

        # 底部帮助行
        help_text = "输入问题开始对话 · /help 查看命令"
        print(f"  {color_text('─' * 2, 'muted', caps)} "
              f"{color_text(help_text, 'muted', caps)} "
              f"{color_text('─' * max(0, content_width - _display_width(help_text) - 6), 'muted', caps)}")

        sys.stdout.flush()


def _parse_panel_groups(lines: list[str]) -> list[tuple[str, list[str]]]:
    """将面板行按分组解析。"""

    groups: list[tuple[str, list[str]]] = []
    current_group = ""
    current_lines: list[str] = []

    for line in lines:
        if not line.strip():
            continue

        colon_idx = line.find(":")
        if colon_idx > 0:
            key = line[:colon_idx].strip().lower()
            group_name = _key_to_group(key)
            if group_name != current_group:
                if current_lines:
                    groups.append((current_group, current_lines))
                current_group = group_name
                current_lines = [line]
            else:
                current_lines.append(line)
        else:
            current_lines.append(line)

    if current_lines:
        groups.append((current_group, current_lines))

    return groups


def _key_to_group(key: str) -> str:
    """将配置键映射到分组名。"""

    mapping = {
        "thinking": "模型",
        "approval": "审批",
        "workspace": "工作区",
        "voice": "语音",
        "temp": "临时区",
    }
    return mapping.get(key, "")


def _render_panel_line(line: str, caps: TerminalCapabilities, content_width: int) -> str:
    """渲染单行配置项：key: value 格式。"""

    colon_idx = line.find(":")
    if colon_idx < 0:
        return color_text(line, "text", caps)

    key = line[:colon_idx].strip()
    value = line[colon_idx + 1:].strip()

    value_rendered = _render_status_value(value, caps)

    return (
        f"{color_text('▸', 'primary', caps)} "
        f"{color_text(key, 'text', caps)}  "
        f"{value_rendered}"
    )


def _render_status_value(value: str, caps: TerminalCapabilities) -> str:
    """渲染带状态徽章的值。

    对 "已启用"/"开启"/"已禁用"/"关闭" 等关键词逐个生成色块徽章，
    其余文字保持正常色。支持同一行中出现多个关键词。
    """

    # 逐个替换所有状态关键词为徽章
    result = ""
    pattern = re.compile(r"(已启用|开启|已禁用|关闭)")
    last_end = 0

    for match in pattern.finditer(value):
        # 关键词前的普通文字
        before = value[last_end:match.start()]
        if before:
            result += color_text(before, "text", caps)

        keyword = match.group(1)
        if keyword in {"已启用", "开启"}:
            badge = color_text(keyword, "success", caps)
            result += f"[{badge}]"
        else:
            badge = color_text(keyword, "muted", caps)
            result += f"[{badge}]"

        last_end = match.end()

    # 关键词后的剩余文字
    remaining = value[last_end:]
    if remaining:
        remaining = remaining.strip()
        if remaining:
            result += " " + color_text(remaining, "text", caps)

    return result if result else color_text(value, "text", caps)


# --- former module: _prompt.py ---
"""确认对话框。"""


import shutil
import threading

from ._colors import (
    ANSI_BOLD,
    ANSI_CLEAR_TO_LINE_END,
    ANSI_RESET,
    ANSI_UNDERLINE,
    ColorRole,
    color_text,
)
from ._capabilities import TerminalCapabilities
from ._display import _dialog_continuation_prefix, _display_width, _ellipsize_display_text

AI_PREFIX = "◆"


def prompt_yes_no(
    prompt: str,
    confirmed_label: str = "",
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> bool:
    """以默认 YES 的方式确认一次高风险操作。

    圆角框包裹 + 高亮选中项。支持上下/左右方向键切换，Enter 确认。
    """

    selected_yes = True

    with lock:
        _render_confirmation_card(prompt, caps)

    def _print_options() -> None:
        with lock:
            indent = _dialog_continuation_prefix(AI_PREFIX)
            line_width = shutil.get_terminal_size((100, 30)).columns
            content_width = max(20, line_width - _display_width(indent) - 4)

            # 分隔线
            sep = color_text("│", "muted", caps) + " " + color_text("─" * (content_width - 2), "muted", caps)
            print(f"{indent}{sep}")

            # 选项行
            if selected_yes:
                yes_style = lambda t: _highlight_selected(t, caps)
                no_style = lambda t: color_text(t, "muted", caps)
                pointer_yes = "❯ "
                pointer_no = "  "
            else:
                yes_style = lambda t: color_text(t, "muted", caps)
                no_style = lambda t: _highlight_selected(t, caps)
                pointer_yes = "  "
                pointer_no = "❯ "

            option_line = (
                f"{indent}{color_text('│', 'muted', caps)} "
                f"{pointer_yes}{yes_style('Yes')}    "
                f"{pointer_no}{no_style('No')}"
                f"{ANSI_CLEAR_TO_LINE_END}"
            )
            print(option_line)

            # 底部框线
            bottom = color_text("╰─", "muted", caps) + color_text("─" * (content_width - 2), "muted", caps)
            print(f"{indent}{bottom}")
            print()
            print(
                f"{indent}{color_text('↑↓ 选择 · Enter 确认 · N 取消', 'muted', caps)}",
                flush=True,
            )

    _print_options()

    # 计算收折行数：卡片行数 + 选项区行数(分隔+选项+底框+空行+提示)
    _card_lines = _count_card_lines(prompt)
    _option_lines = 5

    try:
        import msvcrt
    except ImportError:
        answer = input("确认？[Enter=YES / n=NO] ").strip().lower()
        return answer not in {"n", "no", "否", "false", "2"}

    while True:
        char = msvcrt.getwch()
        if char in {"\r", "\n"}:
            _collapse_and_label(
                confirmed_label, _card_lines + _option_lines,
                confirmed=selected_yes, caps=caps, lock=lock,
            )
            return selected_yes
        if char in {"1", "y", "Y"}:
            _collapse_and_label(
                confirmed_label, _card_lines + _option_lines,
                confirmed=True, caps=caps, lock=lock,
            )
            return True
        if char in {"n", "N", "2"}:
            _collapse_and_label(
                confirmed_label, _card_lines + _option_lines,
                confirmed=False, caps=caps, lock=lock,
            )
            return False
        if char == "\x03":
            _collapse_and_label(
                "", _card_lines + _option_lines,
                confirmed=False, caps=caps, lock=lock,
            )
            raise KeyboardInterrupt
        if char in {"\x00", "\xe0"}:
            key = msvcrt.getwch()
            next_selected_yes = _selection_from_key(key, selected_yes)
            if next_selected_yes != selected_yes:
                selected_yes = next_selected_yes
                _redraw_options(_print_options)
            continue


# ── 卡片渲染 ──────────────────────────────────────────────────


def _render_confirmation_card(prompt: str, caps: TerminalCapabilities) -> None:
    """将确认提示渲染为带圆角框线的结构化卡片。

    输入格式（由 format_tool_confirmation 生成）：
        Agent 想要执行 MCP 工具 local_project.workspace.run_command。
        参数：{"command": "python -c ..."}
        是否允许执行？

    渲染为：
        ╭─ ⚡ 确认执行 ─ ─ ─
        │ Agent 想要执行 MCP 工具 local_project.workspace.run_command。
        │ 参数：{"command": "python -c ..."}
        │
        │ 是否允许执行？
    """
    indent = _dialog_continuation_prefix(AI_PREFIX)
    line_width = shutil.get_terminal_size((100, 30)).columns
    content_width = max(20, line_width - _display_width(indent) - 4)

    # 顶部框线：╭─ ⚡ 确认执行 ─ ─ ─
    warning_icon = color_text("⚡", "warning", caps) if caps.ansi else "!"
    header_label = color_text("确认执行", "warning", caps) if caps.ansi else "确认执行"
    header_prefix_width = _display_width("╭─ ⚡ 确认执行 ")
    dash_count = max(3, content_width - header_prefix_width - 1)
    header = (
        f"{indent}"
        f"{color_text('╭─', 'muted', caps)} "
        f"{warning_icon} "
        f"{header_label} "
        f"{color_text('─' * dash_count, 'muted', caps)}"
    )
    print(f"\n{header}")

    # 内容行
    lines = prompt.split("\n")
    for line in lines:
        if line.strip() == "":
            # 空行只输出框线
            print(f"{indent}{color_text('│', 'muted', caps)}")
        elif line.startswith("命令：") or line.startswith("参数："):
            # 关键参数行用 accent 色高亮标签
            _render_detail_line(indent, line, caps, content_width)
        elif "是否允许执行" in line:
            # 核心问题用 primary + bold 突出
            question = color_text(line, "primary", caps)
            if caps.ansi:
                question = f"{ANSI_BOLD}{question}{ANSI_RESET}"
            print(f"{indent}{color_text('│', 'muted', caps)} {question}")
        else:
            # 普通描述行
            print(f"{indent}{color_text('│', 'muted', caps)} {color_text(line, 'text', caps)}")


def _render_detail_line(
    indent: str, line: str, caps: TerminalCapabilities, content_width: int,
) -> None:
    """渲染参数/命令详情行，标签着色 + 内容截断。"""
    # 分离标签和内容
    for sep in ("命令：", "参数：", "文件：", "替换 ", "写入 "):
        if line.startswith(sep):
            label_part = line[: len(sep)]
            content_part = line[len(sep) :]
            break
    else:
        label_part = ""
        content_part = line

    # 计算内容可用宽度
    label_width = _display_width(label_part)
    available = content_width - 3 - label_width  # 3 = "│ " + 1 margin
    truncated = _ellipsize_display_text(content_part, max(1, available))

    if label_part:
        rendered_label = color_text(label_part, "accent", caps)
    else:
        rendered_label = ""
    rendered_content = color_text(truncated, "text", caps)

    print(f"{indent}{color_text('│', 'muted', caps)} {rendered_label}{rendered_content}")


def _count_card_lines(prompt: str) -> int:
    """计算卡片渲染后的总行数（含顶部框线 + 前导空行）。"""
    # 前导空行 1 + 顶部框线 1 + 内容行数
    return 2 + prompt.count("\n") + 1


# ── 交互辅助 ──────────────────────────────────────────────────


def _highlight_selected(text: str, caps: TerminalCapabilities) -> str:
    """高亮选中项：PRIMARY + BOLD + UNDERLINE。"""
    if not caps.ansi:
        return text
    seq = get_color_sequence_static("primary", caps)
    return f"{seq}{ANSI_BOLD}{ANSI_UNDERLINE}{text}{ANSI_RESET}"


def get_color_sequence_static(role: str, caps: TerminalCapabilities) -> str:
    """获取颜色序列（供 _highlight_selected 使用）。"""
    from ._colors import get_color_sequence, ColorRole
    return get_color_sequence(role, caps)


def _selection_from_key(key: str, selected_yes: bool) -> bool:
    if key in {"H", "K"}:
        return True
    if key in {"P", "M"}:
        return False
    return selected_yes


def _redraw_options(print_fn) -> None:
    """回到选项区起始位置，用当前选中状态重绘。"""
    # 选项区共 5 行：分隔线 + 选项 + 底框 + 空行 + 提示
    print("\033[5A", end="")
    print_fn()


def _collapse_and_label(
    label: str,
    total_lines: int,
    confirmed: bool = True,
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> None:
    """选中后把完整确认块收折为单行缩略。"""
    if not caps.ansi:
        return

    symbol = "✓" if confirmed else "✗"
    role: ColorRole = "success" if confirmed else "error"
    with lock:
        print(f"\033[{total_lines}A\033[J", end="")
        if label:
            print(f"{color_text(f'  {symbol} {label}', role, caps)}", flush=True)
        else:
            # 收折为简洁的单行结果
            action = color_text("已允许" if confirmed else "已拒绝", role, caps)
            print(f"  {symbol} {action}", flush=True)


# --- former module: _spinner.py ---
"""等待动画（spinner），带预输入支持和持久输入栏。"""


import os
import threading
import time
from typing import Any

from ._capabilities import TerminalCapabilities
from ._colors import ANSI_CLEAR_LINE, ANSI_PREVIOUS_LINE, color_text
from ._status import StatusLine, prompt_status_line

# Spinner 动画帧
SPINNER_FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]

# 轮播状态文字
_STATUS_LABELS = ["正在思考", "正在分析", "正在生成"]
_STATUS_LABEL_INTERVAL_SECONDS = 2.0


class InputBar:
    """终端底部持久输入栏（▸ + token 状态行）。

    在 AI 输出期间以“输出前清除、输出后追加”的方式维护，避免直接在
    模型输出位置原地重绘时覆盖正文。输入栏同时收集用户预输入。
    """

    def __init__(self, ui: Any, *, caps: TerminalCapabilities | None = None) -> None:
        self._ui = ui
        self._caps = caps or ui.capabilities
        self._pre_input: str = ""
        self._submitted_pre_input: str = ""
        self._visible = False
        # 预输入行数：1 行输入 + (0 或 1) 行 token 状态
        self._info_lines = 1  # 至少输入行

    @property
    def info_lines(self) -> int:
        """当前输入栏占用的终端行数（不含 spinner）。"""
        return self._info_lines

    @property
    def pre_input(self) -> str:
        """当前已收集但未必提交的预输入草稿。"""
        return self._pre_input

    @property
    def submitted_pre_input(self) -> str:
        """用户按 Enter 提交的预输入文本。"""
        return self._submitted_pre_input

    def _pre_input_enabled(self) -> bool:
        return self._caps.ansi and os.name == "nt"

    def _build_token_line(self) -> str:
        if not self._ui.model_label:
            return ""
        return prompt_status_line(
            self._ui.model_label, self._ui._input_tokens,
            self._ui._output_tokens, self._ui._cached_input_tokens,
            caps=self._caps,
        )

    def show(self, spinner_text: str = "") -> None:
        """渲染输入栏。spinner_text 非空时同时显示 spinner 行。"""
        if not self._caps.ansi:
            return
        if not self._pre_input_enabled():
            return

        ui = self._ui
        prompt_prefix = color_text("▸", "primary", self._caps)
        pre_input_text = self._pre_input
        token_line = self._build_token_line()

        has_spinner = bool(spinner_text)
        lines: list[str] = []
        if has_spinner:
            indent = " " * ui.prompt_width()
            lines.append(f"{ANSI_CLEAR_LINE}{ui.bright(f'{indent}{spinner_text}')}")
        lines.append(f"{prompt_prefix} {pre_input_text}")
        if token_line:
            lines.append(token_line)

        with ui._lock:
            if self._visible:
                self._clear_visible_locked()
                leading = ""
            else:
                leading = "\n"
            rendered_lines = "\n".join(lines)
            print(f"{leading}{rendered_lines}", end="", flush=True)
            self._visible = True
            self._info_lines = len(lines)

    def push_up(self) -> None:
        """清除输入栏，为 AI 输出腾出空间。"""
        if not self._visible or not self._caps.ansi:
            return
        with self._ui._lock:
            self._clear_visible_locked()

    def pop_down(self) -> None:
        """AI 输出后，在当前位置下方重新追加输入栏。"""
        if not self._pre_input_enabled():
            return
        self.show()

    def clear(self) -> str:
        """清除输入栏，返回已提交的预输入文本。"""
        pre = self._submitted_pre_input
        if self._visible and self._caps.ansi:
            with self._ui._lock:
                self._clear_visible_locked()
        return pre

    def _clear_visible_locked(self) -> None:
        """从输入栏最后一行向上清理，调用方必须持有 UI 锁。"""
        for index in range(max(0, self._info_lines)):
            print(f"\r{ANSI_CLEAR_LINE}", end="")
            if index < self._info_lines - 1:
                print(ANSI_PREVIOUS_LINE, end="")
        self._visible = False
        self._info_lines = 0

    def poll_pre_input(self) -> None:
        """非阻塞轮询键盘输入。"""
        if not self._pre_input_enabled():
            return
        try:
            import msvcrt
        except ImportError:
            return

        while msvcrt.kbhit():
            char = msvcrt.getwch()
            if char in {"\r", "\n"}:
                submitted = self._pre_input.strip()
                if submitted:
                    self._submitted_pre_input = submitted
                continue
            if char == "\x03":
                raise KeyboardInterrupt
            if char in {"\x00", "\xe0"}:
                if msvcrt.kbhit():
                    msvcrt.getwch()
                continue
            if char in {"\b", "\x7f"}:
                if self._pre_input:
                    self._pre_input = self._pre_input[:-1]
                continue
            if char.isprintable() or char == " ":
                self._pre_input += char


class WaitingIndicator:
    """模型返回前的现代 spinner 等待动画。

    带轮播状态文字、经过时间计数，以及预输入收集。
    spinner 运行时同时显示输入栏（▸ + token 状态行）。
    """

    def __init__(self, status_line: StatusLine, *, caps: TerminalCapabilities | None = None, input_bar: InputBar | None = None) -> None:
        self._status_line = status_line
        self._caps = caps or status_line._ui.capabilities
        self._input_bar = input_bar
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._frame_index = 0
        self._start_time: float = 0.0
        self._has_info_lines = False
        self._rendered_info_lines = 0
        # 无 InputBar 时的后备存储
        self._pre_input: str = ""
        self._submitted_pre_input: str = ""

    @property
    def input_bar(self) -> InputBar | None:
        return self._input_bar

    def start(self) -> None:
        if self._thread is not None and self._thread.is_alive():
            return
        self._stop.clear()
        self._frame_index = 0
        self._start_time = time.monotonic()
        self._has_info_lines = False
        self._rendered_info_lines = 0
        if self._input_bar is not None:
            self._input_bar.clear()
            self._input_bar._pre_input = ""
            self._input_bar._submitted_pre_input = ""
        else:
            self._pre_input = ""
            self._submitted_pre_input = ""
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> str:
        """停止 spinner，保留输入栏，返回已提交的预输入文本。"""

        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None

        pre = self._input_bar._submitted_pre_input if self._input_bar else self._submitted_pre_input

        if self._has_info_lines and self._caps.ansi:
            with self._status_line._ui._lock:
                for index in range(max(0, self._rendered_info_lines)):
                    print(f"\r{ANSI_CLEAR_LINE}", end="")
                    if index < self._rendered_info_lines - 1:
                        print(ANSI_PREVIOUS_LINE, end="")
                print("", end="", flush=True)
            self._has_info_lines = False
            self._rendered_info_lines = 0
        else:
            self._status_line.clear()

        # spinner 停止后，立即以无 spinner 文本的方式重新渲染输入栏
        if self._input_bar is not None and self._input_bar._pre_input_enabled():
            self._input_bar.show()

        return pre

    @property
    def pre_input(self) -> str:
        """当前已收集但未必提交的预输入草稿。"""

        if self._input_bar is not None:
            return self._input_bar.pre_input
        return self._pre_input

    def _pre_input_enabled(self) -> bool:
        """仅在可控的 Windows ANSI 终端中启用预输入布局。"""
        return self._caps.ansi and os.name == "nt"

    def _render_status(self, text: str) -> None:
        """渲染等待状态；预输入模式下维护 spinner 行、输入行和 token 状态行。"""

        if not self._pre_input_enabled():
            self._status_line.show(text)
            return

        ui = self._status_line._ui
        indent = " " * ui.prompt_width()
        rendered_status = ui.bright(f"{indent}{text}")
        prompt_prefix = color_text("▸", "primary", self._caps)
        pre_input_text = self._input_bar._pre_input if self._input_bar else self._pre_input

        # 构建 token 状态行
        token_line = ""
        if ui.model_label:
            token_line = prompt_status_line(
                ui.model_label, ui._input_tokens,
                ui._output_tokens, ui._cached_input_tokens,
                caps=self._caps,
            )

        lines: list[str] = [
            f"{ANSI_CLEAR_LINE}{rendered_status}",
            f"{prompt_prefix} {pre_input_text}",
        ]
        if token_line:
            lines.append(token_line)

        with ui._lock:
            if self._has_info_lines:
                for index in range(max(0, self._rendered_info_lines)):
                    print(f"\r{ANSI_CLEAR_LINE}", end="")
                    if index < self._rendered_info_lines - 1:
                        print(ANSI_PREVIOUS_LINE, end="")
                leading = ""
            else:
                leading = "\n"
            rendered_lines = "\n".join(lines)
            print(f"{leading}{rendered_lines}", end="", flush=True)
            self._has_info_lines = True
            self._rendered_info_lines = len(lines)

    def _poll_pre_input(self) -> None:
        """非阻塞轮询键盘输入。"""

        if not self._pre_input_enabled():
            return
        try:
            import msvcrt
        except ImportError:
            return

        while msvcrt.kbhit():
            char = msvcrt.getwch()
            if char in {"\r", "\n"}:
                # Enter 提交预输入
                draft = self._input_bar._pre_input if self._input_bar else self._pre_input
                submitted = draft.strip()
                if submitted:
                    if self._input_bar is not None:
                        self._input_bar._submitted_pre_input = submitted
                    else:
                        self._submitted_pre_input = submitted
                    self._stop.set()
                continue
            if char == "\x03":
                self._stop.set()
                continue
            if char in {"\x00", "\xe0"}:
                if msvcrt.kbhit():
                    msvcrt.getwch()
                continue
            if char in {"\b", "\x7f"}:
                if self._input_bar is not None:
                    if self._input_bar._pre_input:
                        self._input_bar._pre_input = self._input_bar._pre_input[:-1]
                elif self._pre_input:
                    self._pre_input = self._pre_input[:-1]
                continue
            if char.isprintable() or char == " ":
                if self._input_bar is not None:
                    self._input_bar._pre_input += char
                else:
                    self._pre_input += char

    def _run(self) -> None:
        while not self._stop.is_set():
            self._poll_pre_input()
            if self._stop.is_set():
                break

            frame = SPINNER_FRAMES[self._frame_index % len(SPINNER_FRAMES)]

            # 轮播状态文字
            elapsed = time.monotonic() - self._start_time
            label_index = int(elapsed / _STATUS_LABEL_INTERVAL_SECONDS) % len(_STATUS_LABELS)
            label = _STATUS_LABELS[label_index]

            # 时间计数
            seconds = int(elapsed)

            rendered_frame = color_text(frame, "primary", self._caps) if self._caps.ansi else frame
            rendered_label = color_text(label, "muted", self._caps)
            rendered_time = color_text(f"{seconds}s", "secondary", self._caps) if self._caps.ansi else f"{seconds}s"

            self._render_status(f"{rendered_frame} {rendered_label} · {rendered_time}")
            self._frame_index += 1
            self._stop.wait(0.08)


# --- former module: _core.py ---
"""TerminalUI 核心类。"""


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
from ._tools import ToolDisplayState

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
    'ANSI_BLINK',
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
    '_contains_complex_display_width',
    '_dialog_continuation_prefix',
    '_display_width',
    '_ellipsize_display_text',
    '_normalize_terminal_text',
    '_preview_display_rows',
    '_split_display_rows',
    '_take_display_width',
    'MarkdownSpan',
    'MarkdownStreamState',
    'ToolDisplayState',
    'TerminalUI',
    'StatusLine',
    'InputBar',
    'WaitingIndicator',
    'SPINNER_FRAMES',
]