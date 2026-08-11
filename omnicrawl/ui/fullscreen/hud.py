"""全屏工作台 HUD 的纯 Rich 文本格式化函数。"""

from __future__ import annotations

from rich.text import Text

from .theme import (
    ACCENT_GREEN,
    BORDER_MUTED,
    BORDER_SUBTLE,
    TEXT_MUTED,
    TEXT_PRIMARY,
)

# 索引加载动画的帧序列：与对话区状态指示器（⠧ 正在思考…）同款十帧
# Braille 旋转动画，配合 STATUS_SPINNER_INTERVAL_SECONDS 刷新。
SEARCH_INDEX_SPINNER_FRAMES = ("⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏")


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
    #status-summary 自带 “⁕ ” 前置分隔符衔接模型/状态段。CTX 段为
    用量/总量 + 百分比（无进度条），输入/输出/缓存精简为 ↑/↓/† 三组，
    缓存率以 “CH0%” 显示在 ↑/↓/† 后：缓存率 = 缓存命中的输入 token
    （†）÷ 本次请求总输入 token（↑），CA 是 IN 的子集，因此缓存率
    不超过 100%。``tokens_per_second`` 是 Agent 流式生成的实时速率
    （字符估算），仅在生成期间大于 0，空闲时显示 ``-- t/s``。
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


def search_index_status_text(status: object, animation_frame: int = 0) -> Text:
    """生成第一行行尾的后台索引状态：旋转动画 + “加载索引”。

    加载/构建期间显示与状态指示器（⠧ 正在思考…）同款的十帧 Braille
    旋转动画，文本固定为 “加载索引”；索引就绪或空闲时返回空文本
    （组件随之隐藏）；错误时显示简短提示。
    """

    file_state = str(getattr(status, "file_state", "disabled"))
    content_state = str(getattr(status, "content_state", "disabled"))

    if (
        file_state in {"loading", "building"}
        or content_state in {"loading", "building"}
    ):
        frame = SEARCH_INDEX_SPINNER_FRAMES[
            int(animation_frame) % len(SEARCH_INDEX_SPINNER_FRAMES)
        ]
        return Text(f"{frame} 加载索引", style=TEXT_MUTED)
    if file_state == "error" or content_state == "error":
        return Text("索引不可用", style=TEXT_MUTED)
    return Text()


def gradient_text(text: str) -> Text:
    """保留既有调用接口，以终端 ANSI 主强调色渲染品牌文字。"""

    return Text(text, style=f"{ACCENT_GREEN} bold")


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
    compact_hud_value 截断（保留首尾）。尾部 1 空格与行尾 #index-status
    组件衔接（该组件自带 “⁕ ” 前置分隔符，空闲隐藏时第一行只有路径）。
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
