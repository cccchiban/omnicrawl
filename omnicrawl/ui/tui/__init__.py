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

    # 重定向到文件、IDE 捕获器或管道时绝不能输出光标控制序列；环境变量
    # 只能说明终端类型，不能证明当前 stdout 仍是交互式 TTY。
    if not sys.stdout.isatty() or os.getenv("NO_COLOR"):
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


def _is_regional_indicator(char: str) -> bool:
    return "\U0001F1E6" <= char <= "\U0001F1FF"


def _is_emoji_modifier(char: str) -> bool:
    return "\U0001F3FB" <= char <= "\U0001F3FF"


def _iter_display_units(text: str):
    """按终端可见单元遍历文本，避免在常见 emoji 字素簇中间截断。

    标准库没有完整的 UAX #29 字素分割器。这里覆盖 Windows Terminal 中最常见、
    且最容易造成光标错位的 ZWJ、变体选择符、键帽、旗帜与肤色修饰组合；其余
    复杂文本仍会走保守的追加渲染路径，不依赖原地列移动。
    """

    index = 0
    length = len(text)
    while index < length:
        start = index
        char = text[index]
        index += 1

        if _is_regional_indicator(char) and index < length and _is_regional_indicator(text[index]):
            index += 1

        while index < length and (unicodedata.combining(text[index]) or text[index] in {"\ufe0e", "\ufe0f"} or _is_emoji_modifier(text[index])):
            index += 1

        if index < length and text[index] == "\u20e3":
            index += 1

        while index < length and text[index] == "\u200d":
            index += 1
            if index >= length:
                break
            index += 1
            while index < length and (unicodedata.combining(text[index]) or text[index] in {"\ufe0e", "\ufe0f"} or _is_emoji_modifier(text[index])):
                index += 1
            if index < length and text[index] == "\u20e3":
                index += 1

        yield text[start:index]


def _display_unit_width(unit: str) -> int:
    """返回一个不可拆分终端单元的保守列宽。"""

    if not unit:
        return 0
    if "\u200d" in unit or any(_is_regional_indicator(char) for char in unit) or any(_is_emoji_modifier(char) for char in unit):
        return 2
    return sum(_char_display_width(char) for char in unit)


def _display_width(text: str) -> int:
    return sum(_display_unit_width(unit) for unit in _iter_display_units(text))


def _take_display_width(text: str, max_width: int) -> str:
    if max_width <= 0:
        return ""

    width = 0
    units: list[str] = []
    for unit in _iter_display_units(text):
        unit_width = _display_unit_width(unit)
        if width + unit_width > max_width:
            break
        units.append(unit)
        width += unit_width
    return "".join(units)


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

    return any(
        _display_unit_width(unit) != 1 or len(unit) != 1
        for unit in _iter_display_units(text)
    )


def _normalize_terminal_text(text: str) -> str:
    """统一终端文本换行，避免 CRLF 在行数计算里被当成额外字符。"""

    return text.replace("\r\n", "\n").replace("\r", "\n")


def _delete_last_display_unit(text: str) -> str:
    """删除末尾完整显示单元，供等待时的预输入退格复用。"""

    units = list(_iter_display_units(text))
    return "".join(units[:-1])


def _combine_surrogate_pair(pending_high: str, low: str) -> str | None:
    """将 Windows ``msvcrt.getwch`` 分次返回的 UTF-16 代理对合并为 Unicode 字符。"""

    if len(pending_high) != 1 or len(low) != 1:
        return None
    high_code = ord(pending_high)
    low_code = ord(low)
    if not (0xD800 <= high_code <= 0xDBFF and 0xDC00 <= low_code <= 0xDFFF):
        return None
    code_point = 0x10000 + ((high_code - 0xD800) << 10) + low_code - 0xDC00
    return chr(code_point)


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
                # 宽度大于当前剩余列的完整字素簇独占一行，绝不把它拆开。
                chunk = next(_iter_display_units(remaining))
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

    @staticmethod
    def _markdown_preview_max_width() -> int:
        terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
        return max(1, terminal_width - 4)

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
from typing import Any

