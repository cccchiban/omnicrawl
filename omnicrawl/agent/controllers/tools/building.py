"""工具表构建与 system prompt 渲染：内置/路由/MCP 工具面与提示词模板。"""
from __future__ import annotations

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


class ToolBuildingMixin:
    """工具表构建与 system prompt 渲染：内置/路由/MCP 工具面与提示词模板。"""

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
            replace_text=self._tool_replace_text,
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
                and bool(
                    getattr(
                        getattr(self.config, "context_compaction", None),
                        "enabled",
                        False,
                    )
                )
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
        if getattr(getattr(self, "config", None), "router_enabled", False):
            # 自优化工具：路由状态查看 / 手动 override / 模式隔离子代理。
            tools.update(self._router_dev_tools())
        return tools

    def _router_dev_tools(self) -> dict[str, ToolDefinition]:
        """任务路由自优化工具（dsh-router-standard 的 dev_router_* 移植）。"""

        runtime = self._router_runtime

        def _session_id() -> str:
            return self._session_facade().current_session_id()

        def _status(_arguments: dict[str, Any]) -> ToolResult:
            session_id = _session_id()
            if not session_id:
                return ToolResult(ok=True, output="no agent session")
            return ToolResult(
                ok=True,
                output=runtime.status_text(
                    session_id,
                    model_id=getattr(self.config.llm, "model", ""),
                    events=self._router_session_events(session_id),
                ),
            )

        def _set_mode(arguments: dict[str, Any]) -> ToolResult:
            session_id = _session_id()
            if not session_id:
                return ToolResult(ok=True, output="no agent session")
            mode_token = arguments.get("mode")
            message = runtime.set_mode(session_id, mode_token)
            # override 持久化为 router_override 事件（含 auto 清除），resume 时
            # 由 RouterRuntime.restore_from_events 恢复，override 不随进程丢失。
            try:
                self._append_session_event(
                    "router_override", {"mode": str(mode_token or "")}
                )
            except Exception:  # noqa: BLE001 - 状态工具写入失败不阻断主流程
                pass
            return ToolResult(ok=True, output=message)

        def _mode_subagent(arguments: dict[str, Any]) -> ToolResult:
            return ToolResult(ok=True, output=self._router_mode_subagent(arguments))

        return {
            "dev_router_status": ToolDefinition(
                name="dev_router_status",
                description=(
                    "显示当前会话的思维模式路由状态：mode、band、persona、首轮核心工具、"
                    "测试抑制、override 与晋升状态。"
                ),
                argument_schema=(
                    '{"type": "object", "properties": {}, "required": [], '
                    '"additionalProperties": false}'
                ),
                requires_confirmation=False,
                run=_status,
            ),
            "dev_router_mode": ToolDefinition(
                name="dev_router_mode",
                description=(
                    "设置当前会话的思维模式：spec（计划优先）/ weak（内部路由）/ "
                    "mixed（transition 陷阱，仅显式选择）/ react（执行者）。"
                    "接受带名、0-100 或 0.0-1.0；auto 清除 override。下一次请求生效。"
                ),
                argument_schema=(
                    '{"type": "object", "properties": {"mode": {"type": "string", '
                    '"minLength": 1, "description": "spec / weak / mixed / react，'
                    '或 0-100、0.0-1.0、auto"}}, "required": ["mode"], '
                    '"additionalProperties": false}'
                ),
                requires_confirmation=False,
                run=_set_mode,
            ),
            "dev_mode_subagent": ToolDefinition(
                name="dev_mode_subagent",
                description=(
                    "在隔离的新上下文（独立 system prompt）中以不同思维模式运行一个任务，"
                    "不污染当前会话轨迹。返回模式子代理的回答文本（截断到 3000 字符）。"
                ),
                argument_schema=(
                    '{"type": "object", "properties": {"mode": {"type": "string", '
                    '"minLength": 1, "description": "spec / weak / react / balanced '
                    '（或 0-100）"}, "task": {"type": "string", "minLength": 1, '
                    '"description": "交给模式隔离子代理的任务"}, "max_tokens": '
                    '{"type": "integer", "minimum": 1, "maximum": 32768, '
                    '"description": "输出上限（默认 1024）"}}, "required": ["mode", '
                    '"task"], "additionalProperties": false}'
                ),
                requires_confirmation=False,
                run=_mode_subagent,
            ),
        }

    def _router_mode_subagent(self, arguments: dict[str, Any]) -> str:
        """模式隔离子代理：独立 system prompt 的全新 LLM 调用（P6 隔离）。"""

        from ...router import band_for, parse_mode, persona_for

        parsed = parse_mode(arguments.get("mode"))
        if parsed is None or parsed == "auto":
            mode_token = arguments.get("mode")
            return (
                f'invalid mode "{mode_token}": use spec/weak/react/balanced, '
                "0-100, or 0.0-1.0"
            )
        task = str(arguments.get("task") or "").strip()
        if not task:
            return "invalid task: empty task text"
        model_id = getattr(self.config.llm, "model", "")
        persona = persona_for(parsed, model_id)
        try:
            max_tokens = int(arguments.get("max_tokens") or 1024)
        except (TypeError, ValueError):
            max_tokens = 1024
        max_tokens = max(1, min(32768, max_tokens))
        try:
            protocol = AgentLLMProtocol(
                client=self._llm_client(),
                model=model_id,
                request_timeout_seconds=getattr(
                    self.config, "request_timeout_seconds", 180
                ),
                request_retry_count=1,
                workspace_root=self.workspace_root,
                system_prompt_provider=lambda: persona,
                prompt_cache_identity_provider=lambda: {
                    "router": "mode-subagent",
                    "persona": persona,
                    "model": model_id,
                },
                tools_provider=lambda: [],
                extra_body_provider=self._build_extra_body,
                tool_name_from_function_name=lambda function_name: function_name,
                function_name_for_tool=lambda tool_name: tool_name,
            )
            chunks: list[str] = []
            reasoning_chars = 0

            def _on_reasoning(delta: str) -> None:
                nonlocal reasoning_chars
                reasoning_chars += len(delta or "")

            protocol.request_reply(
                messages=[{"role": "user", "content": task}],
                on_delta=lambda delta: chunks.append(delta or ""),
                on_token_usage=lambda *_args: None,
                on_protocol_wait=lambda: None,
                on_retry_status=lambda _status_text: None,
                on_reasoning_delta=_on_reasoning,
            )
        except Exception as exc:  # noqa: BLE001
            return f"subagent error: {exc}"
        text = "".join(chunks).strip()
        head = text[:3000]
        suffix = "\n…(truncated)" if len(text) > 3000 else ""
        return (
            f"[mode-subagent {band_for(parsed)} | reasoning {reasoning_chars} chars]\n"
            f"{head}{suffix}"
        )

    def _build_mcp_tools(self) -> list[ToolDefinition]:
        return build_mcp_tools(
            mcp_manager=self._mcp_manager,
            mcp_call=self._tool_mcp_call,
            mcp_read_resource=self._tool_mcp_read_resource,
            mcp_get_prompt=self._tool_mcp_get_prompt,
        )

    def _system_prompt(self) -> str:
        """返回静态 system prompt；动态上下文由 `_context_messages` 提供。

        任务路由开启时，由 RouterRuntime 按会话模式注入 persona（standard 模式
        RL 句置顶；spec 模式分类 persona 置顶，均保留原始模板的安全/协作协议）。
        """

        template = build_system_prompt(self._system_prompt_template)
        # TTS 指令由当前回合在首次模型请求前准备；工具结果后的后续请求
        # 不重复注入，但仍保留 tts_synthesize 工具可用。
        tts_instruction = getattr(self, "_active_tts_instruction", "")
        if tts_instruction:
            template = f"{template}\n\n{tts_instruction}"
        config = getattr(self, "config", None)
        if config is not None and getattr(config, "router_enabled", False):
            session_id = self._session_facade().current_session_id()
            if session_id:
                return self._router_runtime.apply_system_prompt(
                    template,
                    session_id=session_id,
                    model_id=getattr(getattr(config, "llm", None), "model", ""),
                    events=self._router_session_events(session_id),
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
        """返回跨项目工作知识库实例（惰性创建）。

        方法名刻意避开 ``self._knowledge_base`` 实例属性（缓存），否则
        实例属性会遮蔽同名方法，导致 ``self._knowledge_base()`` 变成调用 None。
        """

        if self._knowledge_base is None:
            self._knowledge_base = KnowledgeBase()
        return self._knowledge_base
