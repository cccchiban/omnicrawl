#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""启动画面：黑色背景 + 左侧黄色 Logo + 右侧圆角日志框 + Windows XP 风格滚动条。

布局类似 fastfetch：左侧显示 Logo，右侧用圆角矩形框（╭ ╮ │ ╰ ╯）展示启动
日志。启动准备（加载配置、MCP、插件、Agent、连接器等）在后台线程执行，并
通过 ``StartupLogSink`` 逐阶段写入日志；日志按级别着色（信息「- 」、警告
「! 」、错误「× 」），超出显示框时向上滚动只保留最新几行。

画面底部仍为黄色滑块滚动条，在 TUI 接管终端前显示（默认直到准备完成）。

系统信息采集已移除；本模块渲染纯用 ANSI 转义序列，跨 Windows / macOS /
Linux 均可运行。
"""

from __future__ import annotations

import shutil
import sys
import threading
import time
import unicodedata
from collections import deque
from typing import Any, Callable, TextIO

# ANSI 转义序列
_CLEAR = "\x1b[2J"
_HOME = "\x1b[H"
_RESET = "\x1b[0m"
_BG_BLACK = "\x1b[40m"
_FG_YELLOW = "\x1b[33m"  # 标准黄（警告）
_FG_BRIGHT_YELLOW = "\x1b[93m"  # 亮黄（Logo / 日志框边框）
_FG_RED = "\x1b[91m"  # 亮红（错误）
_HIDE_CURSOR = "\x1b[?25l"
_SHOW_CURSOR = "\x1b[?25h"

# Windows XP 滚动条：灰色轨道 + 黄色小方块
_TRACK_BG = "\x1b[100m"  # 亮黑/灰
_SLIDER_BG = "\x1b[103m"  # 亮黄

# Logo（来自用户指定的桌面样式文件，去除行尾空白）
LOGO_LINES = [
    '                       !cpmZmmn_',
    '                     tdO0ZmOZwOmZOt,',
    '                   .wmZOZmO0pwmpmwmqqZ+',
    '                   qZwOmwwwmdpwdqpbkkbbbpO>.                    ..',
    "                  ?bwmpZwwwpdkbpdbhahkhaohoaaap-;.        .I{kO0mOL'",
    '                  Qpmwbqpbdpdkabhbokhooooh*o*#**######MWWWaOZOOOO0Y',
    '                  Oqwkbqpkkbdboahoao***M*###*##MMWWWWWWMp] `I!:',
    "                  xpwhbwpkpoooaao***##*M####MMWMWWWWW#L'",
    '                  .bqakhada*oao*o**##*#WMMWWWW&&&&War.',
    '                   1kkkhaoo***o#*#*M##MMWWWWWW&WWb]',
    '                    )hha*oa**o*#**#M#MWMWMW&&&#Z:',
    '                     :do**o*#o**#MMMWWWMWWWMb/.',
    '                       Im*****###M#MWWWMMkv"',
    '                          :jpo**#*##obv_',
]

# 0 表示不设置人为最短时长；启动页仅等待 prepare 完成。
DEFAULT_DURATION = 0.0

# Logo 左侧留白列数与 Logo / 日志框之间的列间距
_LOGO_MARGIN = 2
_BOX_GAP = 4

# 日志框内容区的最小宽度（列）
_BOX_MIN_WIDTH = 30


class StartupLogSink:
    """线程安全的启动日志收集器；渲染线程按帧读取并展示。

    ``prepare`` 在后台线程写入，splash 渲染循环在 UI 主线程读取，
    通过锁保证可见性；快照按写入顺序返回，渲染层只取尾部窗口。
    """

    _LEVELS = frozenset({"info", "warning", "error"})
    # 级别 → 行首标记（颜色在渲染时应用）
    MARKERS = {"info": "- ", "warning": "! ", "error": "× "}

    def __init__(self) -> None:
        self._entries: deque[tuple[str, str]] = deque()
        self._lock = threading.Lock()

    def write_line(self, text: str, level: str = "info") -> None:
        """追加一行启动日志；空行与未知级别被忽略。"""

        if level not in self._LEVELS:
            level = "info"
        message = str(text).strip() if text else ""
        if not message:
            return
        with self._lock:
            # 单行渲染：日志内的换行统一折叠为空格。
            self._entries.append((level, " ".join(message.splitlines())))

    def snapshot(self) -> tuple[tuple[str, str], ...]:
        """返回当前全部日志（按写入顺序）。"""

        with self._lock:
            return tuple(self._entries)


def _logo_render_lines() -> list[str]:
    """返回实际渲染的 Logo 行：去掉所有行共有的前导空格。

    原 Logo 文本自带大量前导空白，会把图形推到第 25 列附近，挤占右侧
    日志框；统一去掉公共前导空格后，图形从 ``_LOGO_MARGIN`` 处开始。
    """
    indent = min(len(line) - len(line.lstrip(" ")) for line in LOGO_LINES)
    return [line[indent:] for line in LOGO_LINES]


def _char_width(char: str) -> int:
    """返回字符的显示宽度：CJK 等全角字符计 2 列，其余计 1 列。"""

    return 2 if unicodedata.east_asian_width(char) in {"W", "F"} else 1


def _wrap_text(text: str, width: int) -> list[str]:
    """按显示宽度折行：CJK/全角字符计 2 列，超宽单词按字符拆分。"""

    if width <= 0:
        return [text]

    lines: list[str] = []
    current = ""
    current_width = 0
    for char in text:
        char_width = _char_width(char)
        if current and current_width + char_width > width:
            lines.append(current)
            current = char
            current_width = char_width
        else:
            current += char
            current_width += char_width
    if current:
        lines.append(current)
    return lines or [""]


def _pad_to_width(line: str, width: int) -> str:
    """把一行补齐/截断到指定显示宽度（全角字符计 2 列）。

    折行已保证内容不超过 ``width``，这里只做兜底截断并按剩余宽度补空格，
    避免重绘时行尾残留旧文本，也不会因 CJK 双宽字符把内容推出日志框。
    """

    chars: list[str] = []
    used = 0
    for char in line:
        char_width = _char_width(char)
        if used + char_width > width:
            break
        chars.append(char)
        used += char_width
    return "".join(chars) + " " * (width - used)


def _is_tty(stream: TextIO | None) -> bool:
    """判断输出流是否支持交互式终端渲染。"""
    if stream is None:
        return False
    isatty = getattr(stream, "isatty", None)
    return callable(isatty) and bool(isatty())


def run_startup_splash(
    prepare: Callable[[StartupLogSink], Any],
    duration: float = DEFAULT_DURATION,
    stream: TextIO | None = None,
) -> Any:
    """显示启动画面，同时后台执行 ``prepare``（加载所有启动依赖）。

    Args:
        prepare: 启动准备回调，在 splash 显示期间于后台线程执行；接收
            一个 :class:`StartupLogSink`，可随时写入启动日志行，返回值
            会透传给调用方，异常会在 splash 结束后重新抛出。
        duration: 可选的最短显示秒数；为 0 时不设置人为等待，默认直到
            prepare 完成就结束；若 prepare 耗时更长，则持续显示滚动条。
        stream: 输出流，默认 ``sys.stdout``；非交互流（如测试管道）时
            直接同步执行 ``prepare`` 并返回，不显示动画。

    Returns:
        ``prepare()`` 的返回值。
    """

    out = stream if stream is not None else sys.stdout
    sink = StartupLogSink()

    def _prepare() -> Any:
        return prepare(sink)

    if not _is_tty(out):
        # 非交互环境（测试、管道、重定向）：同步执行，不渲染动画。
        return _prepare()

    result: dict[str, Any] = {}
    error_box: dict[str, BaseException] = {}

    def _worker() -> None:
        try:
            result["value"] = _prepare()
        except BaseException as exc:  # noqa: BLE001
            error_box["error"] = exc

    thread = threading.Thread(target=_worker, name="omnicrawl-splash-prepare", daemon=True)
    thread.start()
    try:
        _render_splash(
            out,
            duration,
            sink,
            is_done=lambda: "value" in result,
            has_error=lambda: bool(error_box),
        )
    finally:
        thread.join(timeout=max(1.0, duration + 5.0))
    if error_box:
        raise error_box["error"]
    return result.get("value")


def _render_splash(
    out: TextIO,
    duration: float,
    sink: StartupLogSink,
    is_done: Callable[[], bool],
    has_error: Callable[[], bool],
) -> None:
    """绘制启动画面：左侧 Logo + 右侧圆角日志框 + 底部 XP 滚动条。

    仅在显式设置最短时长时等待到时长满足，并且始终等待准备完成；任何异常
    都不允许影响启动主流程。日志框只保留最新的 ``box_inner_rows`` 行，
    内容变化时整窗重绘，避免重叠残留。
    """

    width, height = shutil.get_terminal_size(fallback=(80, 24))
    logo_lines = _logo_render_lines()
    logo_width = max(len(line) for line in logo_lines)
    logo_rows = len(logo_lines)

    # 左侧 Logo 从第 3 列开始；日志框紧跟 Logo 顶部对齐
    left = _LOGO_MARGIN
    box_left = left + logo_width + _BOX_GAP
    # 窄终端保护：日志框至少保留 30 列；放不下时整体左移，但下限必须保证
    # Logo 与日志框之间至少 1 列间隙，绝不能覆盖 Logo；若终端仍不够宽，
    # 允许日志框超出右边界（由终端换行），优先保证 Logo 完整可见。
    if box_left + _BOX_MIN_WIDTH > width:
        box_left = max(left + logo_width + 1, width - _BOX_MIN_WIDTH)
    inner_width = max(6, width - box_left - 2)  # 左右各 1 列边框

    # 日志框高度跟随 Logo：上下边框 + 内容区，垂直与 Logo 主体对齐
    box_inner_rows = max(4, min(logo_rows, height - 8))
    box_outer_rows = box_inner_rows + 2
    body_rows = max(logo_rows, box_outer_rows)
    total_rows = body_rows + 4
    top = max(0, (height - total_rows) // 2)
    box_top = top + (body_rows - box_outer_rows) // 2
    bar_row = top + body_rows + 2

    out.write(_BG_BLACK + _CLEAR + _HOME + _HIDE_CURSOR)
    out.flush()

    # 左侧 logo（亮黄色）
    for i, line in enumerate(logo_lines):
        row = top + i + 1
        out.write(f"\x1b[{row};{left + 1}H" + _FG_BRIGHT_YELLOW + line + _RESET)

    _draw_box_border(out, box_top, box_left, inner_width, box_inner_rows)
    _draw_box_content(
        out,
        box_top,
        box_left,
        inner_width,
        box_inner_rows,
        sink.snapshot(),
    )
    out.flush()

    bar_width = min(30, max(10, (width - 4) // 2))
    slider_width = max(4, bar_width // 5)
    bar_left = max(0, (width - bar_width) // 2)

    start = time.monotonic()
    frame = 0
    last_entries: tuple[tuple[str, str], ...] = ()
    while True:
        if has_error():
            # 准备失败：立即结束画面，让调用方快速看到错误。
            break
        elapsed = time.monotonic() - start
        if elapsed >= duration and is_done():
            break
        entries = sink.snapshot()
        if entries != last_entries:
            _draw_box_content(
                out,
                box_top,
                box_left,
                inner_width,
                box_inner_rows,
                entries,
            )
            last_entries = entries
        _draw_progress_bar(out, bar_row, bar_left, bar_width, slider_width, frame)
        frame += 1
        time.sleep(0.05)

    # 复位：显示光标并清屏，避免残留
    out.write(_SHOW_CURSOR + _RESET + _CLEAR + _HOME)
    out.flush()


def _draw_box_border(
    out: TextIO,
    top: int,
    left: int,
    inner_width: int,
    inner_rows: int,
) -> None:
    """绘制圆角矩形日志框边框（╭ ╮ │ ╰ ╯），亮黄色以匹配 Logo。"""

    right = left + inner_width + 1
    out.write(
        f"\x1b[{top};{left + 1}H"
        + _FG_BRIGHT_YELLOW
        + "╭"
        + "─" * inner_width
        + "╮"
        + _RESET
    )
    for row in range(1, inner_rows + 1):
        out.write(
            f"\x1b[{top + row};{left + 1}H"
            + _FG_BRIGHT_YELLOW
            + "│"
            + _RESET
            + f"\x1b[{top + row};{right + 1}H"
            + _FG_BRIGHT_YELLOW
            + "│"
            + _RESET
        )
    out.write(
        f"\x1b[{top + inner_rows + 1};{left + 1}H"
        + _FG_BRIGHT_YELLOW
        + "╰"
        + "─" * inner_width
        + "╯"
        + _RESET
    )


def _draw_box_content(
    out: TextIO,
    top: int,
    left: int,
    inner_width: int,
    inner_rows: int,
    entries: tuple[tuple[str, str], ...],
) -> None:
    """把日志尾窗口绘制进日志框；内容超出时只保留最新行（向上滚动）。

    信息「- 」默认前景色，警告「! 」黄色，错误「× 」红色；每行按框内
    宽度折行并补齐空白，重绘时不残留旧文本。
    """

    display: list[str] = []
    for level, text in entries:
        prefix = StartupLogSink.MARKERS.get(level, "- ")
        chunks = _wrap_text(text, max(1, inner_width - len(prefix)))
        for index, chunk in enumerate(chunks):
            if index == 0:
                display.append(f"{prefix}{chunk}")
            else:
                display.append(" " * len(prefix) + chunk)

    visible = display[-inner_rows:]
    for row, line in enumerate(visible):
        padded = _pad_to_width(line, inner_width)
        if line.startswith("! "):
            style = _FG_YELLOW
        elif line.startswith("× "):
            style = _FG_RED
        else:
            style = ""
        out.write(f"\x1b[{top + 1 + row};{left + 2}H" + style + padded + _RESET)


def _draw_progress_bar(
    out: TextIO,
    row: int,
    left: int,
    width: int,
    slider_width: int,
    frame: int,
) -> None:
    """在指定行绘制一帧滚动条（灰色轨道 + 黄色滑块循环滑动）。

    动画流程（每周期 39 帧，约 2 秒）：
      1. 滑入：滑块从轨道左侧外右移进入，可见部分逐帧变宽，直到完整出现在左端；
      2. 横穿：滑块保持全宽匀速滑过整个轨道，到达右端；
      3. 滑出：滑块整体继续右移、滑出右边界，可见部分逐帧变窄直至完全消失；
      4. 空档：轨道上无滑块，停顿片刻后重新从左侧滑入，形成单向循环。
    """

    fade = slider_width  # 滑入/滑出各占 slider_width 帧
    travel = max(1, width - slider_width)  # 全宽横穿帧数
    gap = max(2, slider_width // 2)  # 完全消失后的空档帧数
    cycle = fade + travel + fade + gap
    t = frame % cycle

    if t < fade:
        # 阶段 1：滑入 —— 滑块左端从 -slider_width 推进到 0，逐渐出现在左端
        pos = t - slider_width
    elif t < fade + travel:
        # 阶段 2：横穿 —— 滑块左端从 0 推进到 width - slider_width
        pos = t - fade
    elif t < fade + travel + fade:
        # 阶段 3：滑出 —— 滑块左端从 width - slider_width 推进到 width，逐渐消失
        pos = (t - fade - travel) + (width - slider_width)
    else:
        # 阶段 4：空档 —— 滑块完全移出轨道，不绘制
        pos = width  # 超出轨道右边界，裁剪区间为空

    # 统一裁剪：只绘制滑块与轨道 [0, width) 相交的可见部分
    start = max(0, pos)
    end = min(width, pos + slider_width)

    line_parts: list[str] = []
    line_parts.append(f"\x1b[{row};{left + 1}H")
    line_parts.append(_TRACK_BG)
    line_parts.append(" " * start)
    if end > start:
        line_parts.append(_SLIDER_BG)
        line_parts.append(" " * (end - start))
    # 滑块右侧的轨道必须重新声明灰色背景，否则终端会沿用滑块黄色背景，
    # 导致滑块右侧整段都被染黄（此前误渲染成"进度条填充"观感）。
    line_parts.append(_TRACK_BG)
    line_parts.append(" " * (width - end))
    line_parts.append(_RESET)
    out.write("".join(line_parts))
    out.flush()


__all__ = [
    "DEFAULT_DURATION",
    "LOGO_LINES",
    "StartupLogSink",
    "run_startup_splash",
]