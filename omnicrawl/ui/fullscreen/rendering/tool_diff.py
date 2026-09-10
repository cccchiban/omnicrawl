"""全屏 TUI 文件变更工具卡的 git 旁注行号 diff 渲染（纯格式化，无 Textual）。

展示策略（用户选定）：
- 1G4：旁注行号 diff
- 2A：仅全屏工具卡，不改确认框/工具返回协议
- 3A：write_file 覆盖且无旧内容时显示 rewrite +N lines，不编造假 diff
- 4B：Edit_file 正文保留旁注行号 diff 预览，结果区只显示“替换 N 处”
  摘要，不再整段展示工具返回的带行号上下文文本；旁注行号从工具返回
  的上下文块解析出的文件真实行号开始，而非 snippet 内相对计数。
"""

from __future__ import annotations

import re
from difflib import SequenceMatcher
from typing import Any

from rich.text import Text

from ....agent.toolkit.tools import ASK_USER_TOOL_NAME
from ...tool_labels import format_duration, format_tool_status, tool_display
from ..terminal.theme import (
    ACCENT_AMBER,
    ACCENT_BLUE,
    ACCENT_GREEN,
    ACCENT_RED,
    TEXT_FAINT,
    TEXT_MUTED,
    TEXT_PRIMARY,
    TEXT_SECONDARY,
    TOOL_TEXT,
)


FILE_CHANGE_TOOLS = frozenset({"write_file", "Edit_file"})
# 豁免“原始输出 + 五行折叠”规则的工具：write_file 与 Edit_file 保留
# 文件变更预览（diff/rewrite 摘要），其余工具一律直接展示工具返回的原始输出。
# （与 FILE_CHANGE_TOOLS 同集，语义都是“保留文件变更预览的工具”。）
FULL_BODY_TOOLS = FILE_CHANGE_TOOLS
# 记忆类工具（4 组 × 4 动作）。统一在此登记：正文隐藏、标题 query 摘要
# 等规则都引用本集合，新增记忆工具只需改这一处。
MEMORY_TOOLS = frozenset({
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
})
# 知识库类工具：正文对用户无展示价值，与记忆工具一起隐藏。
KB_TOOLS = frozenset({
    "kb_search",
    "kb_read",
    "kb_write",
    "kb_append",
    "kb_list",
})
# 正文对用户没有展示价值、完全隐藏的工具：read（文件内容只读，标题已
# 含路径与行号摘要）；全部记忆工具与知识库工具（搜索/读取/展开/写入
# 结果只供模型消费，对用户无意义）。不显示返回内容，也不显示任何
# “已隐藏”提示行，只保留标题行。
HIDDEN_BODY_TOOLS = frozenset({"read"}) | MEMORY_TOOLS | KB_TOOLS
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


def _tool_title_context(tool_name: str, arguments: Any, result_text: str) -> str:
    """返回可放在工具标题末尾的公开操作上下文。"""

    del result_text
    args = arguments if isinstance(arguments, dict) else {}
    operation = _tool_operation(tool_name)
    if operation in {"bash", "powershell"}:
        # 命令完整展示，不做长度截断：短标题只保留单字母标识，命令本身
        # 是用户最关心的信息，超长时由 Textual 自动换行。
        command = str(args.get("command") or "").strip()
        return command if command else ""
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
    if operation in MEMORY_TOOLS:
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


def _append_tail(
    rendered: Text,
    *,
    status: str,
    status_display: Any,
    duration_seconds: float,
) -> None:
    """向标题追加「· 状态 · 耗时」暗色尾段（方案6：状态只由行首色点表达）。"""

    rendered.append(" · ", style=COLOR_META)
    rendered.append(
        f"{status_display.icon} {status_display.label}",
        style=TEXT_MUTED,
    )
    rendered.append(" · ", style=COLOR_META)
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
    display_name: str,
    arguments: Any,
    result_text: str,
    status: str,
    status_display: Any,
    duration_seconds: float,
) -> Text | None:
    """生成读取/搜索/目录列表工具的单行摘要（方案6：色点 + 原名 + 暗色上下文）。"""

    args = arguments if isinstance(arguments, dict) else {}
    rendered = Text()
    rendered.append("● ", style=_status_color(status))
    if operation == "list":
        path = _compact_title_value(args.get("path") or ".", max_chars=MAX_PATH_CHARS)
        rendered.append(display_name, style=TEXT_PRIMARY)
        rendered.append(f" {path}", style=TEXT_MUTED)
        summary = _list_result_summary(result_text)
        if summary is not None:
            rendered.append(f" · {summary}", style=TEXT_MUTED)
        else:
            rendered.append(" · 目录", style=TEXT_MUTED)
    elif operation == "read":
        path = _compact_title_value(
            args.get("path") or "(未指定文件)",
            max_chars=MAX_PATH_CHARS,
        )
        rendered.append(display_name, style=TEXT_PRIMARY)
        rendered.append(f" {path}", style=TEXT_MUTED)
        line_range = _read_result_line_range(result_text)
        if line_range is not None:
            rendered.append(f" · 第 {line_range[0]}-{line_range[1]} 行", style=TEXT_MUTED)
    elif operation == "read_image":
        path = _compact_title_value(
            args.get("path") or "(未指定图片)",
            max_chars=MAX_PATH_CHARS,
        )
        rendered.append(display_name, style=TEXT_PRIMARY)
        rendered.append(f" {path} · 图片", style=TEXT_MUTED)
    elif operation in {"find", "grep"}:
        path = _compact_title_value(args.get("path") or ".", max_chars=MAX_PATH_CHARS)
        pattern = _compact_title_value(args.get("pattern"), max_chars=36)
        rendered.append(display_name, style=TEXT_PRIMARY)
        rendered.append(f" {path} · 目标: {pattern}", style=TEXT_MUTED)
    else:
        return None

    _append_tail(
        rendered,
        status=status,
        status_display=status_display,
        duration_seconds=duration_seconds,
    )
    return rendered


