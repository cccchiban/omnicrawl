from __future__ import annotations

import json
import logging
import os
import re
import subprocess
from pathlib import Path
from typing import Any

from ..config.features.approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    approval_mode_label,
    save_approval_mode,
)
from ..agent import AgentError, LocalToolAgent
from ..agent.toolkit.tools import public_tool_arguments
from ..config.models.llm import LLMError, save_active_model_ref, save_reasoning_effort
from ..config.models.model_catalog import save_llm_model
from ..config.core.runtime import RuntimeConfigError
from ..config.features.advisor import (
    ADVISOR_EFFORT_OPTIONS,
    AdvisorConfig,
    AdvisorConfigError,
    DEFAULT_ADVISOR_EFFORT,
    clear_advisor_config,
    load_advisor_config,
    save_advisor_config,
)
from .framework import (
    CommandContext,
    CommandRegistry,
    CommandResult,
    CommandType,
)

_LOGGER = logging.getLogger(__name__)

#: 全仓库唯一的斜杠命令注册表。命令在下方通过 ``@REGISTRY.command`` 声明,
#: 解析/分发/帮助/补全统一由 :class:`~omnicrawl.commands.framework.CommandRegistry`
#: 负责；各入口（TUI、Telegram、飞书）只调用 ``REGISTRY.dispatch``。
REGISTRY = CommandRegistry()


# ── 工具确认展示 ──────────────────────────────────────────────

