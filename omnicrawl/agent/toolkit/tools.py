"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

import json
import re
from typing import Any, Callable, Sequence

from .approval_policy import GIT_SUPPORTED_ACTIONS
from ..context_compaction.evidence import RECALL_SESSION_EVIDENCE_TOOL_NAME
from ..types import ToolCall, ToolDefinition, ToolResult
from ...mcp import MCPClientManager, MCPToolMeta
from ...state.session_artifacts import redact_sensitive_text
from ...workspace_tools import (
    DEFAULT_COMMAND_TIMEOUT_SECONDS,
    MAX_COMMAND_TIMEOUT_SECONDS,
    WorkspaceToolError,
)


TODO_TOOL_NAME = "update_todos"

TOOL_NAME_ALIASES = {
    "bashcommand": "bash",
    "monitorcommand": "monitor",
    "powershellcommand": "powershell",
    "readimage": "read_image",
    "replacetext": "replace_text",
    "writefile": "write_file",
}
ARGUMENT_NAME_ALIASES = {
    "cmd": "command",
    "caseSensitive": "case_sensitive",
    "casesensitive": "case_sensitive",
    "maxLines": "max_lines",
    "maxlines": "max_lines",
    "maxResults": "max_results",
    "maxresults": "max_results",
    "newText": "new_text",
    "newtext": "new_text",
    "oldText": "old_text",
    "oldtext": "old_text",
    "startLine": "start_line",
    "startline": "start_line",
    "function": "function_name",
    "functionName": "function_name",
    "functionname": "function_name",
    "textSnippet": "text",
    "textsnippet": "text",
    "contextLines": "context_lines",
    "contextlines": "context_lines",
    "monitorId": "monitor_id",
    "monitorid": "monitor_id",
    "maxEvents": "max_events",
    "maxevents": "max_events",
    "tabId": "tab",
    "tabid": "tab",
    "timeoutSeconds": "timeout_seconds",
    "timeoutseconds": "timeout_seconds",
    "windowHandle": "window_handle",
    "windowhandle": "window_handle",
    "titleContains": "title_contains",
    "titlecontains": "title_contains",
    "className": "class_name",
    "classname": "class_name",
    "classNameContains": "class_name_contains",
    "classnamecontains": "class_name_contains",
    "visibleOnly": "visible_only",
    "visibleonly": "visible_only",
    "includeUntitled": "include_untitled",
    "includeuntitled": "include_untitled",
    "automationId": "automation_id",
    "automationid": "automation_id",
    "controlType": "control_type",
    "controltype": "control_type",
    "wheelDelta": "wheel_delta",
    "wheeldelta": "wheel_delta",
    "maxDimension": "max_dimension",
    "maxdimension": "max_dimension",
}


MEMORY_WRITE_ARGUMENT_SCHEMA = json.dumps(
    {
        "type": "object",
        "properties": {
            "memories": {
                "type": "array",
                "minItems": 1,
                "description": "待写入或合并的记忆对象列表。",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "minLength": 1,
                            "description": "简洁、可独立理解且已经确认的记忆正文。",
                        },
                        "related_directories": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "与该记忆相关的分类目录，可省略或传空数组。",
                        },
                        "storage_directory": {
                            "type": "string",
                            "minLength": 1,
                            "description": "可选存储分类目录；省略时由后端自动分类。",
                        },
                        "source_event": {
                            "type": "string",
                            "minLength": 1,
                            "description": "可选来源标识，例如本轮对话或上下文压缩。",
                        },
                    },
                    "required": ["content"],
                    "additionalProperties": False,
                },
            }
        },
        "required": ["memories"],
        "additionalProperties": False,
    },
    ensure_ascii=False,
)


ToolRunner = Callable[[dict[str, Any]], ToolResult]
MCPToolRunner = Callable[[MCPToolMeta, dict[str, Any]], ToolResult]
MCPResourceRunner = Callable[[str], ToolResult]
MCPPromptRunner = Callable[[str, dict[str, Any]], ToolResult]


