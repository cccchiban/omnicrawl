"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Callable

from .types import ToolCall, ToolDefinition, ToolResult
from ..mcp import MCPClientManager, MCPToolMeta
from ..workspace_tools import DEFAULT_COMMAND_TIMEOUT_SECONDS, WorkspaceToolError


TOOL_NAME_ALIASES = {
    "bashcommand": "bash",
    "listfiles": "list_files",
    "monitorcommand": "monitor",
    "powershellcommand": "powershell",
    "readfile": "read_file",
    "searchtext": "search_text",
    "replacetext": "replace_text",
    "writefile": "write_file",
    "bbbrowser": "bb_browser_cli",
    "bbbrowsercli": "bb_browser_cli",
    "bb-browser": "bb_browser_cli",
    "bb-browser-cli": "bb_browser_cli",
    "bb_browser": "bb_browser_cli",
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
}

ToolRunner = Callable[[dict[str, Any]], ToolResult]
MCPToolRunner = Callable[[MCPToolMeta, dict[str, Any]], ToolResult]
MCPResourceRunner = Callable[[str], ToolResult]
MCPPromptRunner = Callable[[str, dict[str, Any]], ToolResult]


def build_agent_tools(
    *,
    mcp_manager: MCPClientManager,
    memory_enabled: bool,
    list_files: ToolRunner,
    read_file: ToolRunner,
    search_text: ToolRunner,
    replace_text: ToolRunner,
    write_file: ToolRunner,
    bash: ToolRunner,
    powershell: ToolRunner,
    monitor: ToolRunner,
    bb_browser_cli: ToolRunner,
    memory_search: ToolRunner,
    memory_read: ToolRunner,
    memory_expand_related: ToolRunner,
    memory_write: ToolRunner,
    display_html: ToolRunner,
    mcp_call: MCPToolRunner,
    mcp_read_resource: MCPResourceRunner,
    mcp_get_prompt: MCPPromptRunner,
) -> dict[str, ToolDefinition]:
    """构建 Agent 可用工具表，执行函数仍由 LocalToolAgent 绑定提供。"""

    tools = build_mcp_tools(
        mcp_manager=mcp_manager,
        mcp_call=mcp_call,
        mcp_read_resource=mcp_read_resource,
        mcp_get_prompt=mcp_get_prompt,
    )
    tools.extend(
        [
            ToolDefinition(
                name="list_files",
                description="列出工作区内的文件和目录，可选择递归。",
                argument_schema='{"path": ".", "recursive": false}',
                requires_confirmation=True,
                run=list_files,
            ),
            ToolDefinition(
                name="read_file",
                description=(
                    "读取 UTF-8 文本文件。可按 start_line/max_lines 读取行范围，"
                    "按 function_name 定位函数或方法，或按 text 定位首次文字片段及上下文。"
                ),
                argument_schema=(
                    '{"path":"main.py","start_line":1,"max_lines":200,'
                    '"function_name":"Class.method","text":"目标片段","context_lines":20}'
                ),
                requires_confirmation=True,
                run=read_file,
            ),
            ToolDefinition(
                name="search_text",
                description="在工作区文本文件中搜索正则或普通文本。",
                argument_schema='{"pattern": "class Agent", "path": ".", "case_sensitive": false, "max_results": 50}',
                requires_confirmation=True,
                run=search_text,
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
                    '{"path": ".agent_tmp/files/notes.md", "content": "...", '
                    '"mode": "overwrite"}'
                ),
                requires_confirmation=True,
                run=write_file,
            ),
            ToolDefinition(
                name="bash",
                description=(
                    "使用 Git Bash 在工作区执行 Bash 命令。"
                    "适合 POSIX Shell 语法、管道和 Bash 脚本；不应使用 PowerShell 语法。"
                ),
                argument_schema=(
                    '{"command":"git status --short | sed -n \'1,20p\'",'
                    f'"timeout_seconds":{DEFAULT_COMMAND_TIMEOUT_SECONDS}}}'
                ),
                requires_confirmation=True,
                run=bash,
            ),
            ToolDefinition(
                name="powershell",
                description=(
                    "使用 PowerShell 在 Windows 工作区执行命令，优先使用 PowerShell 7。"
                    "适合 PowerShell cmdlet、对象管道和 Windows 系统查询。"
                ),
                argument_schema=(
                    '{"command":"Get-ChildItem -File | Select-Object -First 20",'
                    f'"timeout_seconds":{DEFAULT_COMMAND_TIMEOUT_SECONDS}}}'
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
            ToolDefinition(
                name="bb_browser_cli",
                description=(
                    "调用 bb-browser CLI 控制真实浏览器。bb-browser CLI 会自动启动 "
                    "daemon 和受管浏览器；适合网页打开、tab 管理、snapshot、click、"
                    "fill、eval、fetch、network、site adapter 等浏览器任务。"
                ),
                argument_schema=(
                    '{"args": ["status", "--json"], "timeout_seconds": '
                    f"{DEFAULT_COMMAND_TIMEOUT_SECONDS}}}"
                ),
                requires_confirmation=True,
                run=bb_browser_cli,
            ),
            ToolDefinition(
                name="display_html",
                description=(
                    "向支持 HTML 的客户端提供网页或数据看板 artifact。"
                    "适合爬虫结果、表格、图表、网页预览等需要直观看的内容；"
                    "可直接传 html，或传工作区内 .html/.htm 文件路径。"
                ),
                argument_schema=(
                    '{"title": "数据预览", "html": "<!doctype html>...", '
                    '"path": ".agent_tmp/files/result.html"}'
                ),
                requires_confirmation=False,
                run=display_html,
            ),
        ]
    )
    if memory_enabled:
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
                    argument_schema=(
                        '{"memories":[{"content":"用户偏好中文交付摘要。",'
                        '"related_directories":["user-preferences/communication-style"],'
                        '"storage_directory":"user-preferences/communication-style",'
                        '"source_event":"本轮对话"}]}'
                    ),
                    requires_confirmation=False,
                    run=memory_write,
                ),
            ]
        )
    return {tool.name: tool for tool in tools}


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
    if raw_name not in tools:
        resource_fallback = mcp_resource_tool_fallback(raw_name, tools)
        if resource_fallback is not None:
            fallback_name, fallback_path = resource_fallback
            arguments = dict(tool_call.arguments)
            arguments.setdefault("path", fallback_path)
            return ToolCall(
                name=fallback_name,
                arguments=normalize_tool_arguments(
                    fallback_name,
                    arguments,
                    tools,
                    argument_name_aliases=argument_aliases,
                ),
                id=tool_call.id,
                function_name=tool_call.function_name,
            )

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
    """把 readfile/tablist 这类常见误写映射为当前 Host 真实工具名。"""

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


