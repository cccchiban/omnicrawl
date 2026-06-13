"""终端文本显示宽度计算与截断。"""

from __future__ import annotations

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
