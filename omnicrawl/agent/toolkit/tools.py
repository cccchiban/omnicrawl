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
    edit_file: ToolRunner,
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
                description='是什么：列出工作区指定路径下的文件和目录。怎么做：需要了解目录结构时使用；只需文件内容时不用，单文件路径会直接返回路径。怎样做：成功按行返回相对路径，目录末尾带 /；空目录返回‘目录为空’，超限追加截断提示；失败返回文本错误。建议：先非递归定位，再按需递归；结果超限时缩小范围。',
                argument_schema='{"path": ".", "recursive": false}',
                requires_confirmation=True,
                run=list,
            ),
            *(
                [
                    ToolDefinition(
                        name="find",
                        description='是什么：按名称或相对路径查找工作区文件和目录，不读取内容。怎么做：不知道目标路径、需要 glob 模式或筛选 kind 时使用；已知精确路径或要搜内容时不用。怎样做：成功按行返回路径，目录末尾带 /；无结果返回‘未找到匹配结果’；超限保留前 max_results 并给出完整结果路径；失败返回文本错误。建议：先用具体 path/pattern 缩小范围，再用 read 读取内容；不要用 find 替代 grep。',
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
                description='是什么：读取工作区或内置文档中的 UTF-8 文本，并支持按行、函数或片段定位。怎么做：需要查看内容、实现或上下文时使用；只想按名称定位时不用；function_name 与 text 不可同时传。怎样做：普通读取返回‘行号: 内容’及 End of file 或 Showing lines X-Y of Z footer；函数/片段定位返回定位标题和带行号内容，超限追加续读提示；超长行标记截断，失败返回文本错误。建议：先读小范围，按普通读取的 footer 用 start_line 续读；用 function_name/text 缩小上下文，降低无关输出。',
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
                        description='是什么：读取本机 PNG、JPEG、WebP 或 GIF 图片并提供视觉附件。怎么做：需要分析图片内容时使用；只需文件名或图片 URL 时不用；path 可使用工作区相对路径或本机绝对路径，不支持 URL。怎样做：成功返回 JSON：path、media_type、bytes、detail、vision_attachment；同时附加图片；失败返回文本错误。建议：先确定图片路径，再按模型能力选择 detail；超大图片会增加内存占用和请求延迟。',
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
                description='是什么：在工作区文本中按正则或精确子串搜索内容。怎么做：需要查找代码、配置或引用位置时使用；只按文件名查找时用 find；path 不要指向用户目录或文件系统根。怎样做：普通模式返回 path:line: text；count 返回文件计数，files_with_matches 只返回路径；长行标记截断，超限给出完整结果路径；失败返回文本错误。建议：优先限定 path、include 和 pattern；用 context_lines 查看邻近代码，宽泛正则会增加输出和耗时。',
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
                        description='是什么：在 Bing、DuckDuckGo 或雅虎搜索公开网页。怎么做：需要发现公开资料时使用；已知 URL 要读页面时用 fetcher；遇验证码或异常流量不继续尝试。怎样做：成功返回来源、查询、耗时、数量，以及编号标题、URL 和可选摘要；失败返回文本错误。建议：用具体关键词和合适 engine/language；搜索结果是线索，需用 fetcher 获取正文并核实来源。',
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
                        description='是什么：使用浏览器指纹抓取用户指定的一个或多个 URL，并提取正文或返回 HTML。怎么做：已有明确 URL 且需要页面内容时使用；需要发现 URL 时用 web_search；不执行 JS、不绕过验证码。怎样做：每个 URL 返回 url、状态、最终地址、可选标题和内容；失败条目返回 url 与 error；结果按输入顺序，内容受 max_chars 限制。建议：默认正文最省上下文；仅在需要源码时用 max_html；insecure 只用于可信内网自签名证书，避免削弱 TLS 校验。',
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
                        description='是什么：按 prompt 生成图片，或用本地 image 作为参考图编辑图片。怎么做：需要创建或修改图像资产时使用；只需分析已有图片时用 read_image；prompt 必填，image 非空即进入编辑。怎样做：成功返回生成数量、模型、每张图片保存路径和字节数；失败返回文本错误；图片写入 path 或默认临时目录。建议：prompt 明确主体、风格和约束；n、size、quality、output_format 可覆盖设置，参数会影响耗时、成本和画质。',
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
                        description='是什么：把文本合成为 WAV 语音，支持设置中的内置音色或参考音频克隆。怎么做：需要生成语音文件时使用；只需文字分析或朗读建议时不用；先在 /settings → TTS 启用并准备模型。怎样做：成功返回 JSON：ok、audio_path、sample_rate、duration_seconds、voice、text_chunks；失败返回 JSON：ok=false、error。建议：text 传最终朗读稿，长文本可自动分块；prompt_audio 会改变音色来源，path 决定文件位置，配置可控制自动播放。',
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
                name="Edit_file",
                description='是什么：在单个 UTF-8 文本文件中按字面替换 old_text。怎么做：需要小范围、明确目标的编辑时使用；不适合大范围重写或不确定匹配内容时使用；old_text 必须非空。怎样做：成功返回‘已修改 path，替换 N 处’，只展示首个替换位置前后各 2 行（文件边界除外）的修改后内容，格式为‘行号: 内容’；省略 count 时必须恰好匹配 1 处，匹配 0 处或多处均返回错误码、原因和调整建议且不写入；显式 count=1 替换第 1 处，count=0 替换全部。建议：先 read 确认原文并保留足够上下文；多处匹配时提供 count 或补充上下文使其唯一，错误码可指导重试。',
                argument_schema='{"path": "main.py", "old_text": "...", "new_text": "...", "count": 1}',
                requires_confirmation=True,
                run=edit_file,
            ),
            ToolDefinition(
                name="write_file",
                description='是什么：以 overwrite 或 append 模式写入 UTF-8 文本文件。怎么做：需要创建、覆盖或追加文本文件时使用；只修改局部内容时用 Edit_file；临时文件优先放 .omnicrawl/.agent_tmp。怎样做：成功返回‘已写入/追加 path，字符数：N’；失败返回文本错误；overwrite 会替换原内容，append 保留原内容。建议：覆盖前先 read 核对目标；明确使用 mode，path 使用工作区相对路径以减少误写范围。',
                argument_schema=(
                    '{"path": ".omnicrawl/.agent_tmp/files/notes.md", "content": "...", '
                    '"mode": "overwrite"}'
                ),
                requires_confirmation=True,
                run=write_file,
            ),
            ToolDefinition(
                name="bash",
                description='是什么：在工作区用 Git Bash 执行 POSIX Shell 命令。怎么做：需要运行测试、构建或 Unix 命令时使用；PowerShell 语法用 powershell，结构化 Git 操作用 git；主命令不可自行裁剪输出。怎样做：返回退出码、Shell、stdout/stderr；可附 diagnostic_command 的独立结果；超长输出保留首尾并给出日志路径，失败或超时返回错误。建议：主命令保留完整验证过程，日志筛选放 diagnostic_command；pipefail 保证管道上游失败不被掩盖。',
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
                description='是什么：在工作区用 PowerShell 执行命令。怎么做：需要 Windows 命令、测试或构建时使用；POSIX Shell 用 bash，结构化 Git 操作用 git；主命令不可用 Select-Object/Select-String 裁剪输出。怎样做：返回退出码、Shell、stdout/stderr；可附 diagnostic_command 的独立结果；超长输出保留首尾并给出日志路径，失败或超时返回错误。建议：只传 PowerShell 语法并保留真实退出码；诊断筛选放独立 diagnostic_command，避免把验证结果截断。',
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
                description='是什么：启动、轮询、列出或停止由 Agent 管理的后台命令。怎么做：需要服务持续运行或观察长命令时使用；短命令直接用 bash/powershell；poll/stop 必须使用 start 返回的 monitor_id。怎样做：start 返回 monitor_id、Shell、状态和下一游标；poll/stop 返回状态、退出码、下一游标和事件；list 返回任务摘要；失败返回错误文本。建议：start 后用 poll(cursor) 增量读取并保存下一游标；任务会随 Agent 关闭或工作区切换终止，避免依赖长期存活。',
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
                        description='是什么：在当前工作区直接执行受约束的结构化 Git 子命令。怎么做：需要查看状态、差异、日志或提交变更时使用；不要用 bash 拼 Git；push、merge、reset --hard、clean 等高危动作需额外审查。怎样做：成功返回有界的 Git stdout 文本；失败返回错误文本和退出原因；不返回固定 JSON；paths 必须在工作区内。建议：优先 status/diff 确认范围，再执行变更；commit 提供 message；禁止 --git-dir、--work-tree、--no-verify 及全局/系统配置参数。',
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
                        description='是什么：搜索独立于当前项目的工作知识库笔记摘要。怎么做：需要跨项目查找工作记录、决策或研究线索时使用；只查当前项目代码或完整笔记时不用，先搜摘要再 kb_read。怎样做：成功返回 JSON 数组，每项含 path、title、project、type、status、tags、snippet、score；空结果为 []，失败返回文本错误。建议：用 project/tags/type/status 缩小范围；摘要只用于筛选，命中后用 kb_read 读取全文，避免无关内容污染上下文。',
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
                        description='是什么：读取工作知识库中的一篇 Markdown 笔记全文。怎么做：kb_search 命中后需要完整正文时使用；只需定位笔记时不用；path 必须是知识库内相对路径，可省略 .md。怎样做：成功返回原始 Markdown 文本；超过 max_chars 时追加‘已截断’提示；不存在、越界或非 UTF-8 时返回文本错误。建议：先 kb_search 再 kb_read，并按需要设置 max_chars；不要把知识库路径当作工作区路径使用。',
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
                        description='是什么：新建或更新知识库 Markdown 笔记，并维护 frontmatter 与索引。怎么做：需要长期保存已确认的工作记录时使用；临时内容或项目代码不要写入知识库；create 仅新建，overwrite 覆盖正文，append 追加正文。怎样做：成功返回 JSON：note（path、title、created、updated、project、type、status、tags）和 mode；失败返回文本错误。建议：正文短而可独立理解，补充 project/tags/type/status 便于检索；overwrite 前先 kb_read，避免覆盖有价值内容。',
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
                        description='是什么：向已有知识库笔记追加正文。怎么做：需要补充同一笔记的新信息时使用；新建笔记用 kb_write；不需要修改 frontmatter 时使用。怎样做：成功返回 JSON：note 元数据和 mode=append；失败返回文本错误；只更新 updated，不改其他 frontmatter 字段。建议：追加独立、简短且已确认的内容；追加前先 kb_read 确认目标，避免把不同主题混入同一笔记。',
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
                        description='是什么：列出知识库笔记或按元数据筛选笔记。怎么做：需要浏览目录、核对元数据或批量定位笔记时使用；只需按关键词搜索时用 kb_search；不返回正文。怎样做：成功返回 JSON 数组，每项含 path、title、created、updated、project、type、status、tags；无结果为 []，失败返回文本错误。建议：优先用 project/tags/type/status/path 过滤并控制 max_results；找到目标后用 kb_read 获取正文。',
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
                     "是什么：维护当前任务的紧凑执行清单。"
                     "怎么做：多步骤任务开始和每步完成时使用；单步任务或无需展示进度时不用；todos 为空表示清除计划。"
                     "怎样做：成功返回 JSON：updated 和 todos 数组，每项含 id、step、completed；最多保留 20 项，非法项会被忽略；参数错误返回文本错误。"
                     "建议：首次提交完整列表，之后只更新 completed；step 短而可验证，避免把计划正文重复写入回复。"
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
                     "是什么：按当前有效摘要授权的事件 ID 恢复本会话的精确证据。"
                     "怎么做：摘要缺少细节且已知 source event ID 时使用；没有有效摘要、只想浏览历史或不知道 ID 时不用；不接受 Session ID 或任意 artifact 路径。"
                     "怎样做：成功或部分成功返回 JSON：schema_version、ok、summary_event_id、requested_count、items、diagnostics、truncated、budget、estimated_tokens；未授权/缺失返回诊断项。"
                     "建议：先从摘要读取已引用 ID，单次少量恢复；结果可能按 token 预算截断，按 diagnostics 和 truncated 决定是否分批重试。"
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
                    description='是什么：在 Windows 枚举、读取或激活顶层窗口。怎么做：需要发现窗口或确认窗口状态时使用；非 Windows、只需控件定位或不应改变前台窗口时不用；activate 不绕过焦点保护。怎样做：list 返回 JSON：action、matched_count、truncated、windows；get/activate 返回 action、window（含 handle、title、class_name、process_id、状态和 bounds）；失败返回文本错误。建议：先 list 取得稳定的 window_handle，再 get 或 activate；用标题/类名过滤减少误选，激活失败应由用户手动切换。',
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
                    description='是什么：在 Windows 窗口内发现 UI Automation 控件并执行语义化操作。怎么做：需要操作按钮、输入框或选择控件时使用；非 Windows 或只需鼠标坐标时不用；非 list 必须提供定位条件，多匹配需 index。怎样做：list 返回 JSON：action、matched_count、truncated、controls；其他动作返回 action、index、target；失败返回文本错误，不返回控件外的任意脚本结果。建议：先 list 再用 name/automation_id/control_type 精确定位，优先语义定位而非坐标；set_value 仅用于字符串值。',
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
                    description='是什么：在 Windows 发送受限的鼠标、滚轮、按键、组合键或 Unicode 文本输入。怎么做：目标应用明确且需要真实输入时使用；能用 UI Automation 时优先不用坐标输入；click/move 必须有虚拟桌面坐标。怎样做：成功返回 JSON，按动作包含 action、坐标、按键、clicks、keys 或 text_length；type_text 不回显文本；失败返回文本错误。建议：先确认前台窗口和坐标，再执行最小动作；敏感文本不回显但仍会进入目标应用，输入后不要重复发送。',
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
                    description='是什么：读取、写入或清空 Windows Unicode 文本剪贴板。怎么做：需要在应用间传递文本时使用；只需控件内设置值时优先用 windows_control；read_text 之外不要假设有二进制剪贴板支持。怎样做：read_text 返回 JSON：action、text、total_chars、truncated；write_text 返回 action、text_length；clear 仅返回 action；失败返回文本错误。建议：读取时设置合理 max_chars；写入内容不会回显到结果或会话参数，但会改变用户剪贴板，执行前确认目标。',
                    argument_schema=(
                        '{"action":"read_text|write_text|clear","text":"仅 write_text",'
                        '"max_chars":8000}'
                    ),
                    requires_confirmation=True,
                    run=windows_clipboard,
                ),
                ToolDefinition(
                    name="windows_screenshot",
                    description='是什么：用 Windows GDI 截取整个桌面、区域或指定窗口，并提供 PNG 视觉附件。怎么做：需要观察桌面或窗口画面时使用；只需窗口元数据时用 windows_window；window 截图先取得 handle，最小化、越界或受保护内容可能失败。怎样做：成功返回 JSON：target、path、source_bounds、image（media_type、width、height、bytes、scaled），并附加 PNG；失败返回文本错误。建议：优先截取最小必要区域并设置合理 max_dimension；截图会保存到临时目录且可能含敏感画面，提供给模型前确认范围。',
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
                description='是什么：按受限 profile 调度 SubAgent，支持同步/后台任务、查询取消和 worktree 控制。怎么做：任务可独立拆分、需要并行分析或隔离修改时使用；简单问题不用；模型不能指定模型，写回主工作区必须显式 apply。怎样做：成功返回 JSON；run 含 batch_id/status/results，spawn 含 batch_id/task_ids/status，list/get/cancel/worktree 返回对应安全摘要；失败含 error.code/message。建议：description 说明目标，prompt 写完整任务，合理限制并发；默认只读，优先 worktree 隔离，先检查结果再 apply/discard。',
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
                                 f"是什么：搜索{label}记忆摘要。"
                                 f"怎么做：需要恢复{purpose}时使用；只需完整正文时不用，先搜索再按 id 读取；不确定目录可省略 candidate_directories。"
                                 "怎样做：成功返回 JSON 数组，每项含 id、summary、storage_directory、related_directories、timestamp；空结果为 []，失败返回文本错误。"
                                 "建议：query 写具体主题并提供 reason；用 candidate_directories 缩小范围，命中后再调用对应 memory_read，减少上下文。"
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
                            description=(
                                 f"是什么：读取{label}记忆的完整正文并加深实际读取项。"
                                 f"怎么做：已通过对应 memory_search 命中且需要细节时使用；只有模糊主题时不用；memory_ids 必须来自同一作用域。"
                                 "怎样做：成功返回 JSON 数组，每项含 id、timestamp、related_directories、content；缺失 ID 不出现在数组中，失败返回文本错误。"
                                 "建议：只读取与当前决策相关的 ID，避免一次加载过多正文；读取后再决定是否 expand_related。"
                             ),
                            argument_schema='{"memory_ids":["20260603-164500"]}',
                            requires_confirmation=False,
                            run=read_runner,
                        ),
                        ToolDefinition(
                            name=f"{prefix}_memory_expand_related",
                            description=(
                                 f"是什么：沿关联目录扩展{label}记忆的候选摘要。"
                                 f"怎么做：初次搜索未覆盖相关背景、且已有 memory_ids 时使用；没有已知 ID 或需要全文时不用。"
                                 "怎样做：成功返回 JSON 数组，每项含 id、summary、storage_directory、related_directories、timestamp；按 max_depth/max_results 限制，失败返回文本错误。"
                                 "建议：默认一层、少量结果即可；先 expand_related 找候选，再按需 memory_read，扩展过深会增加噪声。"
                             ),
                            argument_schema=(
                                '{"memory_ids":["20260603-164500"],'
                                '"max_depth":1,"max_results":5}'
                            ),
                            requires_confirmation=False,
                            run=expand_runner,
                        ),
                        ToolDefinition(
                            name=f"{prefix}_memory_write",
                            description=(
                                 f"是什么：写入或合并{label}长期记忆。"
                                 f"怎么做：只有信息已确认且具有长期复用价值时使用；临时进度、完整对话、凭据或不属于{purpose}的内容不用写。"
                                 "怎样做：成功返回 JSON 数组，每项含 id、timestamp、related_directories、content；memories 为空或字段类型错误时返回文本错误。"
                                 "建议：content 短小、准确、可独立理解，补充 related_directories；批量写入前去重，避免污染后续检索。"
                             ),
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
                        description='是什么：按当前任务检索候选长期记忆摘要。怎么做：需要查找项目背景或用户偏好时使用；不需要长期上下文时不用，且不直接返回正文。怎样做：成功返回 JSON 数组，每项含 id、summary、storage_directory、related_directories、timestamp；失败返回文本错误。建议：先 search 再 memory_read，query 具体并说明 reason；只读取真正相关的 ID，减少上下文噪声。',
                        argument_schema=(
                            '{"query":"用户偏好或项目主题","reason":"为什么当前需要查记忆",'
                            '"candidate_directories":["project-context/general"],"max_results":5}'
                        ),
                        requires_confirmation=False,
                        run=memory_search,
                    ),
                    ToolDefinition(
                        name="memory_read",
                        description='是什么：按记忆 ID 读取完整长期记忆内容并加深回忆。怎么做：已有候选 ID 且需要细节时使用；只有主题没有 ID 时先 memory_search。怎样做：成功返回 JSON 数组，每项含 id、timestamp、related_directories、content；缺失项不返回，失败返回文本错误。建议：限制 memory_ids 数量并按需读取，避免把无关正文带入当前决策。',
                        argument_schema='{"memory_ids":["20260603-164500"]}',
                        requires_confirmation=False,
                        run=memory_read,
                    ),
                    ToolDefinition(
                        name="memory_expand_related",
                        description='是什么：沿已读记忆的关联目录扩展候选摘要。怎么做：已有记忆 ID 但需要查找相关背景时使用；没有已读 ID 或需要完整正文时不用。怎样做：成功返回 JSON 数组，每项含 id、summary、storage_directory、related_directories、timestamp；失败返回文本错误。建议：默认一层、小批量扩展；命中后再 memory_read，深度或数量过大会增加噪声。',
                        argument_schema='{"memory_ids":["20260603-164500"],"max_depth":1,"max_results":5}',
                        requires_confirmation=False,
                        run=memory_expand_related,
                    ),
                    ToolDefinition(
                        name="memory_write",
                        description='是什么：写入或合并具有长期复用价值的长期记忆。怎么做：只保存已确认的稳定事实、偏好或决策时使用；临时进度、凭据和未经验证结论不用写。怎样做：成功返回 JSON 数组，每项含 id、timestamp、related_directories、content；参数无效时返回文本错误。建议：content 短而可独立理解，先检查是否已有重复记忆；相关目录有助于后续检索。',
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
        return ToolResult(
            ok=False,
            output=exc.formatted_message(),
            error_code=exc.code,
            retryable=exc.retryable,
        )


def workspace_command_tool_result(
    operation: Callable[[dict[str, Any]], Any],
    arguments: dict[str, Any],
) -> ToolResult:
    """适配显式 Shell 命令结果，保留其自带的 ok/output 语义。"""

    try:
        result = operation(arguments)
    except WorkspaceToolError as exc:
        return ToolResult(
            ok=False,
            output=exc.formatted_message(),
            error_code=exc.code,
            retryable=exc.retryable,
        )
    return ToolResult(
        ok=result.ok,
        output=result.output,
        error_code=getattr(result, "error_code", None),
        retryable=getattr(result, "retryable", False),
    )


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
