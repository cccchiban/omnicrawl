"""工具表构建与 system prompt 渲染：内置与 MCP 工具面。"""
from __future__ import annotations

import re
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Sequence
from ...toolkit.tools import (
    build_agent_tools,
    build_mcp_tools,
    mcp_prompt_result,
    mcp_resource_result,
    mcp_tool_result,
    normalize_tool_call,
    public_tool_arguments,
    TODO_TOOL_NAME,
    workspace_command_tool_result,
    workspace_tool_result,
)
from ...runtime.llm_protocol import (
    AgentLLMProtocol,
    AgentProtocolError,
    build_extra_body,
    chat_completion_tools,
    function_name_for_tool,
    resolve_tool_name_from_hashed_function_name,
    tool_name_from_function_name,
)
from ....knowledge import KnowledgeBase, KnowledgeBaseError
from ...context.prompt_context import (
    build_context_messages,
    build_project_instructions_messages,
    build_prompt_cache_identity,
    build_skill_context_message,
    build_system_prompt,
)
from ...types import AgentModelReply, ToolCall, ToolDefinition, ToolResult

from ..shared import (
    AgentError,
    SYSTEM_PROMPT_FILE,
)


_MODE_NAME_PATTERN = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")


class ToolBuildingMixin:
    """工具表构建与 system prompt 渲染：内置与 MCP 工具面。"""

    def _build_tools(self) -> dict[str, ToolDefinition]:
        windows_desktop = self._windows_desktop_toolbox()
        disabled_tools = frozenset(
            getattr(getattr(self, "config", None), "disabled_tools", ())
        )
        tools = build_agent_tools(
            mcp_manager=self._mcp_manager,
            memory_enabled=getattr(
                self,
                "_project_memory_store",
                getattr(self, "_memory_store", None),
            ) is not None,
            list=self._tool_list,
            find=self._tool_find,
            read=self._tool_read,
            read_image=self._tool_read_image,
            grep=self._tool_grep,
            web_search=self._tool_web_search,
            fetcher=self._tool_fetcher,
            image_gen=self._tool_image_gen,
            tts=self._tool_tts_synthesize,
            edit_file=self._tool_edit_file,
            write_file=self._tool_write_file,
            bash=self._tool_bash,
            powershell=self._tool_powershell,
            monitor=self._tool_monitor,
            git=self._tool_git,
            memory_search=self._tool_memory_search,
            memory_read=self._tool_memory_read,
            memory_expand_related=self._tool_memory_expand_related,
            memory_write=self._tool_memory_write,
            project_memory_search=self._tool_project_memory_search,
            project_memory_read=self._tool_project_memory_read,
            project_memory_expand_related=self._tool_project_memory_expand_related,
            project_memory_write=self._tool_project_memory_write,
            session_memory_search=self._tool_session_memory_search,
            session_memory_read=self._tool_session_memory_read,
            session_memory_expand_related=self._tool_session_memory_expand_related,
            session_memory_write=self._tool_session_memory_write,
            user_memory_search=self._tool_user_memory_search,
            user_memory_read=self._tool_user_memory_read,
            user_memory_expand_related=self._tool_user_memory_expand_related,
            user_memory_write=self._tool_user_memory_write,
            kb_search=self._tool_kb_search,
            kb_read=self._tool_kb_read,
            kb_write=self._tool_kb_write,
            kb_append=self._tool_kb_append,
            kb_list=self._tool_kb_list,
            mcp_call=self._tool_mcp_call,
            mcp_read_resource=self._tool_mcp_read_resource,
            mcp_get_prompt=self._tool_mcp_get_prompt,
            evidence_recall=(
                self._tool_recall_session_evidence
                if getattr(self, "_session_store", None) is not None
                else None
            ),
            subagent=(
                self._tool_subagent
                if getattr(self, "_subagent_coordinator", None) is not None
                else None
            ),
            subagent_types=(
                self._subagent_coordinator.available_agent_types()
                if getattr(self, "_subagent_coordinator", None) is not None
                else ()
            ),
            update_todos=self._tool_update_todos,
            windows_window=(windows_desktop.run_window if windows_desktop is not None else None),
            windows_control=(windows_desktop.run_control if windows_desktop is not None else None),
            windows_input=(windows_desktop.run_input if windows_desktop is not None else None),
            windows_clipboard=(
                windows_desktop.run_clipboard if windows_desktop is not None else None
            ),
            windows_screenshot=(
                windows_desktop.run_screenshot if windows_desktop is not None else None
            ),
            disabled_tools=disabled_tools,
        )
        return tools

    def _build_mcp_tools(self) -> list[ToolDefinition]:
        return build_mcp_tools(
            mcp_manager=self._mcp_manager,
            mcp_call=self._tool_mcp_call,
            mcp_read_resource=self._tool_mcp_read_resource,
            mcp_get_prompt=self._tool_mcp_get_prompt,
        )

    @property
    def active_mode(self) -> str:
        """返回当前 Agent 的活动模式；未启用模式时为空字符串。"""

        return str(getattr(self, "_active_mode_name", "") or "")

    def activate_mode(self, mode: str) -> str:
        """加载并启用包内模式模板，模式切换成功后才更新 Agent 状态。"""

        if not isinstance(mode, str):
            raise AgentError("模式名称必须是字符串。")
        normalized = mode.strip().casefold()
        if not _MODE_NAME_PATTERN.fullmatch(normalized):
            raise AgentError("模式名称只能使用小写字母、数字和单连字符。")

        prompt_path = Path(__file__).resolve().parents[3] / "templates" / f"{normalized}.md"
        try:
            prompt = prompt_path.read_text(encoding="utf-8").strip()
        except UnicodeDecodeError as exc:
            raise AgentError(f"模式模板 {normalized}.md 必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise AgentError(f"读取模式模板 {normalized}.md 失败：{exc}") from exc
        if not prompt:
            raise AgentError(f"模式模板 {normalized}.md 不能为空。")

        self._active_mode_name = normalized
        self._active_mode_prompt = prompt
        return normalized

    def _system_prompt(self) -> str:
        """返回基础 system prompt，并在末尾追加当前活动模式提示词。"""

        template = build_system_prompt(self._system_prompt_template)
        # TTS 指令由当前回合在首次模型请求前准备；工具结果后的后续请求
        # 不重复注入，但仍保留 tts_synthesize 工具可用。
        tts_instruction = getattr(self, "_active_tts_instruction", "")
        if tts_instruction:
            template = f"{template}\n\n{tts_instruction}"
        confirmation_instruction = getattr(
            self,
            "_active_user_confirmation_instruction",
            "",
        )
        if confirmation_instruction:
            template = f"{template}\n\n{confirmation_instruction}"
        mode_prompt = str(getattr(self, "_active_mode_prompt", "") or "").strip()
        if mode_prompt:
            mode_name = self.active_mode or "active"
            template = (
                f'{template}\n\n<active_mode_prompt name="{mode_name}">\n'
                f"{mode_prompt}\n"
                "</active_mode_prompt>"
            )
        return template

    def _render_system_prompt_template(self, tool_lines: str) -> str:
        """兼容旧测试入口；新链路不再向 system prompt 注入动态工具清单。"""

        _ = tool_lines
        return self._system_prompt()

    def _agent_temp_dir_display(self) -> str:
        temp_workspace = getattr(self, "_temp_workspace", None)
        return temp_workspace.display_path if temp_workspace is not None else ".omnicrawl/.agent_tmp"

    def _load_system_prompt_template(self) -> str:
        """读取独立系统提示词模板，避免把长规范硬编码在 Python 代码里。"""

        # system_prompt.md 固定在 agent 包根；本文件位于 controllers/tools/ 下两层。
        prompt_path = Path(__file__).resolve().parents[2] / SYSTEM_PROMPT_FILE
        try:
            template = prompt_path.read_text(encoding="utf-8").strip()
        except UnicodeDecodeError as exc:
            raise AgentError(f"{SYSTEM_PROMPT_FILE} 必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise AgentError(f"读取 {SYSTEM_PROMPT_FILE} 失败：{exc}") from exc

        try:
            return build_system_prompt(template)
        except ValueError as exc:
            raise AgentError(str(exc)) from exc

    def _get_knowledge_base(self) -> KnowledgeBase:
        """返回跨项目工作知识库实例（惰性创建）。"""

        if self._knowledge_base is None:
            self._knowledge_base = KnowledgeBase()
        return self._knowledge_base
