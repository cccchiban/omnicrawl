"""全屏工作台 HUD 的纯 Rich 文本格式化函数。"""

from __future__ import annotations

from pathlib import PurePosixPath, PureWindowsPath

from rich.text import Text

from .theme import (
    ACCENT_GREEN,
    BORDER_MUTED,
    BORDER_SUBTLE,
    TEXT_MUTED,
    TEXT_PRIMARY,
)

# “正在加载项目搜索索引”光波的周期帧数。8 帧比原 4 帧更细，配合
# 0.25s 刷新间隔（见 OmniCrawlApp.SEARCH_INDEX_ANIMATION_INTERVAL_SECONDS）
# 让光效更快且更平滑。
SEARCH_INDEX_WAVE_FRAMES = 8


def compact_token_count(value: int) -> str:
    """使用 K/M 缩写压缩 Token 数，同时保留小数量的精确值。"""

    value = max(0, int(value))
    if value < 1_000:
        return str(value)
    if value < 1_000_000:
        return f"{value / 1_000:.1f}K".replace(".0K", "K")
    return f"{value / 1_000_000:.1f}M".replace(".0M", "M")


def context_usage_text(input_tokens: int, context_limit: int) -> Text:
    """生成 CTX 占用文本：百分比 + 进度条 + 用量/总量。

    整段固定 29 cell（配合段前后各 1 空格共 31 cell）：进度条格数吸收
    百分比/用量文本的宽度变化，使 `0/1M` 与段尾竖线始终保持 1 空格
    间隔，段尾竖线稳定对齐第一行 MDL 段尾竖线；超限时百分比可超过
    100（如 150%），进度条封顶全满。
    """

    context_limit = max(1, int(context_limit))
    input_tokens = max(0, int(input_tokens))
    ratio = input_tokens / context_limit
    percent = min(999, round(ratio * 100))
    usage = f"{compact_token_count(input_tokens)}/{compact_token_count(context_limit)}"
    # 29 = "CTX "(4) + 百分比 + 空格(1) + 进度条 + 空格(1) + 用量；
    # 进度条 = 29 - 4 - 百分比宽 - 用量宽 - 2 个分隔空格。
    cells = max(4, 29 - 4 - len(f"{percent}%") - len(usage) - 2)
    filled = min(cells, max(0, round(min(1.0, ratio) * cells)))
    rendered = Text()
    rendered.append("CTX ", style=TEXT_MUTED)
    rendered.append(f"{percent}%", style=TEXT_PRIMARY)
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append("█" * filled, style=TEXT_PRIMARY)
    rendered.append("░" * (cells - filled), style=TEXT_PRIMARY)
    rendered.append(" ", style=TEXT_MUTED)
    rendered.append(usage, style=TEXT_PRIMARY)
    return rendered