def public_tool_arguments(tool_name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    """返回可进入 Session、确认 UI 和 SSE 的工具参数投影。

    普通工具保持既有参数语义；`subagent` 的完整任务 prompt 只存在于实际执行
    调用栈中，公开出口仅保留调度元数据和有界描述。
    """

    if tool_name == "invoke_tool":
        target = str(arguments.get("tool_name") or "")[:200]
        inner = arguments.get("arguments")
        public = {"tool_name": target}
        if isinstance(inner, dict):
            public["argument_keys"] = sorted(str(key)[:100] for key in inner)[:50]
            public["argument_count"] = len(inner)
        else:
            public["arguments_valid"] = False
        return public

    if tool_name in {
        "windows_window",
        "windows_control",
        "windows_input",
        "windows_clipboard",
        "windows_screenshot",
    }:
        return _public_windows_desktop_arguments(tool_name, arguments)

    if tool_name != "subagent":
        return dict(arguments)

    action = str(arguments.get("action") or "run").strip() or "run"
    # worktree 控制面只公开调度键与策略，不回传 diff 正文。
    if action in {"apply_worktree", "discard_worktree", "list_worktrees"}:
        public: dict[str, Any] = {"action": action}
        for key in ("task_id", "batch_id", "branch", "strategy"):
            value = arguments.get(key)
            if value is None:
                continue
            text = redact_sensitive_text(str(value).strip())
            if text:
                public[key] = text[:200]
        if "cleanup" in arguments:
            public["cleanup"] = bool(arguments.get("cleanup"))
        if "remove_branch" in arguments:
            public["remove_branch"] = bool(arguments.get("remove_branch"))
        return public

    tasks = arguments.get("tasks")
    if not isinstance(tasks, list) and "task_count" in arguments:
        raw_descriptions = arguments.get("descriptions")
        raw_agent_types = arguments.get("agent_types")
        task_count = arguments.get("task_count", 0)
        descriptions_source = (
            raw_descriptions if isinstance(raw_descriptions, list) else []
        )
        agent_types_source = raw_agent_types if isinstance(raw_agent_types, list) else []
        valid_task_count = (
            isinstance(task_count, int)
            and not isinstance(task_count, bool)
            and task_count >= 0
        )
        return {
            "action": action if action in {"run", "spawn", "list", "get", "cancel"} else "run",
            "task_count": task_count if valid_task_count else 0,
            "descriptions": [
                redact_sensitive_text(str(item))[:120]
                for item in descriptions_source[:4]
            ],
            "agent_types": [
                redact_sensitive_text(str(item))[:120]
                for item in agent_types_source[:4]
            ],
            "max_concurrency": arguments.get("max_concurrency"),
            "fail_fast": bool(arguments.get("fail_fast", False)),
        }
    safe_tasks = tasks if isinstance(tasks, list) else []
    descriptions: list[str] = []
    agent_types: list[str] = []
    for item in safe_tasks[:4]:
        if not isinstance(item, dict):
            continue
        description = redact_sensitive_text(str(item.get("description", "")).strip())
        if description:
            descriptions.append(description[:120])
        agent_type = redact_sensitive_text(str(item.get("subagent_type", "")).strip())
        if agent_type:
            agent_types.append(agent_type[:120])
    return {
        "action": action if action in {"run", "spawn", "list", "get", "cancel"} else "run",
        "task_count": len(safe_tasks),
        "descriptions": descriptions,
        "agent_types": agent_types,
        "max_concurrency": arguments.get("max_concurrency"),
        "fail_fast": bool(arguments.get("fail_fast", False)),
    }


def _public_windows_desktop_arguments(
    tool_name: str,
    arguments: dict[str, Any],
) -> dict[str, Any]:
    """投影桌面自动化参数，避免输入文本和剪贴板内容进入确认页或 Session。"""

    action = str(arguments.get("action") or "").strip()
    public: dict[str, Any] = {"action": action}
    for key in (
        "window_handle",
        "title_contains",
        "class_name_contains",
        "visible_only",
        "include_untitled",
        "max_results",
        "name",
        "automation_id",
        "class_name",
        "control_type",
        "index",
        "x",
        "y",
        "button",
        "clicks",
        "wheel_delta",
        "key",
        "keys",
        "presses",
        "max_chars",
        "target",
        "width",
        "height",
        "max_dimension",
    ):
        if key in arguments:
            public[key] = arguments[key]

    # value/text 经常承载密码、令牌或私有内容；确认页只显示长度，执行层仍取得原值。
    if tool_name == "windows_control" and isinstance(arguments.get("value"), str):
        public["value_length"] = len(arguments["value"])
    if tool_name in {"windows_input", "windows_clipboard"} and isinstance(
        arguments.get("text"),
        str,
    ):
        public["text_length"] = len(arguments["text"])
    return public


def build_agent_tools(
    *,
    mcp_manager: MCPClientManager,
    memory_enabled: bool,
    list: ToolRunner,
    read: ToolRunner,
    grep: ToolRunner,
    web_search: ToolRunner | None = None,
    fetcher: ToolRunner | None = None,
    image_gen: ToolRunner | None = None,
    tts: ToolRunner | None = None,
    replace_text: ToolRunner,
    write_file: ToolRunner,
    bash: ToolRunner,
    powershell: ToolRunner,
    monitor: ToolRunner,
    git: ToolRunner | None = None,
    memory_search: ToolRunner,
    memory_read: ToolRunner,
    memory_expand_related: ToolRunner,
    memory_write: ToolRunner,
    mcp_call: MCPToolRunner,
    mcp_read_resource: MCPResourceRunner,
    mcp_get_prompt: MCPPromptRunner,
    read_image: ToolRunner | None = None,
    find: ToolRunner | None = None,
    evidence_recall: ToolRunner | None = None,
    subagent: ToolRunner | None = None,
    subagent_types: Sequence[str] = (),
    update_todos: ToolRunner | None = None,
    windows_window: ToolRunner | None = None,
    windows_control: ToolRunner | None = None,
    windows_input: ToolRunner | None = None,
    windows_clipboard: ToolRunner | None = None,
    windows_screenshot: ToolRunner | None = None,
    project_memory_search: ToolRunner | None = None,
    project_memory_read: ToolRunner | None = None,
    project_memory_expand_related: ToolRunner | None = None,
    project_memory_write: ToolRunner | None = None,
    session_memory_search: ToolRunner | None = None,
    session_memory_read: ToolRunner | None = None,
    session_memory_expand_related: ToolRunner | None = None,
    session_memory_write: ToolRunner | None = None,
    user_memory_search: ToolRunner | None = None,
    user_memory_read: ToolRunner | None = None,
    user_memory_expand_related: ToolRunner | None = None,
    user_memory_write: ToolRunner | None = None,
    kb_search: ToolRunner | None = None,
    kb_read: ToolRunner | None = None,
    kb_write: ToolRunner | None = None,
    kb_append: ToolRunner | None = None,
    kb_list: ToolRunner | None = None,
    disabled_tools: frozenset[str] = frozenset(),
) -> dict[str, ToolDefinition]:
    """构建 Agent 可用工具表，执行函数仍由 LocalToolAgent 绑定提供。

    ``disabled_tools`` 中的工具名（含 MCP 动态工具）不会出现在结果表中；
    模型不可见即不可调用，与审批模式无关。
    """

    kb_runners = (kb_search, kb_read, kb_write, kb_append, kb_list)
    if any(runner is not None for runner in kb_runners) and not all(
        runner is not None for runner in kb_runners
    ):
        raise ValueError("知识库工具必须作为完整工具组注册。")

    tools = build_mcp_tools(
        mcp_manager=mcp_manager,
        mcp_call=mcp_call,
        mcp_read_resource=mcp_read_resource,
        mcp_get_prompt=mcp_get_prompt,
    )
    tools.extend(
        [
            ToolDefinition(
                name="list",
                description="列出指定路径下的文件和目录（相对路径基于工作区），可选择递归。",
                argument_schema='{"path": ".", "recursive": false}',
                requires_confirmation=True,
                run=list,
            ),
            *(
                [
                    ToolDefinition(
                        name="find",
                        description=(
                            "仅按文件名、目录名或相对路径查找本地条目（相对路径基于工作区），不读取文件内容；"
                            "pattern 支持 glob 通配符（如 * 匹配所有文件名）。"
                            "搜索自动读取 .gitignore 跳过 node_modules 等被忽略目录；"
                            "结果超过 max_results 时保留前 N 条并保存完整列表，footer 给出保存路径供继续读取。"
                        ),
                        argument_schema=(
                            '{"pattern":"agent","path":".","kind":"all|file|directory",'
                            '"case_sensitive":false,"max_results":50}'
                        ),
                        requires_confirmation=True,
                        run=find,
                    )
                ]
                if find is not None
                else []
            ),
            ToolDefinition(
                name="read",
                description=(
                    "读取本地 UTF-8 文本文件或 omnicrawl://docs/<文件名> 内置文档。"
                    "可按 start_line/max_lines 读取行范围，按 function_name 定位函数或方法，"
                    "或按 text 定位首次文字片段及上下文。"
                    "输出带行号与 footer：若文件还有更多内容，footer 会给出"
                    "(Showing lines X-Y of Z. Use start_line=Y+1 to continue.)，"
                    "用返回的 start_line 继续向后读取。"
                ),
                argument_schema=(
                    '{"path":"main.py","start_line":1,"max_lines":200,'
                    '"function_name":"Class.method","text":"目标片段","context_lines":20}'
                ),
                requires_confirmation=True,
                run=read,
            ),
            *(
                [
                    ToolDefinition(
                        name="read_image",
                        description=(
                            "读取本机 PNG、JPEG、WebP 或 GIF 图片。path 可使用工作区相对路径或本机绝对路径；"
                            "图片内容会在 vision 模型可用时以内联方式提供给模型，不支持 URL。"
                        ),
                        argument_schema=json.dumps(
                            {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string", "minLength": 1},
                                    "detail": {
                                        "type": "string",
                                        "enum": ["auto", "low", "high"],
                                    },
                                },
                                "required": ["path"],
                                "additionalProperties": False,
                            },
                            ensure_ascii=False,
                        ),
                        requires_confirmation=True,
                        run=read_image,
                    )
                ]
                if read_image is not None
                else []
            ),
            ToolDefinition(
                name="grep",
                description=(
                    "在本地 UTF-8 文本文件中执行 grep 风格搜索：pattern 默认按正则表达式"
                    "解释，可用 | 连接多个候选目标（如 messages|context|tool_calls）；"
                    "use_regex=false 时按精确子串，支持大小写开关、匹配行上下文、"
                    "每文件计数、仅列出匹配文件，以及 include/exclude 文件名过滤。"
                    "超长匹配行自动截断到 2000 字符并标记 (line truncated)；"
                    "结果超过 max_results 时保留前 N 条并保存完整结果，footer 给出保存路径供继续读取。"
                ),
                argument_schema=(
                    '{"pattern": "class Agent", "path": ".", "use_regex": true, '
                    '"case_sensitive": false, "context_lines": 0, "count": false, '
                    '"files_with_matches": false, "include": "*.py", '
                    '"exclude": "*.min.js", "max_results": 50}'
                ),
                requires_confirmation=True,
                run=grep,
            ),
            *(
                [
                    ToolDefinition(
                        name="web_search",
                        description=(
                            "搜索公开网页（Bing/DuckDuckGo/雅虎），返回标题、链接与摘要。"
                            "自带桌面 Chrome 环境模拟（UA、Sec-Fetch-* 请求头、跟随重定向）降低拦截；"
                            "遇验证码返回明确错误、不绕过。engine 默认 bing，language 可选，max_results 默认 5。"
                        ),
                        argument_schema=(
                            '{"query": "关键词", "engine": "bing|duckduckgo|yahoo", '
                            '"max_results": 5, "language": "zh-CN"}'
                        ),
                        requires_confirmation=True,
                        run=web_search,
                    )
                ]
                if web_search is not None
                else []
            ),
            *(
                [
                    ToolDefinition(
                        name="fetcher",
                        description=(
                            "抓取 URL 网页内容：模拟 Chrome/Firefox/Safari/Edge 浏览器指纹与桌面请求头，"
                            "支持并行多 URL；insecure=true 跳过 TLS 校验（内网证书）；跟随重定向；"
                            "默认返回正文（max_chars 限长），max_html=true 返回原始 HTML。"
                            "不执行 JS、不绕过验证码。"
                        ),
                        argument_schema=(
                            '{"urls": "https://a.example,https://b.example", "insecure": false, '
                            '"parallel": true, "timeout": 15, "max_chars": 8000, '
                            '"max_html": false, "impersonate": "chrome"}'
                        ),
                        requires_confirmation=True,
                        run=fetcher,
                    )
                ]
                if fetcher is not None
                else []
            ),
            *(
                [
                    ToolDefinition(
                        name="image_gen",
                        description=(
                            "生成或编辑图片（OpenAI 兼容 Image API）：prompt 文生图，或传本地图片路径 image 加 prompt 编辑。"
                            "接口/Key/模型在 /settings 配置；size/quality/output_format/n 可覆盖；"
                            "图片默认存 .omnicrawl/.agent_tmp/images/。"
                        ),
                        argument_schema=(
                            '{"prompt": "图像描述", "image": "编辑时传入的本地图片路径", '
                            '"n": 1, "size": "auto", "quality": "auto", '
                            '"output_format": "png", "path": "可选保存路径"}'
                        ),
                        requires_confirmation=True,
                        run=image_gen,
                    )
                ]
                if image_gen is not None
                else []
            ),
            *(
                [
                    ToolDefinition(
                        name="tts_synthesize",
                        description=(
                            "把文本合成为语音（MOSS-TTS-Nano，本地 ONNX CPU 推理，无需联网）。"
                            "text 必填；音色固定使用设置中的配置（/settings → TTS），"
                            "模型不可指定音色；prompt_audio 为参考音频路径时可语音克隆；"
                            "path 可指定 WAV 保存位置（默认 .omnicrawl/.agent_tmp/tts/）。"
                            "输出 48kHz 立体声 WAV，生成后默认自动播放。"
                        ),
                        argument_schema=(
                            '{"text": "要朗读的文本", '
                            '"prompt_audio": "参考音频路径（可选，语音克隆）", '
                            '"path": "输出 wav 路径（可选）"}'
                        ),
                        requires_confirmation=True,
                        run=tts,
                    )
                ]
                if tts is not None
                else []
            ),
            ToolDefinition(
                name="replace_text",
                description="在单个文件中替换指定文本，适合小范围代码修改。",
                argument_schema='{"path": "main.py", "old_text": "...", "new_text": "...", "count": 1}',
                requires_confirmation=True,
                run=replace_text,
            ),
            ToolDefinition(
                name="write_file",
                description=(
                    "写入或追加 UTF-8 文本文件；一次性脚本、中间文件和临时交付物"
                    "应优先写入 Agent 临时目录。"
                ),
                argument_schema=(
                    '{"path": ".omnicrawl/.agent_tmp/files/notes.md", "content": "...", '
                    '"mode": "overwrite"}'
                ),
                requires_confirmation=True,
                run=write_file,
            ),
            ToolDefinition(
                name="bash",
                description=(
                    "使用 Git Bash 执行主命令并保留真实退出码。测试/构建命令不得用 tail/head/grep/rg 裁剪输出，"
                    "查看日志应放独立 diagnostic_command。Bash 管道启用 pipefail，不得掩盖上游失败。"
                    "只接受 POSIX Shell 语法。"
                ),
                argument_schema=json.dumps(
                    {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "minLength": 1},
                            "diagnostic_command": {
                                "type": "string",
                                "minLength": 0,
                            },
                            "timeout_seconds": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": MAX_COMMAND_TIMEOUT_SECONDS,
                                "default": DEFAULT_COMMAND_TIMEOUT_SECONDS,
                            },
                        },
                        "required": ["command"],
                        "additionalProperties": False,
                    },
                    ensure_ascii=False,
                ),
                requires_confirmation=True,
                run=bash,
            ),
            ToolDefinition(
                name="powershell",
                description=(
                    "使用 PowerShell 执行完整的主命令并保留真实退出码。测试或构建命令不得在主命令中"
                    "使用 Select-Object、Select-String 等裁剪输出；需要查看诊断日志时使用独立的 "
                    "diagnostic_command。只接受 PowerShell 语法，不得使用 Bash 语法。"
                ),
                argument_schema=json.dumps(
                    {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "minLength": 1},
                            "diagnostic_command": {
                                "type": "string",
                                "minLength": 0,
                            },
                            "timeout_seconds": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": MAX_COMMAND_TIMEOUT_SECONDS,
                                "default": DEFAULT_COMMAND_TIMEOUT_SECONDS,
                            },
                        },
                        "required": ["command"],
                        "additionalProperties": False,
                    },
                    ensure_ascii=False,
                ),
                requires_confirmation=True,
                run=powershell,
            ),
            ToolDefinition(
                name="monitor",
                description=(
                    "在后台启动受 Agent 管理的命令，或按任务 ID 轮询增量日志、查看任务列表、"
                    "停止任务。启动后立即返回 monitor_id；Agent 关闭或切换工作区时会自动终止任务。"
                ),
                argument_schema=(
                    '{"action":"start","command":"python -m http.server",'
                    '"shell":"powershell","monitor_id":"monitor-...","cursor":0,'
                    '"max_events":100}'
                ),
                requires_confirmation=True,
                run=monitor,
            ),
            *(
                [
                    ToolDefinition(
                        name="git",
                        description=(
                            "在工作区执行结构化 git 操作（不经 shell）。action 为 git 子命令，"
                            "args 是其参数（标志、引用名、分支名等），paths 是工作区内相对路径，"
                            "message 是提交信息。readonly 动作（status/diff/log/show/ls-files 等）"
                            "自动放行；本地变更（add/commit/branch/stash/restore 等）按模式确认；"
                            "push/rebase/merge/pull/clean/reset --hard 等高危动作需额外审查。"
                            "commit 必须提供 message；禁止 --git-dir/--work-tree/--no-verify"
                            "及全局/系统配置写入。git 操作应使用本工具，不要用 bash 拼命令。"
                        ),
                        argument_schema=json.dumps(
                            {
                                "type": "object",
                                "properties": {
                                    "action": {
                                        "type": "string",
                                        # 参数名 list 是工具 runner，用解包代替 list() 避免遮蔽。
                                        "enum": [*GIT_SUPPORTED_ACTIONS],
                                        "description": "git 子命令（见描述）。",
                                    },
                                    "args": {
                                        "type": "array",
                                        "items": {"type": "string"},
                                        "description": (
                                            "子命令参数：标志（--short/--oneline/-n 20）与"
                                            "位置参数（分支名、引用名等）；stash 的"
                                            "list/push/pop/drop 等子动词也放在这里。"
                                        ),
                                    },
                                    "message": {
                                        "type": "string",
                                        "minLength": 1,
                                        "description": "commit（或 annotated tag）提交信息。",
                                    },
                                    "paths": {
                                        "type": "array",
                                        "items": {"type": "string", "minLength": 1},
                                        "description": "工作区内相对路径，追加在命令末尾。",
                                    },
                                },
                                "required": ["action"],
                                "additionalProperties": False,
                            },
                            ensure_ascii=False,
                        ),
                        requires_confirmation=True,
                        run=git,
                        model_output_is_bounded=True,
                    )
                ]
                if git is not None
                else []
            ),
            *(
                [
                    ToolDefinition(
                        name="kb_search",
                        description=(
                            "搜索跨项目工作知识库（~/.OmniCrawl/knowledge/）中的笔记摘要。"
                            "知识库存放工作记录、其他项目资料、会议纪要、决策与研究笔记，"
                            "独立于当前项目。支持按 project、tags、type、status 过滤；"
                            "先搜索摘要，再按需 kb_read 全文。"
                        ),
                        argument_schema=json.dumps(
                            {
                                "type": "object",
                                "properties": {
                                    "query": {"type": "string", "minLength": 1},
                                    "project": {"type": "string"},
                                    "tags": {
                                        "type": "array",
                                        "items": {"type": "string"},
                                    },
                                    "type": {
                                        "type": "string",
                                        "enum": [
                                            "note",
                                            "meeting",
                                            "decision",
                                            "log",
                                            "research",
                                            "reference",
                                        ],
                                    },
                                    "status": {
                                        "type": "string",
                                        "enum": ["draft", "done", "archived"],
                                    },
                                    "max_results": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": 50,
                                        "default": 10,
                                    },
                                },
                                "required": ["query"],
                                "additionalProperties": False,
                            },
                            ensure_ascii=False,
                        ),
                        requires_confirmation=False,
                        run=kb_search,
                    ),
                    ToolDefinition(
                        name="kb_read",
                        description=(
                            "读取工作知识库笔记全文。path 为知识库内相对路径，"
                            "例如 projects/客户A/2026-06-18-会议纪要.md；省略 .md 后缀会自动补全。"
                        ),
                        argument_schema=json.dumps(
                            {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string", "minLength": 1},
                                    "max_chars": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": 200000,
                                        "default": 50000,
                                    },
                                },
                                "required": ["path"],
                                "additionalProperties": False,
                            },
                            ensure_ascii=False,
                        ),
                        requires_confirmation=False,
                        run=kb_read,
                    ),
                    ToolDefinition(
                        name="kb_write",
                        description=(
                            "新建或更新知识库笔记（~/.OmniCrawl/knowledge/），自动维护 YAML frontmatter 与 INDEX.md。"
                            "mode=create 仅新建、overwrite 覆盖正文并合并已有字段、append 追加正文。"
                        ),
                        argument_schema=json.dumps(
                            {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string", "minLength": 1},
                                    "content": {"type": "string"},
                                    "mode": {
                                        "type": "string",
                                        "enum": ["create", "overwrite", "append"],
                                        "default": "overwrite",
                                    },
                                    "title": {"type": "string"},
                                    "project": {"type": "string"},
                                    "tags": {
                                        "type": "array",
                                        "items": {"type": "string"},
                                    },
                                    "type": {
                                        "type": "string",
                                        "enum": [
                                            "note",
                                            "meeting",
                                            "decision",
                                            "log",
                                            "research",
                                            "reference",
                                        ],
                                    },
                                    "status": {
                                        "type": "string",
                                        "enum": ["draft", "done", "archived"],
                                    },
                                },
                                "required": ["path", "content"],
                                "additionalProperties": False,
                            },
                            ensure_ascii=False,
                        ),
                        requires_confirmation=False,
                        run=kb_write,
                    ),
                    ToolDefinition(
                        name="kb_append",
                        description=(
                            "向已有工作知识库笔记追加正文，自动更新 updated 与 INDEX.md；"
                            "不修改 frontmatter 的其他字段。"
                        ),
                        argument_schema=json.dumps(
                            {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string", "minLength": 1},
                                    "content": {"type": "string"},
                                },
                                "required": ["path", "content"],
                                "additionalProperties": False,
                            },
                            ensure_ascii=False,
                        ),
                        requires_confirmation=False,
                        run=kb_append,
                    ),
                    ToolDefinition(
                        name="kb_list",
                        description=(
                            "列出工作知识库目录或按 project、tags、type、status 筛选笔记；"
                            "返回每篇笔记的路径与 frontmatter 元数据。"
                        ),
                        argument_schema=json.dumps(
                            {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string"},
                                    "project": {"type": "string"},
                                    "tags": {
                                        "type": "array",
                                        "items": {"type": "string"},
                                    },
                                    "type": {
                                        "type": "string",
                                        "enum": [
                                            "note",
                                            "meeting",
                                            "decision",
                                            "log",
                                            "research",
                                            "reference",
                                        ],
                                    },
                                    "status": {
                                        "type": "string",
                                        "enum": ["draft", "done", "archived"],
                                    },
                                    "max_results": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": 200,
                                        "default": 100,
                                    },
                                },
                                "additionalProperties": False,
                            },
                            ensure_ascii=False,
                        ),
                        requires_confirmation=False,
                        run=kb_list,
                    ),
                ]
                if kb_search is not None
                else []
            ),
        ]
    )
    if update_todos is not None:
        tools.append(
            ToolDefinition(
                name=TODO_TOOL_NAME,
                description=(
                    "维护当前任务的紧凑执行清单。处理多步骤项目任务时，先用本工具提交完整步骤列表，"
                    "每完成一步就再次提交同一列表并将对应 completed 设为 true；不要把计划正文写进回复。"
                    "todos 为空表示清除当前计划。"
                ),
                argument_schema=json.dumps(
                    {
                        "type": "object",
                        "properties": {
                            "todos": {
                                "type": "array",
                                "maxItems": 20,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": {"type": "string", "maxLength": 80},
                                        "step": {"type": "string", "minLength": 1, "maxLength": 240},
                                        "completed": {"type": "boolean"},
                                    },
                                    "required": ["step", "completed"],
                                    "additionalProperties": False,
                                },
                            }
                        },
                        "required": ["todos"],
                        "additionalProperties": False,
                    },
                    ensure_ascii=False,
                ),
                requires_confirmation=False,
                run=update_todos,
            )
        )
    if evidence_recall is not None:
        tools.append(
            ToolDefinition(
                name=RECALL_SESSION_EVIDENCE_TOOL_NAME,
                description=(
                    "按结构化会话摘要中显示的来源事件 ID，恢复当前 Session 的精确证据。"
                    "仅允许读取当前有效摘要引用的事件；单次最多 8 个 ID、合计约 4000 Token。"
                    "缺失、未授权或不可读内容会返回结构化诊断，不会恢复整个冷历史。"
                ),
                argument_schema=json.dumps(
                    {
                        "type": "object",
                        "properties": {
                            "event_ids": {
                                "type": "array",
                                "minItems": 1,
                                "maxItems": 8,
                                "items": {
                                    "type": "string",
                                    "minLength": 1,
                                    "maxLength": 128,
                                },
                            }
                        },
                        "required": ["event_ids"],
                        "additionalProperties": False,
                    },
                    ensure_ascii=False,
                ),
                requires_confirmation=False,
                run=evidence_recall,
                model_output_is_bounded=True,
            )
        )
    windows_runners = (
        windows_window,
        windows_control,
        windows_input,
        windows_clipboard,
        windows_screenshot,
    )
    if any(runner is not None for runner in windows_runners):
        if not all(runner is not None for runner in windows_runners):
            raise ValueError("Windows 桌面工具必须作为完整工具组注册。")
        tools.extend(
            [
                ToolDefinition(
                    name="windows_window",
                    description=(
                        "仅限 Windows：枚举可见顶层窗口、读取窗口标题/类名/进程/几何位置，"
                        "或激活指定窗口。先用 list 获取 window_handle；activate 不会绕过 Windows 的前台焦点保护。"
                    ),
                    argument_schema=(
                        '{"action":"list|get|activate","window_handle":"0x...",'
                        '"title_contains":"可选标题片段","class_name_contains":"可选类名片段",'
                        '"visible_only":true,"include_untitled":false,"max_results":50}'
                    ),
                    requires_confirmation=True,
                    run=windows_window,
                ),
                ToolDefinition(
                    name="windows_control",
                    description=(
                        "仅限 Windows：UI Automation 在 window_handle 内列出控件，或按 "
                        "name/automation_id/class_name/control_type 定位后 "
                        "invoke/set_value/select/toggle/focus。非 list 须定位；多匹配 list/index。"
                    ),
                    argument_schema=(
                        '{"action":"list|invoke|set_value|select|toggle|focus",'
                        '"window_handle":"0x...","name":"精确名称","automation_id":"自动化ID",'
                        '"class_name":"类名","control_type":"button|edit|...","index":0,'
                        '"value":"仅 set_value","max_results":30}'
                    ),
                    requires_confirmation=True,
                    run=windows_control,
                ),
                ToolDefinition(
                    name="windows_input",
                    description=(
                        "仅限 Windows：通过 SendInput 移动/点击鼠标、滚轮、按键、组合键或输入 Unicode 文本。"
                        "click/move 必须提供虚拟桌面坐标；type_text 不会回显输入内容。"
                    ),
                    argument_schema=(
                        '{"action":"move|click|scroll|key|hotkey|type_text",'
                        '"x":100,"y":200,"button":"left|right|middle","clicks":1,'
                        '"wheel_delta":-120,"key":"enter","keys":["ctrl","s"],'
                        '"presses":1,"text":"Unicode 文本"}'
                    ),
                    requires_confirmation=True,
                    run=windows_input,
                ),
                ToolDefinition(
                    name="windows_clipboard",
                    description=(
                        "仅限 Windows：读取、写入或清空 Unicode 文本剪贴板。"
                        "read_text 可用 max_chars 限制返回长度；写入文本不会进入确认页或会话参数记录。"
                    ),
                    argument_schema=(
                        '{"action":"read_text|write_text|clear","text":"仅 write_text",'
                        '"max_chars":8000}'
                    ),
                    requires_confirmation=True,
                    run=windows_clipboard,
                ),
                ToolDefinition(
                    name="windows_screenshot",
                    description=(
                        "仅限 Windows：Win32 GDI 截取虚拟桌面、指定区域或窗口，保存到临时图片目录；"
                        "模型声明 vision 时下一轮直接提供给模型。窗口目标先用 windows_window.list 获取 "
                        "window_handle；最小化/越界/受保护内容可能截取失败。"
                    ),
                    argument_schema=(
                        '{"target":"desktop|region|window","window_handle":"0x...",'
                        '"x":0,"y":0,"width":1280,"height":720,"max_dimension":2048}'
                    ),
                    requires_confirmation=True,
                    run=windows_screenshot,
                ),
            ]
        )
    if subagent is not None:
        available_subagent_types = sorted(
            {
                str(name).strip().casefold()
                for name in subagent_types
                if isinstance(name, str) and name.strip()
            }
        )
        if not available_subagent_types:
            raise ValueError("注册 SubAgent 工具时必须提供至少一个可用角色。")
        tools.append(
            ToolDefinition(
                name="subagent",
                description=(
                    "管理受限 SubAgent：run/spawn/list/get/cancel；worktree 由父 Agent 显式 apply/discard。"
                    "默认只读；allow_fork 继承脱敏父上下文；模型覆盖仅 Host 解析；verify 仅 Host 固定检查；"
                    "写能力需配置开启，写回主工作区须父 Agent 处理。"
                ),
                argument_schema=json.dumps(
                    {
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": [
                                    "run",
                                    "spawn",
                                    "list",
                                    "get",
                                    "cancel",
                                    "apply_worktree",
                                    "discard_worktree",
                                    "list_worktrees",
                                ],
                            },
                            "task_id": {"type": "string"},
                            "batch_id": {"type": "string"},
                            "branch": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 200,
                            },
                            "strategy": {
                                "type": "string",
                                "enum": ["checkout", "merge"],
                            },
                            "cleanup": {"type": "boolean"},
                            "remove_branch": {"type": "boolean"},
                            "tasks": {
                                "type": "array",
                                "minItems": 1,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "description": {
                                            "type": "string",
                                            "minLength": 1,
                                        },
                                        "prompt": {
                                            "type": "string",
                                            "minLength": 1,
                                        },
                                        "subagent_type": {
                                            "type": "string",
                                            "enum": available_subagent_types,
                                        },
                                        "context": {
                                            "type": "string",
                                            "enum": ["fresh", "fork"],
                                        },
                                    },
                                    "required": [
                                        "description",
                                        "prompt",
                                        "subagent_type",
                                    ],
                                    "additionalProperties": False,
                                },
                            },
                            "max_concurrency": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 4,
                            },
                            "fail_fast": {"type": "boolean"},
                        },
                        "required": ["action"],
                        "additionalProperties": False,
                    },
                    ensure_ascii=False,
                ),
                # 委派本身只开放受限 profile；用户已将子任务人工确认收窄为
                # 删除与变更性 Git 操作，普通 read_only 分发不再重复弹窗。
                requires_confirmation=False,
                run=subagent,
            )
        )
    if memory_enabled:
        scoped_runners = (
            project_memory_search,
            project_memory_read,
            project_memory_expand_related,
            project_memory_write,
            session_memory_search,
            session_memory_read,
            session_memory_expand_related,
            session_memory_write,
            user_memory_search,
            user_memory_read,
            user_memory_expand_related,
            user_memory_write,
        )
        if any(runner is not None for runner in scoped_runners):
            if not all(runner is not None for runner in scoped_runners):
                raise ValueError("三类记忆工具必须完整提供 search/read/expand/write 绑定。")
            scope_definitions = (
                (
                    "project",
                    "项目级",
                    "当前项目的具体技术信息；存储与检索严格绑定当前工作区",
                    project_memory_search,
                    project_memory_read,
                    project_memory_expand_related,
                    project_memory_write,
                ),
                (
                    "session",
                    "会话级",
                    "当前会话的目标、约束、决策、文件、完成状态和后续事项；禁止跨会话读取",
                    session_memory_search,
                    session_memory_read,
                    session_memory_expand_related,
                    session_memory_write,
                ),
                (
                    "user",
                    "用户级",
                    "用户习惯、稳定偏好和用户纠错；跨项目、跨会话共享",
                    user_memory_search,
                    user_memory_read,
                    user_memory_expand_related,
                    user_memory_write,
                ),
            )
            for (
                prefix,
                label,
                purpose,
                search_runner,
                read_runner,
                expand_runner,
                write_runner,
            ) in scope_definitions:
                assert search_runner is not None
                assert read_runner is not None
                assert expand_runner is not None
                assert write_runner is not None
                tools.extend(
                    [
                        ToolDefinition(
                            name=f"{prefix}_memory_search",
                            description=(
                                f"搜索{label}记忆摘要。{purpose}。"
                                "不确定记忆目录时省略 candidate_directories 即搜索全部目录。"
                            ),
                            argument_schema=(
                                '{"query":"要检索的主题","reason":"为什么当前需要该作用域记忆",'
                                '"candidate_directories":["project-context/general"],"max_results":5}'
                            ),
                            requires_confirmation=False,
                            run=search_runner,
                        ),
                        ToolDefinition(
                            name=f"{prefix}_memory_read",
                            description=f"按 id 读取{label}记忆全文，并加深实际读取的记忆。",
                            argument_schema='{"memory_ids":["20260603-164500"]}',
                            requires_confirmation=False,
                            run=read_runner,
                        ),
                        ToolDefinition(
                            name=f"{prefix}_memory_expand_related",
                            description=f"沿关联目录扩展{label}记忆摘要，默认只展开一层。",
                            argument_schema=(
                                '{"memory_ids":["20260603-164500"],'
                                '"max_depth":1,"max_results":5}'
                            ),
                            requires_confirmation=False,
                            run=expand_runner,
                        ),
                        ToolDefinition(
                            name=f"{prefix}_memory_write",
                            description=f"写入或合并{label}记忆。仅允许写入：{purpose}。",
                            argument_schema=MEMORY_WRITE_ARGUMENT_SCHEMA,
                            requires_confirmation=False,
                            run=write_runner,
                        ),
                    ]
                )
        else:
            # 兼容旧调用方；LocalToolAgent 已始终提供三类作用域绑定。
            tools.extend(
                [
                    ToolDefinition(
                        name="memory_search",
                        description="按当前任务检索候选长期记忆摘要，不返回完整正文。",
                        argument_schema=(
                            '{"query":"用户偏好或项目主题","reason":"为什么当前需要查记忆",'
                            '"candidate_directories":["project-context/general"],"max_results":5}'
                        ),
                        requires_confirmation=False,
                        run=memory_search,
                    ),
                    ToolDefinition(
                        name="memory_read",
                        description="按记忆 id 读取完整长期记忆内容，并对实际读取的记忆加深回忆。",
                        argument_schema='{"memory_ids":["20260603-164500"]}',
                        requires_confirmation=False,
                        run=memory_read,
                    ),
                    ToolDefinition(
                        name="memory_expand_related",
                        description="沿已读记忆的关联目录扩展候选摘要，默认只展开一层关系。",
                        argument_schema='{"memory_ids":["20260603-164500"],"max_depth":1,"max_results":5}',
                        requires_confirmation=False,
                        run=memory_expand_related,
                    ),
                    ToolDefinition(
                        name="memory_write",
                        description="写入或合并具有长期价值的记忆，内容应短而准确。",
                        argument_schema=MEMORY_WRITE_ARGUMENT_SCHEMA,
                        requires_confirmation=False,
                        run=memory_write,
                    ),
                ]
            )
    return {
        tool.name: tool for tool in tools if tool.name not in disabled_tools
    }


