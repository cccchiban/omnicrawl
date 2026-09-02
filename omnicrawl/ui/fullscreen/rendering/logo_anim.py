"""启动空会话页欢迎 Logo 的“解密扫描”入场动画（纯函数 + 常量）。

动画只在应用首次挂载 Logo 后播放一次：乱码从左到右逐行侵蚀字形、
再让清晰字形从左到右“吐出”，各行的扫描进度按行号错开形成自上而下的
波浪，与底部轮播 HUD 的单行横向扫描保持同一视觉语言。

本模块只提供帧生成纯函数与可复用常量，不持有任何 Widget/定时器，
因此可以脱离 Textual 做确定性测试；播放驱动的接线点在 ``app/core.py``。
``█``/``▒`` 都是单宽字符，乱码按列原位替换不会破坏块字对齐；行首缩进
与内部空格是字形定位，必须保留为空格，不做乱码化。
"""

from __future__ import annotations

import random

from rich.text import Text

from ..terminal.theme import TEXT_MUTED
from .welcome_logo import LOGO_STYLE, welcome_logo_lines, welcome_logo_text

# 复用底部轮播的解密字符集与观感常量，保证两处“解密”特效风格一致。
GARBLE_CHARS = (
    "#@%&*+=<>/\\?^$!~|0123456789"
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
)
# 乱码右侧区间内偶发闪现真实目标字符，形成“解码中”的闪烁观感。
SHIMMER_CHANCE = 0.16
# 总时长与帧间隔（与轮播 0.05s 帧一致；约 24 帧完成一次入场）。
LOGO_ANIM_SECONDS = 1.2
LOGO_ANIM_FRAME_SECONDS = 0.05
# 每行扫描进度相对全局进度的错位比例：行号越大越滞后，产生波浪推进。
ROW_STAGGER = 0.05
# 行内扫描波前越靠右越滞后（横向 + 纵向双重波浪）。
COL_STAGGER = 0.012


def _row_progress(global_progress: float, row_index: int) -> float:
    """计算第 ``row_index`` 行相对全局进度的局部扫描进度。

    全局进度 0~1 被行号错位压缩到各行的 [0, 1] 区间：行错位越大的行
    越晚开始、越晚结束，从而形成自上而下的波浪。行末不足部分让最后
    几行同时收尾，避免动画拖尾过长。
    """

    span = max(1.0 - ROW_STAGGER * 7, 0.1)
    start = ROW_STAGGER * row_index
    row = (global_progress - start) / span
    return min(1.0, max(0.0, row))


def _col_progress(row_progress: float, column: int, row_len: int) -> float:
    """把某行的整体进度按列号进一步错位，形成行内从左到右的扫描波。

    ``column`` 为该字符在行内字形区（去除行首缩进与行尾空白的可见区）的
    相对索引：行首缩进是定位空格，不参与进度错位，避免字形区被大段前导
    空白拖慢。
    """

    if row_len <= 1:
        return row_progress
    span = max(1.0 - COL_STAGGER * (row_len - 1), 0.1)
    col = (row_progress - COL_STAGGER * column) / span
    return min(1.0, max(0.0, col))


def _garble(rand: random.Random) -> str:
    """生成替换乱码；块字字符本身单宽，按 1:1 替换保持宽度。"""

    return rand.choice(GARBLE_CHARS)


def welcome_logo_frame(
    progress: float,
    *,
    rand_source: random.Random | None = None,
) -> Text:
    """生成欢迎 Logo 解密扫描入场动画的一帧。

    进度 ``0.0`` 为纯乱码、``1.0`` 为完整白色 Logo；每一行先被乱码
    波前从左到右侵蚀、随后由清晰字形波前从左到右吐出。行首缩进与
    行内空格始终保留（它们决定块字对齐，不做乱码化）；乱码统一使用
    弱化灰色，清晰字形使用白色，与底部轮播特效同风格。
    """

    rand = rand_source if rand_source is not None else random
    progress = min(1.0, max(0.0, float(progress)))
    if progress >= 1.0:
        # 终态快路径：列错位会残留极少量乱码，这里直接落定完整字形，
        # 保证动画收口与静态 Logo 逐字符一致。
        return welcome_logo_text()
    lines = welcome_logo_lines()
    rendered = Text()
    for row_index, line in enumerate(lines):
        if row_index:
            rendered.append("\n")
        row_prog = _row_progress(progress, row_index)
        # 行尚未进入扫描窗：字形先以乱码形态占位（行首缩进/内部空格保留），
        # 待行进度进入 (0,1) 后由白色字形波前从左到右解出。
        if row_prog <= 0.0:
            for ch in line:
                if ch == " ":
                    rendered.append(" ", style=TEXT_MUTED)
                else:
                    rendered.append(_garble(rand), style=TEXT_MUTED)
            continue
        visible_region = line.strip()
        visible_len = len(visible_region)
        # 字形区相对列号：从第一个非空格字符起算（行首缩进不参与进度错位）。
        glyph_col = -1
        for ch in line:
            if ch == " ":
                rendered.append(" ", style=TEXT_MUTED)
                continue
            glyph_col += 1
            col_prog = _col_progress(row_prog, glyph_col, visible_len)
            if col_prog >= 1.0:
                rendered.append(ch, style=LOGO_STYLE)
            elif col_prog > 0.0:
                # 波前：未解密部分偶发闪现真实字符，其余为灰色乱码。
                if rand.random() < SHIMMER_CHANCE:
                    rendered.append(ch, style=LOGO_STYLE)
                else:
                    rendered.append(_garble(rand), style=TEXT_MUTED)
            else:
                rendered.append(_garble(rand), style=TEXT_MUTED)
    return rendered


__all__ = [
    "GARBLE_CHARS",
    "SHIMMER_CHANCE",
    "LOGO_ANIM_SECONDS",
    "LOGO_ANIM_FRAME_SECONDS",
    "ROW_STAGGER",
    "COL_STAGGER",
    "welcome_logo_frame",
]
