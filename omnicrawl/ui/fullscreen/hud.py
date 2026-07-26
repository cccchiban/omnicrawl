"""全屏工作台 HUD 的纯 Rich 文本格式化函数。"""

from __future__ import annotations

from pathlib import PurePosixPath, PureWindowsPath

from rich.text import Text

from .theme import (
    ACCENT_GREEN,
    BORDER_SUBTLE,
    TEXT_MUTED,
    TEXT_PRIMARY,
)


def compact_token_count(value: int) -> str:
    """使用 K/M 缩写压缩 Token 数，同时保留小数量的精确值。"""

    value = max(0, int(value))
    if value < 1_000:
        return str(value)
    if value < 1_000_000:
        return f"{value / 1_000:.1f}K".replace(".0K", "K")
    return f"{value / 1_000_000:.1f}M".replace(".0M", "M")


def token_telemetry_text(
    input_tokens: int,
    output_tokens: int,
    cached_input_tokens: int,
    context_limit: int,
) -> Text:
    """生成 Token 统计与上下文占用进度。"""

    context_limit = max(1, int(context_limit))
    input_tokens = max(0, int(input_tokens))
    output_tokens = max(0, int(output_tokens))
    cached_input_tokens = max(0, int(cached_input_tokens))
    ratio = input_tokens / context_limit
    percent = min(999, round(ratio * 100))
    filled = min(12, max(0, round(min(1.0, ratio) * 12)))
    rendered = Text()
    rendered.append("IN ", style=TEXT_MUTED)
    rendered.append(compact_token_count(input_tokens), style=TEXT_PRIMARY)
    rendered.append("  OUT ", style=TEXT_MUTED)
    rendered.append(compact_token_count(output_tokens), style=TEXT_PRIMARY)
    rendered.append("  CA ", style=TEXT_MUTED)
    rendered.append(compact_token_count(cached_input_tokens), style=TEXT_PRIMARY)
    rendered.append("  CTX ", style=TEXT_MUTED)
    rendered.append(
        f"{compact_token_count(input_tokens)}/{compact_token_count(context_limit)} ",
        style=TEXT_PRIMARY,
    )
    rendered.append("█" * filled, style=TEXT_PRIMARY)
    rendered.append("░" * (12 - filled), style=TEXT_PRIMARY)
    rendered.append(f" {percent}%", style=TEXT_PRIMARY)
    return rendered


def pending_queue_text(pending_count: int) -> Text:
    """生成与 Token 遥测字段一致的排队数量文本。"""

    pending_count = max(0, int(pending_count))
    rendered = Text()
    rendered.append("排队 ", style=TEXT_MUTED)
    rendered.append(str(pending_count), style=f"{TEXT_PRIMARY} bold")
    return rendered


def version_status_text(current_version: str, latest_version: str | None = None) -> Text:
    """生成右上角版本号；存在更新时同时显示最新版本。"""

    current = str(current_version or "unknown").strip()
    rendered = Text(f"v{current}", style=f"{TEXT_PRIMARY} bold")
    if latest_version:
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


def context_summary_text(
    *,
    workspace: str,
    model: str,
    reasoning_effort: str,
    approval_mode: str,
) -> Text:
    """用短键值字段渲染项目、模型、推理强度和审批模式。

    字段集固定为 PRJ/MDL/THK/APR（方案 1A），不做额外信息扩展。
    长值在此截断，组件层再用 ellipsis 兜底窄屏。
    """

    path_type = (
        PureWindowsPath
        if "\\" in workspace or (len(workspace) >= 2 and workspace[1] == ":")
        else PurePosixPath
    )
    workspace_name = path_type(workspace).name or workspace
    raw_approval = approval_mode.strip()
    approval = {
        "manual": "MAN",
        "人工确认": "MAN",
        "auto": "AUTO",
        "完全自动批准": "AUTO",
        "review": "REV",
        "模型审查": "REV",
    }.get(raw_approval.lower(), raw_approval.upper())
    rendered = Text()
    fields = (
        ("PRJ", compact_hud_value(workspace_name, 24)),
        ("MDL", compact_hud_value(model, 28)),
        ("THK", compact_hud_value(reasoning_effort.upper(), 8)),
        ("APR", compact_hud_value(approval, 6)),
    )
    for index, (label, value) in enumerate(fields):
        if index:
            rendered.append("  ·  ", style=BORDER_SUBTLE)
        rendered.append(f"{label} ", style=TEXT_MUTED)
        rendered.append(value, style=f"{TEXT_PRIMARY} bold")
    return rendered
