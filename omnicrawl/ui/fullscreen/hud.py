"""全屏工作台 HUD 的纯 Rich 文本格式化函数。"""

from __future__ import annotations

from pathlib import PurePosixPath, PureWindowsPath

from rich.text import Text


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
    bar_color = "#00e5c3" if ratio < 0.6 else "#f4b860" if ratio < 0.85 else "#ff5470"

    rendered = Text()
    rendered.append("IN ", style="#66757b")
    rendered.append(compact_token_count(input_tokens), style="#39a7ff bold")
    rendered.append("  OUT ", style="#66757b")
    rendered.append(compact_token_count(output_tokens), style="#a36bff bold")
    rendered.append("  CA ", style="#66757b")
    rendered.append(compact_token_count(cached_input_tokens), style="#00e5c3 bold")
    rendered.append("  CTX ", style="#66757b")
    rendered.append(
        f"{compact_token_count(input_tokens)}/{compact_token_count(context_limit)} ",
        style="#d9e4e8",
    )
    rendered.append("█" * filled, style=bar_color)
    rendered.append("░" * (12 - filled), style="#23333a")
    rendered.append(f" {percent}%", style=f"{bar_color} bold")
    return rendered


def gradient_text(text: str) -> Text:
    """用逐字符真彩色插值生成青绿、电蓝到紫色的 HUD 渐变。"""

    stops = ((0, 229, 195), (57, 167, 255), (163, 107, 255))
    rendered = Text()
    denominator = max(1, len(text) - 1)
    for index, char in enumerate(text):
        position = index / denominator
        segment = min(1, int(position * 2))
        local = position * 2 - segment
        start = stops[segment]
        end = stops[segment + 1]
        red, green, blue = (
            round(start[channel] + (end[channel] - start[channel]) * local)
            for channel in range(3)
        )
        rendered.append(char, style=f"rgb({red},{green},{blue})")
    return rendered


def context_summary_text(
    *,
    workspace: str,
    model: str,
    reasoning_effort: str,
    approval_mode: str,
) -> Text:
    """用短键值字段渲染项目、模型、推理强度和审批模式。"""

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
    approval_color = {
        "MAN": "#f4b860",
        "AUTO": "#00e5c3",
        "REV": "#a36bff",
    }.get(approval, "#d9e4e8")

    rendered = Text()
    fields = (
        ("PRJ", workspace_name, "#00e5c3"),
        ("MDL", model, "#39a7ff"),
        ("THK", reasoning_effort.upper(), "#a36bff"),
        ("APR", approval, approval_color),
    )
    for index, (label, value, color) in enumerate(fields):
        if index:
            rendered.append("  ·  ", style="#31434b")
        rendered.append(f"{label} ", style="#59676d")
        rendered.append(value, style=f"{color} bold")
    return rendered