from ._colors import (
    ANSI_CLEAR_LINE,
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
) -> None:
    """打印工具开始执行的结构化记录。"""

    indent = _dialog_continuation_prefix(AI_PREFIX)
    # 闪烁在 Windows Terminal/VS Code 中表现不一且容易分散注意力；使用静态标记。
    marker = color_text("◌", "warning", caps)
    line_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    # 窄终端优先保证信息不溢出，不强行保留装饰性框线。
    header_prefix = f"◌ 步骤 {max(1, step)} · "
    decoration = " ─ ─ ─" if line_width >= 36 else ""
    tool_width = max(1, line_width - _display_width(indent) - _display_width(header_prefix) - _display_width(decoration))
    visible_tool_name = _ellipsize_display_text(tool_name, tool_width)

    # 工具记录是次级信息：使用单一语义色的紧凑标题，避免小窗口内多层框线挤压正文。
    header = (
        f"{indent}{marker} "
        f"{color_text('步骤', 'muted', caps)} "
        f"{color_text(str(max(1, step)), 'primary', caps)}"
        f"{color_text(' · ', 'muted', caps)}"
        f"{color_text(visible_tool_name, 'accent', caps)}"
        f"{color_text(decoration, 'muted', caps)}"
    )
    detail_rows = _tool_call_detail_rows(tool_name, arguments, caps)
    prefix = "\n" if leading_blank or step > 1 else ""

    with lock:
        print(f"{prefix}{header}")
        for row in detail_rows:
            print(f"{indent}{row}")
        sys.stdout.flush()


def print_tool_result_record(
    ok: bool,
    output: str | None = None,
    *,
    tool_name: str = "",
    caps: TerminalCapabilities = TerminalCapabilities(ansi=False),
    lock: threading.Lock = threading.Lock(),  # noqa: B008 — 模块级默认值，仅作回退
) -> None:
    """打印工具执行结果摘要。"""

    indent = _dialog_continuation_prefix(AI_PREFIX)
    terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    result_marker_text = "✓" if ok else "✗"
    result_label_text = "成功" if ok else "失败"
    result_prefix = f"│ {result_marker_text} {result_label_text}"
    suffix = _tool_result_suffix(
        tool_name,
        output or "",
        caps,
        available_width=max(0, terminal_width - _display_width(indent) - _display_width(result_prefix)),
    )
    output_rows = _tool_result_output_rows(tool_name, output or "", caps)

    with lock:
        result_marker = color_text(result_marker_text, "success" if ok else "error", caps)
        if output_rows:
            # 有输出行：结果行用 │ 继续，输出行最后一行用 ╰─ 关闭
            print(
                f"{indent}{color_text('│ ', 'muted', caps)}{result_marker} {color_text(result_label_text, 'success' if ok else 'error', caps)}{suffix}",
            )
            for row in output_rows:
                print(f"{indent}{row}")
        else:
            # 无输出行：结果行本身就是最后一行，用 ╰─ 关闭
            print(
                f"{indent}{color_text('╰─ ', 'muted', caps)}{result_marker} {color_text(result_label_text, 'success' if ok else 'error', caps)}{suffix}",
            )
        sys.stdout.flush()


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


def _tool_result_suffix(
    tool_name: str,
    output: str,
    caps: TerminalCapabilities,
    *,
    available_width: int,
) -> str:
    if tool_name != "run_command":
        return ""
    match = re.search(r"^退出码：(-?\d+)", output)
    if match is None:
        return ""
    suffix = f" · 退出码 {match.group(1)}"
    if _display_width(suffix) > available_width:
        suffix = f" · {match.group(1)}"
    return color_text(suffix, "muted", caps)


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
    terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
    # 调用方还会输出一个额外空格；此处统一扣除，保证物理行不越界。
    return max(1, terminal_width - indent_width - _display_width("│  "))


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


def _prompt_status_parts(
    model_label: str | None,
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
) -> list[tuple[str, ColorRole]]:
    """返回未着色的 token 状态片段，供单行和窄屏换行渲染共用。"""

    parts: list[tuple[str, ColorRole]] = []
    if model_label:
        parts.append((f"- {model_label}", "muted"))
    parts.extend(
        [
            (f"in:{input_tokens}", "primary"),
            (f"cache:{cached_input_tokens}", "success"),
            (f"out:{output_tokens}", "secondary"),
        ]
    )
    return parts


def _render_styled_status_rows(
    text: str,
    *,
    role: ColorRole,
    caps: TerminalCapabilities,
    max_width: int,
) -> list[str]:
    """在文本折行后为每个物理行独立应用状态色。"""

    return [
        color_text(row, role, caps)
        for row in _split_display_rows(text, max(1, max_width))
    ]


