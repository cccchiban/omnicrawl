"""全屏 TUI 文件变更工具卡的 git 旁注行号 diff 渲染（纯格式化，无 Textual）。

展示策略（用户选定）：
- 1G4：旁注行号 diff
- 2A：仅全屏工具卡，不改确认框/工具返回协议
- 3A：write_file 覆盖且无旧内容时显示 rewrite +N lines，不编造假 diff
"""

from __future__ import annotations

import re
from difflib import SequenceMatcher
from typing import Any

from rich.text import Text

from ..tool_labels import format_duration, format_tool_status, tool_display
from .theme import (
    ACCENT_AMBER,
    ACCENT_BLUE,
    ACCENT_GREEN,
    ACCENT_RED,
    TEXT_FAINT,
    TEXT_MUTED,
    TEXT_PRIMARY,
    TEXT_SECONDARY,
)


FILE_CHANGE_TOOLS = frozenset({"write_file", "replace_text"})
MAX_DIFF_BODY_LINES = 80
MAX_PATH_CHARS = 48
MAX_PREVIEW_CHARS_PER_LINE = 160

COLOR_ADD = ACCENT_GREEN
COLOR_DEL = ACCENT_RED
COLOR_MOD = ACCENT_AMBER
COLOR_META = TEXT_MUTED
COLOR_GUTTER = TEXT_FAINT
COLOR_CTX = TEXT_SECONDARY
COLOR_HUNK = ACCENT_BLUE
COLOR_TITLE = ACCENT_AMBER


def _compact_title_value(value: Any, *, max_chars: int) -> str:
    """压缩工具标题中的路径或搜索目标，避免长参数撑破终端。"""

    text = re.sub(r"\s+", " ", str(value or "").strip())
    if not text:
        return "(未指定)"
    if len(text) <= max_chars:
        return text
    return text[: max_chars - 1] + "…"


def _read_result_line_range(result_text: str) -> tuple[int, int] | None:
    """从 read 的行号输出中提取实际返回内容的首尾源码行号。"""

    if not result_text:
        return None
    line_numbers = [
        int(match)
        for match in re.findall(r"(?m)^\s*(\d+):\s", result_text)
    ]
    if line_numbers:
        return line_numbers[0], line_numbers[-1]
    return None


def _list_result_summary(result_text: str) -> str | None:
    """把目录结果压缩为标题摘要，避免把完整列表挤进消息流。"""

    if not result_text:
        return None
    lines = [line.strip() for line in result_text.splitlines() if line.strip()]
    if not lines:
        return "0 项"
    if lines == ["目录为空。"]:
        return "0 项"
    truncated = any(line.startswith("...") for line in lines)
    visible_count = sum(not line.startswith("...") for line in lines)
    return f"{visible_count}{'+' if truncated else ''} 项"


def _generic_tool_name(operation: str, display_name: str) -> str:
    """返回普通工具标题中的短名称，命令工具只保留其单字母标识。"""

    short_names = {
        "bash": "",
        "powershell": "",
        "subagent": "SubAgent",
    }
    return short_names.get(operation, display_name.removeprefix("执行 "))


def _tool_title_context(tool_name: str, arguments: Any, result_text: str) -> str:
    """返回可放在工具标题末尾的公开操作上下文。"""

    del result_text
    args = arguments if isinstance(arguments, dict) else {}
    operation = _tool_operation(tool_name)
    if operation in {"bash", "powershell"}:
        command = str(args.get("command") or "").strip()
        return _compact_title_value(command, max_chars=64) if command else ""
    if operation == "monitor":
        action = str(args.get("action") or "").strip()
        command = str(args.get("command") or "").strip()
        monitor_id = str(args.get("monitor_id") or "").strip()
        context = action or "任务"
        if command:
            context += f"  {_compact_title_value(command, max_chars=48)}"
        elif monitor_id:
            context += f"  {_compact_title_value(monitor_id, max_chars=32)}"
        return context
    if operation == "subagent":
        action = str(args.get("action") or "").strip()
        tasks = args.get("tasks")
        task_count = len(tasks) if isinstance(tasks, list) else 0
        context = action or "任务"
        if task_count:
            context += f"  {task_count} 项"
        return context
    if operation == "windows_screenshot":
        target = str(args.get("target") or "").strip()
        return target or "截图"
    if operation in {
        "memory_search",
        "memory_read",
        "memory_expand_related",
        "memory_write",
        "project_memory_search",
        "project_memory_read",
        "project_memory_expand_related",
        "project_memory_write",
        "session_memory_search",
        "session_memory_read",
        "session_memory_expand_related",
        "session_memory_write",
        "user_memory_search",
        "user_memory_read",
        "user_memory_expand_related",
        "user_memory_write",
    }:
        query = str(args.get("query") or "").strip()
        ids = args.get("memory_ids")
        if query:
            return f"{_compact_title_value(query, max_chars=48)}"
        if isinstance(ids, list):
            return f"{len(ids)} 条记忆"
    return ""