def build_mcp_tools(
    *,
    mcp_manager: MCPClientManager,
    mcp_call: MCPToolRunner,
    mcp_read_resource: MCPResourceRunner,
    mcp_get_prompt: MCPPromptRunner,
) -> list[ToolDefinition]:
    """把 MCP Tool 元数据适配为 Chat Completions function tool。"""

    definitions: list[ToolDefinition] = []
    for meta in mcp_manager.registry.tools.values():
        definitions.append(
            ToolDefinition(
                name=meta.logical_name,
                description=f"{meta.description}（MCP Server：{meta.server_name}）",
                argument_schema=meta.argument_schema,
                requires_confirmation=meta.requires_confirmation,
                run=lambda arguments, tool_meta=meta: mcp_call(tool_meta, arguments),
            )
        )
    for meta in mcp_manager.registry.resources.values():
        definitions.append(
            ToolDefinition(
                name=f"mcp_read_resource__{meta.logical_uri}",
                description=f"读取 MCP Resource：{meta.logical_uri}（MCP Server：{meta.server_name}）",
                argument_schema="{}",
                requires_confirmation=False,
                run=lambda _arguments, logical_uri=meta.logical_uri: mcp_read_resource(logical_uri),
            )
        )
    for meta in mcp_manager.registry.prompts.values():
        definitions.append(
            ToolDefinition(
                name=f"mcp_get_prompt__{meta.logical_name}",
                description=f"获取 MCP Prompt：{meta.logical_name}（MCP Server：{meta.server_name}）",
                argument_schema='{"arguments": {}}',
                requires_confirmation=False,
                run=lambda arguments, logical_name=meta.logical_name: mcp_get_prompt(
                    logical_name,
                    arguments,
                ),
            )
        )
    return definitions


