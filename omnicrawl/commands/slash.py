from __future__ import annotations

import json
import os
from typing import Any

from ..config.approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    approval_mode_label,
    save_approval_mode,
)
from ..agent import AgentError, LocalToolAgent
from ..agent.tools import public_tool_arguments
from ..config.llm import LLMError, save_reasoning_effort
from ..config.llm import ActiveModelRef, save_active_model_ref
from ..config.model_catalog import (
    CatalogModel,
    ModelCatalogError,
    build_catalog,
    clear_discovery_cache,
    detect_model_options,
    ensure_current_model_option,
    format_model_options,
    model_env_override_active,
    save_llm_model,
)
from ..config.runtime import RuntimeConfigError


# ── 工具确认展示 ──────────────────────────────────────────────

_TOOL_HUMAN_DESCRIPTIONS: dict[str, str] = {
    "list_files": "列出目录内容",
    "read_file": "读取文件内容",
    "search_text": "在文件中搜索文本",
    "replace_text": "替换文件中的文本",
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

    if tool_name == "memory_write":
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
    origin = arguments.get("_subagent_origin")

    lines = [f"Agent 想要{description}。"]
    if isinstance(origin, dict):
        agent_label = _truncate_for_display(str(origin.get("agent_label", "subagent")), 80)
        task_id = _truncate_for_display(str(origin.get("task_id", "")), 80)
        task_description = _truncate_for_display(
            str(origin.get("description", "")),
            120,
        )
        source = f"来源：子任务 {agent_label}"
        if task_id:
            source += f"（{task_id}）"
        if task_description:
            source += f"，任务：{task_description}"
        lines.append(source)
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


def handle_subagent_task_command(agent: LocalToolAgent, command: str) -> str | None:
    """处理后台 SubAgent 任务的只读查询和单任务取消命令。"""

    text = command.strip()
    normalized = text.casefold()
    if normalized == "/tasks":
        return format_subagent_tasks_list(agent)
    if normalized != "/task" and not normalized.startswith("/task "):
        return None

    parts = text.split()
    if len(parts) == 1:
        return "用法：/task <task_id>；/task cancel <task_id>。"
    if len(parts) == 2 and parts[1].casefold() != "cancel":
        return format_subagent_task(agent, parts[1])
    if len(parts) != 3 or parts[1].casefold() != "cancel":
        return "用法：/task <task_id>；/task cancel <task_id>。"

    task_id = parts[2]
    try:
        result = agent.cancel_subagent_task(task_id)
    except AgentError as exc:
        return f"子任务取消失败：{exc}"
    if not bool(result.get("ok")):
        return "未找到当前会话的 SubAgent 任务。"

    task_status = str(result.get("status") or "cancelling")
    if task_status == "cancelled":
        return f"已取消后台子任务：{task_id}。"
    if task_status == "already_terminal":
        return f"子任务已结束，无需取消：{task_id}。"
    return f"已请求取消后台子任务：{task_id}。"


def handle_session_command(agent: LocalToolAgent, command: str) -> str | None:
    """处理会话查看与恢复命令；返回 None 表示不是会话命令。"""

    text = command.strip()
    normalized = text.lower()
    if normalized == "/sessions":
        return format_sessions_list(agent)
    if normalized == "/archives":
        return format_archived_sessions_list(agent)
    if normalized == "/archive":
        try:
            archived_state = agent.archive_current_session()
        except AgentError as exc:
            return f"会话归档失败：{exc}"
        return (
            f"已归档会话：{archived_state.session_id}\n"
            "已自动开启新会话。查看归档：/archives；恢复归档：/resume <session_id>。"
        )
    if normalized == "/history" or normalized.startswith("/history "):
        parts = text.split(None, 1)
        query = parts[1].strip() if len(parts) > 1 else ""
        return format_prompt_history(agent, query=query)
    if normalized == "/undo":
        try:
            agent.undo_last_turn()
        except AgentError as exc:
            return f"会话回退失败：{exc}"
        return (
            "已回退最近一轮对话，持久化转录与模型上下文已同步更新。\n"
            "注意：该轮已经执行的文件修改、命令或其他外部副作用不会自动撤销。"
        )
    if normalized.startswith("/undo "):
        return "用法：/undo。"
    if normalized == "/compact":
        try:
            summary = agent.compact_conversation()
        except AgentError as exc:
            return f"会话压缩失败：{exc}"
        return f"已压缩当前会话，后续恢复将从摘要边界继续。\n{summary}"
    if normalized == "/compact --model":
        try:
            summary = agent.compact_conversation_model()
        except AgentError as exc:
            return f"模型会话压缩失败：{exc}"
        return f"已使用结构化摘要模型压缩当前会话，完整转录仍保留。\n{summary}"
    if normalized.startswith("/compact "):
        return "用法：/compact 或 /compact --model。"
    if normalized == "/rename" or normalized.startswith("/rename "):
        parts = text.split(None, 1)
        if len(parts) == 1 or not parts[1].strip():
            return "用法：/rename <会话标题>。"
        try:
            state = agent.rename_current_session(parts[1].strip())
        except AgentError as exc:
            return f"会话重命名失败：{exc}"
        return f"当前会话已重命名为：{state.title}"
    if normalized != "/resume" and not normalized.startswith("/resume "):
        return None

    parts = text.split(None, 1)
    if len(parts) == 1 or not parts[1].strip():
        return "用法：/resume <session_id>。可先用 /sessions 查看最近会话。"

    session_id = parts[1].strip()
    try:
        state = agent.resume_session(session_id)
    except AgentError as exc:
        return f"会话恢复失败：{exc}"
    return (
        f"已恢复会话：{state.session_id}\n"
        f"标题：{state.title or '未命名会话'}\n"
        f"已恢复 {len(state.messages)} 条上下文消息。"
    )


def handle_approval_command(agent: LocalToolAgent, command: str) -> str | None:
    """处理审批模式斜杠命令；返回 None 表示不是审批命令。"""

    normalized = command.strip().lower()
    mode_by_command = {
        "/approval:manual": APPROVAL_MODE_MANUAL,
        "/approval:auto": APPROVAL_MODE_AUTO,
        "/approval:review": APPROVAL_MODE_REVIEW,
        "/auto-approve:off": APPROVAL_MODE_MANUAL,
        "/auto-approve:on": APPROVAL_MODE_AUTO,
        "/auto-review:on": APPROVAL_MODE_REVIEW,
    }
    if normalized == "/approval":
        return f"当前工具审批模式：{approval_mode_label(agent.approval_mode)}。"
    if normalized not in mode_by_command:
        return None

    mode = mode_by_command[normalized]
    agent.set_approval_mode(mode)
    try:
        path = save_approval_mode(mode)
    except RuntimeConfigError as exc:
        return f"审批模式已临时切换为 {approval_mode_label(mode)}，但写入 config.yaml 失败：{exc}"
    return f"审批模式已切换为 {approval_mode_label(mode)}，并已同步到 {path}。"


def handle_model_command(agent: LocalToolAgent, command: str) -> str | None:
    """处理模型查看与切换命令；返回 None 表示不是模型命令。

    语义：
    - `/model` / `/models`：列出双列目录（非交互环境）
    - `/model --refresh`：刷新发现缓存后列出
    - `/model <key|alias|model_id|profile/model_id>`：直接切换
    """

    text = command.strip()
    normalized = text.lower()
    if normalized == "/models":
        text = "/model"
        normalized = text
    if normalized != "/model" and not normalized.startswith("/model "):
        return None

    parts = text.split(None, 1)
    if len(parts) == 1:
        return _format_catalog_models(agent, refresh=False)

    argument = parts[1].strip()
    if not argument:
        return (
            "用法：/model 查看模型列表；"
            "/model --refresh 刷新后查看；"
            "/model <key|alias|model_id|profile/model_id> 切换。"
        )

    if argument.lower() in {"--refresh", "-r", "refresh"}:
        return _format_catalog_models(agent, refresh=True)

    return _switch_model_selection(agent, argument)


def handle_reasoning_command(agent: LocalToolAgent, command: str) -> str | None:
    """处理推理强度查看与切换命令；返回 None 表示不是推理强度命令。"""

    text = command.strip()
    normalized = text.lower()
    if normalized != "/reasoning" and not normalized.startswith("/reasoning "):
        return None

    parts = text.split(None, 1)
    if len(parts) == 1:
        current = agent.reasoning_effort or "默认"
        return (
            f"当前推理强度：{current}。\n"
            "可选：/reasoning none|low|medium|high|xhigh|max"
        )

    effort = parts[1].strip()
    if not effort:
        return "用法：/reasoning none|low|medium|high|xhigh|max"

    try:
        normalized_effort = agent.set_reasoning_effort(effort)
    except AgentError as exc:
        return f"推理强度切换失败：{exc}"
    try:
        path = save_reasoning_effort(normalized_effort)
    except LLMError as exc:
        return f"推理强度已临时切换为 {normalized_effort}，但写入 config.yaml 失败：{exc}"
    env_message = _reasoning_env_override_message()
    return f"推理强度已切换为 {normalized_effort}，并已同步到 {path}{env_message}"


def _format_catalog_models(agent: LocalToolAgent, *, refresh: bool) -> str:
    """文本模式展示双列目录；发现失败时回退到旧 /models 列表。"""

    current_model = agent.current_model
    if refresh:
        clear_discovery_cache()
    try:
        catalog = build_catalog(config=agent.config.llm, refresh=refresh)
    except ModelCatalogError as exc:
        return _format_detected_models_fallback(agent, prefix=f"双列目录失败：{exc}\n")

    custom_items: list[CatalogModel] = list(catalog.get("custom") or [])
    detected_items: list[CatalogModel] = list(catalog.get("detected") or [])
    diagnostics = list(catalog.get("diagnostics") or [])

    if not custom_items and not detected_items:
        fallback = _format_detected_models_fallback(agent)
        if diagnostics:
            diag = "\n".join(
                f"! {item.get('profile')}: {item.get('message') or item.get('status')}"
                for item in diagnostics[:5]
            )
            return f"{fallback}\n\n诊断：\n{diag}"
        return fallback

    lines = [f"当前模型：{current_model}"]
    if custom_items:
        lines.append(f"自定义模型（{len(custom_items)}）：")
        for index, item in enumerate(custom_items[:20], start=1):
            marker = " *" if _catalog_item_matches_current(item, current_model) else ""
            lines.append(
                f"{index:>2}. {item.key} · {item.display_name or item.model_id}"
                f" [{item.protocol}]{marker}"
            )
        if len(custom_items) > 20:
            lines.append(f"... 还有 {len(custom_items) - 20} 个自定义模型未显示。")
    else:
        lines.append("自定义模型：空（可在 models.yaml 中添加）")

    if detected_items:
        lines.append(f"自动检测（{len(detected_items)}）：")
        for index, item in enumerate(detected_items[:20], start=1):
            marker = " *" if _catalog_item_matches_current(item, current_model) else ""
            badge = f" custom:{item.matched_custom_key}" if item.matched_custom_key else ""
            lines.append(
                f"{index:>2}. {item.profile_id}/{item.model_id} [{item.protocol}]{badge}{marker}"
            )
        if len(detected_items) > 20:
            lines.append(f"... 还有 {len(detected_items) - 20} 个检测模型未显示。")
    else:
        lines.append("自动检测：空或失败（自定义模型仍可切换）")

    if diagnostics:
        lines.append("诊断：")
        for item in diagnostics[:5]:
            lines.append(
                f"- {item.get('profile')}: {item.get('message') or item.get('status')}"
            )

    lines.extend(
        [
            "",
            "切换：/model <key|alias|model_id|profile/model_id>",
            "刷新：/model --refresh",
            "全屏 TUI 中 /model 会打开双列选择界面。",
        ]
    )
    return "\n".join(lines).rstrip()


def _format_detected_models_fallback(agent: LocalToolAgent, prefix: str = "") -> str:
    current_model = agent.current_model
    try:
        options = ensure_current_model_option(
            detect_model_options(agent.config.llm),
            current_model,
        )
    except ModelCatalogError as exc:
        return f"{prefix}当前模型：{current_model}\n模型列表检测失败：{exc}"

    if not options:
        return f"{prefix}当前模型：{current_model}\n模型列表为空。"

    lines = [
        f"{prefix}当前模型：{current_model}".rstrip(),
        f"从 {agent.config.llm.base_url.rstrip('/')}/models 检测到 {len(options)} 个模型：",
        format_model_options(options, current_model=current_model),
        "",
        "切换模型：/model <模型ID>",
    ]
    return "\n".join(lines).rstrip()


def _switch_model_selection(agent: LocalToolAgent, selection: str) -> str:
    """解析并切换 custom key / alias / detected ref / 旧 model id。"""

    selection = selection.strip()
    resolved = _resolve_model_selection(agent, selection)
    if resolved.get("error"):
        return str(resolved["error"])

    model_token = str(resolved["token"])
    save_message = ""

    def persist() -> None:
        nonlocal save_message
        save_message = _persist_resolved_selection(resolved, raise_on_error=True)

    try:
        try:
            agent.set_model(model_token, persist=persist)
        except TypeError as exc:
            # 兼容嵌入方/旧测试的单参数 set_model。
            # 必须先 persist 成功再切换内存，避免磁盘失败后 runtime 已变更。
            if "persist" not in str(exc):
                raise
            persist()
            agent.set_model(model_token)
    except (AgentError, ModelCatalogError, LLMError) as exc:
        return f"模型切换失败：{exc}"

    env_message = _model_env_override_message()
    suffix = "".join(part for part in (save_message, env_message) if part)
    display = str(resolved.get("display") or model_token)
    return f"当前模型已切换为 {display}{suffix}"


def _resolve_model_selection(agent: LocalToolAgent, selection: str) -> dict[str, Any]:
    """优先匹配双列目录，再回退到旧 base_url 校验。"""

    try:
        catalog = build_catalog(config=agent.config.llm, refresh=False)
    except ModelCatalogError:
        catalog = {"custom": [], "detected": []}

    custom_items: list[CatalogModel] = list(catalog.get("custom") or [])
    detected_items: list[CatalogModel] = list(catalog.get("detected") or [])

    lowered = selection.lower()
    for item in custom_items:
        aliases = {item.key.lower(), *(alias.lower() for alias in item.aliases)}
        if lowered in aliases or selection == item.model_id:
            return {
                "token": item.key,
                "display": item.key,
                "source": "custom",
                "item": item,
            }

    if "/" in selection:
        profile_id, model_id = selection.split("/", 1)
        profile_id = profile_id.strip()
        model_id = model_id.strip()
        for item in detected_items:
            if item.profile_id == profile_id and item.model_id == model_id:
                return {
                    "token": f"{profile_id}/{model_id}",
                    "display": f"{profile_id}/{model_id}",
                    "source": "detected",
                    "item": item,
                }
        # 目录未命中时仍允许 profile/model_id 手动切换。
        return {
            "token": f"{profile_id}/{model_id}",
            "display": f"{profile_id}/{model_id}",
            "source": "detected",
            "profile": profile_id,
            "model_id": model_id,
        }

    for item in detected_items:
        if item.model_id == selection:
            return {
                "token": f"{item.profile_id}/{item.model_id}" if item.profile_id else item.model_id,
                "display": item.model_id,
                "source": "detected",
                "item": item,
            }

    validation_message = _validate_model_id_against_base_url(agent, selection)
    if validation_message is not None:
        return {"error": validation_message}
    return {"token": selection, "display": selection, "source": "legacy"}


def _persist_resolved_selection(
    resolved: dict[str, Any],
    *,
    raise_on_error: bool = False,
) -> str:
    item = resolved.get("item")
    source = str(resolved.get("source") or "legacy")
    try:
        if source == "custom" and isinstance(item, CatalogModel):
            path = save_active_model_ref(
                ActiveModelRef(source="custom", key=item.key, model_id=item.model_id)
            )
            return f"，并已同步到 {path}。"
        if source == "detected":
            if isinstance(item, CatalogModel):
                path = save_active_model_ref(
                    ActiveModelRef(
                        source="detected",
                        profile=item.profile_id,
                        model_id=item.model_id,
                        protocol=item.protocol,
                    )
                )
                return f"，并已同步到 {path}。"
            path = save_llm_model(str(resolved["token"]))
            return f"，并已同步到 {path}。"
        path = save_llm_model(str(resolved["token"]))
        return f"，并已同步到 {path}。"
    except (ModelCatalogError, LLMError) as exc:
        if raise_on_error:
            raise
        return f"，但写入配置失败：{exc}。"


def _validate_model_id_against_base_url(agent: LocalToolAgent, model_id: str) -> str | None:
    try:
        options = detect_model_options(agent.config.llm)
    except ModelCatalogError:
        # 有些 OpenAI 兼容网关不开放 /models。此时仍允许手动切换，
        # 但下一次模型请求会由真实接口继续校验模型是否可用。
        return None

    if any(option.id == model_id for option in options):
        return None

    available = format_model_options(options, current_model=agent.current_model, limit=20)
    return (
        f"模型 {model_id} 不在当前 base_url 的 /models 返回列表中，未切换。\n"
        f"可用模型：\n{available}"
    )


def _catalog_item_matches_current(item: CatalogModel, current_model: str) -> bool:
    current = (current_model or "").strip()
    if not current:
        return False
    if item.key and current == item.key:
        return True
    if current == item.model_id:
        return True
    if item.profile_id and current == f"{item.profile_id}/{item.model_id}":
        return True
    return current in item.aliases


def _model_env_override_message() -> str:
    if not model_env_override_active():
        return ""
    return " 注意：当前存在 OMNICRAWL_MODEL/OPENAI_MODEL 环境变量，重启后会优先使用环境变量。"


def _reasoning_env_override_message() -> str:
    if not os.getenv("REASONING_EFFORT", "").strip():
        return ""
    return " 注意：当前存在 REASONING_EFFORT 环境变量，重启后会优先使用环境变量。"


def build_slash_commands(agent: LocalToolAgent) -> list[str]:
    """构建所有可用的斜杠命令列表（含内置命令和动态 Skill 命令）。"""

    commands = [
        "/new",
        "/workspace",
        "/model",
        "/models",
        "/settings",
        "/reasoning",
        "/skills",
        "/memory:clean",
        "/mcp",
        "/plugins",
        "/sessions",
        "/tasks",
        "/task",
        "/resume",
        "/history",
        "/undo",
        "/compact",
        "/compact --model",
        "/rename",
        "/archive",
        "/archives",
        "/approval",
        "/approval:manual",
        "/approval:auto",
        "/approval:review",
        "/auto-approve:off",
        "/auto-approve:on",
        "/auto-review:on",
    ]
    sm = agent.skill_manager
    if sm is not None:
        for meta in sm.list_all():
            commands.append(f"/skill:{meta.name}")
    return commands


def build_slash_command_options(agent: LocalToolAgent) -> list[dict[str, str]]:
    """构建可供交互客户端使用的斜杠命令元数据。

    TUI 使用命令字符串做 Tab 补全；API 客户端可使用说明、显示标题和搜索文本。
    这里复用同一套命令来源，避免不同交互入口的命令不一致。
    """

    builtin_descriptions = {
        "/workspace": "切换当前 Agent 的工作区目录。",
        "/new": "开启一个空白会话。",
        "/model": "打开/查看模型目录，或输入 key、alias、profile/model_id 切换。",
        "/models": "/model 的兼容别名。",
        "/settings": "打开中文设置面板，修改运行时开关并立即保存。",
        "/reasoning": "查看或切换推理强度。",
        "/skills": "查看当前已加载的 Skill。",
        "/memory:clean": "清理过期长期记忆。",
        "/mcp": "查看 MCP 开关、服务和工具状态。",
        "/plugins": "查看 Hook 插件加载与 Worker 状态（只读）。",
        "/sessions": "查看当前工作区最近会话。",
        "/tasks": "查看当前会话可见的后台 SubAgent 任务。",
        "/task": "查看或取消一个后台 SubAgent 任务。",
        "/resume": "恢复指定会话 ID。",
        "/history": "查看或筛选提示历史。",
        "/undo": "回退最近一轮对话；不撤销已产生的外部副作用。",
        "/compact": "使用本地确定性规则压缩当前会话上下文。",
        "/compact --model": "使用结构化摘要模型压缩当前会话上下文。",
        "/rename": "重命名当前会话。",
        "/archive": "归档当前会话并开启新会话。",
        "/archives": "查看已归档会话。",
        "/approval": "查看当前工具审批模式。",
        "/approval:manual": "工具执行前逐次询问。",
        "/approval:auto": "自动批准工具执行。",
        "/approval:review": "仅对疑似删除行为进行审查。",
        "/auto-approve:off": "兼容命令：关闭自动审批。",
        "/auto-approve:on": "兼容命令：开启自动审批。",
        "/auto-review:on": "兼容命令：开启审查模式。",
    }
    argument_commands = {
        "/model",
        "/reasoning",
        "/resume",
        "/task",
        "/history",
        "/rename",
        "/workspace",
    }

    options: list[dict[str, str]] = []
    for command in build_slash_commands(agent):
        if command.startswith("/skill:"):
            continue
        options.append(
            {
                "command": command,
                "insert": f"{command} " if command in argument_commands else command,
                "title": command,
                "description": builtin_descriptions.get(command, "执行斜杠命令。"),
                "category": "命令",
                "search": command,
            }
        )

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
