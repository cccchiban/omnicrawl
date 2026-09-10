"""全屏工作台 HUD 的纯 Rich 文本格式化函数。"""

from __future__ import annotations

import random
import unicodedata
from importlib import resources

from rich.text import Text

from ..terminal.theme import (
    ACCENT_GREEN,
    BORDER_MUTED,
    TEXT_MUTED,
    TEXT_PRIMARY,
)

# 底部轮播留言页的候选文本文件（与代码同目录，随 pip 分发）。
CAROUSEL_MESSAGES_FILE = "carousel_messages.txt"


def load_carousel_message_lines() -> list[str]:
    """读取包内轮播候选文本（与代码同目录，可随 pip 分发、可被编辑）。

    逐行去除首尾空白并丢弃空行；文件缺失/不可读时返回空列表，
    由消息页展示占位文本兜底。
    """

    try:
        resource = (
            resources.files("omnicrawl.ui.fullscreen.status")
            .joinpath(CAROUSEL_MESSAGES_FILE)
        )
        text = resource.read_text(encoding="utf-8")
    except (OSError, UnicodeError, ModuleNotFoundError):
        return []
    return [line.strip() for line in text.splitlines() if line.strip()]


def compact_token_count(value: int) -> str:
    """使用 K/M 缩写压缩 Token 数，同时保留小数量的精确值。"""

    value = max(0, int(value))
    if value < 1_000:
        return str(value)
    if value < 1_000_000:
        return f"{value / 1_000:.1f}K".replace(".0K", "K")
    return f"{value / 1_000_000:.1f}M".replace(".0M", "M")


def context_usage_text(input_tokens: int, context_limit: int) -> Text:
    """生成上下文占用文本：用量/总量 + 百分比（无 CTX 前缀、无进度条）。

    内容紧排不补固定宽度，`0/1M 0%` 直接跟随分隔符；超限时百分比可
    超过 100（如 150%）。
    """

    context_limit = max(1, int(context_limit))
    input_tokens = max(0, int(input_tokens))
    ratio = input_tokens / context_limit
    percent = min(999, round(ratio * 100))
    usage = f"{compact_token_count(input_tokens)}/{compact_token_count(context_limit)}"
    rendered = Text()
    rendered.append(usage, style=TEXT_PRIMARY)
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append(f"{percent}%", style=TEXT_PRIMARY)
    return rendered


def token_telemetry_text(
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
    context_limit: int,
    tokens_per_second: float = 0.0,
) -> Text:
    """生成第二行遥测（最左段）：上下文占用 ⁕ 输入/输出/缓存+缓存率 ⁕ 速率。

    字段顺序固定为 上下文占用 ⁕ ↑/↓/† CH% ⁕ t/s，段间用 ⁕ 分隔、内容
    紧排不补固定宽度；行首直接开始（无竖线/分隔符），行尾由
    status_summary_text 自带 “⁕ ” 前置分隔符衔接模型/状态段。CTX 段为
    用量/总量 + 百分比（无进度条），输入/输出/缓存精简为 ↑/↓/† 三组，
    缓存率以 “CH0%” 显示在 ↑/↓/† 后：缓存率 = 缓存命中的输入 token
    （†）÷ 本次请求总输入 token（↑），CA 是 IN 的子集，因此缓存率
    不超过 100%。``tokens_per_second`` 是会话累计的统计平均速率（总输出
    token ÷ 总输出时长，字符估算）：输出停止或回合结束后平均值保留不归零，
    尚无任何输出记录时为 0，显示 ``-- t/s``。
    """

    input_tokens = max(0, int(input_tokens))
    output_tokens = max(0, int(output_tokens))
    cached_input_tokens = max(0, int(cached_input_tokens))
    try:
        tokens_per_second = max(0.0, float(tokens_per_second))
    except (TypeError, ValueError):
        tokens_per_second = 0.0
    context_limit = max(1, int(context_limit))
    # 缓存率 = 缓存命中输入 ÷ 本次总输入；无输入时视为 0%，
    # 异常数据（CA > IN）封顶 100%。
    if input_tokens > 0:
        cache_percent = min(100, round(cached_input_tokens * 100 / input_tokens))
    else:
        cache_percent = 0
    rendered = Text()
    # CTX 段：用量/总量 + 百分比，行首直接开始，后接 ⁕ 分隔符。
    rendered.append_text(context_usage_text(input_tokens, context_limit))
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append("⁕", style=BORDER_MUTED)
    # 输入/输出/缓存段：↑/↓/† + 值，/ 分隔，缓存占比 CH% 后随 1 空格。
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append(f"↑{compact_token_count(input_tokens)}", style=TEXT_PRIMARY)
    rendered.append("/", style=TEXT_MUTED)
    rendered.append(f"↓{compact_token_count(output_tokens)}", style=TEXT_PRIMARY)
    rendered.append("/", style=TEXT_MUTED)
    rendered.append(f"†{compact_token_count(cached_input_tokens)}", style=TEXT_PRIMARY)
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append(f"CH{cache_percent}%", style=TEXT_PRIMARY)
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append("⁕", style=BORDER_MUTED)
    # 实时生成速率段：-- t/s 或实际速率，段尾 1 空格衔接右段前置 ⁕。
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append(
        f"{tokens_per_second:.1f}" if tokens_per_second > 0 else "--",
        style=TEXT_PRIMARY,
    )
    rendered.append(" t/s", style=TEXT_MUTED)
    rendered.append(" ", style=TEXT_MUTED)
    return rendered