def workspace_tool_result(
    operation: Callable[[dict[str, Any]], str],
    arguments: dict[str, Any],
) -> ToolResult:
    """把 WorkspaceTools 文本型工具结果适配成 Agent ToolResult。

    命令工具会额外携带 ok 字段，仍留在 Agent 内单独处理；这里仅覆盖
    list/read/search/replace/write 这组成功即 ok=True 的文本工具，避免改变返回语义。
    """

    try:
        return ToolResult(ok=True, output=operation(arguments))
    except WorkspaceToolError as exc:
        return ToolResult(ok=False, output=str(exc))


def workspace_command_tool_result(
    operation: Callable[[dict[str, Any]], Any],
    arguments: dict[str, Any],
) -> ToolResult:
    """适配显式 Shell 命令结果，保留其自带的 ok/output 语义。"""

    try:
        result = operation(arguments)
    except WorkspaceToolError as exc:
        return ToolResult(ok=False, output=str(exc))
    return ToolResult(ok=result.ok, output=result.output)


def normalize_tool_call(
    tool_call: ToolCall,
    tools: dict[str, ToolDefinition],
    *,
    tool_name_aliases: dict[str, str] | None = None,
    argument_name_aliases: dict[str, str] | None = None,
) -> ToolCall:
    """在执行前归一化模型常见的工具名和参数名误写。"""

    aliases = tool_name_aliases or TOOL_NAME_ALIASES
    argument_aliases = argument_name_aliases or ARGUMENT_NAME_ALIASES
    raw_name = re.sub(r"\s+", "", tool_call.name.strip())
    name = normalize_tool_name(raw_name, tools, tool_name_aliases=aliases)
    return ToolCall(
        name=name,
        arguments=normalize_tool_arguments(
            name,
            tool_call.arguments,
            tools,
            argument_name_aliases=argument_aliases,
        ),
        id=tool_call.id,
        function_name=tool_call.function_name,
    )