def _status_color(status: str) -> str:
    """返回工具状态的语义色，避免整行被工具色覆盖。"""

    return {
        "成功": ACCENT_GREEN,
        "失败": ACCENT_RED,
        "调用中": ACCENT_BLUE,
        "等待确认": ACCENT_AMBER,
        "已取消": TEXT_MUTED,
    }.get(status, TEXT_MUTED)


def _append_status(
    rendered: Text,
    *,
    status: str,
    status_display: Any,
    duration_seconds: float,
) -> None:
    """向标题追加状态和耗时，并保持与文件变更标题相同的间距。"""

    rendered.append(
        f"{status_display.icon} {status_display.label}",
        style=_status_color(status),
    )
    rendered.append("  ", style=COLOR_META)
    rendered.append(format_duration(duration_seconds), style=TEXT_MUTED)


def _append_stats(rendered: Text, stats_label: str) -> None:
    """按增删语义色渲染标题中的行数统计。"""

    for part in re.split(r"([+-]\d+)", stats_label):
        if not part:
            continue
        if part.startswith("+"):
            style = COLOR_ADD
        elif part.startswith("-"):
            style = COLOR_DEL
        else:
            style = COLOR_META
        rendered.append(part, style=style)


def _tool_operation(tool_name: str) -> str:
    """返回工具的末级操作名，兼容带命名空间的工具名。"""

    return str(tool_name or "").rsplit(".", 1)[-1]


def _workspace_tool_title(
    *,
    operation: str,
    display_icon: str,
    arguments: Any,
    result_text: str,
    status: str,
    status_display: Any,
    duration_seconds: float,
) -> Text | None:
    """生成读取/搜索/目录列表工具与文件变更一致的单行摘要。"""

    args = arguments if isinstance(arguments, dict) else {}
    rendered = Text()
    if operation == "list":
        path = _compact_title_value(args.get("path") or ".", max_chars=MAX_PATH_CHARS)
        rendered.append("L  ", style=COLOR_TITLE)
        rendered.append(path, style=TEXT_PRIMARY)
        rendered.append("  |  ", style=COLOR_GUTTER)
        summary = _list_result_summary(result_text)
        if summary is not None:
            rendered.append(summary, style=COLOR_META)
        else:
            rendered.append("目录", style=TEXT_SECONDARY)
    elif operation == "read":
        path = _compact_title_value(
            args.get("path") or "(未指定文件)",
            max_chars=MAX_PATH_CHARS,
        )
        rendered.append(f"{display_icon}  ", style=COLOR_TITLE)
        rendered.append(path, style=TEXT_PRIMARY)
        line_range = _read_result_line_range(result_text)
        if line_range is not None:
            rendered.append("  |  ", style=COLOR_GUTTER)
            rendered.append(f"第 {line_range[0]}-{line_range[1]} 行", style=COLOR_META)
    elif operation == "read_image":
        path = _compact_title_value(
            args.get("path") or "(未指定图片)",
            max_chars=MAX_PATH_CHARS,
        )
        rendered.append(f"{display_icon}  ", style=COLOR_TITLE)
        rendered.append(path, style=TEXT_PRIMARY)
        rendered.append("  |  图片", style=COLOR_META)
    elif operation in {"find", "grep"}:
        path = _compact_title_value(args.get("path") or ".", max_chars=MAX_PATH_CHARS)
        pattern = _compact_title_value(args.get("pattern"), max_chars=36)
        rendered.append(f"{display_icon}  ", style=COLOR_TITLE)
        rendered.append(path, style=TEXT_PRIMARY)
        rendered.append("  |  目标: ", style=COLOR_META)
        rendered.append(pattern, style=TEXT_SECONDARY)
    else:
        return None

    rendered.append("  ", style=COLOR_META)
    _append_status(
        rendered,
        status=status,
        status_display=status_display,
        duration_seconds=duration_seconds,
    )
    return rendered


def is_file_change_tool(tool_name: str) -> bool:
    return str(tool_name or "") in FILE_CHANGE_TOOLS


