from __future__ import annotations

import json
import os
from typing import Any

from .approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    approval_mode_label,
    save_approval_mode,
)
from .agent import AgentError, LocalToolAgent
from .llm import LLMError, save_reasoning_effort
from .model_catalog import (
    ModelCatalogError,
    detect_model_options,
    ensure_current_model_option,
    format_model_options,
    model_env_override_active,
    save_llm_model,
)
from .runtime_config import RuntimeConfigError


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

    if "." in tool_name and arguments:
        return f"参数：{_truncate_for_display(json.dumps(arguments, ensure_ascii=False), 240)}"

    return ""


def format_tool_confirmation(tool_name: str, arguments: dict[str, Any]) -> str:
    """把工具调用格式化为行内 UI 的确认提示。

    只读工具仅显示描述，写入/执行工具额外展示关键内容供审查。
    """

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
    if normalized == "/compact":
        try:
            summary = agent.compact_conversation()
        except AgentError as exc:
            return f"会话压缩失败：{exc}"
        return f"已压缩当前会话，后续恢复将从摘要边界继续。\n{summary}"
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
        return f"审批模式已临时切换为 {approval_mode_label(mode)}，但写入 config.json 失败：{exc}"
    return f"审批模式已切换为 {approval_mode_label(mode)}，并已同步到 {path}。"


def handle_model_command(agent: LocalToolAgent, command: str) -> str | None:
    """处理模型查看与切换命令；返回 None 表示不是模型命令。"""

    text = command.strip()
    normalized = text.lower()
    if normalized == "/models":
        text = "/model"
        normalized = text
    if normalized != "/model" and not normalized.startswith("/model "):
        return None

    parts = text.split(None, 1)
    if len(parts) == 1:
        return _format_detected_models(agent)

    model_id = parts[1].strip()
    if not model_id:
        return "用法：/model 查看模型列表，或 /model <模型ID> 切换当前模型。"

    validation_message = _validate_model_id_against_base_url(agent, model_id)
    if validation_message is not None:
        return validation_message

    try:
        agent.set_model(model_id)
    except AgentError as exc:
        return f"模型切换失败：{exc}"

    save_message = _save_model_change(model_id)
    env_message = _model_env_override_message()
    suffix = "".join(part for part in (save_message, env_message) if part)
    return f"当前模型已切换为 {model_id}{suffix}"


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
        return f"推理强度已临时切换为 {normalized_effort}，但写入 config.json 失败：{exc}"
    env_message = _reasoning_env_override_message()
    return f"推理强度已切换为 {normalized_effort}，并已同步到 {path}{env_message}"


def _format_detected_models(agent: LocalToolAgent) -> str:
    current_model = agent.current_model
    try:
        options = ensure_current_model_option(
            detect_model_options(agent.config.llm),
            current_model,
        )
    except ModelCatalogError as exc:
        return f"当前模型：{current_model}\n模型列表检测失败：{exc}"

    if not options:
        return f"当前模型：{current_model}\n模型列表为空。"

    lines = [
        f"当前模型：{current_model}",
        f"从 {agent.config.llm.base_url.rstrip('/')}/models 检测到 {len(options)} 个模型：",
        format_model_options(options, current_model=current_model),
        "",
        "切换模型：/model <模型ID>",
    ]
    return "\n".join(lines).rstrip()


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


def _save_model_change(model_id: str) -> str:
    try:
        path = save_llm_model(model_id)
    except ModelCatalogError as exc:
        return f"，但写入 config.json 失败：{exc}。"
    return f"，并已同步到 {path}。"


def _model_env_override_message() -> str:
    if not model_env_override_active():
        return ""
    return " 注意：当前存在 OPENAI_MODEL 环境变量，重启后会优先使用环境变量。"


def _reasoning_env_override_message() -> str:
    if not os.getenv("REASONING_EFFORT", "").strip():
        return ""
    return " 注意：当前存在 REASONING_EFFORT 环境变量，重启后会优先使用环境变量。"


def build_slash_commands(agent: LocalToolAgent) -> list[str]:
    """构建所有可用的斜杠命令列表（含内置命令和动态 Skill 命令）。"""

    commands = [
        "/new",
        "/model",
        "/models",
        "/reasoning",
        "/skills",
        "/memory:clean",
        "/mcp",
        "/sessions",
        "/resume",
        "/history",
        "/compact",
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
    """构建 Qt 输入框斜杠菜单使用的命令元数据。

    TUI 只需要命令字符串做 Tab 补全；Qt 菜单需要额外的说明、显示标题和
    搜索文本。这里复用同一套命令来源，避免 GUI 与终端可用命令不一致。
    """

    builtin_descriptions = {
        "/new": "开启一个空白会话。",
        "/model": "查看模型列表，或输入模型 ID 切换当前模型。",
        "/models": "查看当前接口可用的模型列表。",
        "/reasoning": "查看或切换推理强度。",
        "/skills": "查看当前已加载的 Skill。",
        "/memory:clean": "清理过期长期记忆。",
        "/mcp": "查看 MCP 开关、服务和工具状态。",
        "/sessions": "查看当前工作区最近会话。",
        "/resume": "恢复指定会话 ID。",
        "/history": "查看或筛选提示历史。",
        "/compact": "压缩当前会话上下文。",
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
        "/history",
        "/rename",
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