def normalize_tool_name(
    raw_name: str,
    tools: dict[str, ToolDefinition],
    *,
    tool_name_aliases: dict[str, str] | None = None,
) -> str:
    """把 bashcommand/readimage 这类常见误写映射为当前 Host 真实工具名。"""

    name = re.sub(r"\s+", "", raw_name.strip())
    if name in tools:
        return name

    aliases = tool_name_aliases or TOOL_NAME_ALIASES
    alias = aliases.get(name) or aliases.get(normalize_identifier(name))
    if alias:
        return alias

    if tools:
        normalized_name = normalize_identifier(name)
        matches = [
            tool_name
            for tool_name in tools
            if normalize_identifier(tool_name) == normalized_name
        ]
        if len(matches) == 1:
            return matches[0]
    return name


def normalize_tool_arguments(
    tool_name: str,
    arguments: dict[str, Any],
    tools: dict[str, ToolDefinition],
    *,
    argument_name_aliases: dict[str, str] | None = None,
) -> dict[str, Any]:
    """按工具 schema 归一化参数名，兼容 startline/maxlines/tabId 等写法。

    同时做空串归一化：可选字段若传入空字符串或纯空白，视为未提供并丢弃，
    避免 minLength 等约束把"显式传空"误判为参数错误。必填字段的空串
    仍保留给后续 Schema 校验报错，不会静默通过。
    """

    aliases = argument_name_aliases or ARGUMENT_NAME_ALIASES
    canonical_keys = tool_argument_keys(tool_name, tools)
    normalized_to_key = {
        normalize_identifier(key): key
        for key in canonical_keys
    }
    optional_blank_ignored_keys = _tool_optional_blank_ignored_keys(tool_name, tools)
    normalized: dict[str, Any] = {}
    for key, value in arguments.items():
        canonical_key = key
        alias_key = aliases.get(key) or aliases.get(normalize_identifier(key))
        if alias_key in canonical_keys:
            canonical_key = alias_key
        else:
            canonical_key = normalized_to_key.get(normalize_identifier(key), key)
        if (
            canonical_key in optional_blank_ignored_keys
            and isinstance(value, str)
            and not value.strip()
        ):
            continue
        normalized[canonical_key] = value
    return normalized