def pending_queue_text(pending_count: int) -> Text:
    """生成顶部右段状态卡片中的 FIFO 排队数量文本。"""

    pending_count = max(0, int(pending_count))
    rendered = Text()
    rendered.append("QUE ", style=TEXT_MUTED)
    rendered.append(str(pending_count), style=f"{TEXT_PRIMARY} bold")
    return rendered

def gradient_text(text: str) -> Text:
    """保留既有调用接口，以终端 ANSI 主强调色渲染品牌文字。"""

    return Text(text, style=f"{ACCENT_GREEN} bold")


# 底部轮播解密扫描特效：进度 0~EROSION_FRACTION 为乱码侵蚀旧文本，
# 之后把乱码从左到右逐步"吐出"为清晰的新文本。
EROSION_FRACTION = 0.45
# 解密扫描波前右侧的乱码区中，每字符以该概率闪现真实目标字符，
# 形成"解码中"的闪烁观感。
SHIMMER_CHANCE = 0.16
# 乱码字符集：随机符号 + 数字 + 大小写字母。
GARBLE_CHARS = (
    "#@%&*+=<>/\\?^$!~|0123456789"
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
)


def _is_wide_char(ch: str) -> bool:
    """判断字符是否为终端双宽（CJK 全角/宽字符）。"""

    return unicodedata.east_asian_width(ch) in ("W", "F")


def _text_styles(text: Text) -> list[object]:
    """返回每个 plain 字符位置的样式（span 覆盖优先，其余用文本基样式）。"""

    plain = text.plain
    styles: list[object] = [None] * len(plain)
    for span in text.spans:
        for index in range(max(0, span.start), min(len(plain), span.end)):
            styles[index] = span.style
    for index in range(len(plain)):
        if styles[index] is None:
            styles[index] = text.style
    return styles


def _garble_cells(
    original: str,
    style: object,
    rand: random.Random,
) -> list[tuple[str, object]]:
    """生成替换乱码字符；双宽字符用两个单宽乱码保持终端宽度。"""

    count = 2 if _is_wide_char(original) else 1
    return [(rand.choice(GARBLE_CHARS), style) for _ in range(count)]


def decrypt_frame(
    old_text: Text,
    new_text: Text,
    progress: float,
    *,
    rand_source: random.Random | None = None,
) -> Text:
    """生成底部轮播切换时的"解密扫描特效"的一帧。

    进度 ``0.0`` 完整显示旧文本、``1.0`` 完整显示新文本。前半段
    （0 ~ ``EROSION_FRACTION``）乱码波从左到右侵蚀旧文本；后半段扫描
    波从左到右把乱码逐步蜕变成清晰的新文本——波前左侧已解密、波前右侧
    仍是闪烁乱码（偶发闪现真实字符）。``rand_source`` 传入固定随机源时
    输出可复现，便于测试。
    """

    rand = rand_source if rand_source is not None else random
    progress = min(1.0, max(0.0, float(progress)))
    old_plain = old_text.plain
    new_plain = new_text.plain
    old_styles = _text_styles(old_text)
    new_styles = _text_styles(new_text)
    garble_style: object = TEXT_MUTED
    cells: list[tuple[str, object]] = []
    if progress < EROSION_FRACTION:
        # 侵蚀阶段：乱码波从左到右吃掉旧文本，波前左侧已乱码、右侧完好。
        front = 0
        if progress > 0 and old_plain:
            front = min(
                len(old_plain),
                int(len(old_plain) * progress / EROSION_FRACTION + 0.999),
            )
        for index, ch in enumerate(old_plain):
            if index < front:
                cells.extend(_garble_cells(ch, garble_style, rand))
            else:
                cells.append((ch, old_styles[index]))
    else:
        # 解密阶段：扫描波从左到右把乱码吐出为清晰新文本。
        reveal = (progress - EROSION_FRACTION) / (1.0 - EROSION_FRACTION)
        front = min(len(new_plain), int(len(new_plain) * reveal))
        for index, ch in enumerate(new_plain):
            if index < front:
                cells.append((ch, new_styles[index]))
            elif index == front:
                cells.extend(_garble_cells(ch, garble_style, rand))
            elif rand.random() < SHIMMER_CHANCE:
                cells.append((ch, new_styles[index]))
            else:
                cells.extend(_garble_cells(ch, garble_style, rand))
    rendered = Text()
    for ch, style in cells:
        if style is None:
            rendered.append(ch)
        else:
            rendered.append(ch, style=style)
    return rendered


