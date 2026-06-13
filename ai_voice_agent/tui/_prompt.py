"""确认对话框。"""

from __future__ import annotations

import shutil
import threading

from ._colors import (
    ANSI_BOLD,
    ANSI_CLEAR_TO_LINE_END,
    ANSI_RESET,
    ANSI_UNDERLINE,
    ColorRole,
    color_text,
)
from ._capabilities import TerminalCapabilities
from ._display import _dialog_continuation_prefix, _display_width, _ellipsize_display_text

AI_PREFIX = "◆"


def prompt_yes_no(
    prompt: str,
    confirmed_label: str = "",
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> bool:
    """以默认 YES 的方式确认一次高风险操作。

    圆角框包裹 + 高亮选中项。支持上下/左右方向键切换，Enter 确认。
    """

    selected_yes = True

    with lock:
        _render_confirmation_card(prompt, caps)

    def _print_options() -> None:
        with lock:
            indent = _dialog_continuation_prefix(AI_PREFIX)
            line_width = shutil.get_terminal_size((100, 30)).columns
            content_width = max(20, line_width - _display_width(indent) - 4)

            # 分隔线
            sep = color_text("│", "muted", caps) + " " + color_text("─" * (content_width - 2), "muted", caps)
            print(f"{indent}{sep}")

            # 选项行
            if selected_yes:
                yes_style = lambda t: _highlight_selected(t, caps)
                no_style = lambda t: color_text(t, "muted", caps)
                pointer_yes = "❯ "
                pointer_no = "  "
            else:
                yes_style = lambda t: color_text(t, "muted", caps)
                no_style = lambda t: _highlight_selected(t, caps)
                pointer_yes = "  "
                pointer_no = "❯ "

            option_line = (
                f"{indent}{color_text('│', 'muted', caps)} "
                f"{pointer_yes}{yes_style('Yes')}    "
                f"{pointer_no}{no_style('No')}"
                f"{ANSI_CLEAR_TO_LINE_END}"
            )
            print(option_line)

            # 底部框线
            bottom = color_text("╰─", "muted", caps) + color_text("─" * (content_width - 2), "muted", caps)
            print(f"{indent}{bottom}")
            print()
            print(
                f"{indent}{color_text('↑↓ 选择 · Enter 确认 · N 取消', 'muted', caps)}",
                flush=True,
            )

    _print_options()

    # 计算收折行数：卡片行数 + 选项区行数(分隔+选项+底框+空行+提示)
    _card_lines = _count_card_lines(prompt)
    _option_lines = 5

    try:
        import msvcrt
    except ImportError:
        answer = input("确认？[Enter=YES / n=NO] ").strip().lower()
        return answer not in {"n", "no", "否", "false", "2"}

    while True:
        char = msvcrt.getwch()
        if char in {"\r", "\n"}:
            _collapse_and_label(
                confirmed_label, _card_lines + _option_lines,
                confirmed=selected_yes, caps=caps, lock=lock,
            )
            return selected_yes
        if char in {"1", "y", "Y"}:
            _collapse_and_label(
                confirmed_label, _card_lines + _option_lines,
                confirmed=True, caps=caps, lock=lock,
            )
            return True
        if char in {"n", "N", "2"}:
            _collapse_and_label(
                confirmed_label, _card_lines + _option_lines,
                confirmed=False, caps=caps, lock=lock,
            )
            return False
        if char == "\x03":
            _collapse_and_label(
                "", _card_lines + _option_lines,
                confirmed=False, caps=caps, lock=lock,
            )
            raise KeyboardInterrupt
        if char in {"\x00", "\xe0"}:
            key = msvcrt.getwch()
            next_selected_yes = _selection_from_key(key, selected_yes)
            if next_selected_yes != selected_yes:
                selected_yes = next_selected_yes
                _redraw_options(_print_options)
            continue


# ── 卡片渲染 ──────────────────────────────────────────────────


def _render_confirmation_card(prompt: str, caps: TerminalCapabilities) -> None:
    """将确认提示渲染为带圆角框线的结构化卡片。

    输入格式（由 format_tool_confirmation 生成）：
        Agent 想要执行 MCP 工具 local_project.workspace.run_command。
        参数：{"command": "python -c ..."}
        是否允许执行？

    渲染为：
        ╭─ ⚡ 确认执行 ─ ─ ─
        │ Agent 想要执行 MCP 工具 local_project.workspace.run_command。
        │ 参数：{"command": "python -c ..."}
        │
        │ 是否允许执行？
    """
    indent = _dialog_continuation_prefix(AI_PREFIX)
    line_width = shutil.get_terminal_size((100, 30)).columns
    content_width = max(20, line_width - _display_width(indent) - 4)

    # 顶部框线：╭─ ⚡ 确认执行 ─ ─ ─
    warning_icon = color_text("⚡", "warning", caps) if caps.ansi else "!"
    header_label = color_text("确认执行", "warning", caps) if caps.ansi else "确认执行"
    header_prefix_width = _display_width("╭─ ⚡ 确认执行 ")
    dash_count = max(3, content_width - header_prefix_width - 1)
    header = (
        f"{indent}"
        f"{color_text('╭─', 'muted', caps)} "
        f"{warning_icon} "
        f"{header_label} "
        f"{color_text('─' * dash_count, 'muted', caps)}"
    )
    print(f"\n{header}")

    # 内容行
    lines = prompt.split("\n")
    for line in lines:
        if line.strip() == "":
            # 空行只输出框线
            print(f"{indent}{color_text('│', 'muted', caps)}")
        elif line.startswith("命令：") or line.startswith("参数："):
            # 关键参数行用 accent 色高亮标签
            _render_detail_line(indent, line, caps, content_width)
        elif "是否允许执行" in line:
            # 核心问题用 primary + bold 突出
            question = color_text(line, "primary", caps)
            if caps.ansi:
                question = f"{ANSI_BOLD}{question}{ANSI_RESET}"
            print(f"{indent}{color_text('│', 'muted', caps)} {question}")
        else:
            # 普通描述行
            print(f"{indent}{color_text('│', 'muted', caps)} {color_text(line, 'text', caps)}")


def _render_detail_line(
    indent: str, line: str, caps: TerminalCapabilities, content_width: int,
) -> None:
    """渲染参数/命令详情行，标签着色 + 内容截断。"""
    # 分离标签和内容
    for sep in ("命令：", "参数：", "文件：", "替换 ", "写入 "):
        if line.startswith(sep):
            label_part = line[: len(sep)]
            content_part = line[len(sep) :]
            break
    else:
        label_part = ""
        content_part = line

    # 计算内容可用宽度
    label_width = _display_width(label_part)
    available = content_width - 3 - label_width  # 3 = "│ " + 1 margin
    truncated = _ellipsize_display_text(content_part, max(1, available))

    if label_part:
        rendered_label = color_text(label_part, "accent", caps)
    else:
        rendered_label = ""
    rendered_content = color_text(truncated, "text", caps)

    print(f"{indent}{color_text('│', 'muted', caps)} {rendered_label}{rendered_content}")


def _count_card_lines(prompt: str) -> int:
    """计算卡片渲染后的总行数（含顶部框线 + 前导空行）。"""
    # 前导空行 1 + 顶部框线 1 + 内容行数
    return 2 + prompt.count("\n") + 1


# ── 交互辅助 ──────────────────────────────────────────────────


def _highlight_selected(text: str, caps: TerminalCapabilities) -> str:
    """高亮选中项：PRIMARY + BOLD + UNDERLINE。"""
    if not caps.ansi:
        return text
    seq = get_color_sequence_static("primary", caps)
    return f"{seq}{ANSI_BOLD}{ANSI_UNDERLINE}{text}{ANSI_RESET}"


def get_color_sequence_static(role: str, caps: TerminalCapabilities) -> str:
    """获取颜色序列（供 _highlight_selected 使用）。"""
    from ._colors import get_color_sequence, ColorRole
    return get_color_sequence(role, caps)


def _selection_from_key(key: str, selected_yes: bool) -> bool:
    if key in {"H", "K"}:
        return True
    if key in {"P", "M"}:
        return False
    return selected_yes


def _redraw_options(print_fn) -> None:
    """回到选项区起始位置，用当前选中状态重绘。"""
    # 选项区共 5 行：分隔线 + 选项 + 底框 + 空行 + 提示
    print("\033[5A", end="")
    print_fn()


def _collapse_and_label(
    label: str,
    total_lines: int,
    confirmed: bool = True,
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> None:
    """选中后把完整确认块收折为单行缩略。"""
    if not caps.ansi:
        return

    symbol = "✓" if confirmed else "✗"
    role: ColorRole = "success" if confirmed else "error"
    with lock:
        print(f"\033[{total_lines}A\033[J", end="")
        if label:
            print(f"{color_text(f'  {symbol} {label}', role, caps)}", flush=True)
        else:
            # 收折为简洁的单行结果
            action = color_text("已允许" if confirmed else "已拒绝", role, caps)
            print(f"  {symbol} {action}", flush=True)