def _render_prompt_status_rows(
    model_label: str | None,
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
    *,
    caps: TerminalCapabilities,
    max_width: int,
) -> list[str]:
    """按可见宽度分行后再着色，绝不在 ANSI SGR 序列中间换行。"""

    max_width = max(1, max_width)
    parts = _prompt_status_parts(
        model_label,
        input_tokens,
        output_tokens,
        cached_input_tokens,
    )
    rows: list[list[tuple[str, ColorRole]]] = [[]]
    row_width = 0
    for text, role in parts:
        separator = " " if rows[-1] else ""
        part_width = _display_width(separator + text)
        if row_width and row_width + part_width > max_width:
            rows.append([])
            row_width = 0
            separator = ""
        if separator:
            rows[-1].append((separator, "text"))
            row_width += 1
        for unit in _iter_display_units(text):
            unit_width = _display_unit_width(unit)
            if row_width and row_width + unit_width > max_width:
                rows.append([])
                row_width = 0
            rows[-1].append((unit, role))
            row_width += unit_width

    rendered_rows: list[str] = []
    for row in rows:
        if not row:
            continue
        # 同一语义色的连续显示单元合并后再套 SGR，既保证换行边界安全，
        # 又避免窄屏 token 行产生大量单字符 ANSI 序列。
        fragments: list[tuple[str, ColorRole]] = []
        for text, role in row:
            if fragments and fragments[-1][1] == role:
                fragments[-1] = (fragments[-1][0] + text, role)
            else:
                fragments.append((text, role))
        rendered_rows.append(
            "".join(color_text(text, role, caps) for text, role in fragments)
        )
    return rendered_rows or [""]


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

    terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    margin_width = _display_width("  ")
    content_width = max(1, terminal_width - margin_width - 1)
    groups = _parse_panel_groups(lines)

    with lock:
        # 现代工具界面：一条主色标记、紧凑分组、弱化辅助信息，不依赖固定卡片宽度。
        for row in _split_display_rows(title, max(1, content_width - _display_width("█ "))):
            print(f"  {color_text('█', 'primary', caps)} {color_text(row, 'heading', caps)}")
        print(f"  {color_text('─' * max(1, content_width - 1), 'muted', caps)}")

        for group_name, group_lines in groups:
            if group_name:
                for row in _split_display_rows(group_name, content_width):
                    print(f"  {color_text(row, 'secondary', caps)}")
            for line in group_lines:
                for row in _render_panel_rows(line, caps, content_width):
                    print(f"  {row}")

        help_text = "输入问题开始对话 · /help 查看命令"
        for row in _split_display_rows(help_text, content_width):
            print(f"  {color_text(row, 'muted', caps)}")
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


def _render_panel_rows(line: str, caps: TerminalCapabilities, content_width: int) -> list[str]:
    """渲染不会超过终端实际宽度的配置行。"""

    colon_idx = line.find(":")
    if colon_idx < 0:
        return [color_text(row, "text", caps) for row in _split_display_rows(line, content_width)]

    key = line[:colon_idx].strip()
    value = line[colon_idx + 1:].strip()
    prefix = f"▸ {key}  "
    first_width = max(1, content_width - _display_width(prefix))
    value_rows = _split_display_rows(value, first_width)
    rows = [
        f"{color_text('▸', 'primary', caps)} {color_text(key, 'text', caps)}  "
        f"{_render_status_value(value_rows[0], caps)}"
    ]
    rows.extend(color_text(row, "text", caps) for row in value_rows[1:])
    return rows


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
    ColorRole,
    color_text,
)
from ._capabilities import TerminalCapabilities
from ._display import _dialog_continuation_prefix, _display_width

AI_PREFIX = "◆"