def mcp_resource_tool_fallback(
    requested_name: str,
    tools: dict[str, ToolDefinition],
) -> tuple[str, str] | None:
    """兼容模型把项目文档 Resource 工具名写成未注册具体 URI 的情况。"""

    prefix = "mcp_read_resource__"
    if not requested_name.startswith(prefix):
        return None

    logical_uri = requested_name[len(prefix) :]
    server_name, separator, resource_uri = logical_uri.partition(":")
    if not separator or not server_name or not resource_uri.startswith("project://"):
        return None

    relative_path = resource_uri[len("project://") :].strip().lstrip("/\\")
    if not relative_path or "\\" in relative_path:
        return None
    path = Path(relative_path)
    if path.is_absolute() or ".." in path.parts or path.suffix.lower() != ".md":
        return None

    fallback_name = f"{server_name}.workspace.read_file"
    if fallback_name not in tools:
        return None
    return fallback_name, relative_path


def normalize_tool_arguments(
    tool_name: str,
    arguments: dict[str, Any],
    tools: dict[str, ToolDefinition],
    *,
    argument_name_aliases: dict[str, str] | None = None,
) -> dict[str, Any]:
    """按工具 schema 归一化参数名，兼容 startline/maxlines/tabId 等写法。"""

    aliases = argument_name_aliases or ARGUMENT_NAME_ALIASES
    canonical_keys = tool_argument_keys(tool_name, tools)
    normalized_to_key = {
        normalize_identifier(key): key
        for key in canonical_keys
    }
    normalized: dict[str, Any] = {}
    for key, value in arguments.items():
        canonical_key = key
        alias_key = aliases.get(key) or aliases.get(normalize_identifier(key))
        if alias_key in canonical_keys:
            canonical_key = alias_key
        else:
            canonical_key = normalized_to_key.get(normalize_identifier(key), key)
        normalized[canonical_key] = value
    return normalized


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
    return ToolResult(ok=result.ok, output="\n".join(output_parts))


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
    return ToolResult(ok=result.ok, output="\n".join(output_parts))


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
    return ToolResult(ok=result.ok, output="\n".join(output_parts))


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
