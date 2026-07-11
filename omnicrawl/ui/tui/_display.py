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
