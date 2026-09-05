"""SubAgent 父侧编排：定义刷新、任务执行、事件注入与 LLM 协议。

Coordinator/TaskManager 等已在 ``subagents`` 子包；这里只保留父 Agent
侧的状态冻结、模型快照与事件映射胶水。"""
from __future__ import annotations

import logging
import json
import threading
import time
import uuid
from dataclasses import replace
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence
from ...toolkit.host_tools import (
    HostToolCatalog,
    build_provider_tools,
)
from ...toolkit.tools import (
    public_tool_arguments,
)
from ...runtime.execution import AgentLoopLimits, AgentLoopRunner
from ...runtime.llm_protocol import (
    AgentLLMProtocol,
    build_extra_body,
    chat_completion_tools,
    function_name_for_tool,
    tool_name_from_function_name,
)
from ...context.prompt_context import (
    build_context_messages,
    build_skill_context_message,
)
from ...subagents.coordinator import (
    SubAgentCoordinator,
    SubAgentExecutionResult,
    SubAgentPublicResult,
)
from ...subagents.definitions import AgentDefinition, AgentDefinitionRegistry
from ...subagents.execution import (
    FORK_BOILERPLATE,
    SubAgentExecutionContext,
    SubAgentModelSnapshot,
)
from ...subagents.tasks import SubAgentTaskManager
from ...subagents.worktree import (
    WorktreeError,
    cleanup_worktree_session,
    create_worktree_session,
)
from ....extensions.plugin_manager import (
    PluginDispatchContext,
    activate_plugin_dispatch_context,
)
from ...subagents.verify import VERIFY_COMMAND_TOOL_NAME, build_verify_command_tool
from ...types import AgentModelReply, ToolDefinition, ToolResult
from ....approval import (
    APPROVAL_MODE_AUTO,
)
from ....config.models.llm_multi import apply_model_selection, llm_config_to_profile_and_descriptor
from ....llm import (
    LLMConfig,
    LLMError,
    ModelRuntimeManager,
)
from ....state.session_artifacts import (
    redact_sensitive_text,
    redact_sensitive_values,
)

from ..shared import (
    AgentError,
)

LOGGER = logging.getLogger(__name__)


class _SubAgentTaskState:
    """单次 _execute_subagent_task 执行的局部状态。

    并发 worker 各自创建自己的 state 实例；阶段方法与闭包只读写该对象，
    不触碰其它任务的可变状态，避免把任务局部数据提升到 self 造成串扰。
    """

    __slots__ = (
        "execution_context",
        "model_snapshot",
        "owns_runtime_manager",
        "runtime_manager",
        "runtime_snapshot",
        "protocol",
        "cancel_check",
        "messages",
        "stream_conversation",
        "task_identity",
        "plugin_dispatch",
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "tool_start_times",
        "tool_result_cache",
        "approval_override",
    )

    def __init__(self, execution_context: SubAgentExecutionContext) -> None:
        self.execution_context = execution_context
        self.model_snapshot = execution_context.model_snapshot
        self.owns_runtime_manager = self.model_snapshot is not None
        self.runtime_manager: ModelRuntimeManager | None = None
        self.runtime_snapshot = None
        self.protocol = None
        self.cancel_check: Callable[[], None] | None = None
        self.messages: list[dict[str, Any]] = []
        self.stream_conversation = False
        self.task_identity: tuple[str, str] = ("", "")
        self.plugin_dispatch = None
        self.input_tokens = 0
        self.output_tokens = 0
        self.cached_input_tokens = 0
        self.tool_start_times: dict[Any, float] = {}
        self.tool_result_cache: dict[str, ToolResult] = {}
        self.approval_override = False