def compact_hud_value(value: str, max_chars: int) -> str:
    """压缩 HUD 字段，避免长模型名把整行挤乱。

    终端按字符截断；超长时保留首尾可读片段，中间用省略号。
    """

    text = " ".join(str(value or "").split())
    if not text:
        return "-"
    limit = max(4, int(max_chars))
    if len(text) <= limit:
        return text
    if limit <= 4:
        return text[: limit - 1] + "…"
    head = max(1, (limit - 1) // 2)
    tail = max(1, limit - 1 - head)
    return f"{text[:head]}…{text[-tail:]}"




# 顶部两行字段段不使用固定宽度：各段内容紧排（段内自带前后 1 空格，
# 段间自然 2 空格），数字位数增长时后续字段自然顺移，不产生大段空白。
# 分隔符为 ⁕，行首与行尾不再有闭合竖线；PRJ/MDL 为无前缀的裸值段。
# 超长值（如模型名）由 compact_hud_value 截断，宽度由内容决定。


def context_summary_text(
    *,
    workspace: str,
) -> Text:
    """渲染第一行左段：项目绝对路径（灰色，行首直接开始）。

    直接显示完整路径不做 basename 截断，用灰色弱化视觉；超长路径由
    compact_hud_value 截断（保留首尾）。尾部 1 空格保持字段间距。
    """

    rendered = Text()
    rendered.append(compact_hud_value(workspace, 40), style=TEXT_MUTED)
    rendered.append(" ", style=TEXT_MUTED)
    return rendered


def status_summary_text(
    approval_mode: str,
    mcp_enabled_count: int,
    pending_count: int,
    *,
    model: str = "",
    reasoning_effort: str = "",
) -> Text:
    """生成第二行右段：模型、推理强度、审批模式、MCP 数量、排队数。

    行首自带 “⁕ ” 前置分隔符（衔接第二行左段 CTX 段的 t/s），MDL 为
    裸值段（无前缀标签），THK 保留标签且值区紧凑；MDL/THK 之间用
    ⁕ 分隔，APR/MCP/QUE 段间不使用竖线（段内自带空格，段间自然
    2 空格），内容紧排不补固定宽度，行尾不再有闭合竖线。
    """

    raw_approval = str(approval_mode or "").strip()
    approval = {
        "manual": "MAN",
        "人工确认": "MAN",
        "auto": "AUTO",
        "完全自动批准": "AUTO",
        "review": "REV",
        "模型审查": "REV",
    }.get(raw_approval.lower(), raw_approval.upper())
    rendered = Text()
    # 行首 ⁕ 前置分隔符：衔接第二行左段（t/s），随本组件恒显示。
    rendered.append("⁕", style=BORDER_MUTED)
    rendered.append(" ", style=TEXT_MUTED)
    # MDL 段：无标签裸值，直接跟随分隔符；超长模型名截断。
    rendered.append(compact_hud_value(model, 18), style=f"{TEXT_PRIMARY} bold")
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append("⁕", style=BORDER_MUTED)
    # THK 段：保留标签，内容紧排使 THK 与右侧 APR 段紧贴（段间无分隔符）。
    rendered.append(" THK ", style=TEXT_MUTED)
    rendered.append(
        compact_hud_value(reasoning_effort.upper(), 5), style=f"{TEXT_PRIMARY} bold"
    )
    rendered.append(" ", style=TEXT_MUTED)
    # APR 段：无竖线，紧贴 THK 段（段间自然 2 空格）。
    rendered.append(" APR ", style=TEXT_MUTED)
    rendered.append(compact_hud_value(approval, 8), style=f"{TEXT_PRIMARY} bold")
    rendered.append(" ", style=TEXT_MUTED)
    # MCP 段：段内自带前后空格，内容紧排。
    rendered.append(" MCP ", style=TEXT_MUTED)
    rendered.append(str(max(0, int(mcp_enabled_count))), style=f"{TEXT_PRIMARY} bold")
    rendered.append(" ", style=TEXT_MUTED)
    # QUE 段：段内自带前后空格，内容紧排。
    rendered.append(" QUE ", style=TEXT_MUTED)
    rendered.append(str(max(0, int(pending_count))), style=f"{TEXT_PRIMARY} bold")
    rendered.append(" ", style=TEXT_MUTED)
    return rendered
