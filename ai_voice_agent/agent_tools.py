from __future__ import annotations

from typing import Any, Callable

from .agent_types import ToolDefinition, ToolResult
from .mcp import MCPClientManager, MCPToolMeta
from .workspace_tools import WorkspaceToolError


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
    run_command: ToolRunner,
    memory_search: ToolRunner,
    memory_read: ToolRunner,
    memory_expand_related: ToolRunner,
    memory_write: ToolRunner,
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
                description="读取 UTF-8 文本文件，可指定起始行和最多行数。",
                argument_schema='{"path": "main.py", "start_line": 1, "max_lines": 200}',
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
                name="run_command",
                description="以工作区为当前目录执行任意本地命令、脚本或 shell 片段。",
                argument_schema='{"command": "python -m py_compile main.py", "timeout_seconds": 120}',
                requires_confirmation=True,
                run=run_command,
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

    run_command 会额外携带 ok 字段，仍留在 Agent 内单独处理；这里仅覆盖
    list/read/search/replace/write 这组成功即 ok=True 的文本工具，避免改变返回语义。
    """

    try:
        return ToolResult(ok=True, output=operation(arguments))
    except WorkspaceToolError as exc:
        return ToolResult(ok=False, output=str(exc))