def _tool_optional_blank_ignored_keys(
    tool_name: str,
    tools: dict[str, ToolDefinition],
) -> set[str]:
    """返回 schema 中非必填、且值为空串/纯空白时应视为未提供的字段名。

    规则：字段不在 required 中，且 schema 对字符串显式声明了 minLength >= 1。
    这类字段的语义是"提供就必须非空"，因此空串与未提供等价；
    未声明 minLength 的可选字段（如替换文本）保留原值，避免改变行为。
    """

    tool = tools.get(tool_name)
    if tool is None:
        return set()
    try:
        schema = json.loads(tool.argument_schema)
    except json.JSONDecodeError:
        return set()
    if not isinstance(schema, dict):
        return set()
    properties = schema.get("properties")
    if not isinstance(properties, dict):
        return set()
    required = schema.get("required")
    required_keys = set(required) if isinstance(required, list) else set()
    ignored: set[str] = set()
    for key, child in properties.items():
        if not isinstance(key, str) or not isinstance(child, dict):
            continue
        if key in required_keys:
            continue
        if child.get("type") != "string":
            continue
        min_length = child.get("minLength")
        if isinstance(min_length, int) and min_length >= 1:
            ignored.add(key)
    return ignored


def tool_argument_keys(tool_name: str, tools: dict[str, ToolDefinition]) -> set[str]:
    tool = tools.get(tool_name)
    if tool is None:
        return set()

    try:
        schema = json.loads(tool.argument_schema)
    except json.JSONDecodeError:
        return set()
    if not isinstance(schema, dict):
        return set()

    properties = schema.get("properties")
    if isinstance(properties, dict):
        return {key for key in properties if isinstance(key, str)}
    return {key for key in schema if isinstance(key, str)}