def _ask_user_tool_title(status: str, duration_seconds: float) -> Text | None:
    """ask_user 工具卡标题：等待回复/已收到回复 + 耗时。

    提问期间显示「↘ 等待回复...」并实时计时；用户回答后收口为
    「↗ 已收到回复」并冻结实际耗时。取消等异常状态回退通用标题。
    """

    if status == "等待回复":
        label, color = "↘ 等待回复...", ACCENT_BLUE
    elif status == "已收到回复":
        label, color = "↗ 已收到回复", ACCENT_GREEN
    else:
        return None
    rendered = Text()
    rendered.append("● ", style=color)
    rendered.append(label, style=color)
    rendered.append(" · ", style=COLOR_META)
    rendered.append(format_duration(duration_seconds), style=TEXT_MUTED)
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
    if tool_name == ASK_USER_TOOL_NAME:
        ask_title = _ask_user_tool_title(status, duration_seconds)
        if ask_title is not None:
            return ask_title
    operation = _tool_operation(tool_name)
    workspace_title = _workspace_tool_title(
        operation=operation,
        display_name=display.name,
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
        rendered.append("● ", style=_status_color(status))
        rendered.append(display.name, style=TEXT_PRIMARY)
        if context:
            rendered.append(" ", style=COLOR_META)
            rendered.append(context, style=TEXT_MUTED)
        _append_tail(
            rendered,
            status=status,
            status_display=status_display,
            duration_seconds=duration_seconds,
        )
        return rendered

    change = describe_file_change(tool_name, arguments)
    rendered = Text()
    rendered.append("● ", style=_status_color(status))
    rendered.append(display.name, style=TEXT_PRIMARY)
    rendered.append(f" {change.path_display}", style=TEXT_MUTED)
    rendered.append(" · ", style=COLOR_META)
    _append_stats(rendered, change.stats_label)
    _append_tail(
        rendered,
        status=status,
        status_display=status_display,
        duration_seconds=duration_seconds,
    )
    return rendered


def fetcher_body(result_text: str) -> Text:
    """fetcher 结果正文：保留汇总/URL/状态/标题，隐藏网页正文内容。

    抓取到的页面正文体积大、对排障价值低，只展示元信息（URL、状态、
    标题），页面正文直接折叠，不显示任何“已隐藏”提示，避免刷屏。
    失败条目（"失败: ..."）原样保留，便于排障。
    """

    lines = result_text.splitlines()
    kept: list[str] = []
    skipping_content = False
    for line in lines:
        if skipping_content:
            # 内容块结束后（下一个条目、失败条目或结尾）恢复保留。
            if re.match(r"^\d+\.\s", line) or line.startswith("   失败: "):
                skipping_content = False
            else:
                continue
        if line.startswith("   内容: "):
            skipping_content = True
            continue
        kept.append(line)

    rendered = Text()
    if kept:
        rendered.append("\n".join(kept), style=TOOL_TEXT)
    return rendered


def tool_disclosure_body(
    *,
    tool_name: str,
    arguments: Any,
    result_text: str,
) -> Text:
    """生成展开后的正文（不含标题行）。

    除 write_file 与 Edit_file 外的所有工具统一直接展示工具返回的
    原始输出（灰色），不再包装“工具/参数/结果”元信息；write_file 与
    Edit_file 保留文件变更预览正文（diff/rewrite 摘要），不受五行
    折叠限制；Edit_file 的结果区只保留“替换 N 处”摘要，不整段展示
    带行号上下文文本；fetcher 只展示 URL/状态/标题，隐藏页面正文；
    read 与全部记忆工具的正文完全不展示给终端用户（正文为空，不保留
    任何提示行）。
    """

    operation = _tool_operation(tool_name)
    if operation in FULL_BODY_TOOLS:
        return file_change_body(tool_name, arguments, result_text)
    if operation == "fetcher":
        return fetcher_body(result_text)
    if operation in HIDDEN_BODY_TOOLS:
        # read 正文不展示给终端用户：正文为空，不保留任何提示行。
        return Text()

    rendered = Text()
    if result_text:
        rendered.append(result_text, style=TOOL_TEXT)
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


def describe_file_change(
    tool_name: str,
    arguments: Any,
    *,
    start_line: int = 1,
) -> FileChangeView:
    args = arguments if isinstance(arguments, dict) else {}
    path = str(args.get("path") or "").strip() or "(unknown path)"

    if tool_name == "Edit_file":
        old_text = str(args.get("old_text") or "")
        new_text = str(args.get("new_text") or "")
        body, added, removed = gutter_diff_text(
            old_text,
            new_text,
            start_line=start_line,
        )
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


def _edit_result_summary(result_text: str) -> str:
    """从 Edit_file 返回文本中提取“替换 N 处”摘要，丢弃带行号上下文。"""

    if not result_text:
        return ""
    match = re.search(r"替换\s*\d+\s*处", result_text)
    return match.group(0) if match else ""


def _edit_start_line(result_text: str, new_text: str) -> int:
    """从 Edit_file 返回文本的上下文块解析替换位置的真实起始文件行号。

    上下文块格式为“N: 内容”的连续行，其中间部分即替换后的内容；
    优先用 new_text 首行精确匹配，匹配失败时跳过前置上下文行数取
    替换位置首行，最后回退到块首行。
    """

    if not result_text:
        return 1
    lines = result_text.splitlines()
    context_lines = 2
    block_start: int | None = None
    for index, line in enumerate(lines):
        if "首个替换位置上下文" in line:
            block_start = index + 1
            match = re.search(r"前后各\s*(\d+)\s*行", line)
            if match:
                context_lines = int(match.group(1))
            break
    if block_start is None:
        return 1
    numbered: list[tuple[int, str]] = []
    for line in lines[block_start:]:
        match = re.match(r"^\s*(\d+):\s*(.*)$", line)
        if match:
            numbered.append((int(match.group(1)), match.group(2).strip()))
    if not numbered:
        return 1
    new_first = new_text.splitlines()[0].strip() if new_text.splitlines() else ""
    if new_first:
        for lineno, content in numbered:
            if content == new_first:
                return lineno
    # 回退：跳过前置上下文行，取替换位置首行；否则取块首行。
    if len(numbered) > context_lines:
        return numbered[context_lines][0]
    return numbered[0][0]


def file_change_body(
    tool_name: str,
    arguments: Any,
    result_text: str,
    *,
    include_result: bool = True,
) -> Text:
    args = arguments if isinstance(arguments, dict) else {}
    start_line = 1
    if tool_name == "Edit_file":
        start_line = _edit_start_line(
            result_text,
            str(args.get("new_text") or ""),
        )
    change = describe_file_change(tool_name, arguments, start_line=start_line)
    rendered = Text()
    rendered.append("工具：", style=COLOR_META)
    rendered.append(str(tool_name), style=COLOR_CTX)
    rendered.append("\n")
    rendered.append_text(change.body)
    if not include_result or not result_text.strip():
        return rendered
    if rendered.plain and not rendered.plain.endswith("\n"):
        rendered.append("\n")
    if tool_name == "Edit_file":
        summary = _edit_result_summary(result_text)
        if summary:
            rendered.append("结果：", style=COLOR_META)
            rendered.append(summary, style=COLOR_META)
    else:
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


def gutter_diff_text(
    old_text: str,
    new_text: str,
    *,
    start_line: int = 1,
) -> tuple[Text, int, int]:
    """把 old/new 渲染成旁注行号 diff，并返回完整 diff 的 (+added, -removed)。

    行数统计反映完整改动的真实增删（不受预览行数上限截断影响），
    避免工具卡标题在超大 diff 时把「截断预览统计」当成「改动统计」。

    ``start_line`` 为替换位置在文件中的真实起始行号（默认 1）；旁注
    行号从该行号开始递增，而不是 snippet 内的相对计数。
    """

    old_lines = old_text.splitlines()
    new_lines = new_text.splitlines()
    matcher = SequenceMatcher(a=old_lines, b=new_lines, autojunk=False)
    rendered = Text()
    rendered.append("@@ snippet @@\n", style=COLOR_HUNK)

    # 完整 diff 的增删统计：单独遍历 opcode，不受渲染行数上限影响。
    added, removed = _full_change_counts(matcher, old_lines, new_lines)

    body_lines = 0
    old_no = start_line
    new_no = start_line
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
                new_no += 1

    if truncated:
        rendered.append(" ... diff truncated\n", style=COLOR_META)
    if added == 0 and removed == 0:
        rendered.append(" (no textual changes)\n", style=COLOR_META)

    return rendered, added, removed


def _full_change_counts(
    matcher: SequenceMatcher,
    old_lines: list[str],
    new_lines: list[str],
) -> tuple[int, int]:
    """统计完整 diff 的真实增删行数（不受预览行数上限影响）。"""

    added = 0
    removed = 0
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "delete":
            removed += i2 - i1
        elif tag == "insert":
            added += j2 - j1
        elif tag == "replace":
            removed += i2 - i1
            added += j2 - j1
    return added, removed


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
    "FULL_BODY_TOOLS",
    "fetcher_body",
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