class SubAgentOrchestrationMixin:
    """SubAgent 父侧编排：定义刷新、任务执行、事件注入与 LLM 协议。"""

    def _refresh_subagent_definitions(self, *, include_plugins: bool = True) -> None:
        """按当前工作区重建定义索引；插件来源可在切换提交阶段暂时排除。"""

        registry = self._subagent_registry or AgentDefinitionRegistry()
        plugin_definitions: Sequence[tuple[str, Path]] = ()
        plugin_manager = getattr(self, "_plugin_manager", None)
        path_provider = (
            getattr(plugin_manager, "agent_definition_paths", None)
            if include_plugins
            else None
        )
        if callable(path_provider):
            try:
                plugin_definitions = tuple(path_provider())
            except Exception:
                # 插件定义是增量能力；插件状态异常不能阻止内置和用户定义加载。
                plugin_definitions = ()
        registry.discover(
            self.workspace_root,
            plugin_definitions=plugin_definitions,
        )
        self._subagent_registry = registry
        current_coordinator = getattr(self, "_subagent_coordinator", None)
        task_manager = (
            current_coordinator._task_manager
            if current_coordinator is not None
            else SubAgentTaskManager(
                retention_seconds=max(
                    60.0,
                    self.config.subagents.task_retention_minutes * 60,
                ),
                max_workers=self.config.subagents.max_concurrency,
            )
        )
        coordinator = SubAgentCoordinator(
            config=self.config.subagents,
            registry=registry,
            tools_provider=lambda: getattr(self, "_tools", {}),
            execute_task=self._execute_subagent_task,
            prepare_execution=self._prepare_subagent_execution,
            event_sink=self._handle_subagent_event,
            result_processor=self._prepare_subagent_public_result,
            task_manager=task_manager,
            owner_id=f"agent-{id(self)}",
            session_id_provider=lambda: self.current_session_id,
            observer_provider=lambda: getattr(self, "_subagent_event_callback", None),
            verify_tools_provider=self._subagent_verify_tools,
            apply_worktree=self.apply_subagent_worktree,
            discard_worktree=self.discard_subagent_worktree,
            list_worktrees=self.list_subagent_worktrees,
        )
        coordinator.set_cancel_check_provider(
            lambda: getattr(self, "_cancel_check", None)
        )
        self._subagent_coordinator = coordinator

    def _tool_subagent(self, arguments: dict[str, Any]) -> ToolResult:
        """父模型通过 subagent 工具调用受限子代理。

        ``review`` 角色（评审子代理）返回**渲染后的完整评审报告**（而非原始
        JSON + 截断摘要），并让父模型在同一工具结果里直接看到报告，以便继续
        处理（如修复发现的问题、提交并推送变更）。其他角色保持原行为。
        """

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is None:
            return ToolResult(ok=False, output="SubAgent 功能未启用。")
        tasks = arguments.get("tasks") if isinstance(arguments, dict) else None
        wants_review = bool(
            tasks
            and any(
                isinstance(task, dict)
                and str(task.get("subagent_type") or "").strip().casefold()
                == "review"
                for task in tasks
            )
        )
        if not wants_review:
            return coordinator.run(arguments)
        try:
            result = coordinator.run(arguments, keep_full_text=True)
        except BaseException:
            raise
        if not result.ok:
            return result
        try:
            payload = json.loads(result.output)
        except (TypeError, ValueError):
            return result
        results = payload.get("results") or ()
        if not results:
            return result
        task = results[0]
        if task.get("status") != "completed":
            return result
        full_text = task.get("full_text")
        if not isinstance(full_text, str) or not full_text.strip():
            return result
        # 渲染成父模型可直接阅读的报告；渲染失败时回退完整原文。
        from ....commands.slash import format_review_report

        rendered = format_review_report(full_text)
        return ToolResult(
            ok=True,
            output=rendered,
            full_output=rendered,
        )

    def run_subagent_task(
        self,
        *,
        agent_type: str,
        description: str,
        prompt: str,
        on_subagent_event: Callable[[str, dict[str, Any]], None] | None = None,
    ) -> str:
        """同步运行一个 SubAgent 任务并返回其最终文本结果。

        供斜杠命令等主线程入口使用：主线程只负责转发任务并接收/渲染结果，
        实际的模型循环、git 收集与工具执行全部由子 Agent 完成。
        任务失败时抛 :class:`AgentError`，带子任务的安全错误信息。

        ``on_subagent_event`` 可选：在任务执行期间把 Coordinator 生命周期事件
        （subagent.task.started/completed 等）转发给调用方，供 UI 显示审查进度。
        回调在任务结束、异常抛出或调用方取消时都会恢复为之前的监听器。
        """

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is None:
            raise AgentError("SubAgent 功能未启用，无法执行该任务。")
        definition = coordinator.registry.get(agent_type)
        if definition is None:
            available = ", ".join(coordinator.available_agent_types()) or "无"
            raise AgentError(
                f"未找到 SubAgent 定义：{agent_type}。当前可用：{available}。"
            )

        previous_callback = getattr(self, "_subagent_event_callback", None)
        previous_stream = bool(getattr(self, "_stream_subagent_conversation", False))
        if on_subagent_event is not None:
            self._subagent_event_callback = on_subagent_event
        # 只有通过本入口派生（如 /review）的任务才流式上报对话/工具事件；
        # 父 Agent 回合内派生的 SubAgent 保持原有进度树行为。
        self._stream_subagent_conversation = on_subagent_event is not None
        try:
            result = coordinator.run(
                {
                    "action": "run",
                    "tasks": [
                        {
                            "description": description,
                            "prompt": prompt,
                            "subagent_type": agent_type,
                        }
                    ],
                    "fail_fast": False,
                },
                # 主线程入口（如 /review）需要完整结果：评审 JSON 可能超过
                # result_summary_chars 截断阈值，截断会破坏结构化输出。
                keep_full_text=True,
            )
        finally:
            self._stream_subagent_conversation = previous_stream
            self._subagent_event_callback = previous_callback
        try:
            payload = json.loads(result.output)
        except (TypeError, ValueError):
            raise AgentError("子任务返回结果无法解析。") from None
        if not result.ok or payload.get("status") != "completed":
            raise AgentError(self._describe_subagent_run_failure(payload))
        results = payload.get("results") or []
        if not results:
            raise AgentError("子任务未返回结果。")
        task = results[0]
        if task.get("status") != "completed":
            raise AgentError(self._describe_subagent_run_failure(payload, task=task))
        # 优先使用未截断全文（keep_full_text），否则回退截断摘要。
        full_text = task.get("full_text")
        if isinstance(full_text, str) and full_text.strip():
            return full_text
        return str(task.get("summary") or "")

    @staticmethod
    def _describe_subagent_run_failure(
        payload: dict[str, Any],
        *,
        task: Mapping[str, Any] | None = None,
    ) -> str:
        """把子任务失败投影为有信息量、脱敏的安全错误消息。

        真实失败原因（message + error code + diagnostic category）位于
        per-task 结果的 ``error`` 字段；顶层 ``error`` 只在整批前校验失败时
        存在。两者都缺失时回退通用文案，避免向用户暴露未经处理的异常。
        """

        def _detail(error: Mapping[str, Any]) -> str:
            message = str(error.get("message") or "").strip()
            code = str(error.get("code") or "").strip()
            diagnostic = error.get("diagnostic")
            category = ""
            detail = ""
            if isinstance(diagnostic, dict):
                category = str(diagnostic.get("category") or "").strip()
                detail = str(diagnostic.get("detail") or "").strip()
            parts: list[str] = []
            if detail and detail != message:
                parts.append(detail)
            elif message:
                parts.append(message)
            labels = [part for part in (code, category) if part]
            if labels:
                parts.append("（" + "，".join(labels) + "）")
            return "".join(parts)

        candidates: list[Mapping[str, Any]] = []
        if task is not None:
            candidates.append(task.get("error") or {})
        else:
            for item in payload.get("results") or ():
                if isinstance(item, dict) and item.get("status") != "completed":
                    candidates.append(item.get("error") or {})
            candidates.append(payload.get("error") or {})
        for error in candidates:
            detail = _detail(error)
            if detail:
                return detail
        return "子任务执行失败。"

    def _subagent_verify_tools(self) -> Mapping[str, ToolDefinition]:
        """构造仅供 verify profile 使用的固定检查工具表。

        该工具表不会合并进 ``self._tools``，因此父 Agent 与其他子角色既看不到
        也无法调用 ``verify_command``。工作区切换后 Coordinator 会重建，并在准备
        新任务时重新读取当前 WorkspaceTools，避免旧工作区对象被后台任务复用。
        """

        return {
            VERIFY_COMMAND_TOOL_NAME: build_verify_command_tool(
                self._workspace_toolbox(),
                max_timeout_seconds=self.config.subagents.verify_command_timeout_seconds,
            )
        }

    def _prepare_subagent_public_result(
        self,
        task_id: str,
        agent_type: str,
        description: str,
        result_text: str,
    ) -> SubAgentPublicResult:
        """在结果进入父模型、Session、API 或 TUI 前完成一次统一安全投影。"""

        summary_chars = self.config.subagents.result_summary_chars
        text = str(result_text or "")
        worktree_artifact_items: list[dict] = []
        marker = "[worktree]"
        if marker in text:
            head, tail = text.split(marker, 1)
            text = head.rstrip()
            body = tail.strip()
            if body:
                worktree_artifact_items.append(
                    {
                        "type": "worktree",
                        "content": redact_sensitive_text(body)[:8000],
                    }
                )
        if getattr(self, "_session_store", None) is not None and getattr(
            self,
            "_session_state",
            None,
        ) is not None:
            prepared = self._session_facade().prepare_subagent_result(
                task_id=task_id,
                agent_type=agent_type,
                description=description,
                result_text=text,
                summary_chars=summary_chars,
            )
            artifacts = list(prepared.get("artifacts", ()))
            artifacts.extend(worktree_artifact_items)
            return SubAgentPublicResult(
                summary=str(prepared.get("summary", "")),
                artifacts=tuple(artifacts),
            )

        safe_summary = redact_sensitive_text(str(text or "").strip())
        if len(safe_summary) > summary_chars:
            safe_summary = safe_summary[:summary_chars] + "\n... 子任务结果已截断。"
        return SubAgentPublicResult(
            summary=safe_summary,
            artifacts=tuple(worktree_artifact_items),
        )

    def _drain_subagent_notifications(self) -> list[dict[str, Any]]:
        """消费当前 Session 的后台终态通知，不写入 Session 恢复历史。"""

        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is None:
            return []
        try:
            return coordinator.drain_notifications(
                session_id=self.current_session_id,
            )
        except Exception as exc:
            LOGGER.warning("SubAgent notification drain failed: %s", type(exc).__name__)
            return []

    def _inject_subagent_notifications(
        self,
        messages: list[dict[str, Any]],
    ) -> None:
        """把新完成任务追加到本轮临时 user 消息，供下一次模型请求消费。

        AgentLoopRunner 会复用同一个 ``messages`` 列表，因此这里原地修改当前
        user 消息：通知在后续工具循环请求中仍可见，但不会写入 ``_history`` 或
        Session 普通消息。避免插入中途 system 消息，以兼容 Anthropic/Gemini
        对系统提示位置的严格协议要求。
        """

        notifications = self._drain_subagent_notifications()
        if not notifications:
            return
        notification_text = (
            "\n\n<subagent-notifications>\n"
            + json.dumps(
                notifications[:16],
                ensure_ascii=False,
            )[:12000]
            + "\n</subagent-notifications>"
        )
        for message in reversed(messages):
            if message.get("role") != "user":
                continue
            content = message.get("content")
            message["content"] = (
                str(content or "") + notification_text
            )
            return
        messages.append({"role": "user", "content": notification_text.strip()})

    def _handle_subagent_event(self, event_name: str, payload: dict[str, Any]) -> None:
        """把 Coordinator 生命周期映射到父 Session 和当前公开流式回调。"""

        safe_payload = redact_sensitive_values(payload)
        lock = getattr(self, "_subagent_event_lock", None)
        if lock is None:
            lock = threading.RLock()
            self._subagent_event_lock = lock
        with lock:
            self._append_session_event(event_name.replace(".", "_"), safe_payload)
            # API 的持久观察者独立于当前 parent Run，确保后台审批和终态事件不会
            # 因 run_stream 返回而丢失；它与临时回调分别服务于会话级和回合级 SSE。
            persistent_handler = getattr(self, "_subagent_event_handler", None)
            if callable(persistent_handler):
                try:
                    persistent_handler(event_name, dict(safe_payload))
                except Exception as exc:  # noqa: BLE001 - observer 不能破坏任务状态机
                    LOGGER.warning(
                        "SubAgent persistent event observer failed: %s",
                        type(exc).__name__,
                    )
            callback = getattr(self, "_subagent_event_callback", None)
            if callback is not None:
                try:
                    callback(event_name, dict(safe_payload))
                except Exception as exc:  # noqa: BLE001 - observer 不能破坏任务状态机
                    LOGGER.warning(
                        "SubAgent public event observer failed: %s",
                        type(exc).__name__,
                    )

    def _emit_subagent_live_event(
        self,
        event_name: str,
        payload: dict[str, Any],
    ) -> None:
        """只向实时回调转发子代理对话/工具事件，不写入 Session。

        /review 等入口在 ``run_subagent_task`` 期间挂接的临时回调用于实时展示
        子代理会话；这些事件不进 Session 持久化，避免大段 diff/工具输出污染
        会话记录。观察者失败只记日志，不能破坏子代理执行。
        """

        callback = getattr(self, "_subagent_event_callback", None)
        if callback is None:
            return
        try:
            callback(event_name, dict(payload))
        except Exception as exc:  # noqa: BLE001 - observer 不能破坏任务状态机
            LOGGER.warning(
                "SubAgent conversation event observer failed: %s",
                type(exc).__name__,
            )

    @staticmethod
    def _freeze_fork_context_messages(
        messages: Sequence[Mapping[str, Any]],
    ) -> tuple[dict[str, Any], ...]:
        """深拷贝并脱敏一组公开协议消息，供 Fork 在后续线程独立使用。

        父 Agent Loop 会原地追加 assistant tool-call 与 tool-result 消息；不能把
        其可变列表交给子线程。这里先走既有脱敏器，再保留协议所需的 role/content
        及可能的 tool_calls 结构，避免不同 Provider 转换时收到半截消息。
        """

        redacted = redact_sensitive_values(list(messages))
        if not isinstance(redacted, list):
            raise AgentError("Fork 上下文必须是消息数组。")
        snapshot: list[dict[str, Any]] = []
        for message in redacted:
            if isinstance(message, dict):
                snapshot.append(dict(message))
        return tuple(snapshot)

    def _freeze_subagent_skill_context(self) -> str:
        """冻结当前 Skill 索引或已激活 Skill 正文，供 fresh 子任务继承。"""

        manager = getattr(self, "_skill_manager", None)
        if manager is None:
            return ""
        message = build_skill_context_message(
            manager,
            tuple(getattr(self, "_active_skills", ()) or ()),
        )
        if not message:
            return ""
        return redact_sensitive_text(str(message.get("content") or ""))

    def _prepare_subagent_execution(
        self,
        definition: AgentDefinition,
        context: str,
        task_model: str,
    ) -> SubAgentExecutionContext:
        """在 Coordinator 排队前冻结模型和可选 Fork 上下文。

        该方法只在父 Agent 的 ``subagent`` 工具调用线程中执行。随后同步或后台
        worker 只消费返回的私有快照，绝不再读取父 ``_history``、当前模型或本轮
        可变 messages，从而避免父后续工具回合、模型切换与 Session 状态串扰。
        """

        normalized_context = str(context or "").strip()
        if normalized_context not in {"fresh", "fork"}:
            raise AgentError("SubAgent context 必须是 fresh 或 fork。")
        model_snapshot = self._freeze_subagent_model_snapshot(definition, task_model)
        isolation = str(getattr(definition, "isolation", "shared") or "shared").strip()

        # 先冻结所有可能失败的非文件上下文，再创建 Git Worktree。否则 Plugin
        # dispatch、Fork 快照或父提示构造失败时，会留下已登记但永远不会执行的
        # 临时分支和目录，形成跨任务/跨工作区残留写能力。
        plugin_dispatch = self._freeze_subagent_plugin_dispatch_context()
        fork_messages: tuple[dict[str, Any], ...] = ()
        parent_system_prompt = ""
        skill_context = ""
        if normalized_context == "fork":
            active_messages = getattr(self, "_active_fork_context_messages", None)
            if not isinstance(active_messages, tuple):
                # Fork 只能由当前父 Agent 回合内的工具分发创建；回合外直接调用会
                # 缺失当前用户目标和已完成的上下文协议，宁可明确拒绝也不猜测补齐。
                raise AgentError("Fork 只能在活动父 Agent 回合内创建。")
            fork_messages = self._freeze_fork_context_messages(active_messages)
            parent_system_prompt = redact_sensitive_text(self._system_prompt())
        else:
            skill_context = self._freeze_subagent_skill_context()

        worktree_session = None
        # 测试 / 轻量构造可能没有完整 workspace_root；shared 模式允许空根。
        raw_root = getattr(self, "workspace_root", None) or getattr(self, "_workspace_root", None) or "."
        workspace_root = str(Path(raw_root).expanduser().resolve())
        if isolation == "worktree":
            # worktree 会话在入队前创建，确保后台 worker 拿到独立目录而不是
            # 与父工作区共享写入路径。
            try:
                worktree_session = create_worktree_session(
                    workspace_root=Path(workspace_root),
                    task_id=f"{definition.name}-{uuid.uuid4().hex[:8]}",
                )
            except WorktreeError as exc:
                raise AgentError(f"创建 SubAgent worktree 失败：{exc}") from exc
            workspace_root = str(worktree_session.worktree_path)
            # 登记失败也必须回收刚创建的 Git 资源，不能留下无控制面入口的孤儿。
            try:
                self._register_subagent_worktree_session(worktree_session)
            except BaseException:
                try:
                    cleanup_worktree_session(worktree_session, remove_branch=True)
                except Exception as cleanup_exc:  # noqa: BLE001 - 保留原始登记异常
                    LOGGER.warning(
                        "SubAgent worktree rollback failed after registration error: %s",
                        type(cleanup_exc).__name__,
                    )
                raise

        return SubAgentExecutionContext(
            context=normalized_context,
            model_snapshot=model_snapshot,
            fork_messages=fork_messages,
            parent_system_prompt=parent_system_prompt,
            skill_context=skill_context,
            plugin_dispatch=plugin_dispatch,
            worktree_session=worktree_session,
            workspace_root=workspace_root,
            isolation=isolation,
        )

    def _freeze_subagent_model_snapshot(
        self,
        definition: AgentDefinition,
        task_model: str,
    ) -> SubAgentModelSnapshot | None:
        """按 subagents.toml 角色配置 > 父模型两级解析并复制独立模型运行视图。

        父代理不再能通过任务字段指定子代理模型：``task_model`` 参数保留仅为
        兼容既有注入点，实际始终为空。模型来源只有 subagents.toml 的
        ``[subagents.models.<角色>]`` 项目级配置；未配置或显式 ``inherit``
        时沿用父模型。定义文件里的 model 字段不参与选择。
        """

        parent_llm = getattr(self.config, "llm", None)
        subagent_config = getattr(self.config, "subagents", None)
        configured_model = str(
            (getattr(subagent_config, "model_overrides", None) or {}).get(
                definition.name, ""
            )
        ).strip()
        # subagents.toml 中的 ``[subagents.models.<角色>]`` 是唯一的模型来源；
        # 未配置或显式 ``inherit`` 时沿用父模型。
        if configured_model and configured_model.casefold() != "inherit":
            selection = configured_model
        else:
            selection = "inherit"

        # 最小夹具和遗留直接 OpenAI 路径没有完整 LLMConfig；保留原有 fresh
        # 执行兼容性，但不允许它们伪装成可跨 Profile 的模型覆盖。
        if not isinstance(parent_llm, LLMConfig):
            if selection != "inherit":
                raise AgentError("当前运行态不支持 SubAgent 模型覆盖。")
            return None

        try:
            selected_llm = (
                apply_model_selection(parent_llm, selection)
                if selection != "inherit"
                else replace(
                    parent_llm,
                    provider_options=dict(parent_llm.provider_options),
                )
            )
            # ``apply_model_selection`` 返回新的 LLMConfig；这里仍复制可变映射，
            # 确保配置对象之后被 UI 更新时不会改变已排队任务的请求参数。
            frozen_llm = replace(
                selected_llm,
                provider_options=dict(selected_llm.provider_options),
            )
            profile, descriptor = llm_config_to_profile_and_descriptor(frozen_llm)
            profile = replace(profile, provider_options=dict(profile.provider_options))
            descriptor = replace(
                descriptor,
                provider_options=dict(descriptor.provider_options),
            )
        except LLMError as exc:
            raise AgentError(f"SubAgent 模型无法解析：{exc}") from exc
        except Exception as exc:  # noqa: BLE001 - Runtime 前的配置错误统一为 AgentError
            raise AgentError("SubAgent 模型配置无效。") from exc

        return SubAgentModelSnapshot(
            selection=(frozen_llm.catalog_key or frozen_llm.model),
            llm_config=frozen_llm,
            profile=profile,
            descriptor=descriptor,
        )

    @staticmethod
    def _fork_task_message(description: str, prompt: str) -> dict[str, str]:
        """构造追加到冻结父上下文后的独立任务指令。"""

        return {
            "role": "user",
            "content": (
                '<subagent_task context="fork">\n'
                f"描述：{description}\n"
                f"任务：\n{prompt}\n"
                "</subagent_task>"
            ),
        }

    def _build_subagent_messages(
        self,
        execution_context: SubAgentExecutionContext,
        child_tools: Mapping[str, ToolDefinition],
        description: str,
        prompt: str,
    ) -> list[dict[str, Any]]:
        """为 fresh/Fork 分别构造完全独立的可变协议消息列表。"""

        if execution_context.context == "fork":
            return [
                *self._freeze_fork_context_messages(execution_context.fork_messages),
                self._fork_task_message(description, prompt),
            ]
        return [
            *build_context_messages(
                workspace_root=self.workspace_root,
                project_instructions=self._load_agents_instructions(),
                skill_manager=None,
                active_skills=(),
                tools=build_provider_tools(
                    HostToolCatalog(child_tools)
                ).values(),
                agent_temp_dir=self._agent_temp_dir_display(),
                workspace_detection_summary=getattr(
                    self.config,
                    "workspace_detection_summary",
                    "",
                ),
                inherited_skill_context=execution_context.skill_context,
            ),
            {
                "role": "user",
                "content": (
                    "<subagent_task context=\"fresh\">\n"
                    f"描述：{description}\n"
                    f"任务：\n{prompt}\n"
                    "</subagent_task>"
                ),
            },
        ]

    def _create_subagent_runtime_manager(
        self,
        snapshot: SubAgentModelSnapshot,
    ) -> ModelRuntimeManager:
        """为单个任务建立独立 Runtime，不能借用父 Agent 的可变当前模型。"""

        manager = ModelRuntimeManager()
        try:
            manager.bootstrap(snapshot.profile, snapshot.descriptor)
        except BaseException:
            # bootstrap 期间 Adapter 可能已创建底层 client；失败时不能把该
            # 半初始化 Runtime 留给无引用的临时 Manager。
            manager.close()
            raise
        return manager

    def _execute_subagent_task(
        self,
        definition: AgentDefinition,
        child_tools: Mapping[str, ToolDefinition],
        description: str,
        prompt: str,
        cancel_check: Callable[[], None] | None = None,
        execution_context: SubAgentExecutionContext | None = None,
    ) -> SubAgentExecutionResult:
        """用独立 messages、预算和 Runtime 引用执行一个 fresh 或 Fork 子任务。

        本方法不修改父 `_history`、`_pending_user_text`、`_active_skills`、
        `_active_runtime_snapshot` 或普通 Session 消息。Fork 只消费 Coordinator 在
        排队前冻结的公开消息，不读取父回合此后的可变状态；带模型快照的任务始终
        使用独立 RuntimeManager，避免父模型切换与子执行相互阻塞或错配。
        """

        if execution_context is None:
            execution_context = self._prepare_subagent_execution(
                definition,
                "fresh",
                "",
            )
        state = _SubAgentTaskState(execution_context)
        state.cancel_check = cancel_check or getattr(self, "_cancel_check", None)
        self._prepare_subagent_task_run(state, definition, child_tools, description, prompt)
        return self._run_subagent_task_loop(state, definition, child_tools, prompt)

    def _prepare_subagent_task_run(
        self,
        state: _SubAgentTaskState,
        definition: AgentDefinition,
        child_tools: Mapping[str, ToolDefinition],
        description: str,
        prompt: str,
    ) -> None:
        """准备子任务 Runtime、消息与 worktree 根目录覆盖。"""

        def release_runtime() -> None:
            """释放本任务持有的 Runtime 引用，并在需要时关闭专属 Manager。"""

            if state.runtime_manager is not None and state.runtime_snapshot is not None:
                state.runtime_manager.release_turn(state.runtime_snapshot)
                state.runtime_snapshot = None
            if state.owns_runtime_manager and state.runtime_manager is not None:
                # 专属 Runtime 不可泄漏到下一个任务；父 Runtime 则仍由父 Agent
                # 生命周期管理，不能由子任务提前关闭。
                try:
                    state.runtime_manager.close()
                except Exception:  # noqa: BLE001 - 清理失败不遮蔽原始模型/取消异常
                    LOGGER.warning("SubAgent dedicated Runtime close failed.")
            self._clear_workspace_root_override()

        try:
            state.runtime_manager = (
                self._create_subagent_runtime_manager(state.model_snapshot)
                if state.model_snapshot is not None
                else self._runtime_manager_for_protocol()
            )
            state.protocol = self._subagent_llm_protocol(
                definition,
                child_tools,
                execution_context=state.execution_context,
                runtime_manager=state.runtime_manager,
            )
            # 最小测试夹具可替换 protocol 工厂并自行提供 RuntimeManager；生产路径
            # 已显式传入专属/父 Manager。回读只用于保持既有依赖注入契约。
            if state.runtime_manager is None:
                state.runtime_manager = getattr(state.protocol, "runtime_manager", None)
            if state.runtime_manager is not None:
                state.runtime_snapshot = state.runtime_manager.acquire_turn()
        except BaseException:
            release_runtime()
            raise

        # Coordinator 将 task 来源放入当前 worker 的 ContextVar；不通过
        # ``self._subagent_coordinator`` 回读，避免定义刷新时旧后台 worker 取到
        # 新 Coordinator 而丢失正确的 task/batch 身份。
        try:
            state.messages = self._build_subagent_messages(
                state.execution_context,
                child_tools,
                description,
                prompt,
            )
            # worktree / 隔离任务：在本 worker 线程内切换 WorkspaceTools 根目录。
            override_root = str(
                getattr(state.execution_context, "workspace_root", "") or ""
            ).strip()
            if override_root and getattr(
                state.execution_context, "isolation", "shared"
            ) == "worktree":
                self._workspace_root_local.root = override_root
        except BaseException:
            release_runtime()
            self._clear_workspace_root_override()
            raise

        # 仅在 /review 等入口（run_subagent_task 挂接了实时回调）时流式上报
        # 子代理对话事件；任务身份由 Coordinator 写入 execution_context。
        state.stream_conversation = bool(
            getattr(self, "_stream_subagent_conversation", False)
        )
        state.task_identity = (
            str(getattr(state.execution_context, "task_id", "") or ""),
            str(getattr(state.execution_context, "batch_id", "") or ""),
        )
        plugin_dispatch = state.execution_context.plugin_dispatch
        if plugin_dispatch is None:
            # 兼容旧测试/调用方未注入 plugin_dispatch 的路径。
            plugin_dispatch = PluginDispatchContext(handlers=(), source="none")
        state.plugin_dispatch = plugin_dispatch

    def _run_subagent_task_loop(
        self,
        state: _SubAgentTaskState,
        definition: AgentDefinition,
        child_tools: Mapping[str, ToolDefinition],
        prompt: str,
    ) -> SubAgentExecutionResult:
        """以独立 messages 运行 AgentLoopRunner 并组装结果。"""

        def record_usage(input_count: int, output_count: int, cached_count: int) -> None:
            state.input_tokens += max(0, int(input_count))
            state.output_tokens += max(0, int(output_count))
            state.cached_input_tokens += max(0, int(cached_count))

        def request_child_reply(working_messages: list[dict[str, Any]]) -> AgentModelReply:
            """在独立任务并发之外，再限制 Provider 模型请求的同时在途数量。"""

            # 子代理线程保存各自回合上下文，供该线程内的自动审查提取
            # 最近用户消息摘要（审查已不再复用完整对话历史）。
            review_local = getattr(self, "_review_context_local", None)
            if review_local is not None:
                review_local.messages = list(working_messages)

            semaphore = getattr(self, "_subagent_model_request_semaphore", None)
            if semaphore is None:
                reply = state.protocol.request_reply(
                    working_messages,
                    lambda _text: None,
                    record_usage,
                    lambda: None,
                    lambda _message: None,
                    state.cancel_check,
                    None,
                    state.runtime_snapshot,
                    lambda: None,
                )
            else:
                # 不能无期限阻塞在并发槽位上；等待期间持续检查父回合取消，
                # 确保尚未发起 Provider 请求的任务也能及时退出。
                while not semaphore.acquire(timeout=0.1):
                    if state.cancel_check is not None:
                        state.cancel_check()
                try:
                    reply = state.protocol.request_reply(
                        working_messages,
                        lambda _text: None,
                        record_usage,
                        lambda: None,
                        lambda _message: None,
                        state.cancel_check,
                        None,
                        state.runtime_snapshot,
                        lambda: None,
                    )
                finally:
                    semaphore.release()
            # 只把带工具调用的过程性文本转发到子代理会话面板（最终评审 JSON
            # 由主线程渲染为报告，不重复展示在面板里）。
            if state.stream_conversation and reply.content and reply.tool_calls:
                self._emit_subagent_live_event(
                    "subagent.turn.text",
                    {
                        "task_id": state.task_identity[0],
                        "batch_id": state.task_identity[1],
                        "agent_type": definition.name,
                        "text": str(reply.content),
                    },
                )
            return reply

        def emit_conversation_event(event_name: str, payload: dict[str, Any]) -> None:
            if not state.stream_conversation:
                return
            self._emit_subagent_live_event(
                event_name,
                {
                    "task_id": state.task_identity[0],
                    "batch_id": state.task_identity[1],
                    "agent_type": definition.name,
                    **payload,
                },
            )

        def report_tool_start(_step: int, call: Any) -> None:
            key = str(getattr(call, "id", "") or "") or id(call)
            state.tool_start_times[key] = time.perf_counter()
            raw_arguments = getattr(call, "arguments", None)
            if not isinstance(raw_arguments, dict):
                raw_arguments = {}
            emit_conversation_event(
                "subagent.tool.started",
                {
                    "tool": str(getattr(call, "name", "") or ""),
                    "arguments": public_tool_arguments(
                        str(getattr(call, "name", "") or ""),
                        raw_arguments,
                    ),
                },
            )

        def report_tool_result(call: Any, result: Any) -> None:
            key = str(getattr(call, "id", "") or "") or id(call)
            started_at = state.tool_start_times.pop(key, None)
            # 优先用 Agent 层记录的真实完成时刻：快工具提前完成、整批等慢工具
            # 时仍显示各自真实耗时；缺省时退回到当前时刻。
            completed_at = getattr(result, "completed_at", None)
            finished_at = completed_at if completed_at is not None else time.perf_counter()
            emit_conversation_event(
                "subagent.tool.completed",
                {
                    "tool": str(getattr(call, "name", "") or ""),
                    "ok": bool(getattr(result, "ok", False)),
                    "output": str(
                        getattr(result, "full_output", "")
                        or getattr(result, "output", "")
                        or ""
                    ),
                    "duration_seconds": (
                        max(0.0, finished_at - started_at)
                        if started_at is not None
                        else None
                    ),
                },
            )

        # gitMode=full 的角色（如 review）在本 worker 线程内强制自动批准：完整
        # git 子命令权限不经过主对话 manual/review 审批，避免收集 diff 时被逐条
        # 打断。覆盖由 try/finally 保证在任务结束后恢复，线程之间互不影响。
        if getattr(definition, "git_mode", "readonly") == "full":
            self._approval_mode_local.mode = APPROVAL_MODE_AUTO
            state.approval_override = True
        try:
            with activate_plugin_dispatch_context(state.plugin_dispatch):
                loop_result = AgentLoopRunner().run(
                    messages=state.messages,
                    request_reply=request_child_reply,
                    execute_tool_batch=lambda calls, first_step: self._execute_tool_batch(
                        calls,
                        first_step,
                        report_tool_start=report_tool_start,
                        report_tool_result=report_tool_result,
                        check_cancelled=state.cancel_check or (lambda: None),
                        status=lambda _message: None,
                        prompt=prompt,
                        active_runtime_snapshot=state.runtime_snapshot,
                        vision_base_llm=(
                            state.model_snapshot.llm_config
                            if state.model_snapshot is not None
                            else getattr(self.config, "llm", None)
                        ),
                        tools=child_tools,
                        execution_cache=state.tool_result_cache,
                        persist_session_events=False,
                    ),
                    limits=AgentLoopLimits(
                        timeout_seconds=self.config.subagents.default_timeout_seconds,
                    ),
                    cancel_check=state.cancel_check,
                )
                worktree_artifacts = self._collect_subagent_worktree_artifacts(
                    state.execution_context
                )
                final_text = str(loop_result.final_text or "")
                if worktree_artifacts:
                    final_text = (
                        f"{final_text.rstrip()}\n\n[worktree]\n"
                        + "\n".join(worktree_artifacts)
                    ).strip()
                return SubAgentExecutionResult(
                    final_text=final_text,
                    model_turns=loop_result.model_turns,
                    tool_calls=loop_result.tool_calls,
                    input_tokens=state.input_tokens,
                    output_tokens=state.output_tokens,
                    cached_input_tokens=state.cached_input_tokens,
                    artifacts=worktree_artifacts,
                )
        finally:
            if state.approval_override:
                self._approval_mode_local.mode = None
            # 释放 Runtime 引用（与准备阶段失败路径一致）。
            if state.runtime_manager is not None and state.runtime_snapshot is not None:
                state.runtime_manager.release_turn(state.runtime_snapshot)
                state.runtime_snapshot = None
            if state.owns_runtime_manager and state.runtime_manager is not None:
                try:
                    state.runtime_manager.close()
                except Exception:  # noqa: BLE001 - 清理失败不遮蔽原始结果异常
                    LOGGER.warning("SubAgent dedicated Runtime close failed.")
            self._clear_workspace_root_override()


    def _subagent_llm_protocol(
        self,
        definition: AgentDefinition,
        child_tools: Mapping[str, ToolDefinition],
        *,
        execution_context: SubAgentExecutionContext | None = None,
        runtime_manager: ModelRuntimeManager | None = None,
    ) -> AgentLLMProtocol:
        """创建只绑定子角色、冻结模型与过滤工具表的轻量协议对象。"""

        model_snapshot = (
            execution_context.model_snapshot if execution_context is not None else None
        )
        model_config = (
            model_snapshot.llm_config if model_snapshot is not None else self.config.llm
        )
        is_fork = execution_context is not None and execution_context.context == "fork"
        if definition.permission_mode == "explicit-command-allowlist":
            capability_rules = (
                "只可读取、搜索，并调用 Host 提供的 verify_command 选择固定检查；"
                "不得传递或拼接命令文本、Shell、路径、环境变量或网络参数；不得写文件、"
                "安装依赖、修改 Git、修改 Memory 或创建其他 SubAgent；"
            )
        elif definition.permission_mode == "standard":
            # standard 写能力仍受 Host 风险审批与 isolation 约束；禁止嵌套 SubAgent。
            isolation = (
                execution_context.isolation
                if execution_context is not None
                else getattr(definition, "isolation", "shared")
            )
            if isolation == "worktree":
                capability_rules = (
                    "可在独立 worktree 中读取、搜索、写入文件并执行经 Host 审批的命令；"
                    "不得修改 Memory、不得创建其他 SubAgent、不得 git commit/push/remote；"
                    "结果由父 Agent 审查后 apply/discard，不得要求静默写回脏主工作区；"
                )
            else:
                capability_rules = (
                    "可在共享工作区读取、搜索、写入文件并执行经 Host 审批的命令；"
                    "必须遵守单写者规则，不得修改 Memory、不得创建其他 SubAgent；"
                    "高风险写/命令操作必须等待 Host 审批；"
                )
        else:
            # 只读子角色默认给只读包装的 git；gitMode=full 时授予完整子命令并
            # 自动批准，能力规则相应改为自律约束：仍只能用于收集 diff 等只读任务。
            if getattr(definition, "git_mode", "readonly") == "full":
                git_rules = (
                    "结构化 git 工具已授予完整子命令权限且调用自动批准；"
                    "但任务只允许用只读子命令（status/diff/log/show 等）收集上下文，"
                    "不得执行 commit/push/reset --hard/clean 等写入性 Git 操作；"
                )
            else:
                git_rules = (
                    "结构化 git 工具仅放行只读档动作（status/diff/log/show 等），"
                )
            capability_rules = (
                "不得使用 write_file、Edit_file、任何 *_memory_write 或创建其他 SubAgent；"
                "可以继承 Host 提供的 MCP、Skill、浏览器、桌面与其他外部能力；"
                "bash、powershell、monitor 仅可执行通过 Host 只读命令策略的命令，"
                f"{git_rules}"
                "不得以重定向、脚本解释器、Git 变更或其他方式修改本地工作区文件；"
            )

        prompt_parts: list[str] = []
        if is_fork and execution_context is not None and execution_context.parent_system_prompt:
            # Fork 必须继承父 Agent 的基础系统规则；受限子角色规则随后追加，
            # 因而只能进一步收窄权限，不能被父提示中的面向用户表述放宽。
            prompt_parts.append(execution_context.parent_system_prompt)
        prompt_parts.extend(
            (
                "你是 OmniCrawl 主 Agent 派生的受限工作进程，不直接面向用户。\n"
                f"不可协商规则：{capability_rules}"
                "不得向用户提问；严格限制在分配任务范围内；只使用 Host 提供的工具；"
                "最终返回有界工作报告，不输出隐藏推理。",
                FORK_BOILERPLATE if is_fork else "",
                f"<agent_definition name=\"{definition.name}\">\n"
                f"{definition.system_prompt}\n"
                "</agent_definition>",
            )
        )
        system_prompt = "\n\n".join(part for part in prompt_parts if part)
        request_timeout = min(
            int(getattr(self.config, "request_timeout_seconds", 180)),
            int(getattr(model_config, "request_timeout_seconds", 180)),
            max(1, int(self.config.subagents.default_timeout_seconds)),
        )
        selected_runtime_manager = (
            runtime_manager
            if runtime_manager is not None
            else self._runtime_manager_for_protocol()
        )
        extra_body_provider = (
            (lambda: build_extra_body(model_config))
            if model_snapshot is not None
            else self._build_extra_body
        )
        provider_tools = build_provider_tools(HostToolCatalog(child_tools))
        return AgentLLMProtocol(
            # 统一 Runtime 路径不会读取旧 OpenAI client；不为独立 Profile 惰性
            # 创建并缓存父 Profile client，避免跨 Profile 凭据或连接复用。
            client=None if selected_runtime_manager is not None else self._llm_client(),
            model=model_config.model,
            request_timeout_seconds=request_timeout,
            # 子代理对可重试错误（CONNECTION_FAILED/限流/空响应等）自动重试 3 次，
            # 避免一过性网络抖动直接中断子任务；重试仍失败才由 Coordinator
            # 返回结构化错误交还主 Agent，不会无限放大请求成本。
            request_retry_count=3,
            workspace_root=self.workspace_root,
            system_prompt_provider=lambda: system_prompt,
            prompt_cache_identity_provider=lambda: {
                "workspace": str(self.workspace_root),
                "subagent": definition.name,
                "definition": str(definition.source_path or definition.source),
                "context": "fork" if is_fork else "fresh",
                "model": model_snapshot.selection if model_snapshot is not None else model_config.model,
            },
            tools_provider=lambda: chat_completion_tools(
                provider_tools.values(),
                function_name_for_tool=function_name_for_tool,
            ),
            extra_body_provider=extra_body_provider,
            tool_name_from_function_name=lambda function_name: tool_name_from_function_name(
                function_name,
                provider_tools,
            ),
            function_name_for_tool=function_name_for_tool,
            runtime_manager=selected_runtime_manager,
            reasoning_effort_provider=lambda: getattr(
                model_config,
                "reasoning_effort",
                "medium",
            ),
        )