def normalize_identifier(value: str) -> str:
    return re.sub(r"[\s_-]+", "", value).lower()


def mcp_tool_result(
    mcp_manager: MCPClientManager,
    meta: MCPToolMeta,
    arguments: dict[str, Any],
) -> ToolResult:
    """执行 MCP Tool，并保持 Agent 原有的文本化输出格式。"""

    result = mcp_manager.call_tool(meta.logical_name, arguments)
    output_parts = [
        f"MCP Tool：{result.server_name}.{result.tool_name}",
        f"审计 ID：{result.audit_id}",
        f"耗时：{result.duration_ms} ms",
    ]
    if result.error_code:
        output_parts.append(f"错误码：{result.error_code}")
    if result.retryable:
        output_parts.append("可重试：是")
    output_parts.append(f"输出：\n{result.output}")
    full_output_parts = list(output_parts)
    full_output_parts[-1] = f"输出：\n{result.full_output or result.output}"
    return ToolResult(
        ok=result.ok,
        output="\n".join(output_parts),
        full_output="\n".join(full_output_parts),
    )


def mcp_resource_result(mcp_manager: MCPClientManager, logical_uri: str) -> ToolResult:
    """读取 MCP Resource，并保持 Agent 原有的文本化输出格式。"""

    result = mcp_manager.read_resource(logical_uri)
    output_parts = [
        f"MCP Resource：{result.server_name}:{result.uri}",
        f"耗时：{result.duration_ms} ms",
    ]
    if result.error_code:
        output_parts.append(f"错误码：{result.error_code}")
    if result.retryable:
        output_parts.append("可重试：是")
    output_parts.append(f"输出：\n{result.output}")
    full_output_parts = list(output_parts)
    full_output_parts[-1] = f"输出：\n{result.full_output or result.output}"
    return ToolResult(
        ok=result.ok,
        output="\n".join(output_parts),
        full_output="\n".join(full_output_parts),
    )