def token_telemetry_text(
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
    context_limit: int,
    tokens_per_second: float = 0.0,
) -> Text:
    """生成第二行遥测：项目名居中 │ CTX 占用 │ IN/OUT/CA │ tok/s。

    字段顺序固定为 项目名 │ CTX │ IN │ OUT │ CA │ tok/s，段内自带
    前后空格、段间用纯灰色竖线 │ 分隔；CTX 段固定 31 cell（内容恒 29，
    进度条吸收宽度）使段尾竖线与第一行 MDL 段尾竖线对齐，并配合
    IN/OUT 固定宽使 OUT 段尾竖线与第一行 THK 段尾竖线对齐。
    ``tokens_per_second`` 是 Agent 流式生成的实时速率（字符估算），
    仅在生成期间大于 0，空闲时显示 ``--``。MCP 数量在第一行；索引
    状态为行尾独立组件（#index-status）。
    """

    input_tokens = max(0, int(input_tokens))
    output_tokens = max(0, int(output_tokens))
    cached_input_tokens = max(0, int(cached_input_tokens))
    try:
        tokens_per_second = max(0.0, float(tokens_per_second))
    except (TypeError, ValueError):
        tokens_per_second = 0.0
    rendered = Text()
    # 项目名固定字段居中显示于 16 cell 段内，段尾竖线（CTX 前）与
    # 第一行 MDL 段前竖线（cell 17）对齐；不随工作目录变化。
    rendered.append("│", style=BORDER_MUTED)
    ws = Text(HUD_PROJECT_NAME, style=f"{TEXT_PRIMARY} bold")
    ws_pad = max(0, HUD_WORKSPACE_WIDTH - ws.cell_len)
    ws.pad_left(ws_pad // 2)
    ws.pad_right(ws_pad - ws_pad // 2)
    rendered.append_text(ws)
    rendered.append("│", style=BORDER_MUTED)
    # CTX 段：固定 31 cell（前 1 + 内容 29 + 后 1）。内容内进度条格数
    # 自动吸收宽度，0/1M 与段尾竖线仅 1 空格间隔；段尾竖线（=IN 前）
    # 与第一行 MDL 段尾竖线（cell 49）对齐，与 IN6/OUT7 共同使 OUT
    # 段尾竖线与第一行 THK 段尾竖线（cell 64）对齐。pad_right 仅作
    # 异常兜底（内容恒 29 cell，正常为 0）。
    ctx = Text()
    ctx.append(" ", style=TEXT_MUTED)
    ctx.append_text(context_usage_text(input_tokens, context_limit))
    ctx.append(" ", style=TEXT_MUTED)
    ctx.pad_right(max(0, HUD_CTX_WIDTH - ctx.cell_len))
    rendered.append_text(ctx)
    rendered.append("│", style=BORDER_MUTED)
    # IN/OUT/CA 段：固定宽度（IN 6/OUT 7/CA 10），CA 段尾竖线
    # 与第一行 APR 段尾竖线（cell 75）对齐；值变长时自然溢出。
    fields = (
        ("IN", compact_token_count(input_tokens), HUD_IN_WIDTH),
        ("OUT", compact_token_count(output_tokens), HUD_OUT_WIDTH),
        ("CA", compact_token_count(cached_input_tokens), HUD_CA_WIDTH),
    )
    for index, (label, value, width) in enumerate(fields):
        if index:
            rendered.append("│", style=BORDER_MUTED)
        seg = Text()
        seg.append(" ", style=TEXT_MUTED)
        seg.append(f"{label} ", style=TEXT_MUTED)
        seg.append(value, style=TEXT_PRIMARY)
        seg.append(" ", style=TEXT_MUTED)
        seg.pad_right(max(0, width - seg.cell_len))
        rendered.append_text(seg)
    # 实时生成速率段：固定 15 cell，段尾竖线与第一行 QUE 段尾竖线
    # （cell 91）对齐；生成中显示估算 tok/s，空闲时固定显示 --。
    rendered.append("│", style=BORDER_MUTED)
    tok = Text()
    tok.append(" ", style=TEXT_MUTED)
    tok.append("tok/s ", style=TEXT_MUTED)
    tok.append(
        f"{tokens_per_second:.1f}" if tokens_per_second > 0 else "--",
        style=TEXT_PRIMARY,
    )
    tok.append(" ", style=TEXT_MUTED)
    tok.pad_right(max(0, HUD_TOK_WIDTH - tok.cell_len))
    rendered.append_text(tok)
    # 行尾索引状态前的分隔竖线（索引状态为独立组件 #index-status）。
    rendered.append("│", style=BORDER_MUTED)
    return rendered


def pending_queue_text(pending_count: int) -> Text:
    """生成顶部右段状态卡片中的 FIFO 排队数量文本。"""

    pending_count = max(0, int(pending_count))
    rendered = Text()
    rendered.append("QUE ", style=TEXT_MUTED)
    rendered.append(str(pending_count), style=f"{TEXT_PRIMARY} bold")
    return rendered


def search_index_status_text(status: object, animation_frame: int = 0) -> Text:
    """生成版本号下方的后台索引进度；加载阶段显示八帧颜色波浪。"""

    file_state = str(getattr(status, "file_state", "disabled"))
    content_state = str(getattr(status, "content_state", "disabled"))
    file_processed = max(0, int(getattr(status, "file_processed", 0) or 0))
    content_processed = max(0, int(getattr(status, "content_processed", 0) or 0))
    content_total = max(0, int(getattr(status, "content_total", 0) or 0))

    if content_state == "building":
        if content_total:
            percent = min(100, round(content_processed * 100 / content_total))
            return Text(f"正在建立项目内容索引 {percent}%", style=TEXT_MUTED)
        suffix = f" {file_processed}项" if file_processed else ""
        return Text(f"正在扫描项目内容索引{suffix}", style=TEXT_MUTED)
    if file_state == "building":
        suffix = f" {file_processed}项" if file_processed else ""
        return Text(f"正在建立项目文件索引{suffix}", style=TEXT_MUTED)
    if file_state == "loading" or content_state == "loading":
        label = "正在加载项目搜索索引"
        # 八帧滑动光波：锋头加粗、锋身常亮、其余减弱为 dim。每帧向前推进
        # 1 字符、整圈 8 字符，比原 4 帧/0.5s 的动画更流畅且速度翻倍。
        frame = int(animation_frame) % SEARCH_INDEX_WAVE_FRAMES
        rendered = Text()
        for index, character in enumerate(label):
            position = (index - frame) % SEARCH_INDEX_WAVE_FRAMES
            if position == 2:
                style = f"{TEXT_PRIMARY} bold"
            elif position == 3:
                style = TEXT_PRIMARY
            else:
                style = TEXT_MUTED
            rendered.append(character, style=style)
        return rendered
    if file_state == "error" or content_state == "error":
        return Text("项目搜索索引不可用", style=TEXT_MUTED)
    return Text()


def version_status_text(
    current_version: str,
    latest_version: str | None = None,
    *,
    checking: bool = False,
    animation_frame: int = 0,
) -> Text:
    """生成右上角版本号、升级提示或检查更新动画。"""

    current = str(current_version or "unknown").strip()
    rendered = Text(f"v{current}", style=f"{TEXT_PRIMARY} bold")
    if checking:
        active_index = animation_frame % 4
        rendered.append("  ", style=TEXT_MUTED)
        for index in range(4):
            rendered.append(
                "●",
                style=TEXT_MUTED if index == active_index else TEXT_PRIMARY,
            )
    elif latest_version:
        rendered.append("  ↑ ", style=ACCENT_GREEN)
        rendered.append(f"v{latest_version}", style=f"{ACCENT_GREEN} bold")
    return rendered


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


def _workspace_name(workspace: str) -> str:
    """从工作目录路径提取短名（basename），兼容 Windows/Unix 路径。"""

    path_type = (
        PureWindowsPath
        if "\\" in workspace or (len(workspace) >= 2 and workspace[1] == ":")
        else PurePosixPath
    )
    return path_type(workspace).name or workspace


# 顶部两行各字段段的固定显示宽度（cell，含段内前后空格与标签），
# 用于让段间竖线在固定列对齐。分隔符为纯竖线 │，空格属于段内容。
HUD_PRJ_WIDTH = 16  # " PRJ " + 值区 10（示例 AI课堂任务 10 cell）
HUD_MDL_WIDTH = 31  # " MDL " + 值区 26
HUD_THK_WIDTH = 14  # " THK " + 值区 9
HUD_APR_WIDTH = 10  # " APR " + 值区，左 1 空格，剩余补右
HUD_MCP_WIDTH = 7   # " MCP " + 值区 1 + 后 1
HUD_QUE_WIDTH = 7   # " QUE " + 值区 1 + 后 1
# 第二行各段固定宽度，使竖线与第一行逐点对齐（cell）：
#   第一行: │(0) PRJ16 │(17) MDL31 │(49) THK14 │(64) APR10 │(75) MCP7 │(83) QUE7 │(91)
#   第二行: │(0) 项目名16 │(17) CTX31 │(49) IN6 │(56) OUT7 │(64) CA10 │(75) tok/s15 │(91)
# 对齐点：CTX前│==MDL前│(17)、OUT尾│==THK尾│(64)、CA尾│==APR尾│(75)、
#          tok/s尾│==QUE尾│(91)；IN/OUT 值变长时段宽溢出，竖线随之右移。
# 第二行首段项目名的固定显示宽度（居中），内容为固定项目名常量。
HUD_WORKSPACE_WIDTH = 16
# 第二行 CTX 段固定宽度（CTX+IN6+OUT7 = 44 → OUT 尾竖线 = 64）。
HUD_CTX_WIDTH = 31
HUD_IN_WIDTH = 6    # " IN 0 "（值 1 位）
HUD_OUT_WIDTH = 7   # " OUT 0 "（值 1 位）
HUD_CA_WIDTH = 10   # CA 段尾竖线对齐第一行 APR 段尾（cell 75）
HUD_TOK_WIDTH = 15  # tok/s 段尾竖线对齐第一行 QUE 段尾（cell 91）
# 第二行尾段索引状态的固定显示宽度（居中）。
HUD_INDEX_WIDTH = 26
# 第二行首段显示的固定项目名（不随工作目录变化）。
HUD_PROJECT_NAME = "omnicrawl"


def context_summary_text(
    *,
    workspace: str,
    model: str,
    reasoning_effort: str,
) -> Text:
    """用短键值字段渲染第一行左段：项目、模型、推理强度。

    段规则：行首竖线后直接接字段段，段内自带前后空格，段间用纯灰色
    竖线 │ 分隔；PRJ/MDL/THK 均为固定段宽左对齐补空格，保证后续竖线
    列位置稳定（THK 段尾竖线位于 cell 64，与第二行 OUT 段尾对齐）。
    审批模式与排队数在右段（见 status_summary_text）。
    """

    workspace_name = _workspace_name(workspace)
    rendered = Text()
    # PRJ 段：固定 16 cell（前 1 + 标签 4 + 值区 10 + 后 1），左对齐补空格。
    rendered.append("│", style=BORDER_MUTED)
    prj = Text()
    prj.append(" PRJ ", style=TEXT_MUTED)
    prj.append(compact_hud_value(workspace_name, 10), style=f"{TEXT_PRIMARY} bold")
    prj.pad_right(max(0, HUD_PRJ_WIDTH - prj.cell_len))
    rendered.append_text(prj)
    rendered.append("│", style=BORDER_MUTED)
    # MDL 段：固定 31 cell（前 1 + 标签 4 + 值区 26），左对齐补空格。
    mdl = Text()
    mdl.append(" MDL ", style=TEXT_MUTED)
    mdl.append(compact_hud_value(model, 26), style=f"{TEXT_PRIMARY} bold")
    mdl.pad_right(max(0, HUD_MDL_WIDTH - mdl.cell_len))
    rendered.append_text(mdl)
    rendered.append("│", style=BORDER_MUTED)
    # THK 段：固定 14 cell（前 1 + 标签 4 + 值区 9），段尾竖线在 cell 64。
    thk = Text()
    thk.append(" THK ", style=TEXT_MUTED)
    thk.append(
        compact_hud_value(reasoning_effort.upper(), 9), style=f"{TEXT_PRIMARY} bold"
    )
    thk.pad_right(max(0, HUD_THK_WIDTH - thk.cell_len))
    rendered.append_text(thk)
    return rendered


def status_summary_text(
    approval_mode: str,
    mcp_enabled_count: int,
    pending_count: int,
) -> Text:
    """生成右段状态字段：审批模式 │ MCP 数量 │ 排队数。

    版本号已移至第一行行尾独立组件（version_status_text 的产物，升级
    提示与检查动画只影响行尾，不牵动本段）。
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
    # APR 段：固定 10 cell（前 1 + 标签 4 + 值区，剩余补后），与左侧
    # 竖线仅 1 空格间隔；段尾竖线仍对齐第二行 CA 段尾（cell 75）。
    rendered.append("│", style=BORDER_MUTED)
    apr = Text()
    apr.append(" APR ", style=TEXT_MUTED)
    apr.append(compact_hud_value(approval, 6), style=f"{TEXT_PRIMARY} bold")
    apr.pad_right(max(0, HUD_APR_WIDTH - apr.cell_len))
    rendered.append_text(apr)
    rendered.append("│", style=BORDER_MUTED)
    # MCP 段：固定 7 cell（前 1 后 1），左对齐补空格。
    mcp = Text()
    mcp.append(" MCP ", style=TEXT_MUTED)
    mcp.append(str(max(0, int(mcp_enabled_count))), style=f"{TEXT_PRIMARY} bold")
    mcp.pad_right(max(0, HUD_MCP_WIDTH - mcp.cell_len))
    rendered.append_text(mcp)
    rendered.append("│", style=BORDER_MUTED)
    # QUE 段：固定 7 cell（前 1 后 1），左对齐补空格。
    que = Text()
    que.append(" QUE ", style=TEXT_MUTED)
    que.append(str(max(0, int(pending_count))), style=f"{TEXT_PRIMARY} bold")
    que.pad_right(max(0, HUD_QUE_WIDTH - que.cell_len))
    rendered.append_text(que)
    # 版本号前的分隔竖线（版本号由独立组件 #version-status 呈现）。
    rendered.append("│", style=BORDER_MUTED)
    return rendered