_TOOL_HUMAN_DESCRIPTIONS: dict[str, str] = {
    "list": "列出目录内容",
    "find": "按名称或路径查找文件",
    "read": "读取文件内容",
    "read_image": "读取图片",
    "grep": "在文件中搜索文本",
    "Edit_file": "替换文件中的文本",
    "write_file": "写入文件",
    "bash": "执行 Bash 命令",
    "powershell": "执行 PowerShell 命令",
    "monitor": "管理后台命令",
    "windows_window": "操作 Windows 窗口",
    "windows_control": "操作 Windows UI 控件",
    "windows_input": "模拟 Windows 鼠标或键盘输入",
    "windows_clipboard": "操作 Windows 文本剪贴板",
    "windows_screenshot": "截取 Windows 桌面画面",
    "memory_search": "搜索长期记忆",
    "memory_read": "读取记忆内容",
    "memory_expand_related": "展开相关记忆",
    "memory_write": "写入长期记忆",
    "subagent": "分发只读子任务",
    "advisor": "咨询顾问模型获取第二意见",
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

    if tool_name in {"bash", "powershell"}:
        cmd = arguments.get("command", "")
        if isinstance(cmd, str) and cmd.strip():
            return f"命令：{_truncate_for_display(cmd, 150)}"
        return ""

    if tool_name == "monitor":
        action = arguments.get("action", "start")
        monitor_id = arguments.get("monitor_id", "")
        command = arguments.get("command", "")
        if action == "start" and isinstance(command, str) and command.strip():
            return f"后台命令：{_truncate_for_display(command, 150)}"
        if isinstance(monitor_id, str) and monitor_id.strip():
            return f"操作：{action}，任务：{monitor_id}"
        return f"操作：{action}"

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

    if tool_name == "Edit_file":
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

    if tool_name == "read_image":
        path = arguments.get("path", "")
        detail = f"文件：{path}" if isinstance(path, str) and path.strip() else ""
        if arguments.get("detail"):
            detail += f"，视觉细节：{arguments['detail']}"
        prompt = arguments.get("prompt")
        if isinstance(prompt, str) and prompt.strip():
            detail += f"，分析提示词：{_truncate_for_display(prompt, 120)}"
        return _truncate_for_display(detail, 240)

    if tool_name == "windows_window":
        action = arguments.get("action", "")
        handle = arguments.get("window_handle", "")
        if handle:
            return f"操作：{action}，窗口：{handle}"
        return f"操作：{action}"

    if tool_name == "windows_control":
        action = arguments.get("action", "")
        handle = arguments.get("window_handle", "")
        locator_parts = []
        for key in ("automation_id", "name", "class_name", "control_type", "index"):
            if key in arguments:
                locator_parts.append(f"{key}={arguments[key]}")
        detail = f"操作：{action}"
        if handle:
            detail += f"，窗口：{handle}"
        if locator_parts:
            detail += "，定位：" + "、".join(locator_parts)
        if "value_length" in arguments:
            detail += f"，写入文本：{arguments['value_length']} 字符（内容不展示）"
        return _truncate_for_display(detail, 240)

    if tool_name == "windows_input":
        action = arguments.get("action", "")
        detail = f"操作：{action}"
        if "x" in arguments and "y" in arguments:
            detail += f"，坐标：({arguments['x']}, {arguments['y']})"
        if "button" in arguments:
            detail += f"，按钮：{arguments['button']}"
        if "keys" in arguments:
            detail += "，按键：" + "+".join(map(str, arguments["keys"]))
        elif "key" in arguments:
            detail += f"，按键：{arguments['key']}"
        if "text_length" in arguments:
            detail += f"，输入文本：{arguments['text_length']} 字符（内容不展示）"
        return _truncate_for_display(detail, 240)

    if tool_name == "windows_clipboard":
        action = arguments.get("action", "")
        detail = f"操作：{action}"
        if "text_length" in arguments:
            detail += f"，文本：{arguments['text_length']} 字符（内容不展示）"
        if "max_chars" in arguments:
            detail += f"，最多读取：{arguments['max_chars']} 字符"
        return detail

    if tool_name == "windows_screenshot":
        target = arguments.get("target", "desktop")
        detail = f"目标：{target}"
        if arguments.get("window_handle"):
            detail += f"，窗口：{arguments['window_handle']}"
        if all(key in arguments for key in ("x", "y", "width", "height")):
            detail += (
                f"，区域：({arguments['x']}, {arguments['y']}) "
                f"{arguments['width']}×{arguments['height']}"
            )
        if "max_dimension" in arguments:
            detail += f"，模型图片最大边：{arguments['max_dimension']}"
        return _truncate_for_display(detail, 240)

    if tool_name == "memory_write" or tool_name.endswith("_memory_write"):
        memories = arguments.get("memories", [])
        if isinstance(memories, list) and memories:
            return f"写入 {len(memories)} 条记忆"
        return ""

    if tool_name == "subagent":
        descriptions = arguments.get("descriptions", [])
        detail = f"任务数：{arguments.get('task_count', 0)}"
        if isinstance(descriptions, list) and descriptions:
            detail += "，任务：" + "；".join(str(item) for item in descriptions)
        return _truncate_for_display(detail, 240)

    if "." in tool_name and arguments:
        return f"参数：{_truncate_for_display(json.dumps(arguments, ensure_ascii=False), 240)}"

    return ""


def format_tool_confirmation(tool_name: str, arguments: dict[str, Any]) -> str:
    """把工具调用格式化为行内 UI 的确认提示。

    只读工具仅显示描述，写入/执行工具额外展示关键内容供审查。
    """

    arguments = public_tool_arguments(tool_name, arguments)
    description = _TOOL_HUMAN_DESCRIPTIONS.get(tool_name)
    if description is None and "." in tool_name:
        description = f"执行 MCP 工具 {tool_name}"
    if description is None:
        description = "执行操作"
    detail = _format_dangerous_tool_detail(tool_name, arguments)

    lines = [f"Agent 想要{description}。"]
    if detail:
        lines.append(detail)
    lines.extend(["", "是否允许执行？"])

    return "\n".join(lines)


def format_skills_list(agent: LocalToolAgent) -> str:
    """格式化 Skill 列表为可展示文本。"""

    sm = agent.skill_manager
    if sm is None:
        return "Skill 子系统未启用。"

    metas = sm.list_all()
    if not metas:
        return "当前没有已加载的 Skill。在 .omnicrawl/skills/、~/.omnicrawl/skills/ 或 $OMNICRAWL_ENTERPRISE_DIR/ 下创建 SKILL.md 来添加。"

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


def format_mcp_status(agent: LocalToolAgent) -> str:
    """格式化 MCP 状态为可展示文本。"""

    return agent.format_mcp_status()


def print_mcp_status(agent: LocalToolAgent) -> None:
    """行内 UI 打印 MCP 状态。"""

    print(format_mcp_status(agent))


def format_plugins_status(agent: LocalToolAgent) -> str:
    """格式化插件子系统只读状态。"""

    formatter = getattr(agent, "format_plugins_status", None)
    if callable(formatter):
        return formatter()
    return "插件状态接口不可用。"


def print_plugins_status(agent: LocalToolAgent) -> None:
    """行内 UI 打印插件状态。"""

    print(format_plugins_status(agent))


def format_sessions_list(agent: LocalToolAgent) -> str:
    """格式化当前工作区最近会话列表。"""

    try:
        sessions = agent.list_sessions(limit=10)
    except AgentError as exc:
        return f"会话列表读取失败：{exc}"
    if not sessions:
        return "当前工作区还没有可恢复会话。"

    current_id = agent.current_session_id
    lines = ["最近会话："]
    for entry in sessions:
        marker = "*" if entry.session_id == current_id else " "
        title = entry.title or "未命名会话"
        updated_at = entry.updated_at.astimezone().strftime("%Y-%m-%d %H:%M:%S")
        lines.append(
            f"{marker} {entry.session_id}  {updated_at}  {entry.message_count} 条消息  {title}"
        )
    lines.append("")
    lines.append("恢复会话：/resume <session_id>")
    return "\n".join(lines)


def format_archived_sessions_list(agent: LocalToolAgent) -> str:
    """格式化当前工作区已归档会话列表。"""

    try:
        sessions = agent.list_archived_sessions(limit=10)
    except AgentError as exc:
        return f"归档会话列表读取失败：{exc}"
    if not sessions:
        return "当前工作区还没有已归档会话。"

    lines = ["归档会话："]
    for entry in sessions:
        title = entry.title or "未命名会话"
        archived_at = entry.archived_at.astimezone().strftime("%Y-%m-%d %H:%M:%S") if entry.archived_at else "-"
        lines.append(
            f"  {entry.session_id}  {archived_at}  {entry.message_count} 条消息  {title}"
        )
    lines.append("")
    lines.append("恢复归档会话：/resume <session_id>（恢复后会重新进入最近会话列表）")
    return "\n".join(lines)


def format_prompt_history(agent: LocalToolAgent, query: str = "") -> str:
    """格式化当前项目用户提示历史；只展示，不注入模型上下文。"""

    try:
        entries = agent.search_prompt_history(query=query, limit=20)
    except AgentError as exc:
        return f"提示历史读取失败：{exc}"
    if not entries:
        return "当前工作区还没有匹配的提示历史。"

    title = "提示历史" if not query.strip() else f"提示历史（关键词：{query.strip()}）"
    lines = [f"{title}："]
    for index, entry in enumerate(entries, start=1):
        created_at = entry.created_at.astimezone().strftime("%Y-%m-%d %H:%M:%S")
        display = _truncate_for_display(" ".join(entry.display.split()), 120)
        session_marker = "*" if entry.session_id == agent.current_session_id else " "
        lines.append(f"{index:>2}. {session_marker} {created_at}  {display}")
    lines.append("")
    lines.append("筛选历史：/history <关键词>")
    return "\n".join(lines)


def format_subagent_tasks_list(agent: LocalToolAgent) -> str:
    """格式化当前会话可见的后台 SubAgent 任务，不展示原始 prompt 或完整结果。"""

    try:
        tasks = agent.list_subagent_tasks()
    except AgentError as exc:
        return f"子任务查询失败：{exc}"
    if not tasks:
        return "当前会话没有后台 SubAgent 任务。"

    lines = ["当前会话后台子任务："]
    for task in tasks:
        task_id = str(task.get("task_id") or "-")
        agent_type = str(task.get("agent_type") or "subagent")
        status = str(task.get("status") or "unknown")
        description = str(task.get("description") or "未提供描述")
        lines.append(f"- {task_id} · {agent_type} · {status} · {description}")
    lines.extend(
        [
            "",
            "查看详情：/task <task_id>",
            "取消任务：/task cancel <task_id>",
        ]
    )
    return "\n".join(lines)


def format_subagent_task(agent: LocalToolAgent, task_id: str) -> str:
    """格式化单个安全任务快照，供终端和全屏 TUI 共享使用。"""

    try:
        task = agent.get_subagent_task(task_id)
    except AgentError as exc:
        return f"子任务查询失败：{exc}"
    if task is None:
        return "未找到当前会话的 SubAgent 任务。"

    lines = [
        f"任务：{task.get('task_id') or '-'}",
        f"状态：{task.get('status') or 'unknown'}",
        f"角色：{task.get('agent_type') or 'subagent'}",
        f"描述：{task.get('description') or '未提供描述'}",
    ]
    result = task.get("result")
    if isinstance(result, dict):
        summary = str(result.get("summary") or "").strip()
        if summary:
            lines.extend(["", "摘要：", summary])
        artifacts = result.get("artifacts")
        if isinstance(artifacts, (list, tuple)) and artifacts:
            lines.append(f"关联 artifact：{len(artifacts)} 项")
    error = task.get("error")
    if isinstance(error, dict):
        message = str(error.get("message") or "").strip()
        if message:
            lines.extend(["", f"错误：{message}"])
    return "\n".join(lines)


@REGISTRY.command(
    name="tasks",
    description="查看当前会话可见的后台 SubAgent 任务。",
    usage="/tasks",
    type=CommandType.QUERY,
)
def handle_tasks_command(ctx: CommandContext) -> CommandResult:
    """列出当前会话可见的后台 SubAgent 任务。"""

    return CommandResult(message=format_subagent_tasks_list(ctx.agent))


@REGISTRY.command(
    name="task",
    description="查看或取消一个后台 SubAgent 任务。",
    usage="/task <task_id> [cancel]",
    # 含取消语义，按状态变更处理：排队执行，避免与后台回合并发改状态。
    type=CommandType.ACTION,
    arg_prompt="任务 ID",
)
def handle_task_command(ctx: CommandContext) -> CommandResult:
    """处理后台 SubAgent 任务的只读查询和单任务取消。"""

    argv = ctx.argv
    if not argv:
        return CommandResult(message="用法：/task <task_id>；/task cancel <task_id>。")
    if len(argv) == 1:
        return CommandResult(message=format_subagent_task(ctx.agent, argv[0]))
    if len(argv) != 2 or argv[0].casefold() != "cancel":
        return CommandResult(message="用法：/task <task_id>；/task cancel <task_id>。")

    task_id = argv[1]
    try:
        result = ctx.agent.cancel_subagent_task(task_id)
    except AgentError as exc:
        return CommandResult(message=f"子任务取消失败：{exc}")
    if not bool(result.get("ok")):
        return CommandResult(message="未找到当前会话的 SubAgent 任务。")

    task_status = str(result.get("status") or "cancelling")
    if task_status == "cancelled":
        return CommandResult(message=f"已取消后台子任务：{task_id}。")
    if task_status == "already_terminal":
        return CommandResult(message=f"子任务已结束，无需取消：{task_id}。")
    return CommandResult(message=f"已请求取消后台子任务：{task_id}。")


@REGISTRY.command(
    name="sessions",
    description="查看当前工作区最近会话。",
    usage="/sessions",
    type=CommandType.QUERY,
)
def handle_sessions_command(ctx: CommandContext) -> CommandResult:
    """列出当前工作区最近会话。"""

    return CommandResult(message=format_sessions_list(ctx.agent), refresh_context=True)


@REGISTRY.command(
    name="archives",
    description="查看已归档会话。",
    usage="/archives",
    type=CommandType.QUERY,
)
def handle_archives_command(ctx: CommandContext) -> CommandResult:
    """列出当前工作区已归档会话。"""

    return CommandResult(
        message=format_archived_sessions_list(ctx.agent), refresh_context=True
    )


@REGISTRY.command(
    name="archive",
    description="归档当前会话并开启新会话。",
    usage="/archive",
    type=CommandType.ACTION,
)
def handle_archive_command(ctx: CommandContext) -> CommandResult:
    """归档当前会话，并由 Agent 自动开启新会话。"""

    try:
        archived_state = ctx.agent.archive_current_session()
    except AgentError as exc:
        return CommandResult(message=f"会话归档失败：{exc}", refresh_context=True)
    return CommandResult(
        message=(
            f"已归档会话：{archived_state.session_id}\n"
            "已自动开启新会话。查看归档：/archives；恢复归档：/resume <session_id>。"
        ),
        refresh_context=True,
    )


@REGISTRY.command(
    name="history",
    description="查看或筛选提示历史。",
    usage="/history [关键词]",
    type=CommandType.QUERY,
    arg_prompt="关键词",
)
def handle_history_command(ctx: CommandContext) -> CommandResult:
    """展示当前工作区提示历史，可按关键词筛选（只展示，不注入模型）。"""

    return CommandResult(
        message=format_prompt_history(ctx.agent, query=ctx.args),
        refresh_context=True,
    )


@REGISTRY.command(
    name="undo",
    description="原子回退最近一轮对话与工作区中被 Git 记录的更改；冲突或不可逆操作时拒绝。",
    usage="/undo",
    type=CommandType.ACTION,
)
def handle_undo_command(ctx: CommandContext) -> CommandResult:
    """事务式回退最近一轮；成功后要求 UI 重放会话视图。"""

    try:
        ctx.agent.undo_last_turn()
    except AgentError as exc:
        return CommandResult(message=f"会话回退失败：{exc}", refresh_context=True)
    return CommandResult(
        message=(
            "已回退最近一轮（事务式）：会话转录、模型上下文与工作区中 Git 记录的更改已同步恢复。"
        ),
        refresh_context=True,
        # 已撤回的消息/工具卡需要从事件流重放中消失，不能只追加提示。
        replay_conversation=True,
    )


@REGISTRY.command(
    name="compact",
    description="使用结构化摘要模型压缩当前会话上下文。",
    usage="/compact",
    # 结构化摘要需要模型调用，交给交互端的工作线程执行。
    type=CommandType.BACKGROUND,
)
def handle_compact_command(ctx: CommandContext) -> CommandResult:
    """压缩当前会话上下文：默认使用结构化摘要模型。

    摘要模型不可用或校验失败时，``compact_conversation_model`` 内部会自动
    降级为本地确定性压缩，因此不再保留单独的 ``--model`` 开关。模型调用与
    事件写入推迟到 ``deferred``，避免阻塞交互端主线程；摘要正文只进会话事件
    与投影，不在命令输出里回显。
    """

    if ctx.args.strip():
        return CommandResult(message="参数错误：/compact。", refresh_context=True)

    def run_compact() -> CommandResult:
        try:
            ctx.agent.compact_conversation_model()
        except AgentError as exc:
            return CommandResult(message=f"模型会话压缩失败：{exc}", refresh_context=True)

        notice = getattr(ctx.agent, "_last_compaction_notice", "") or ""
        lead = f"{notice}\n" if notice else ""
        tail = "已压缩当前会话，完整转录仍保留，后续恢复将从摘要边界继续。"
        return CommandResult(message=lead + tail, refresh_context=True)

    return CommandResult(
        message="正在压缩当前会话上下文…",
        working_status="正在压缩上下文",
        deferred=run_compact,
    )


@REGISTRY.command(
    name="rename",
    description="重命名当前会话。",
    usage="/rename <会话标题>",
    type=CommandType.ACTION,
    arg_prompt="会话标题",
)
def handle_rename_command(ctx: CommandContext) -> CommandResult:
    """重命名当前会话；标题可为含空格的一整段文本。"""

    title = ctx.args.strip()
    if not title:
        return CommandResult(message="用法：/rename <会话标题>。", refresh_context=True)
    try:
        state = ctx.agent.rename_current_session(title)
    except AgentError as exc:
        return CommandResult(message=f"会话重命名失败：{exc}", refresh_context=True)
    return CommandResult(message=f"当前会话已重命名为：{state.title}", refresh_context=True)


@REGISTRY.command(
    name="resume",
    description="恢复指定会话 ID。",
    usage="/resume <session_id>",
    type=CommandType.ACTION,
    arg_prompt="会话 ID",
)
def handle_resume_command(ctx: CommandContext) -> CommandResult:
    """恢复指定会话；UI 依据会话 ID 变化重放历史消息。"""

    session_id = ctx.args.strip()
    if not session_id:
        return CommandResult(
            message="用法：/resume <session_id>。可先用 /sessions 查看最近会话。",
            refresh_context=True,
        )
    try:
        state = ctx.agent.resume_session(session_id)
    except AgentError as exc:
        return CommandResult(message=f"会话恢复失败：{exc}", refresh_context=True)
    return CommandResult(
        message=(
            f"已恢复会话：{state.session_id}\n"
            f"标题：{state.title or '未命名会话'}\n"
            f"已恢复 {len(state.messages)} 条上下文消息。"
        ),
        refresh_context=True,
    )


@REGISTRY.command(
    name="approval",
    description="查看当前工具审批模式。",
    usage="/approval",
    type=CommandType.QUERY,
)
def handle_approval_query_command(ctx: CommandContext) -> CommandResult:
    """查看当前工具审批模式；远程连接器额外提示不支持完全自动。"""

    label = approval_mode_label(ctx.agent.approval_mode)
    if ctx.is_remote:
        return CommandResult(
            message=(
                f"当前工具审批模式：{label}。\n"
                "可用切换：/approval:manual（手动确认）\n"
                "          /approval:review（自动审查，默认）\n"
                "远程不支持 /approval:auto（完全自动仅限本地 TUI）"
            )
        )
    return CommandResult(
        message=f"当前工具审批模式：{label}。", refresh_context=True
    )


def _apply_approval_mode(ctx: CommandContext, mode: str) -> CommandResult:
    """切换审批模式并持久化；远程入口按安全边界拒绝完全自动。"""

    label = approval_mode_label(mode)
    if ctx.is_remote and mode == APPROVAL_MODE_AUTO:
        # 完全自动仅限本地 TUI：远程连接器不得绕过工具审批。
        return CommandResult(
            message=(
                "❌ 远程不支持完全自动批准。完全自动仅限本地 TUI 配置；"
                f"当前仍为 {approval_mode_label(ctx.agent.approval_mode)}。"
            )
        )
    ctx.agent.set_approval_mode(mode)
    try:
        path = save_approval_mode(mode)
    except RuntimeConfigError as exc:
        return CommandResult(
            message=f"审批模式已临时切换为 {label}，但写入 config.toml 失败：{exc}",
            refresh_context=True,
        )
    return CommandResult(
        message=f"审批模式已切换为 {label}，并已同步到 {path}。",
        refresh_context=True,
    )


@REGISTRY.command(
    name="approval:manual",
    aliases=("auto-approve:off",),
    description="工具执行前逐次询问。",
    usage="/approval:manual",
    type=CommandType.ACTION,
)
def handle_approval_manual_command(ctx: CommandContext) -> CommandResult:
    """切换到手动确认模式。"""

    return _apply_approval_mode(ctx, APPROVAL_MODE_MANUAL)


@REGISTRY.command(
    name="approval:auto",
    aliases=("auto-approve:on",),
    description="自动批准工具执行。",
    usage="/approval:auto",
    type=CommandType.ACTION,
)
def handle_approval_auto_command(ctx: CommandContext) -> CommandResult:
    """切换到完全自动批准（仅限本地 TUI）。"""

    return _apply_approval_mode(ctx, APPROVAL_MODE_AUTO)


@REGISTRY.command(
    name="approval:review",
    aliases=("auto-review:on",),
    description="自动审查 bash/powershell 命令。",
    usage="/approval:review",
    type=CommandType.ACTION,
)
def handle_approval_review_command(ctx: CommandContext) -> CommandResult:
    """切换到自动审查模式。"""

    return _apply_approval_mode(ctx, APPROVAL_MODE_REVIEW)


_REVIEW_TASK_DESCRIPTION = "评审当前代码变更"
_REVIEW_DEFAULT_SCOPE = "工作区未提交改动（git diff）"


def _check_review_preconditions(workspace_root: Path, scope: str) -> str | None:
    """评审前置检查：返回错误消息（应阻止评审），可评审时返回 None。

    覆盖两类“无意义评审”：工作区不是 git 仓库、工作区没有可评审的改动
    （无提交 + 无未提交改动）。不做精确范围校验——非法范围由子 Agent 的
    git 工具在收集 diff 时给出明确错误。
    """

    root = Path(workspace_root).resolve()

    def run_git(args: list[str]) -> subprocess.CompletedProcess | None:
        try:
            return subprocess.run(
                ["git", "-c", "color.ui=never", "--no-pager", *args],
                cwd=str(root),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=30,
            )
        except FileNotFoundError:
            return None
        except subprocess.TimeoutExpired:
            return None

    is_worktree = run_git(["rev-parse", "--is-inside-work-tree"])
    if is_worktree is None:
        return "未找到 git 可执行文件，请确认 Git 已安装。"
    if is_worktree.returncode != 0 or is_worktree.stdout.decode("utf-8", "replace").strip() != "true":
        return "当前工作区不是 git 仓库，无法评审。"

    if scope:
        # 指定范围评审：至少需要仓库存在提交。
        head = run_git(["rev-parse", "--verify", "HEAD"])
        if head is None:
            return "未找到 git 可执行文件，请确认 Git 已安装。"
        if head.returncode != 0:
            return "当前仓库还没有任何提交，无法按指定范围评审。"
        return None

    # 默认范围（工作区未提交改动）：存在未提交改动或未跟踪文件才值得评审。
    status = run_git(["status", "--porcelain"])
    if status is None:
        return "未找到 git 可执行文件，请确认 Git 已安装。"
    if status.returncode != 0:
        return "无法读取 git 工作区状态，无法评审。"
    if not status.stdout.strip():
        return "工作区没有未提交改动，无需评审。"
    return None


def _build_review_task_prompt(scope: str) -> str:
    """构造评审子 Agent 的任务 prompt：只描述评审范围，评审标准由定义注入。"""

    if scope:
        scope_text = (
            f"评审范围由 /review 参数指定：`{scope}`。"
            "请先使用 git 工具确认当前分支与提交历史，再用 `git diff <范围>` 收集改动。"
        )
    else:
        scope_text = (
            f"评审范围：{_REVIEW_DEFAULT_SCOPE}。"
            "请先用 git status 查看变更文件，再用 `git diff`（含 `git diff --cached`）收集改动。"
        )
    return (
        f"请评审当前工作区的代码变更。\n{scope_text}\n\n"
        "执行步骤：\n"
        "1. 用 git 工具收集 diff（status/diff/log/show 等只读子命令）；\n"
        "2. 必要时用 read / grep 阅读受影响的文件与上下文；\n"
        "3. 严格按系统提示中的评审标准独立评审。\n\n"
        "输出要求：只输出系统提示中 OUTPUT FORMAT 规定的 JSON 审查结果本身，"
        "不要输出 Markdown 代码块、XML 或任何额外解释。"
    )


def _extract_review_json(text: str) -> dict[str, Any] | None:
    """从子 Agent 输出中提取评审 JSON 对象；带围栏/前后缀时宽容解析。"""

    stripped = text.strip()
    if not stripped:
        return None
    candidates: list[str] = []
    # 1) 整体直接解析
    candidates.append(stripped)
    # 2) 去掉 ```json ... ``` 围栏
    fenced = re.search(r"```(?:json)?\s*(\{.*\})\s*```", stripped, re.DOTALL)
    if fenced:
        candidates.append(fenced.group(1))
    # 3) 取第一个 { 到最后一个 } 的子串
    first = stripped.find("{")
    last = stripped.rfind("}")
    if first != -1 and last > first:
        candidates.append(stripped[first:last + 1])
    for candidate in candidates:
        try:
            parsed = json.loads(candidate)
        except (TypeError, ValueError):
            continue
        if isinstance(parsed, dict):
            return parsed
    return None


def _format_priority_tag(priority: Any) -> str:
    """把 JSON 中的 priority 数值映射为 [P0]-[P3] 标签。"""

    if priority is None:
        return ""
    try:
        value = int(priority)
    except (TypeError, ValueError):
        return ""
    if value < 0 or value > 3:
        return ""
    return f"[P{value}] "


def format_review_report(review_text: str) -> str:
    """把评审子 Agent 返回的 JSON 渲染为可读报告；解析失败时原样展示。"""

    data = _extract_review_json(review_text)
    if data is None:
        return review_text.strip() or "（评审子 Agent 未返回内容。）"

    correctness = str(data.get("overall_correctness") or "")
    explanation = str(data.get("overall_explanation") or "").strip()
    confidence = data.get("overall_confidence_score")

    correctness_norm = correctness.casefold()
    if "incorrect" in correctness_norm:
        verdict = "patch is incorrect ❌"
    elif "correct" in correctness_norm:
        verdict = "patch is correct ✅"
    else:
        verdict = "无法判定 ⚠️（overall_correctness 缺失或值无效）"
    confidence_text = (
        f"（置信度 {confidence:g}）"
        if isinstance(confidence, (int, float))
        else ""
    )
    lines = ["## 代码评审结果", "", f"**总体结论**：{verdict}{confidence_text}"]
    if explanation:
        lines.extend(["", explanation])

    findings = data.get("findings")
    if not isinstance(findings, list):
        lines.extend(["", "⚠️ 评审结果缺少有效的 findings 列表，无法展示问题明细。"])
        return "\n".join(lines)
    if not findings:
        lines.extend(["", "未发现问题。"])
        return "\n".join(lines)

    lines.extend(["", f"**发现问题 {len(findings)} 项**：", ""])
    for index, finding in enumerate(findings, start=1):
        if not isinstance(finding, dict):
            continue
        title = str(finding.get("title") or f"问题 {index}")
        tag = _format_priority_tag(finding.get("priority"))
        finding_confidence = finding.get("confidence_score")
        confidence_suffix = (
            f"（置信度 {finding_confidence:g}）"
            if isinstance(finding_confidence, (int, float))
            else ""
        )
        lines.append(f"{index}. **{tag}{title}**{confidence_suffix}")
        location = finding.get("code_location")
        if isinstance(location, dict):
            path = str(location.get("absolute_file_path") or "")
            line_range = location.get("line_range")
            start = None
            end = None
            if isinstance(line_range, dict):
                start = line_range.get("start")
                end = line_range.get("end")
            if path:
                if isinstance(start, int) and isinstance(end, int):
                    lines.append(f"   - 位置：`{path}` 行 {start}-{end}")
                else:
                    lines.append(f"   - 位置：`{path}`")
        body = str(finding.get("body") or "").strip()
        if body:
            indented = "\n".join(f"  {line}" for line in body.splitlines())
            lines.extend(["", indented])
        lines.append("")
    return "\n".join(lines).rstrip()


@REGISTRY.command(
    name="review",
    description=(
        "派生评审子 Agent（完整 git 权限 + 自动批准）收集 diff 并按结构化 JSON "
        "输出审查结果；可选 git 范围参数（如 /review HEAD~3）。"
    ),
    usage="/review [git 范围]",
    # 含子进程 git 预检 + 模型循环，全部放在延迟部分，避免阻塞交互线程。
    type=CommandType.BACKGROUND,
    arg_prompt="git 范围",
)
def handle_review_command(ctx: CommandContext) -> CommandResult:
    """把评审范围转发给 review 子 Agent，并把 JSON 结果渲染为可读报告。

    主线程只解析参数并立即返回：git 预检、子 Agent 模型循环与报告注入都推迟到
    ``deferred`` 中，由交互端的工作线程执行（``ctx.on_subagent_event`` 透传给
    :meth:`run_subagent_task` 供 UI 显示审查进度）。
    """

    scope = ctx.args.strip()

    def run_review() -> CommandResult:
        workspace_root = getattr(ctx.agent, "workspace_root", None)
        if workspace_root is not None:
            precheck_error = _check_review_preconditions(workspace_root, scope)
            if precheck_error:
                return CommandResult(message=precheck_error)

        try:
            report = ctx.agent.run_subagent_task(
                agent_type="review",
                description=_REVIEW_TASK_DESCRIPTION,
                prompt=_build_review_task_prompt(scope),
                on_subagent_event=ctx.on_subagent_event,
            )
        except AgentError as exc:
            return CommandResult(message=f"评审失败：{exc}")
        rendered = format_review_report(report)
        # 报告进父模型上下文：下一轮模型请求能看到报告并继续处理（如修复、提交）。
        remember = getattr(ctx.agent, "remember_review_report", None)
        if callable(remember):
            try:
                remember(rendered)
            except Exception:  # noqa: BLE001 - 注入失败不影响报告展示
                pass
        return CommandResult(message=rendered)

    return CommandResult(
        message=None,
        working_status="正在评审",
        stream_subagent_conversation=True,
        deferred=run_review,
    )


@REGISTRY.command(
    name="reasoning",
    description="查看或切换推理强度。",
    usage="/reasoning [none|low|medium|high|xhigh|max]",
    # 带参数时写 config.toml，按状态变更处理：排队执行。
    type=CommandType.ACTION,
    arg_prompt="推理强度",
)
def handle_reasoning_command(ctx: CommandContext) -> CommandResult:
    """查看或切换推理强度（切换会同步写入 config.toml）。"""

    effort = ctx.args.strip()
    if not effort:
        current = ctx.agent.reasoning_effort or "默认"
        return CommandResult(
            message=(
                f"当前推理强度：{current}。\n"
                "可选：/reasoning none|low|medium|high|xhigh|max"
            ),
            refresh_context=True,
        )

    try:
        normalized_effort = ctx.agent.set_reasoning_effort(effort)
    except AgentError as exc:
        return CommandResult(message=f"推理强度切换失败：{exc}", refresh_context=True)
    try:
        path = save_reasoning_effort(normalized_effort)
    except LLMError as exc:
        return CommandResult(
            message=f"推理强度已临时切换为 {normalized_effort}，但写入 config.toml 失败：{exc}",
            refresh_context=True,
        )
    env_message = _reasoning_env_override_message()
    return CommandResult(
        message=f"推理强度已切换为 {normalized_effort}，并已同步到 {path}{env_message}",
        refresh_context=True,
    )


def _reasoning_env_override_message() -> str:
    if not os.getenv("REASONING_EFFORT", "").strip():
        return ""
    return " 注意：当前存在 REASONING_EFFORT 环境变量，重启后会优先使用环境变量。"


@REGISTRY.command(
    name="model",
    description="查看或切换当前模型（从下一次请求开始生效）。",
    usage="/model <key|profile/model_id|model_id>",
    type=CommandType.ACTION,
    arg_prompt="模型选择",
)
def handle_model_command(ctx: CommandContext) -> CommandResult:
    """查看或切换当前模型。

    支持 /model 查看当前模型，/model <selection> 切换模型（selection 可为
    models.toml key/alias、profile/model_id 或裸 model_id）。切换即时生效：
    运行中的回合继续用旧模型，下一次请求自动使用新模型。
    """

    selection = ctx.args.strip()
    if not selection:
        current = getattr(ctx.agent, "current_model", "") or ctx.agent.config.llm.model
        return CommandResult(
            message=f"当前模型：{current}\n用法：/model <key|profile/model_id|model_id>",
            refresh_context=True,
        )

    def persist() -> None:
        # 与 set_model 的 apply_model_selection 解析保持一致：优先持久化
        # models.toml 引用，否则写回 llm.model。
        try:
            store = _load_model_store()
            record = store.resolve_alias(selection)
        except Exception:
            record = None
        if record is not None:
            from ..config.models.llm import ActiveModelRef

            save_active_model_ref(
                ActiveModelRef(source="custom", key=record.key, model_id=record.model_id)
            )
        else:
            save_llm_model(selection)

    try:
        ctx.agent.set_model(selection, persist=persist)
    except AgentError as exc:
        return CommandResult(message=f"模型切换失败：{exc}", refresh_context=True)
    return CommandResult(
        message=f"模型已切换为：{ctx.agent.current_model}（从下一次请求开始生效）",
        refresh_context=True,
    )


def _load_model_store():
    from ..config.models.model_store import load_model_store

    return load_model_store()



@REGISTRY.command(
    name="advisor",
    description="查看或设置顾问策略模型（advisor）；/advisor off 关闭。",
    usage="/advisor [model_key] [effort]",
    type=CommandType.ACTION,
    arg_prompt="模型 key",
)
def handle_advisor_command(ctx: CommandContext) -> CommandResult:
    """查看或设置顾问策略模型。

    支持：
    - ``/advisor``：查看当前顾问模型/effort，并列出可选模型与用法。
    - ``/advisor <model_key> [effort]``：设置顾问模型（models.toml key/alias、
      profile/model_id 或裸 model_id）与可选推理档位（缺省 high）。
    - ``/advisor off``：清除顾问选择（关闭功能，工具即时剥离）。
    保存后重建工具表，使 advisor 工具即时出现/消失。
    """

    agent = ctx.agent
    current_config = load_advisor_config()
    argv = ctx.argv
    if not argv:
        return CommandResult(
            message=_advisor_status_message(agent, current_config), refresh_context=True
        )

    action = argv[0].strip()
    if action.casefold() in {"off", "none", "clear", "no"}:
        return CommandResult(message=_advisor_clear(agent), refresh_context=True)
    if action.casefold() in {"help", "-h", "--help"}:
        return CommandResult(
            message=_advisor_help_message(current_config), refresh_context=True
        )

    model_key = action
    # effort 取 model_key 之后的整段剩余文本，与旧行为的 split(None, 2) 一致。
    _, _, remainder = ctx.args.strip().partition(" ")
    effort = remainder.strip() or DEFAULT_ADVISOR_EFFORT
    try:
        normalized_effort = _normalize_advisor_effort(effort)
    except AdvisorConfigError as exc:
        return CommandResult(message=f"顾问设置失败：{exc}", refresh_context=True)

    # 校验模型选择可用（apply_model_selection 解析失败即报错），不写无效引用。
    try:
        from ..config.models.llm_multi import apply_model_selection

        apply_model_selection(agent.config.llm, model_key)
    except Exception as exc:  # noqa: BLE001 - LLMError/ModelStoreError 统一转提示
        return CommandResult(message=f"顾问模型无法解析：{exc}", refresh_context=True)

    next_config = AdvisorConfig(
        enabled=True,
        model_key=model_key,
        effort=normalized_effort,
        disabled_for_models=current_config.disabled_for_models,
    )
    try:
        path = save_advisor_config(next_config)
    except AdvisorConfigError as exc:
        return CommandResult(message=f"顾问设置写入失败：{exc}", refresh_context=True)
    # 内存配置同步 + 重建工具表，让 advisor 工具即时出现。
    try:
        agent.config.advisor = next_config
        agent._tools = agent._build_tools()
    except Exception as exc:  # noqa: BLE001 - 工具重建失败不掩盖已保存配置
        return CommandResult(
            message=(
                f"顾问已保存为 {model_key}（effort={normalized_effort}），"
                f"但工具表刷新失败：{exc}"
            ),
            refresh_context=True,
        )
    return CommandResult(
        message=f"顾问已启用：{model_key}（effort={normalized_effort}），已写入 {path}。",
        refresh_context=True,
    )


def _advisor_clear(agent: LocalToolAgent) -> str:
    try:
        path = clear_advisor_config()
    except AdvisorConfigError as exc:
        return f"清除顾问失败：{exc}"
    next_config = AdvisorConfig(enabled=False)
    try:
        agent.config.advisor = next_config
        agent._tools = agent._build_tools()
    except Exception as exc:  # noqa: BLE001 - 工具重建失败不掩盖清除结果
        return f"顾问已清除（{path}），但工具表刷新失败：{exc}"
    return f"顾问已关闭并清除选择（{path}）。advisor 工具已从工具表剥离。"


def _advisor_status_message(agent: LocalToolAgent, config: AdvisorConfig) -> str:
    if config.active:
        return (
            f"当前顾问：{config.model_key}（effort={config.display_effort}）\n"
            f"用法：/advisor <model_key> [effort]，/advisor off 关闭。"
        )
    return (
        "顾问未启用。\n"
        f"用法：/advisor <model_key> [effort]，effort 可选 "
        f"{'/'.join(ADVISOR_EFFORT_OPTIONS)}（缺省 {DEFAULT_ADVISOR_EFFORT}）。\n"
        "可用模型：\n" + _advisor_model_candidates(agent)
    )


def _advisor_help_message(config: AdvisorConfig) -> str:
    state = (
        f"当前顾问：{config.model_key}（effort={config.display_effort}）"
        if config.active
        else "顾问未启用"
    )
    return (
        f"{state}\n"
        "用法：\n"
        "  /advisor                   查看状态与可用模型\n"
        "  /advisor <model_key> [effort]  设置顾问模型与推理档位\n"
        "  /advisor off               关闭并清除顾问\n"
        f"effort 可选：{'/'.join(ADVISOR_EFFORT_OPTIONS)}（缺省 {DEFAULT_ADVISOR_EFFORT}）"
    )


def _advisor_model_candidates(agent: LocalToolAgent) -> str:
    """列出可作顾问的候选模型：models.toml enabled 模型 + 当前主模型。"""

    candidates: list[str] = []
    try:
        store = _load_model_store()
        for record in store.models:
            if not record.enabled:
                continue
            label = record.key
            if record.aliases:
                label += f"（别名：{'/'.join(record.aliases)}）"
            candidates.append(f"  - {label}")
    except Exception:  # noqa: BLE001 - 模型目录不可用时回退到仅主模型
        pass
    current = getattr(agent, "current_model", "") or agent.config.llm.model
    if current:
        candidates.append(f"  - {current}（当前主模型）")
    return "\n".join(candidates) if candidates else "  （无可用模型，请先配置 models.toml）"


def _normalize_advisor_effort(effort: str) -> str:
    normalized = effort.strip().casefold()
    if normalized in {"disabled", "off", "none"}:
        normalized = "none"
    if normalized not in ADVISOR_EFFORT_OPTIONS:
        allowed = "/".join(ADVISOR_EFFORT_OPTIONS)
        raise AdvisorConfigError(f"effort 仅支持 {allowed}，当前值：{effort}。")
    return normalized


#: 主 Agent 模式切换命令：命令名（不含前导斜杠）→ ``activate_mode`` 的模式标识。
_MODE_COMMANDS = {
    "plan": "plan",
}

_MODE_LABELS = {"plan": "计划模式"}


@REGISTRY.command(
    name="plan",
    description="启用主 Agent 计划模式，后续请求追加 templates/plan.md。",
    usage="/plan",
    type=CommandType.ACTION,
)
def handle_mode_command(ctx: CommandContext) -> CommandResult:
    """启用主 Agent 模式（当前仅 plan）。"""

    name = ctx.command.name if ctx.command is not None else "plan"
    mode = _MODE_COMMANDS.get(name)
    if mode is None:
        return CommandResult(message=f"未知模式：{name}")
    try:
        activated = ctx.agent.activate_mode(mode)
    except AgentError as exc:
        return CommandResult(message=f"模式启用失败：{exc}", refresh_context=True)
    label = _MODE_LABELS.get(activated, activated)
    return CommandResult(
        message=f"已启用{label}。后续任务将遵循该模式提示词。",
        refresh_context=True,
    )


@REGISTRY.command(
    name="skills",
    description="查看当前已加载的 Skill。",
    usage="/skills",
    type=CommandType.QUERY,
)
def handle_skills_command(ctx: CommandContext) -> CommandResult:
    """列出当前已加载的 Skill。"""

    return CommandResult(message=format_skills_list(ctx.agent))


@REGISTRY.command(
    name="memory:clean",
    description="清理过期长期记忆。",
    usage="/memory:clean",
    type=CommandType.ACTION,
)
def handle_memory_clean_command(ctx: CommandContext) -> CommandResult:
    """清理过期长期记忆。"""

    return CommandResult(message=format_memory_clean_result(ctx.agent))


@REGISTRY.command(
    name="mcp",
    description="查看 MCP 开关、服务和工具状态。",
    usage="/mcp",
    # 可能连接 MCP Server，交给交互端的工作线程执行。
    type=CommandType.BACKGROUND,
)
def handle_mcp_command(ctx: CommandContext) -> CommandResult:
    """读取 MCP 状态；真正的连接推迟到 ``deferred``。"""

    return CommandResult(
        message="正在读取 MCP 状态",
        deferred=lambda: CommandResult(message=format_mcp_status(ctx.agent)),
    )


@REGISTRY.command(
    name="plugins",
    description="查看 Hook 插件加载与 Worker 状态（只读）。",
    usage="/plugins",
    type=CommandType.QUERY,
)
def handle_plugins_command(ctx: CommandContext) -> CommandResult:
    """读取插件子系统只读状态。"""

    return CommandResult(message=format_plugins_status(ctx.agent))


@REGISTRY.command(
    name="new",
    description="开启一个空白会话。",
    usage="/new",
    type=CommandType.ACTION,
)
def handle_new_command(ctx: CommandContext) -> CommandResult:
    """清空当前对话并开启新会话。"""

    old_session_id = ctx.agent.current_session_id
    ctx.agent.reset_conversation()
    if old_session_id:
        message = f"已新开会话，旧会话：{old_session_id}"
    else:
        message = "已新开会话。"
    return CommandResult(
        message=message,
        refresh_context=True,
        clear_conversation=True,
    )


@REGISTRY.command(
    name="quit",
    aliases=("退出", "结束", "再见"),
    description="退出当前 TUI，不关闭宿主窗口。",
    usage="/quit",
    type=CommandType.UI,
)
def handle_quit_command(ctx: CommandContext) -> CommandResult:
    """请求交互端退出当前 TUI；远程入口无退出语义，只给出提示。"""

    if ctx.is_remote:
        return CommandResult(
            message="远程连接不支持 /quit；如需停止请关闭对应 Bot 会话。"
        )
    return CommandResult(exit_requested=True)


@REGISTRY.command(
    name="settings",
    description="打开中文设置面板，修改运行时开关并立即保存。",
    usage="/settings [--chat]",
    type=CommandType.UI,
    arg_prompt="--chat",
    parameters=(("--chat", "无上下文配置对话"),),
)
def handle_settings_command(ctx: CommandContext) -> CommandResult:
    """请求交互端打开设置面板或无上下文配置对话。"""

    if ctx.is_remote:
        return CommandResult(
            message="远程连接不支持 /settings；请在本地 TUI 中打开设置面板。"
        )
    if ctx.args.strip().casefold() == "--chat":
        return CommandResult(
            message="进入配置对话",
            open_config_chat=True,
            clear_conversation=True,
            refresh_context=True,
        )
    if ctx.args.strip():
        return CommandResult(error="/settings 只支持可选参数 --chat。")
    return CommandResult(
        message="打开设置面板", open_settings=True, refresh_context=True
    )


@REGISTRY.command(
    name="workspace",
    description="切换当前 Agent 的工作区目录。",
    usage="/workspace [路径]",
    # 切换会重建 Session/MCP/Monitor 与临时目录，必须交给工作线程。
    type=CommandType.BACKGROUND,
    arg_prompt="新工作区路径",
)
def handle_workspace_command(ctx: CommandContext) -> CommandResult:
    """查询或切换工作区。

    无参数时是只读查询，直接返回；带参数时把重建与持久化推迟到 ``deferred``，
    避免文件/进程操作阻塞交互线程。
    """

    workspace = ctx.args.strip()
    if not workspace:
        return CommandResult(
            message=(
                f"当前工作区：{ctx.agent.workspace_root}\n"
                "用法：/workspace <新工作区路径>"
            )
        )

    def switch_workspace() -> CommandResult:
        ctx.agent.switch_workspace(workspace)
        # 跨进程同步：把新工作区写回 config.toml，远程入口（Telegram Bot）
        # 在任务开始前重读并跟随；失败不阻断切换。
        try:
            from ..config.core.workspace import save_workspace_root

            save_workspace_root(workspace)
        except Exception as exc:  # noqa: BLE001 - 持久化失败不阻断切换
            _LOGGER.warning("工作区持久化到 config.toml 失败：%s", exc)
        return CommandResult(message=f"已切换工作区：{ctx.agent.workspace_root}")

    return CommandResult(
        message="正在切换工作区",
        refresh_context=True,
        workspace_switch_requested=True,
        deferred=switch_workspace,
    )


def build_slash_commands(agent: LocalToolAgent) -> list[str]:
    """构建所有可用的斜杠命令列表（注册表声明 + 运行时 Skill）。

    内置命令直接来自 :data:`REGISTRY`，不再手写维护；只有运行时发现的 Skill
    需在此按 agent 动态拼接。
    """

    commands = REGISTRY.display_names()
    sm = agent.skill_manager
    if sm is not None:
        for meta in sm.list_all():
            commands.append(f"/skill:{meta.name}")
    return commands


def build_slash_command_options(agent: LocalToolAgent) -> list[dict[str, Any]]:
    """构建可供交互客户端使用的斜杠命令元数据。

    TUI 使用命令字符串做 Tab 补全；API 客户端可使用说明、显示标题和搜索文本；
    ``parameters`` 是命令声明的可选参数 ``(参数, 说明)``，供输入框提示 ``--chat``。
    内置命令的说明与是否接受参数都由注册表声明派生，只有运行时 Skill 在此拼接，
    避免不同交互入口的命令不一致。
    """

    options = REGISTRY.options()
    sm = agent.skill_manager
    if sm is not None:
        for meta in sm.list_all():
            command = f"/skill:{meta.name}"
            plain_name = meta.name.replace("-", " ")
            options.append(
                {
                    "command": command,
                    "insert": f"{command} ",
                    "title": meta.name,
                    "description": meta.description,
                    "category": "Skill",
                    "search": f"{command} /{meta.name} {plain_name} {meta.description}",
                }
            )
    return options
