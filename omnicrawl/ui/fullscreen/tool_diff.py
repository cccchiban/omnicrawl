"""全屏 TUI 文件变更工具卡的 git 旁注行号 diff 渲染（纯格式化，无 Textual）。

展示策略（用户选定）：
- 1G4：旁注行号 diff
- 2A：仅全屏工具卡，不改确认框/工具返回协议
- 3A：write_file 覆盖且无旧内容时显示 rewrite +N lines，不编造假 diff
"""

from __future__ import annotations

from difflib import SequenceMatcher
from typing import Any

from rich.text import Text


FILE_CHANGE_TOOLS = frozenset({"write_file", "replace_text"})
MAX_DIFF_BODY_LINES = 80
MAX_PATH_CHARS = 48
MAX_PREVIEW_CHARS_PER_LINE = 160

COLOR_ADD = "#00e5c3"
COLOR_DEL = "#ff5470"
COLOR_MOD = "#f4b860"
COLOR_META = "#8fa4ad"
COLOR_GUTTER = "#4d5c63"
COLOR_CTX = "#a9c7d3"
COLOR_HUNK = "#39a7ff"
COLOR_TITLE = "#f4b860"


def is_file_change_tool(tool_name: str) -> bool:
    return str(tool_name or "") in FILE_CHANGE_TOOLS


def tool_disclosure_title(
    *,
    tool_name: str,
    arguments: Any,
    status: str,
    duration_seconds: float,
    expanded: bool,
) -> Text:
    """生成折叠/展开标题行。"""

    marker = "▾" if expanded else "▸"
    if not is_file_change_tool(tool_name):
        return Text(
            f"{marker} ⌁ {tool_name} · {status} · {duration_seconds:.2f}s",
            style=COLOR_TITLE,
        )

    change = describe_file_change(tool_name, arguments)
    rendered = Text()
    rendered.append(f"{marker} ", style=COLOR_META)
    rendered.append(change.status_code, style=f"{change.status_color} bold")
    rendered.append("  ", style=COLOR_META)
    rendered.append(change.path_display, style="#d9e4e8 bold")
    rendered.append("  |  ", style=COLOR_GUTTER)
    rendered.append(change.stats_label, style=COLOR_META)
    rendered.append(f" · {status} · {duration_seconds:.2f}s", style=COLOR_META)
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
    if arguments:
        rendered.append("参数：", style=COLOR_META)
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
) -> str:
    return tool_disclosure_title(
        tool_name=tool_name,
        arguments=arguments,
        status=status,
        duration_seconds=duration_seconds,
        expanded=expanded,
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
