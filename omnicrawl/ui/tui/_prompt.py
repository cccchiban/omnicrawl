"""确认对话框。"""

from __future__ import annotations


import shutil
import threading

from ._colors import (
    ColorRole,
    color_text,
)
from ._capabilities import TerminalCapabilities
from ._display import (
    _dialog_continuation_prefix,
    _display_width,
    _normalize_terminal_text,
    _split_display_rows,
)

AI_PREFIX = "◆"


def prompt_yes_no(
    prompt: str,
    confirmed_label: str = "",
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> bool:
    """以可追加的确认记录请求高风险操作。

    不再回跳折叠或原地重绘选项：确认框可能跨越终端滚动边界或在等待时发生
    resize。追加明确的选择和结果比依赖相对行数更稳定，也保留了完整审计历史。
    """

    with lock:
        _render_confirmation_card(prompt, caps)

    try:
        import msvcrt
    except ImportError:
        answer = input("确认？[Enter=YES / n=NO] ").strip().lower()
        confirmed = answer not in {"n", "no", "否", "false", "2"}
    else:
        confirmed = _read_confirmation_choice(msvcrt)

    with lock:
        choice = "允许" if confirmed else "拒绝"
        role: ColorRole = "success" if confirmed else "error"
        print(f"  {color_text(f'选择：{choice}', role, caps)}")
        if confirmed_label:
            print(f"  {color_text(confirmed_label, role, caps)}", flush=True)
    return confirmed


def _read_confirmation_choice(msvcrt) -> bool:
    """读取确认按键；使用显式按键而非动态菜单，避免光标重绘竞争。"""

    while True:
        char = msvcrt.getwch()
        if char in {"\r", "\n", "1", "y", "Y"}:
            return True
        if char in {"n", "N", "2"}:
            return False
        if char == "\x03":
            raise KeyboardInterrupt
        # 追加式确认卡不再重绘选择状态；方向键只会被消费，绝不会让一次
        # 误触直接执行工具，仍需 Enter/Y/1 或 N/2 做出明确决定。
        if char in {"\x00", "\xe0"}:
            msvcrt.getwch()


# ── 卡片渲染 ──────────────────────────────────────────────────


def _confirmation_content_width() -> int:
    terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    indent_width = _display_width(_dialog_continuation_prefix(AI_PREFIX))
    return max(1, terminal_width - indent_width - _display_width("│ "))


def _render_confirmation_text_rows(text: str, content_width: int) -> list[str]:
    rows: list[str] = []
    for raw_line in _normalize_terminal_text(text).split("\n"):
        rows.extend(_split_display_rows(raw_line, content_width))
    return rows or [""]


def _render_confirmation_card(prompt: str, caps: TerminalCapabilities) -> None:
    """在实际可用宽度内绘制稳定、可回读的确认卡片。"""

    indent = _dialog_continuation_prefix(AI_PREFIX)
    content_width = _confirmation_content_width()
    header_text = "⚠ 确认执行"
    # 总宽度包含 indent、╭─ 和一个分隔空格；小窗口省略装饰横线。
    terminal_width = max(1, shutil.get_terminal_size((100, 30)).columns)
    header_available = max(0, terminal_width - _display_width(indent) - _display_width("╭─ ") - _display_width(header_text))
    header_suffix = "─" * header_available
    print(
        f"\n{indent}{color_text('╭─', 'muted', caps)} "
        f"{color_text(header_text, 'warning', caps)}"
        f"{color_text(header_suffix, 'muted', caps)}"
    )

    for raw_line in _normalize_terminal_text(prompt).split("\n"):
        if not raw_line:
            print(f"{indent}{color_text('│', 'muted', caps)}")
            continue

        if raw_line.startswith(("命令：", "参数：", "文件：", "替换 ", "写入 ")):
            label, value = _split_confirmation_detail(raw_line)
            first_width = max(1, content_width - _display_width(label))
            value_rows = _split_display_rows(value, first_width)
            for index, row in enumerate(value_rows):
                if index == 0:
                    rendered = color_text(label, "accent", caps) + color_text(row, "text", caps)
                else:
                    rendered = color_text(row, "text", caps)
                print(f"{indent}{color_text('│', 'muted', caps)} {rendered}")
            continue

        role: ColorRole = "primary" if "是否允许执行" in raw_line else "text"
        for row in _render_confirmation_text_rows(raw_line, content_width):
            print(f"{indent}{color_text('│', 'muted', caps)} {color_text(row, role, caps)}")

    print(f"{indent}{color_text('╰─', 'muted', caps)}")
    hint = "Enter/Y 允许 · N 拒绝"
    hint_width = max(1, max(1, shutil.get_terminal_size((100, 30)).columns) - _display_width(indent))
    for row in _split_display_rows(hint, hint_width):
        print(f"{indent}{color_text(row, 'muted', caps)}")


def _split_confirmation_detail(line: str) -> tuple[str, str]:
    for separator in ("命令：", "参数：", "文件：", "替换 ", "写入 "):
        if line.startswith(separator):
            return line[: len(separator)], line[len(separator) :]
    return "", line
