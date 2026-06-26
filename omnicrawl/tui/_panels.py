"""启动面板。"""

from __future__ import annotations

import re
import shutil
import sys
import threading

from ._colors import color_text
from ._capabilities import TerminalCapabilities
from ._display import _display_width, _take_display_width


def print_startup_panel(
    title: str,
    lines: list[str],
    *,
    caps: TerminalCapabilities,
    lock: threading.Lock,
) -> None:
    """打印现代无框卡片风格的启动面板。

    左侧色条 + 分组展示 + 状态徽章 + 底部帮助行。
    """

    terminal_width = shutil.get_terminal_size((100, 30)).columns
    max_width = max(40, terminal_width - 4)
    content_width = min(max_width, max(_display_width(title) + 4, 52))

    groups = _parse_panel_groups(lines)

    with lock:
        # 标题行：左侧色条 + 标题
        title_text = _take_display_width(title, content_width)
        print(f"  {color_text('█', 'primary', caps)} {color_text(title_text, 'heading', caps)}")
        print(f"  {color_text('─' * (content_width - 2), 'muted', caps)}")

        for group_name, group_lines in groups:
            if group_name:
                print(f"  {color_text(group_name, 'secondary', caps)}")
            for line in group_lines:
                rendered = _render_panel_line(line, caps, content_width)
                print(f"  {rendered}")

        # 底部帮助行
        help_text = "输入问题开始对话 · /help 查看命令"
        print(f"  {color_text('─' * 2, 'muted', caps)} "
              f"{color_text(help_text, 'muted', caps)} "
              f"{color_text('─' * max(0, content_width - _display_width(help_text) - 6), 'muted', caps)}")

        sys.stdout.flush()


def _parse_panel_groups(lines: list[str]) -> list[tuple[str, list[str]]]:
    """将面板行按分组解析。"""

    groups: list[tuple[str, list[str]]] = []
    current_group = ""
    current_lines: list[str] = []

    for line in lines:
        if not line.strip():
            continue

        colon_idx = line.find(":")
        if colon_idx > 0:
            key = line[:colon_idx].strip().lower()
            group_name = _key_to_group(key)
            if group_name != current_group:
                if current_lines:
                    groups.append((current_group, current_lines))
                current_group = group_name
                current_lines = [line]
            else:
                current_lines.append(line)
        else:
            current_lines.append(line)

    if current_lines:
        groups.append((current_group, current_lines))

    return groups


def _key_to_group(key: str) -> str:
    """将配置键映射到分组名。"""

    mapping = {
        "thinking": "模型",
        "approval": "审批",
        "workspace": "工作区",
        "voice": "语音",
        "temp": "临时区",
    }
    return mapping.get(key, "")


def _render_panel_line(line: str, caps: TerminalCapabilities, content_width: int) -> str:
    """渲染单行配置项：key: value 格式。"""

    colon_idx = line.find(":")
    if colon_idx < 0:
        return color_text(line, "text", caps)

    key = line[:colon_idx].strip()
    value = line[colon_idx + 1:].strip()

    value_rendered = _render_status_value(value, caps)

    return (
        f"{color_text('▸', 'primary', caps)} "
        f"{color_text(key, 'text', caps)}  "
        f"{value_rendered}"
    )


def _render_status_value(value: str, caps: TerminalCapabilities) -> str:
    """渲染带状态徽章的值。

    对 "已启用"/"开启"/"已禁用"/"关闭" 等关键词逐个生成色块徽章，
    其余文字保持正常色。支持同一行中出现多个关键词。
    """

    # 逐个替换所有状态关键词为徽章
    result = ""
    pattern = re.compile(r"(已启用|开启|已禁用|关闭)")
    last_end = 0

    for match in pattern.finditer(value):
        # 关键词前的普通文字
        before = value[last_end:match.start()]
        if before:
            result += color_text(before, "text", caps)

        keyword = match.group(1)
        if keyword in {"已启用", "开启"}:
            badge = color_text(keyword, "success", caps)
            result += f"[{badge}]"
        else:
            badge = color_text(keyword, "muted", caps)
            result += f"[{badge}]"

        last_end = match.end()

    # 关键词后的剩余文字
    remaining = value[last_end:]
    if remaining:
        remaining = remaining.strip()
        if remaining:
            result += " " + color_text(remaining, "text", caps)

    return result if result else color_text(value, "text", caps)
