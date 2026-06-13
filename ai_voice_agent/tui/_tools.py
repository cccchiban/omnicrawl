"""工具调用显示。"""

from __future__ import annotations

import json
import re
import shutil
import sys
import threading
from dataclasses import dataclass
from typing import Any

from ._colors import (
    ANSI_BLINK,
    ANSI_CLEAR_LINE,
    ANSI_RESET,
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


@dataclass
class ToolDisplayState:
    """记录一次工具执行块，供执行完成后把运行态标记更新为完成态。"""

    step: int
    tool_name: str
    line_count: int


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
) -> ToolDisplayState:
    """打印工具开始执行的结构化记录。"""

    indent = _dialog_continuation_prefix(AI_PREFIX)
    marker = f"{ANSI_BLINK}◌{ANSI_RESET}" if caps.ansi else "◌"
    line_width = shutil.get_terminal_size((100, 30)).columns
    # header 格式：◌ ╭─ 步骤 N · tool_name ─ ─ ─
    marker_prefix_width = _display_width("◌ ╭─ 步骤 ")
    tool_width = max(
        1,
        line_width
        - _display_width(indent)
        - marker_prefix_width
        - _display_width(str(max(1, step)))
        - _display_width(" · ")
        - 1,
    )
    visible_tool_name = _ellipsize_display_text(tool_name, tool_width)

    # 框线头部（含闪烁运行标记）
    header = (
        f"{indent}{marker} "
        f"{color_text('╭─', 'muted', caps)} "
        f"{color_text('步骤', 'muted', caps)} "
        f"{color_text(str(max(1, step)), 'primary', caps)}"
        f"{color_text(' · ', 'muted', caps)}"
        f"{color_text(visible_tool_name, 'accent', caps)}"
        f"{color_text(' ─' * 3, 'muted', caps)}"
    )
    detail_rows = _tool_call_detail_rows(tool_name, arguments, caps)
    prefix = "\n" if leading_blank or step > 1 else ""
    line_count = 1 + len(detail_rows)

    with lock:
        print(f"{prefix}{header}")
        for row in detail_rows:
            print(f"{indent}{row}")
        sys.stdout.flush()
    return ToolDisplayState(step=max(1, step), tool_name=tool_name, line_count=line_count)


def print_tool_result_record(
    ok: bool,
    output: str | None = None,
    *,
    tool_name: str = "",
    display_state: ToolDisplayState | None = None,
    caps: TerminalCapabilities = TerminalCapabilities(ansi=False),
    lock: threading.Lock = threading.Lock(),  # noqa: B008 — 模块级默认值，仅作回退
) -> None:
    """打印工具执行结果摘要。"""

    result_label = color_text("成功", "success", caps) if ok else color_text("失败", "error", caps)
    indent = _dialog_continuation_prefix(AI_PREFIX)
    suffix = _tool_result_suffix(tool_name, output or "", caps)
    output_rows = _tool_result_output_rows(tool_name, output or "", caps)

    with lock:
        if display_state is not None:
            _refresh_tool_call_header(display_state, ok, caps)
        result_marker = color_text("✓" if ok else "✗", "success" if ok else "error", caps)
        bracket_result = f"[{result_label}]" if caps.ansi else ("成功" if ok else "失败")
        if output_rows:
            # 有输出行：结果行用 │ 继续，输出行最后一行用 ╰─ 关闭
            print(
                f"{indent}{color_text('│ ', 'muted', caps)}{result_marker} {bracket_result}{suffix}",
            )
            for row in output_rows:
                print(f"{indent}{row}")
        else:
            # 无输出行：结果行本身就是最后一行，用 ╰─ 关闭
            print(
                f"{indent}{color_text('╰─ ', 'muted', caps)}{result_marker} {bracket_result}{suffix}",
            )
        sys.stdout.flush()


def _refresh_tool_call_header(
    display_state: ToolDisplayState,
    ok: bool,
    caps: TerminalCapabilities,
) -> None:
    """把正在运行的闪烁标记改成完成态。"""

    if not caps.ansi or display_state.line_count <= 0:
        return

    indent = _dialog_continuation_prefix(AI_PREFIX)
    # 完成态 marker：✓ 或 ✗
    marker = color_text("✓" if ok else "✗", "success" if ok else "error", caps)
    line_width = shutil.get_terminal_size((100, 30)).columns
    # 重新计算宽度——完成态 marker 是 ✓（宽1），与运行态 ◌（宽1）一致
    marker_prefix_width = _display_width("✓ ╭─ 步骤 ")
    tool_width = max(
        1,
        line_width
        - _display_width(indent)
        - marker_prefix_width
        - _display_width(str(display_state.step))
        - _display_width(" · ")
        - 1,
    )
    visible_tool_name = _ellipsize_display_text(display_state.tool_name, tool_width)
    label = (
        f"{indent}{marker} "
        f"{color_text('╭─', 'muted', caps)} "
        f"{color_text('步骤', 'muted', caps)} "
        f"{color_text(str(display_state.step), 'primary', caps)}"
        f"{color_text(' · ', 'muted', caps)}"
        f"{color_text(visible_tool_name, 'accent', caps)}"
        f"{color_text(' ─' * 3, 'muted', caps)}"
    )
    print(
        f"\033[{display_state.line_count}A"
        f"\r{ANSI_CLEAR_LINE}{label}"
        f"\033[{display_state.line_count}B"
        f"\r",
        end="",
    )


def _tool_call_detail_rows(
    tool_name: str,
    arguments: dict[str, Any],
    caps: TerminalCapabilities,
) -> list[str]:
    width = _tool_detail_width()
    if tool_name == "run_command":
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


def _tool_result_suffix(tool_name: str, output: str, caps: TerminalCapabilities) -> str:
    if tool_name != "run_command":
        return ""
    match = re.search(r"^退出码：(-?\d+)", output)
    if match is None:
        return ""
    return (
        f"{color_text(' · 退出码 ', 'muted', caps)}"
        f"{color_text(match.group(1), 'text', caps)}"
    )


def _tool_result_output_rows(tool_name: str, output: str, caps: TerminalCapabilities) -> list[str]:
    width = _tool_detail_width()
    if tool_name == "run_command":
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
    terminal_width = shutil.get_terminal_size((100, 30)).columns
    indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
    return max(20, terminal_width - indent_width - 4)