def prompt_yes_no(
    prompt: str,
    confirmed_label: str = "",
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> bool:
    """以可追加的确认记录请求高风险操作。

    不再回跳折叠或原地重绘选项：确认框可能跨越终端滚动边界或在等待时发生
    resize。追加明确的选择和结果比依赖相对行数更稳定，也保留了完整审计历史。
    """

    with lock:
        _render_confirmation_card(prompt, caps)

    try:
        import msvcrt
    except ImportError:
        answer = input("确认？[Enter=YES / n=NO] ").strip().lower()
        confirmed = answer not in {"n", "no", "否", "false", "2"}
    else:
        confirmed = _read_confirmation_choice(msvcrt)

    with lock:
        choice = "允许" if confirmed else "拒绝"
        role: ColorRole = "success" if confirmed else "error"
        print(f"  {color_text(f'选择：{choice}', role, caps)}")
        if confirmed_label:
            print(f"  {color_text(confirmed_label, role, caps)}", flush=True)
    return confirmed


def _read_confirmation_choice(msvcrt) -> bool:
    """读取确认按键；使用显式按键而非动态菜单，避免光标重绘竞争。"""

    while True:
        char = msvcrt.getwch()
        if char in {"\r", "\n", "1", "y", "Y"}:
            return True
        if char in {"n", "N", "2"}:
            return False
        if char == "\x03":
            raise KeyboardInterrupt
        # 追加式确认卡不再重绘选择状态；方向键只会被消费，绝不会让一次
        # 误触直接执行工具，仍需 Enter/Y/1 或 N/2 做出明确决定。
        if char in {"\x00", "\xe0"}:
            msvcrt.getwch()


# ── 卡片渲染 ──────────────────────────────────────────────────


def _confirmation_content_width() -> int:
    terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
    return max(1, terminal_width - indent_width - _display_width("│ "))


def _render_confirmation_text_rows(text: str, content_width: int) -> list[str]:
    rows: list[str] = []
    for raw_line in _normalize_terminal_text(text).split("\n"):
        rows.extend(_split_display_rows(raw_line, content_width))
    return rows or [""]


def _render_confirmation_card(prompt: str, caps: TerminalCapabilities) -> None:
    """在实际可用宽度内绘制稳定、可回读的确认卡片。"""

    indent = _dialog_continuation_prefix(AI_PREFIX)
    content_width = _confirmation_content_width()
    header_text = "⚠ 确认执行"
    # 总宽度包含 indent、╭─ 和一个分隔空格；小窗口省略装饰横线。
    terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    header_available = max(0, terminal_width - _display_width(indent) - _display_width("╭─ ") - _display_width(header_text))
    header_suffix = "─" * header_available
    print(
        f"\n{indent}{color_text('╭─', 'muted', caps)} "
        f"{color_text(header_text, 'warning', caps)}"
        f"{color_text(header_suffix, 'muted', caps)}"
    )

    for raw_line in _normalize_terminal_text(prompt).split("\n"):
        if not raw_line:
            print(f"{indent}{color_text('│', 'muted', caps)}")
            continue

        if raw_line.startswith(("命令：", "参数：", "文件：", "替换 ", "写入 ")):
            label, value = _split_confirmation_detail(raw_line)
            first_width = max(1, content_width - _display_width(label))
            value_rows = _split_display_rows(value, first_width)
            for index, row in enumerate(value_rows):
                if index == 0:
                    rendered = color_text(label, "accent", caps) + color_text(row, "text", caps)
                else:
                    rendered = color_text(row, "text", caps)
                print(f"{indent}{color_text('│', 'muted', caps)} {rendered}")
            continue

        role: ColorRole = "primary" if "是否允许执行" in raw_line else "text"
        for row in _render_confirmation_text_rows(raw_line, content_width):
            print(f"{indent}{color_text('│', 'muted', caps)} {color_text(row, role, caps)}")

    print(f"{indent}{color_text('╰─', 'muted', caps)}")
    hint = "Enter/Y 允许 · N 拒绝"
    hint_width = max(1, max(1, shutil.get_terminal_size((100, 30)).columns) - _display_width(indent))
    for row in _split_display_rows(hint, hint_width):
        print(f"{indent}{color_text(row, 'muted', caps)}")


def _split_confirmation_detail(line: str) -> tuple[str, str]:
    for separator in ("命令：", "参数：", "文件：", "替换 ", "写入 "):
        if line.startswith(separator):
            return line[: len(separator)], line[len(separator) :]
    return "", line



# --- former module: _spinner.py ---
"""等待动画（spinner），带预输入支持和持久输入栏。"""


import os
import threading
import time
from typing import Any

from ._capabilities import TerminalCapabilities
from ._colors import ANSI_CLEAR_LINE, ANSI_PREVIOUS_LINE, color_text
from ._status import StatusLine

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
        # getwch() 在 Windows 上对非 BMP 字符返回两个 UTF-16 代理项；需要跨
        # 非阻塞轮询暂存高代理，等低代理到达后再插入完整显示单元。
        self._pending_high_surrogate: str = ""
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

    def show(self, spinner_text: str = "") -> None:
        """渲染输入栏，并按实际终端列数记录其物理行数。"""
        if not self._caps.ansi or not self._pre_input_enabled():
            return

        ui = self._ui
        prompt_prefix = color_text("▸", "primary", self._caps)
        terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
        input_width = max(1, terminal_width - ui.prompt_width() - 1)
        input_rows = _split_display_rows(self._pre_input, input_width)
        input_lines = [
            f"{prompt_prefix} {row}" if index == 0 else f"{' ' * ui.prompt_width()}{row}"
            for index, row in enumerate(input_rows)
        ]
        lines: list[str] = []
        if spinner_text:
            indent = " " * ui.prompt_width()
            spinner_rows = _render_styled_status_rows(
                f"{indent}{strip_ansi(spinner_text)}",
                role="muted",
                caps=self._caps,
                max_width=terminal_width,
            )
            lines.extend(f"{ANSI_CLEAR_LINE}{row}" for row in spinner_rows)
        lines.extend(input_lines)
        if ui.model_label:
            lines.extend(
                _render_prompt_status_rows(
                    ui.model_label,
                    ui._input_tokens,
                    ui._output_tokens,
                    ui._cached_input_tokens,
                    caps=self._caps,
                    max_width=terminal_width,
                )
            )

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
            if self._pending_high_surrogate:
                combined = _combine_surrogate_pair(self._pending_high_surrogate, char)
                if combined is not None:
                    self._pre_input += combined
                    self._pending_high_surrogate = ""
                    continue
                self._pre_input += "�"
                self._pending_high_surrogate = ""
            if 0xD800 <= ord(char) <= 0xDBFF:
                self._pending_high_surrogate = char
                continue
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
                    self._pre_input = _delete_last_display_unit(self._pre_input)
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
        self._pending_high_surrogate: str = ""

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
            self._input_bar._pending_high_surrogate = ""
        else:
            self._pre_input = ""
            self._submitted_pre_input = ""
            self._pending_high_surrogate = ""
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
        """渲染等待状态；预输入模式按物理行数维护动态区域。"""

        if not self._pre_input_enabled():
            self._status_line.show(text)
            return

        ui = self._status_line._ui
        terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
        indent = " " * ui.prompt_width()
        # spinner 文本在逻辑层保持纯文本；先换行再为每个物理行独立套样式，
        # 避免 SGR 状态跨行泄漏到输入栏或 token 行。
        status_rows = _render_styled_status_rows(
            f"{indent}{strip_ansi(text)}",
            role="muted",
            caps=self._caps,
            max_width=terminal_width,
        )
        prompt_prefix = color_text("▸", "primary", self._caps)
        pre_input_text = self._input_bar._pre_input if self._input_bar else self._pre_input
        input_rows = _split_display_rows(
            pre_input_text,
            max(1, terminal_width - ui.prompt_width() - 1),
        )
        lines = [f"{ANSI_CLEAR_LINE}{row}" for row in status_rows]
        lines.extend(
            f"{prompt_prefix} {row}" if index == 0 else f"{' ' * ui.prompt_width()}{row}"
            for index, row in enumerate(input_rows)
        )
        if ui.model_label:
            lines.extend(
                _render_prompt_status_rows(
                    ui.model_label,
                    ui._input_tokens,
                    ui._output_tokens,
                    ui._cached_input_tokens,
                    caps=self._caps,
                    max_width=terminal_width,
                )
            )

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
            if self._pending_high_surrogate:
                combined = _combine_surrogate_pair(self._pending_high_surrogate, char)
                if combined is not None:
                    if self._input_bar is not None:
                        self._input_bar._pre_input += combined
                    else:
                        self._pre_input += combined
                    self._pending_high_surrogate = ""
                    continue
                if self._input_bar is not None:
                    self._input_bar._pre_input += "�"
                else:
                    self._pre_input += "�"
                self._pending_high_surrogate = ""
            if 0xD800 <= ord(char) <= 0xDBFF:
                self._pending_high_surrogate = char
                continue
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
                        self._input_bar._pre_input = _delete_last_display_unit(
                            self._input_bar._pre_input
                        )
                elif self._pre_input:
                    self._pre_input = _delete_last_display_unit(self._pre_input)
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

            # _render_status 会在换行后统一为弱化状态色，不能把已含 SGR 的
            # 片段交给普通显示宽度拆行器，否则颜色状态可能跨越物理行。
            self._render_status(f"{frame} {label} · {seconds}s")
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