def tool_disclosure_title(
    *,
    tool_name: str,
    arguments: Any,
    status: str,
    duration_seconds: float,
    expanded: bool,
    result_text: str = "",
) -> Text:
    """生成折叠/展开标题行。"""

    display = tool_display(tool_name)
    status_display = format_tool_status(status)
    operation = _tool_operation(tool_name)
    workspace_title = _workspace_tool_title(
        operation=operation,
        display_icon=display.icon,
        arguments=arguments,
        result_text=result_text,
        status=status,
        status_display=status_display,
        duration_seconds=duration_seconds,
    )
    if workspace_title is not None:
        return workspace_title

    context = _tool_title_context(tool_name, arguments, result_text)
    if not is_file_change_tool(tool_name):
        rendered = Text()
        rendered.append(display.icon, style=COLOR_TITLE)
        rendered.append("  ", style=COLOR_META)
        short_name = _generic_tool_name(operation, display.name)
        if short_name:
            rendered.append(short_name, style=TEXT_PRIMARY)
        if context:
            if short_name:
                rendered.append("  |  ", style=COLOR_GUTTER)
            rendered.append(context, style=TEXT_SECONDARY)
        rendered.append("  ", style=COLOR_META)
        _append_status(
            rendered,
            status=status,
            status_display=status_display,
            duration_seconds=duration_seconds,
        )
        return rendered

    change = describe_file_change(tool_name, arguments)
    rendered = Text()
    rendered.append(change.status_code, style=change.status_color)
    rendered.append("  ", style=COLOR_META)
    rendered.append(change.path_display, style=TEXT_PRIMARY)
    rendered.append("  |  ", style=COLOR_GUTTER)
    _append_stats(rendered, change.stats_label)
    rendered.append("  ", style=COLOR_META)
    _append_status(
        rendered,
        status=status,
        status_display=status_display,
        duration_seconds=duration_seconds,
    )
    return rendered


def tool_disclosure_body(
    *,
    tool_name: str,
    arguments: Any,
    result_text: str,
) -> Text:
    """生成展开后的正文（不含标题行）。"""

    if is_file_change_tool(tool_name):
        return file_change_body(tool_name, arguments, result_text)

    rendered = Text()
    rendered.append("工具：", style=COLOR_META)
    rendered.append(str(tool_name), style=COLOR_CTX)
    if arguments:
        rendered.append("\n参数：", style=COLOR_META)
        rendered.append(str(arguments), style=COLOR_CTX)
        rendered.append("\n")
    if result_text:
        rendered.append("结果：\n", style=COLOR_META)
        rendered.append(result_text, style=COLOR_CTX)
    return rendered


class FileChangeView:
    """一次文件变更工具调用的展示摘要。"""

    def __init__(
        self,
        *,
        status_code: str,
        status_color: str,
        path: str,
        stats_label: str,
        body: Text,
    ) -> None:
        self.status_code = status_code
        self.status_color = status_color
        self.path = path
        self.path_display = compact_path(path)
        self.stats_label = stats_label
        self.body = body


def describe_file_change(tool_name: str, arguments: Any) -> FileChangeView:
    args = arguments if isinstance(arguments, dict) else {}
    path = str(args.get("path") or "").strip() or "(unknown path)"

    if tool_name == "replace_text":
        old_text = str(args.get("old_text") or "")
        new_text = str(args.get("new_text") or "")
        body, added, removed = gutter_diff_text(old_text, new_text)
        stats = format_line_stats(added=added, removed=removed)
        return FileChangeView(
            status_code="M",
            status_color=COLOR_MOD,
            path=path,
            stats_label=stats,
            body=body,
        )

    content = str(args.get("content") or "")
    mode = str(args.get("mode") or "overwrite").lower()
    line_count = 0 if content == "" else len(content.splitlines())

    if mode == "append":
        body = preview_as_added_lines(content, header="append")
        return FileChangeView(
            status_code="M",
            status_color=COLOR_MOD,
            path=path,
            stats_label=f"append +{line_count} lines",
            body=body,
        )

    # 3A：覆盖写无旧内容 → rewrite 摘要，不编造删除侧
    body = preview_as_added_lines(content, header="rewrite")
    return FileChangeView(
        status_code="M",
        status_color=COLOR_MOD,
        path=path,
        stats_label=f"rewrite +{line_count} lines",
        body=body,
    )


def file_change_body(tool_name: str, arguments: Any, result_text: str) -> Text:
    change = describe_file_change(tool_name, arguments)
    rendered = Text()
    rendered.append("工具：", style=COLOR_META)
    rendered.append(str(tool_name), style=COLOR_CTX)
    rendered.append("\n")
    rendered.append_text(change.body)
    if result_text.strip():
        if rendered.plain and not rendered.plain.endswith("\n"):
            rendered.append("\n")
        rendered.append("结果：", style=COLOR_META)
        rendered.append(result_text.strip(), style=COLOR_META)
    return rendered


def format_line_stats(*, added: int, removed: int) -> str:
    parts: list[str] = []
    if added:
        parts.append(f"+{added}")
    if removed:
        parts.append(f"-{removed}")
    return " ".join(parts) if parts else "0"


