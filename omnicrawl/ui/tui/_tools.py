"""工具调用显示。"""

from __future__ import annotations


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
    if tool_name in {"bash", "powershell"}:
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
    if tool_name not in {"bash", "powershell"}:
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
    if tool_name in {"bash", "powershell"}:
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
