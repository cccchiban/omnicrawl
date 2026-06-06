from __future__ import annotations

from typing import Any

from .agent import AgentError, LocalToolAgent


# ── 工具确认展示 ──────────────────────────────────────────────

_TOOL_HUMAN_DESCRIPTIONS: dict[str, str] = {
    "list_files": "列出目录内容",
    "read_file": "读取文件内容",
    "search_text": "在文件中搜索文本",
    "replace_text": "替换文件中的文本",
    "write_file": "写入文件",
    "run_command": "执行命令",
    "memory_search": "搜索长期记忆",
    "memory_read": "读取记忆内容",
    "memory_expand_related": "展开相关记忆",
    "memory_write": "写入长期记忆",
}


def _truncate_for_display(text: str, max_len: int) -> str:
    """截断过长文本，保留可读的关键部分。"""

    if len(text) <= max_len:
        return text
    return text[: max_len - 3] + "..."


def _format_dangerous_tool_detail(tool_name: str, arguments: dict[str, Any]) -> str:
    """对写入/修改/执行类工具，提取关键内容供用户审查。

    只读类工具（读取、搜索、列出）不展示参数，保持界面简洁。
    """

    if tool_name == "run_command":
        cmd = arguments.get("command", "")
        if isinstance(cmd, str) and cmd.strip():
            return f"命令：{_truncate_for_display(cmd, 150)}"
        return ""

    if tool_name == "write_file":
        path = arguments.get("path", "")
        content = arguments.get("content", "")
        mode = arguments.get("mode", "overwrite")
        if isinstance(path, str) and path.strip():
            detail = f"{mode} 到 {path}"
            if isinstance(content, str) and content.strip():
                detail += f"，内容：{_truncate_for_display(content, 200)}"
            return detail
        return ""

    if tool_name == "replace_text":
        path = arguments.get("path", "")
        old = arguments.get("old_text", "")
        new = arguments.get("new_text", "")
        if isinstance(path, str) and path.strip():
            detail = f"文件：{path}"
            if isinstance(old, str) and old.strip():
                detail += f"，替换 \"{_truncate_for_display(old, 80)}\""
                if isinstance(new, str):
                    detail += f" → \"{_truncate_for_display(new, 80)}\""
            elif isinstance(old, str):
                detail += "，替换（空文本）"
                if isinstance(new, str) and new.strip():
                    detail += f" → \"{_truncate_for_display(new, 80)}\""
            return detail
        return ""

    if tool_name == "memory_write":
        memories = arguments.get("memories", [])
        if isinstance(memories, list) and memories:
            return f"写入 {len(memories)} 条记忆"
        return ""

    return ""


def format_tool_confirmation(tool_name: str, arguments: dict[str, Any]) -> str:
    """把工具调用格式化为行内 UI 的确认提示。

    只读工具仅显示描述，写入/执行工具额外展示关键内容供审查。
    """

    description = _TOOL_HUMAN_DESCRIPTIONS.get(tool_name, "执行操作")
    detail = _format_dangerous_tool_detail(tool_name, arguments)

    lines = [
        " Tool use",
        "",
        f"  Agent 想要{description}。",
    ]
    if detail:
        lines.append(f"  {detail}")
    lines.extend(["", " Do you want to proceed?"])

    return "\n".join(lines)


def format_skills_list(agent: LocalToolAgent) -> str:
    """格式化 Skill 列表为可展示文本。"""

    sm = agent.skill_manager
    if sm is None:
        return "Skill 子系统未启用。"

    metas = sm.list_all()
    if not metas:
        return "当前没有已加载的 Skill。在 .claude/skills/ 或 ~/.tui-agent/skills/ 下创建 SKILL.md 来添加。"

    lines = [f"已加载 {sm.count} 个 Skill："]
    for meta in metas:
        suffix = " [手动]" if meta.disable_model_invocation else ""
        lines.append(f"  {meta.name}{suffix}  ({meta.scope})")
        lines.append(f"    {meta.description}")
    return "\n".join(lines)


def print_skills_list(agent: LocalToolAgent) -> None:
    """行内 UI 打印 Skill 列表。"""

    print(format_skills_list(agent))


def format_memory_clean_result(agent: LocalToolAgent) -> str:
    """执行过期记忆清理，并返回适合终端展示的结果。"""

    try:
        deleted_paths = agent.clean_memory()
    except AgentError as exc:
        return f"记忆清理失败：{exc}"
    if not deleted_paths:
        return "没有需要清理的过期记忆。"
    joined = "\n".join(f"  - {path}" for path in deleted_paths)
    return f"已清理 {len(deleted_paths)} 条过期记忆：\n{joined}"


def print_memory_clean_result(agent: LocalToolAgent) -> None:
    """行内 UI 打印记忆清理结果。"""

    print(format_memory_clean_result(agent))


def build_slash_commands(agent: LocalToolAgent) -> list[str]:
    """构建所有可用的斜杠命令列表（含内置命令和动态 Skill 命令）。"""

    commands = ["/new", "/skills", "/memory:clean"]
    sm = agent.skill_manager
    if sm is not None:
        for meta in sm.list_all():
            commands.append(f"/skill:{meta.name}")
    return commands