def compact_path(path: str) -> str:
    text = path.replace("\\", "/")
    if len(text) <= MAX_PATH_CHARS:
        return text
    return "…" + text[-(MAX_PATH_CHARS - 1) :]


def preview_as_added_lines(content: str, *, header: str) -> Text:
    """无旧文件时，把新内容以 + 行预览；不伪造 - 行。"""

    rendered = Text()
    rendered.append(f"@@ {header} · showing new content only @@\n", style=COLOR_HUNK)
    if content == "":
        rendered.append("   1 │ ", style=COLOR_GUTTER)
        rendered.append("+ ", style=f"{COLOR_ADD} bold")
        rendered.append("(empty)\n", style=COLOR_ADD)
        return rendered

    lines = content.splitlines()
    visible = lines[:MAX_DIFF_BODY_LINES]
    for index, line in enumerate(visible, start=1):
        rendered.append(f"{index:>4} │ ", style=COLOR_GUTTER)
        rendered.append("+ ", style=f"{COLOR_ADD} bold")
        rendered.append(_clip_line(line) + "\n", style=COLOR_ADD)
    omitted = len(lines) - len(visible)
    if omitted > 0:
        rendered.append(f" ... {omitted} more lines omitted\n", style=COLOR_META)
    return rendered


def gutter_diff_text(old_text: str, new_text: str) -> tuple[Text, int, int]:
    """把 old/new 渲染成旁注行号 diff，并返回 (+added, -removed) 行统计。"""

    old_lines = old_text.splitlines()
    new_lines = new_text.splitlines()
    matcher = SequenceMatcher(a=old_lines, b=new_lines, autojunk=False)
    rendered = Text()
    rendered.append("@@ snippet @@\n", style=COLOR_HUNK)

    added = 0
    removed = 0
    body_lines = 0
    old_no = 1
    new_no = 1
    truncated = False

    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if body_lines >= MAX_DIFF_BODY_LINES:
            truncated = True
            break

        if tag == "equal":
            for line in old_lines[i1:i2]:
                if body_lines >= MAX_DIFF_BODY_LINES:
                    truncated = True
                    break
                _append_gutter_line(rendered, old_no, " ", line, COLOR_CTX)
                body_lines += 1
                old_no += 1
                new_no += 1
            continue

        if tag in {"delete", "replace"}:
            for line in old_lines[i1:i2]:
                if body_lines >= MAX_DIFF_BODY_LINES:
                    truncated = True
                    break
                _append_gutter_line(rendered, old_no, "-", line, COLOR_DEL)
                body_lines += 1
                removed += 1
                old_no += 1

        if truncated:
            break

        if tag in {"insert", "replace"}:
            for line in new_lines[j1:j2]:
                if body_lines >= MAX_DIFF_BODY_LINES:
                    truncated = True
                    break
                _append_gutter_line(rendered, new_no, "+", line, COLOR_ADD)
                body_lines += 1
                added += 1
                new_no += 1

    if truncated:
        rendered.append(" ... diff truncated\n", style=COLOR_META)
    if added == 0 and removed == 0:
        rendered.append(" (no textual changes)\n", style=COLOR_META)

    return rendered, added, removed


def _append_gutter_line(
    rendered: Text,
    line_no: int,
    marker: str,
    line: str,
    color: str,
) -> None:
    rendered.append(f"{line_no:>4} │ ", style=COLOR_GUTTER)
    if marker == "+":
        rendered.append("+ ", style=f"{COLOR_ADD} bold")
    elif marker == "-":
        rendered.append("- ", style=f"{COLOR_DEL} bold")
    else:
        rendered.append("  ", style=COLOR_GUTTER)
    rendered.append(_clip_line(line) + "\n", style=color)


def _clip_line(line: str) -> str:
    text = line.replace("\t", "    ")
    if len(text) <= MAX_PREVIEW_CHARS_PER_LINE:
        return text
    return text[: MAX_PREVIEW_CHARS_PER_LINE - 1] + "…"


def plain_tool_title(
    *,
    tool_name: str,
    arguments: Any,
    status: str,
    duration_seconds: float,
    expanded: bool = False,
    result_text: str = "",
) -> str:
    return tool_disclosure_title(
        tool_name=tool_name,
        arguments=arguments,
        status=status,
        duration_seconds=duration_seconds,
        expanded=expanded,
        result_text=result_text,
    ).plain


__all__ = [
    "FILE_CHANGE_TOOLS",
    "FileChangeView",
    "describe_file_change",
    "file_change_body",
    "gutter_diff_text",
    "is_file_change_tool",
    "plain_tool_title",
    "preview_as_added_lines",
    "tool_disclosure_body",
    "tool_disclosure_title",
]