def mcp_prompt_result(
    mcp_manager: MCPClientManager,
    logical_name: str,
    arguments: dict[str, Any],
) -> ToolResult:
    """获取 MCP Prompt，并保持 Agent 原有的参数校验和输出格式。"""

    raw_arguments = arguments.get("arguments", {})
    if not isinstance(raw_arguments, dict):
        return ToolResult(ok=False, output="arguments 必须是 JSON 对象。")

    result = mcp_manager.get_prompt(logical_name, raw_arguments)
    output_parts = [
        f"MCP Prompt：{result.server_name}.{result.prompt_name}",
        f"耗时：{result.duration_ms} ms",
    ]
    if result.error_code:
        output_parts.append(f"错误码：{result.error_code}")
    if result.retryable:
        output_parts.append("可重试：是")
    output_parts.append(f"输出：\n{result.output}")
    full_output_parts = list(output_parts)
    full_output_parts[-1] = f"输出：\n{result.full_output or result.output}"
    return ToolResult(
        ok=result.ok,
        output="\n".join(output_parts),
        full_output="\n".join(full_output_parts),
    )


def read_required_string_list(arguments: dict[str, Any], key: str) -> list[str]:
    value = arguments.get(key)
    if not isinstance(value, list):
        return []
    return [item.strip() for item in value if isinstance(item, str) and item.strip()]


def read_optional_string_list(arguments: dict[str, Any], key: str) -> list[str] | None:
    value = arguments.get(key)
    if value is None:
        return None
    if not isinstance(value, list):
        return None
    return read_required_string_list(arguments, key)


def read_limited_int(
    arguments: dict[str, Any],
    key: str,
    *,
    default: int,
    maximum: int,
) -> int:
    value = arguments.get(key, default)
    if isinstance(value, bool):
        return default
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return default
    return max(1, min(maximum, parsed))


def json_tool_result(data: Any) -> ToolResult:
    return ToolResult(ok=True, output=json.dumps(data, ensure_ascii=False, indent=2))
