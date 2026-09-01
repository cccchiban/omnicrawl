"""主模型循环：run_stream、工具批执行、LLM 请求与回合收尾。

这是 Agent 的心脏：``run_stream`` 驱动「模型 -> 工具 -> 观察」循环，
LLM 协议、上下文消息组装与工具结果回填都收在这里。"""
from __future__ import annotations

import logging
import json
import re
import threading
import time
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeoutError
from contextvars import copy_context
from dataclasses import replace
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Mapping, Sequence
from ...toolkit.host_tools import (
    HostToolCatalog,
    INVOKE_TOOL_NAME,
    build_provider_tools,
    public_invoke_arguments,
    tool_validation_error_result,
    validate_tool_arguments,
)
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
from ...context_compaction import (
    ContextCompactionService,
    ModelSummaryCompactor,
    RuntimeSummaryModelAdapter,
    SessionEvidenceRecallService,
    SourceEvent,
    TokenUsageSample,
    estimate_json_tokens,
)
from ...runtime.execution import AgentLoopLimits, AgentLoopObservation, AgentLoopRunner
from ...runtime.run_guard import (
    GuardRetryState,
    activate_pause_event,
    pause_requested,
    reset_pause_event,
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
from ...context.prompt_context import (
    build_context_messages,
    build_project_instructions_messages,
    build_prompt_cache_identity,
    build_skill_context_message,
    build_system_prompt,
)
from ...types import AgentModelReply, ToolCall, ToolDefinition, ToolResult
from ....config.core.runtime import global_agents_path, resolve_config_path
from ....config.models.llm_multi import apply_model_selection, llm_config_to_profile_and_descriptor
from ....config.features.subagents import (
    SubAgentConfig,
    SubAgentConfigError,
    load_subagent_config,
    validate_subagent_advanced_setting,
)
from ....llm import (
    LLMConfig,
    LLMError,
    ModelError,
    ModelErrorCode,
    ModelRuntimeManager,
    OpenAIResponseLLM,
    load_llm_config,
    normalize_reasoning_effort,
)
from ....skill import SkillManager, SkillMatchResult
from ...tool_process import (
    ToolProcessCancelled,
    ToolProcessError,
    ToolProcessTimeout,
    can_serialize_tool_runner,
    run_tool_in_subprocess,
)

from ..shared import (
    AGENTS_INSTRUCTIONS_FILE,
    AgentError,
    DEFAULT_TOOL_TIMEOUT_SECONDS,
    _ActiveTurnSnapshot,
    _CONTEXT_OVERFLOW_ERROR_MARKERS,
    _CONTEXT_OVERFLOW_RECOVERY_PROMPT,
    _CONTINUE_LAST_TASK_TEXTS,
    _RATE_LIMIT_ERROR_MARKERS,
    _tool_timeout_result,
    _unknown_tool_result,
)

LOGGER = logging.getLogger(__name__)

# TTS 启用时追加到 system prompt 的说话指令。
# 顺序必须是「先调用工具，再补文字」：Agent 循环中模型输出文字即回合
# 终止，若要求先给文字再调用工具，模型会直接以文字回复结束回合、跳过
# 工具调用；先调用 tts_synthesize 朗读再补文字说明才符合循环结构。
_TTS_SPEAK_INSTRUCTION = (
    '<tts_instruction source="host-settings" trust="host">\n'
    "TTS 语音合成已启用：生成回复时，请先调用 tts_synthesize 工具"
    "朗读你的回答（或用户明确指定的文本），生成语音会自动播放；"
    "调用工具后再给出简明扼要的文字回复。不要只输出文字而不调用工具；"
    "若模型未就绪或合成失败，直接在回复中说明原因，不要反复重试。\n"
    "</tts_instruction>"
)

class TurnLoopMixin:
    """主模型循环：run_stream、工具批执行、LLM 请求与回合收尾。"""

    def run_stream(
        self,
        user_text: str,
        on_delta: Callable[[str], None],
        on_status: Callable[[str], None] | None = None,
        on_tool_start: Callable[[int, ToolCall], None] | None = None,
        on_tool_result: Callable[[ToolCall, ToolResult], None] | None = None,
        on_token_usage: Callable[[int, int, int], None] | None = None,
        on_protocol_wait: Callable[[], None] | None = None,
        on_retry_status: Callable[[str], None] | None = None,
        cancel_check: Callable[[], None] | None = None,
        on_reasoning_delta: Callable[[str], None] | None = None,
        on_subagent_event: Callable[[str, dict[str, Any]], None] | None = None,
        on_todo_update: Callable[[dict[str, Any]], None] | None = None,
        on_stream_rollback: Callable[[], None] | None = None,
    ) -> str:
        """执行一轮 Agent 任务，并把最终回答交给 on_delta 输出。

        外层继续拥有用户输入、Skill、Session、Plugin 与 Runtime 生命周期；内部
        ``AgentLoopRunner`` 只处理模型与整批工具观察之间的协议循环。

        ``on_stream_rollback`` 可选：模型流式输出已展示部分内容后中断并自动重试
        前调用，外层应撤销已显示的半截回复，避免与新内容拼接错乱。
        """

        text = user_text.strip()
        if not text:
            raise AgentError("用户输入为空，无法发送给 Agent。")

        status = on_status or (lambda _message: None)
        report_tool_start = on_tool_start or (lambda _step, _tool_call: None)
        report_tool_result = on_tool_result or (lambda _tool_call, _result: None)
        external_token_usage = on_token_usage or (
            lambda _input_tokens, _output_tokens, _cached_input_tokens: None
        )
        turn_usage = TokenUsageSample()
        visible_output_seen = False
        run_guard = getattr(self.config, "run_guard", None)
        run_guard_enabled = bool(getattr(run_guard, "enabled", False))
        run_guard_config = (
            getattr(run_guard, "guard", None)
            if run_guard_enabled and bool(getattr(run_guard, "guard", None))
            and bool(getattr(getattr(run_guard, "guard", None), "enabled", False))
            else None
        )
        continuation_config = (
            getattr(run_guard, "continuation", None)
            if run_guard_enabled
            and bool(getattr(getattr(run_guard, "continuation", None), "enabled", False))
            else None
        )
        guard_retry_state = GuardRetryState(
            max_retries=(
                int(getattr(run_guard_config, "max_guard_retries", 0))
                if run_guard_config is not None
                and bool(getattr(run_guard_config, "enabled", False))
                else 0
            )
        )
        pause_event = threading.Event()
        pause_token = None
        pause_event_enabled = False
        tool_execution_seen = False
        context_overflow_recovered = False
        active_turn_snapshot: _ActiveTurnSnapshot | None = None
        turn_snapshot_finalization_started = False

        def report_token_usage(
            input_tokens: int,
            output_tokens: int,
            cached_input_tokens: int,
        ) -> None:
            nonlocal turn_usage
            turn_usage = turn_usage.add(
                input_tokens,
                output_tokens,
                cached_input_tokens,
            )
            external_token_usage(input_tokens, output_tokens, cached_input_tokens)

        _report_protocol_wait = on_protocol_wait or (lambda: None)
        report_retry_status = on_retry_status or status

        def check_cancelled() -> None:
            if cancel_check is not None:
                cancel_check()

        def should_stop_after_pause() -> bool:
            """暂停工具只停止自动路径，不影响用户后续主动发送的新回合。"""

            return pause_requested()

        previous_cancel_check = getattr(self, "_cancel_check", None)
        previous_reasoning_callback = getattr(self, "_reasoning_delta_callback", None)
        previous_subagent_callback = getattr(self, "_subagent_event_callback", None)
        previous_todo_callback = getattr(self, "_todo_update_callback", None)
        had_previous_fork_snapshot = "_active_fork_context_messages" in self.__dict__
        previous_fork_snapshot = self.__dict__.get("_active_fork_context_messages")
        self._cancel_check = cancel_check
        self._reasoning_delta_callback = on_reasoning_delta
        self._active_guard_retry_state = guard_retry_state
        self._active_run_guard_config = run_guard_config
        previous_todo_items = getattr(self, "_active_todo_items", ())
        self._active_todo_items: list[dict[str, Any]] = []
        self._subagent_event_callback = on_subagent_event
        self._todo_update_callback = on_todo_update
        self._ensure_mcp_tools_ready(status)
        text = self._apply_skill_command(text, status)
        continue_requested = self._is_continue_last_task_request(text)
        pending_text = getattr(self, "_pending_user_text", None)
        if not pending_text:
            pending_text = getattr(
                getattr(self, "_session_state", None),
                "pending_user_text",
                None,
            )
        text = self._resolve_continue_request(text)
        if continue_requested:
            session_todos = getattr(
                getattr(self, "_session_state", None),
                "todo_items",
                (),
            )
            source_todos = session_todos or previous_todo_items
            self._active_todo_items.extend(
                dict(item) for item in source_todos if isinstance(item, dict)
            )

        # turn.start 必须先于 PromptHistory / Session user_message，确保插件改写后的文本
        # 成为所有持久化与模型上下文使用的唯一权威版本。
        turn_id = f"turn-{id(text)}-{len(self._history)}"
        self._plugin_begin_turn()
        turn_payload = self._dispatch_plugin_hook(
            "turn.start",
            {"userText": text, "tags": []},
            turn_id=turn_id,
        )
        if turn_payload is None:
            self._plugin_end_turn()
            raise AgentError("turn.start 被插件拒绝。")
        text = str(turn_payload.get("userText", text) or text).strip()
        if not text:
            self._plugin_end_turn()
            raise AgentError("用户输入为空，无法发送给 Agent。")

        turn_terminal_sent = False
        runtime_manager: ModelRuntimeManager | None = None
        runtime_snapshot = None
        user_message_persisted = False
        pending_task_text = (
            pending_text if continue_requested and pending_text else text
        ).strip()
        try:
            # 暂停事件激活与 finally 清理必须在同一个保护边界内：任何
            # 后续异常（插件、turn.start、取消检查、上下文恢复失败等）
            # 都会由 finally 重置暂停上下文，避免遗留到下一回合。
            if run_guard_enabled:
                pause_token = activate_pause_event(pause_event)
                pause_event_enabled = True
            # 首个取消检查点放在 try 内：用户提交后立即 ESC 时，取消异常
            # 也能进入统一收尾（补写 user_message 并保留取消摘要），避免
            # 用户任务完全丢失在会话记录之外。
            check_cancelled()
            active_turn_snapshot = self._begin_turn_snapshot()
            self._pending_user_text = pending_task_text
            self._append_prompt_history(text)
            user_event_payload: dict[str, Any] = {
                "content": text,
                "pending_user_text": pending_task_text,
            }
            if continue_requested and self._active_todo_items:
                user_event_payload["todo_items"] = [
                    dict(item) for item in self._active_todo_items
                ]
            self._append_session_event("user_message", user_event_payload)
            user_message_persisted = True
            context_messages = self._context_messages(turn_id=turn_id)
            working_messages = [
                *context_messages,
                *self._history,
                {"role": "user", "content": text},
            ]
            # Fork 只能继承“本轮起点”这一份公开协议消息。之后 AgentLoopRunner
            # 会原地追加 assistant tool-call 与 tool-result；不能让后续状态、未
            # 配对的工具调用或父模型输出进入已创建子任务的上下文。功能关闭时不
            # 额外复制/脱敏历史，避免未启用 Fork 的普通回合承担额外开销。
            subagent_config = getattr(self.config, "subagents", None)
            if (
                isinstance(subagent_config, SubAgentConfig)
                and subagent_config.enabled
                and subagent_config.allow_fork
            ):
                self._active_fork_context_messages = self._freeze_fork_context_messages(
                    working_messages
                )

            # TTS 指令仅供本轮首次模型请求使用；首次请求结束后由
            # request_main_reply 清除，后续工具观察请求不重复注入。
            self._active_tts_instruction = self._tts_speak_instruction()

            # 完整构造的 Agent 才持有带 profile_id 的 LLMConfig；部分内部单测
            # 使用最小对象并替换模型请求方法，此时跳过 Runtime 快照。
            llm_config = getattr(self.config, "llm", None)
            has_injected_runtime_manager = "_ensure_runtime_manager" in self.__dict__
            if llm_config is not None and (
                hasattr(llm_config, "profile_id") or has_injected_runtime_manager
            ):
                runtime_manager = self._ensure_runtime_manager()
                runtime_snapshot = runtime_manager.acquire_turn()
                self._active_runtime_snapshot = runtime_snapshot

            def request_main_reply(
                messages: list[dict[str, Any]],
            ) -> AgentModelReply:
                nonlocal visible_output_seen
                # 初次请求和每次工具观察后的后续请求都先 drain，确保当前回合
                # 内完成的后台任务无需等到下一条用户消息才被父 Agent 看见。
                self._inject_subagent_notifications(messages)

                def report_main_delta(delta: str) -> None:
                    nonlocal visible_output_seen
                    if delta:
                        visible_output_seen = True
                    on_delta(delta)

                try:
                    return self._request_agent_reply(
                        messages,
                        report_main_delta,
                        report_token_usage,
                        _report_protocol_wait,
                        report_retry_status,
                        on_stream_rollback=on_stream_rollback,
                    )
                finally:
                    # 首次模型请求的辅助指令只使用一次；工具结果回填后的后续请求
                    # 保持工具可用，但不重复注入同一段指令。
                    self.__dict__.pop("_active_tts_instruction", None)

            def execute_main_tool_batch(
                calls: Sequence[ToolCall],
                first_step: int,
            ) -> list[AgentLoopObservation]:
                nonlocal tool_execution_seen
                tool_execution_seen = True
                return self._execute_tool_batch(
                    calls,
                    first_step,
                    report_tool_start=report_tool_start,
                    report_tool_result=report_tool_result,
                    check_cancelled=check_cancelled,
                    status=status,
                    prompt=text,
                    active_runtime_snapshot=runtime_snapshot,
                    vision_base_llm=getattr(self.config, "llm", None),
                    record_tool_execution=lambda tool_call: self._record_turn_tool_execution(
                        active_turn_snapshot,
                        tool_call,
                    ),
                )

            try:
                loop_result = AgentLoopRunner().run(
                    messages=working_messages,
                    request_reply=request_main_reply,
                    execute_tool_batch=execute_main_tool_batch,
                    # 主 Agent 明确不设置循环预算；后续 SubAgent 可使用同一 Runner
                    # 传入 AgentLoopLimits，而不改变当前产品行为。
                    cancel_check=check_cancelled,
                    stop_check=should_stop_after_pause,
                )
            except Exception as exc:
                if not self._can_recover_context_overflow(
                    exc,
                    visible_output_seen=visible_output_seen or tool_execution_seen,
                ):
                    raise
                recovered_messages = self._recover_context_overflow_for_retry(
                    status=status,
                    check_cancelled=check_cancelled,
                )
                if recovered_messages is None:
                    raise
                context_overflow_recovered = True
                # 失败请求尚未产生任何可见文本或工具副作用；从新的摘要投影重建
                # Runner，避免把未完成的原始用户消息再次附加到模型上下文。
                working_messages = [*context_messages, *recovered_messages]
                if (
                    isinstance(subagent_config, SubAgentConfig)
                    and subagent_config.enabled
                    and subagent_config.allow_fork
                ):
                    self._active_fork_context_messages = self._freeze_fork_context_messages(
                        working_messages
                    )
                loop_result = AgentLoopRunner().run(
                    messages=working_messages,
                    request_reply=request_main_reply,
                    execute_tool_batch=execute_main_tool_batch,
                    cancel_check=check_cancelled,
                    stop_check=should_stop_after_pause,
                )

            # Continue 是回合内有限状态机：只有 Todo 仍有未完成项，或模型只
            # 输出 reasoning 而没有文本/工具时才自动补发；真实文本、工具调用、
            # 用户取消和 pause_work 都会终止自动路径。续跑请求异步发生在同一
            # worker 内，不会重入 Session 事件追加线程。
            continuation_count = 0
            continuation_reason = ""
            reply_text_parts: list[str] = []
            reasoning_parts: list[str] = []
            content_streamed_seen = False
            while True:
                if loop_result.final_text:
                    reply_text_parts.append(loop_result.final_text)
                if loop_result.reasoning:
                    reasoning_parts.append(loop_result.reasoning)
                content_streamed_seen = content_streamed_seen or loop_result.content_streamed
                if loop_result.paused or pause_requested() or continuation_config is None:
                    break
                last_reply = loop_result.last_reply
                reasoning_only = bool(
                    last_reply is not None
                    and last_reply.reasoning
                    and not last_reply.content.strip()
                    and not last_reply.tool_calls
                )
                incomplete_todo = bool(self._active_todo_items) and any(
                    not bool(item.get("completed"))
                    for item in self._active_todo_items
                )
                max_followups = int(
                    getattr(continuation_config, "max_auto_followups", 0)
                )
                if continuation_count >= max_followups or not (reasoning_only or incomplete_todo):
                    break
                continuation_count += 1
                continuation_reason = (
                    "reasoning_only" if reasoning_only else "todo_incomplete"
                )
                self._append_session_event(
                    "run_guard_continue",
                    {
                        "followup": continuation_count,
                        "reason": continuation_reason,
                        "pending_user_text": pending_task_text,
                    },
                )
                report_retry_status(
                    f"任务仍未完成，正在自动继续（第{continuation_count}次）"
                )
                if last_reply is not None:
                    working_messages.append(last_reply.message)
                working_messages.append(
                    {
                        "role": "user",
                        "content": (
                            "请继续执行上一任务，不要停在计划或推理阶段；"
                            "完成未完成的 Todo，或给出可执行的最终结果。"
                        ),
                    }
                )
                loop_result = AgentLoopRunner().run(
                    messages=working_messages,
                    request_reply=request_main_reply,
                    execute_tool_batch=execute_main_tool_batch,
                    cancel_check=check_cancelled,
                    stop_check=should_stop_after_pause,
                )

            final_reply = "\n\n".join(reply_text_parts).strip()
            combined_reasoning = "\n".join(reasoning_parts).strip()
            final_content_streamed = any(bool(part) for part in reply_text_parts) and content_streamed_seen
            if final_reply and not final_content_streamed:
                on_delta(final_reply)
            if loop_result.paused:
                # pause_work 是模型可见的主动暂停，不应写成错误或取消；保留
                # pending_user_text，用户下一次发送“继续”时可恢复原任务。
                self._append_session_event(
                    "run_guard_paused",
                    {
                        "user_text": text,
                        "pending_user_text": pending_task_text,
                        "reason": "pause_work",
                        "todo_items": [dict(item) for item in self._active_todo_items],
                    },
                )
            else:
                self._append_session_event(
                    "assistant_message",
                    {"content": final_reply},
                )
            if context_overflow_recovered:
                self._history.append(
                    self._assistant_message(
                        final_reply,
                        combined_reasoning,
                    )
                )
                self._run_context_compaction_after_turn(
                    context_messages=context_messages,
                    usage=turn_usage,
                    status=status,
                )
            else:
                self._turn_context_compaction_context_messages = context_messages
                self._turn_context_compaction_usage = turn_usage
                self._turn_context_compaction_status = status
                try:
                    # 保持既有三参数调用形态，兼容宿主扩展和最小测试替身。
                    self._append_history(
                        text,
                        final_reply,
                        combined_reasoning,
                    )
                finally:
                    self.__dict__.pop("_turn_context_compaction_context_messages", None)
                    self.__dict__.pop("_turn_context_compaction_usage", None)
                    self.__dict__.pop("_turn_context_compaction_status", None)
            continuation_exhausted = False
            if not loop_result.paused:
                self._pending_user_text = None
            # Continue 达到上限仍未完成时写入可恢复终态；事件保留待续文本，
            # 但正常 assistant_message 仍可进入模型历史，用户可显式发送“继续”。
            if not loop_result.paused and continuation_config is not None:
                max_followups = int(
                    getattr(continuation_config, "max_auto_followups", 0)
                )
                last_reply = loop_result.last_reply
                last_reasoning_only = bool(
                    last_reply is not None
                    and last_reply.reasoning
                    and not last_reply.content.strip()
                    and not last_reply.tool_calls
                )
                last_todo_incomplete = bool(self._active_todo_items) and any(
                    not bool(item.get("completed"))
                    for item in self._active_todo_items
                )
                continuation_exhausted = bool(
                    continuation_count >= max_followups
                    and continuation_count > 0
                    and (last_reasoning_only or last_todo_incomplete)
                )
            if continuation_exhausted:
                self._append_session_event(
                    "run_guard_continue_exhausted",
                    {
                        "followups": continuation_count,
                        "pending_user_text": pending_task_text,
                        "reason": continuation_reason or "todo_incomplete",
                        "todo_items": [dict(item) for item in self._active_todo_items],
                    },
                )
            if continuation_exhausted:
                self._pending_user_text = pending_task_text
            self._dispatch_plugin_hook(
                "turn.end",
                {
                    "userText": text,
                    "assistantText": final_reply,
                },
                turn_id=turn_id,
            )
            turn_snapshot_finalization_started = True
            self._complete_turn_snapshot(active_turn_snapshot)
            turn_terminal_sent = True
            return final_reply
        except KeyboardInterrupt as exc:
            snapshot_failure: Exception | None = None
            if not turn_snapshot_finalization_started:
                turn_snapshot_finalization_started = True
                try:
                    self._complete_turn_snapshot(active_turn_snapshot)
                except Exception as completion_exc:
                    snapshot_failure = completion_exc
            terminal_exc = snapshot_failure or exc
            if not user_message_persisted:
                # 取消发生在用户消息持久化之前（快速 ESC 竞态）：补写
                # 提示历史与 user_message，保证 Session 恢复投影与内存
                # 历史一致，后续提问仍能看到被取消的任务。
                self._append_prompt_history(text)
                self._append_session_event(
                    "user_message",
                    {"content": text, "pending_user_text": text},
                )
                user_message_persisted = True
            self._append_session_event(
                "turn_cancelled",
                {
                    "user_text": text,
                    "reason": str(terminal_exc),
                    "pending_user_text": pending_task_text,
                    "summary": self._cancelled_turn_summary(active_turn_snapshot),
                },
            )
            # 被取消的回合同样写入历史：只保留任务文本与已执行工具摘要，
            # 保证用户紧接着发送的后续消息仍能看到上一轮任务与进度，
            # 避免 Agent 把延续任务误判为无前置信息的新任务。
            self._history.extend(
                [
                    {"role": "user", "content": text},
                    self._assistant_message(self._cancelled_turn_summary(active_turn_snapshot)),
                ]
            )
            if not turn_terminal_sent:
                self._dispatch_plugin_hook(
                    "turn.cancelled",
                    {"userText": text, "reason": str(terminal_exc)},
                    turn_id=turn_id,
                )
                turn_terminal_sent = True
            if snapshot_failure is not None:
                raise snapshot_failure from exc
            raise
        except Exception as exc:
            snapshot_failure = None
            if not turn_snapshot_finalization_started:
                turn_snapshot_finalization_started = True
                try:
                    self._complete_turn_snapshot(active_turn_snapshot)
                except Exception as completion_exc:
                    snapshot_failure = completion_exc
            terminal_exc = snapshot_failure or exc
            event_type = (
                "turn_cancelled"
                if self._is_turn_cancel_exception(terminal_exc)
                else "session_interrupted"
            )
            if event_type == "turn_cancelled" and not user_message_persisted:
                # 取消被 Provider/协议层包装成普通异常时，同样可能发生在
                # 消息持久化之前；补写后取消回合才能被完整恢复。
                self._append_prompt_history(text)
                self._append_session_event(
                    "user_message",
                    {"content": text, "pending_user_text": text},
                )
                user_message_persisted = True
            self._append_session_event(
                event_type,
                {
                    "user_text": text,
                    "pending_user_text": pending_task_text,
                    "reason": str(terminal_exc),
                    **(
                        {"summary": self._cancelled_turn_summary(active_turn_snapshot)}
                        if event_type == "turn_cancelled"
                        else {}
                    ),
                },
            )
            if event_type == "turn_cancelled":
                # 与 KeyboardInterrupt 取消路径一致：把任务文本与已执行工具摘要
                # 写入历史，避免后续回合丢失被取消任务的前置上下文。
                self._history.extend(
                    [
                        {"role": "user", "content": text},
                        self._assistant_message(self._cancelled_turn_summary(active_turn_snapshot)),
                    ]
                )
            if not turn_terminal_sent:
                hook_name = "turn.cancelled" if event_type == "turn_cancelled" else "turn.error"
                self._dispatch_plugin_hook(
                    hook_name,
                    {"userText": text, "reason": str(terminal_exc)},
                    turn_id=turn_id,
                )
                turn_terminal_sent = True
            if snapshot_failure is not None:
                raise snapshot_failure from exc
            raise
        finally:
            if runtime_manager is not None and runtime_snapshot is not None:
                runtime_manager.release_turn(runtime_snapshot)
            self.__dict__.pop("_active_runtime_snapshot", None)
            self.__dict__.pop("_active_guard_retry_state", None)
            self.__dict__.pop("_active_run_guard_config", None)
            self.__dict__.pop("_active_todo_items", None)
            if pause_event_enabled and pause_token is not None:
                reset_pause_event(pause_token)
            if had_previous_fork_snapshot:
                self._active_fork_context_messages = previous_fork_snapshot
            else:
                self.__dict__.pop("_active_fork_context_messages", None)
            self._plugin_end_turn()
            self._cancel_check = previous_cancel_check
            self._reasoning_delta_callback = previous_reasoning_callback
            self._subagent_event_callback = previous_subagent_callback
            self._todo_update_callback = previous_todo_callback

    def _execute_tool_batch(
        self,
        raw_tool_calls: Sequence[ToolCall],
        first_step: int,
        *,
        report_tool_start: Callable[[int, ToolCall], None],
        report_tool_result: Callable[[ToolCall, ToolResult], None],
        check_cancelled: Callable[[], None],
        status: Callable[[str], None],
        prompt: str = "",
        tools: Mapping[str, ToolDefinition] | None = None,
        active_runtime_snapshot: Any | None = None,
        vision_base_llm: LLMConfig | None = None,
        on_token_usage: Callable[[int, int, int], None] | None = None,
        execution_cache: dict[str, ToolResult] | None = None,
        persist_session_events: bool = True,
        record_tool_execution: Callable[[ToolCall], None] | None = None,
        tool_timeout_seconds: int | None = None,
    ) -> list[AgentLoopObservation]:
        """规范化、审批并执行一次模型回复中的完整工具批次。

        所有调用先按模型顺序完成规范化和审批，之后一次性并发执行；不再以
        工具类型建立串行屏障。最终 observation 仍按模型调用顺序回填，与实际
        完成先后无关；UI 完成事件则在各工具真实结束时立即发送。

        ``tool_timeout_seconds`` 缺省时读 ``AgentConfig.tool_timeout_seconds``
        （默认 600 秒）：批次使用统一的绝对截止时间，挂起工具在限时后返回错误
        结果，不再无限等待。超时后工具线程仍在后台运行（无法安全强杀），其结果
        被丢弃。
        """

        if tool_timeout_seconds is None:
            tool_timeout_seconds = getattr(
                getattr(self, "config", None),
                "tool_timeout_seconds",
                DEFAULT_TOOL_TIMEOUT_SECONDS,
            )

        active_tools = self._tools if tools is None else tools
        active_tools = dict(active_tools)
        catalog = HostToolCatalog(active_tools)
        normalized_calls: list[tuple[int, ToolCall, ToolDefinition | None, ToolResult | None]] = []
        for offset, raw_tool_call in enumerate(raw_tool_calls):
            check_cancelled()
            if raw_tool_call.name == INVOKE_TOOL_NAME:
                # invoke_tool 已从 Provider 工具面移除（顶层注册真实工具名），
                # 但保留 Host 分发路径兼容旧测试与直接构造的内部调用。
                prepared = catalog.prepare_invocation(raw_tool_call.arguments)
                if isinstance(prepared, ToolResult):
                    tool_call = ToolCall(
                        name=INVOKE_TOOL_NAME,
                        arguments=dict(raw_tool_call.arguments),
                        id=raw_tool_call.id,
                        function_name=raw_tool_call.function_name,
                    )
                    tool = None
                    denied_result = prepared
                else:
                    tool_call = ToolCall(
                        name=prepared.tool_name,
                        arguments=prepared.arguments,
                        id=raw_tool_call.id,
                        function_name=raw_tool_call.function_name,
                    )
                    tool = prepared.tool
                    denied_result = None
            else:
                # 顶层注册所有工具：模型回传的真实工具名按 Host 完整目录直接
                # 分发，不再区分 Provider 工具面与 Host 目录（同一套名字）。
                tool_call = normalize_tool_call(raw_tool_call, active_tools)
                tool = active_tools.get(tool_call.name)
                denied_result = None

            if persist_session_events:
                if tool_call.name == INVOKE_TOOL_NAME and tool is None:
                    public_arguments = public_invoke_arguments(tool_call.arguments)
                else:
                    public_arguments = public_tool_arguments(
                        tool_call.name,
                        tool_call.arguments,
                    )
                self._append_session_event(
                    "tool_call_requested",
                    {
                        "tool": tool_call.name,
                        "arguments": public_arguments,
                        "tool_call_id": tool_call.id,
                        "function_name": tool_call.function_name,
                    },
                )

            if tool is not None and denied_result is None:
                if persist_session_events:
                    denied_result = self._approve_tool_for_batch(tool, tool_call.arguments)
                else:
                    denied_result = self._approve_tool_for_batch(
                        tool,
                        tool_call.arguments,
                        persist_session_events=False,
                    )
            normalized_calls.append((first_step + offset, tool_call, tool, denied_result))

        results: list[ToolResult | None] = [None] * len(normalized_calls)
        # 每个工具的真实完成时刻（time.perf_counter 时钟，与 UI 的 started_at/
        # finished_at 同源），供 UI 展示各自耗时与完成先后；与模型回填顺序
        # 无关，绝不影响模型看到的调用顺序。
        completed_at: dict[int, float] = {}
        completion_lock = threading.Lock()
        timed_out: set[int] = set()
        reported: set[int] = set()

        def _run_in_process(
            runner: Callable[[dict[str, Any]], ToolResult],
            arguments: dict[str, Any],
        ) -> ToolResult:
            return runner(arguments)

        def _run_in_subprocess(
            runner: Callable[[dict[str, Any]], ToolResult],
            arguments: dict[str, Any],
        ) -> ToolResult:
            try:
                return run_tool_in_subprocess(
                    runner,
                    arguments,
                    timeout_seconds=tool_timeout_seconds,
                )
            except ToolProcessCancelled as exc:
                raise KeyboardInterrupt(str(exc)) from exc
            except ToolProcessTimeout as exc:
                return ToolResult(ok=False, output=str(exc), retryable=True)
            except ToolProcessError as exc:
                return ToolResult(ok=False, output=str(exc))

        def execute_call(index: int) -> ToolResult:
            _call_step, tool_call, tool, denied_result = normalized_calls[index]
            if denied_result is not None:
                return denied_result
            if tool is None:
                return _unknown_tool_result(
                    tool_call.name,
                    active_tools,
                )
            cache_key = ""
            if execution_cache is not None:
                cache_key = json.dumps(
                    {"tool": tool_call.name, "arguments": tool_call.arguments},
                    ensure_ascii=False,
                    sort_keys=True,
                    separators=(",", ":"),
                )
                cached = execution_cache.get(cache_key)
                if cached is not None:
                    return cached
            if record_tool_execution is not None:
                record_tool_execution(tool_call)
            # 普通工具 callable 位于独立 Python 子进程；ESC 关闭当前 owner 的资源时
            # 可直接杀死整个子进程树，而不是仅设置线程取消标志。审批/插件钩子仍在
            # 父进程执行，只有已批准的同步 runner 进入隔离进程。
            if (
                can_serialize_tool_runner(tool.run)
                and tool.run_in_subprocess
                and tool.name not in {"bash", "powershell", "monitor"}
            ):
                # 审批与插件 hook 仍在父进程；只有已批准的实际 runner 进入子进程。
                result = self._execute_approved_tool(
                    tool,
                    tool_call.arguments,
                    runner=_run_in_subprocess,
                )
            else:
                # Agent 内部绑定方法可能持有锁、线程池或客户端，不能安全复制；这类
                # 状态型 Host 工具保持原有进程内路径。普通顶层函数/闭包进入隔离进程。
                result = self._execute_approved_tool(tool, tool_call.arguments)
            if execution_cache is not None and cache_key:
                execution_cache[cache_key] = result
            return result

        def report_completed_result(index: int, result: ToolResult) -> None:
            """工具线程完成时立即通知 UI，不等待同批次的慢工具。"""

            finished_at = time.perf_counter()
            with completion_lock:
                completed_at[index] = finished_at
                if index in timed_out or index in reported:
                    return
                reported.add(index)
            _step, tool_call, _tool, _denied = normalized_calls[index]
            # 回调可能把事件投递到 UI 线程，也可能由 SubAgent 直接消费；
            # 两种调用方都必须在真实完成时刻收到结果，而不是等模型顺序回填。
            try:
                report_tool_result(
                    tool_call,
                    replace(result, completed_at=finished_at),
                )
            except Exception:  # noqa: BLE001 - 展示回调不得改变工具结果
                LOGGER.warning("工具完成事件回调失败", exc_info=True)

        # 所有已通过规范化/审批的调用一次性提交到线程池。工具结果给模型仍按
        # 调用顺序回填，但 UI 完成事件由各自 worker 立即发送，彻底解除“慢工具
        # 阻塞快工具计时/显示”的隐式串行屏障。
        for call_step, tool_call, _tool, _denied in normalized_calls:
            report_tool_start(call_step, tool_call)
        executor = ThreadPoolExecutor(max_workers=max(1, len(normalized_calls)))
        futures: dict[int, Any] = {}
        submitted_at = time.perf_counter()
        try:
            turn_context = copy_context()

            def execute_call_timed(index: int) -> ToolResult:
                result = execute_call(index)
                report_completed_result(index, result)
                return result

            futures = {
                index: executor.submit(
                    turn_context.copy().run,
                    execute_call_timed,
                    index,
                )
                for index in range(len(normalized_calls))
            }
            # 只按模型调用顺序等待和回填；等待窗口使用同一批次的绝对截止时间，
            # 避免多个慢工具把“每个工具 timeout”错误地累加成批次 timeout。
            for index in range(len(normalized_calls)):
                deadline = submitted_at + tool_timeout_seconds
                remaining = max(0.0, deadline - time.perf_counter())
                try:
                    results[index] = futures[index].result(timeout=remaining)
                except FutureTimeoutError:
                    timeout_result = _tool_timeout_result(tool_timeout_seconds)
                    timeout_at = time.perf_counter()
                    with completion_lock:
                        timed_out.add(index)
                        completed_at[index] = timeout_at
                        should_report = index not in reported
                        reported.add(index)
                    results[index] = timeout_result
                    if should_report:
                        _step, tool_call, _tool, _denied = normalized_calls[index]
                        try:
                            report_tool_result(
                                tool_call,
                                replace(timeout_result, completed_at=timeout_at),
                            )
                        except Exception:  # noqa: BLE001 - 展示回调不得改变超时结果
                            LOGGER.warning("工具超时事件回调失败", exc_info=True)
        finally:
            # 超时线程无法安全强杀；不等待它自然退出，避免已经返回的回合
            # 再次被后台工具拖住。worker 完成后也不会重复发送完成事件。
            executor.shutdown(wait=False)

        check_cancelled()
        # 批次输出预算：单工具 >50K 或回合聚合 >200K 的输出落盘，模型上下文
        # 只保留头尾预览与文件路径（模型可用 read_file 读取完整内容）。
        results = self._apply_batch_output_budget(results)
        observations: list[AgentLoopObservation] = []
        for index, ((_call_step, tool_call, _tool, _denied), tool_result) in enumerate(
            zip(normalized_calls, results)
        ):
            assert tool_result is not None
            prepared_result, followup_messages = self._prepare_tool_result_for_model(
                tool_call,
                tool_result,
                prompt=prompt,
                active_runtime_snapshot=active_runtime_snapshot,
                vision_base_llm=vision_base_llm,
                check_cancelled=check_cancelled,
                on_token_usage=on_token_usage,
            )
            # 工具线程完成时已经向 UI 发出过一次结果事件；这里仅把经过
            # 输出预算/视觉处理的结果写回模型与 Session，避免慢工具回填时
            # 再次触发 UI 结果事件，把已完成工具的时钟重新推进。
            if index in completed_at:
                prepared_result = replace(
                    prepared_result,
                    completed_at=completed_at[index],
                )
            if persist_session_events:
                self._append_session_event(
                    "tool_result",
                    {
                        "tool": tool_call.name,
                        "tool_call_id": tool_call.id,
                        "ok": prepared_result.ok,
                        "output": prepared_result.full_output or prepared_result.output,
                        "model_output": prepared_result.output,
                        "ui_artifact": prepared_result.ui_artifact,
                    },
                )
            observations.append(
                AgentLoopObservation(
                    tool_call=tool_call,
                    result=prepared_result,
                    message=self._tool_result_message(tool_call, prepared_result),
                    followup_messages=followup_messages,
                )
            )
        status("")
        return observations

    def _apply_skill_command(self, text: str, status: Callable[[str], None]) -> str:
        """处理 /skill:name，并在每轮开始时清空上一轮手动 Skill 注入。"""

        self._active_skills = []
        if self._skill_manager is None or not text.startswith("/skill:"):
            return text

        parts = text.split(None, 1)
        skill_name = parts[0][len("/skill:") :].strip()
        skill = self._skill_manager.match_by_name(skill_name)
        if skill is None:
            status(f"未找到 Skill：{skill_name}")
            available = ", ".join(m.name for m in self._skill_manager.list_all()) or "无"
            return f"Skill「{skill_name}」不存在。当前可用的 Skill：{available}"

        self._active_skills = [
            SkillMatchResult(skill=skill, score=1.0, reason=f"手动调用：{skill_name}")
        ]
        status(f"已加载 Skill：{skill_name}")
        return parts[1] if len(parts) > 1 else f"请执行 {skill_name} 技能。"

    def _resolve_continue_request(self, text: str) -> str:
        """把短“继续/重试”恢复为上一轮未完成的真实用户任务。"""

        if not self._is_continue_last_task_request(text):
            return text

        pending_text = (getattr(self, "_pending_user_text", None) or "").strip()
        if not pending_text:
            return text

        return (
            "继续上一轮未完成任务。上一轮任务内容如下，请不要要求用户重复说明，"
            "直接基于这个任务继续执行或重试：\n"
            f"{pending_text}"
        )

    def _tts_speak_instruction(self) -> str:
        """返回本轮首次模型请求使用的 TTS system 指令；条件不满足时返回空串。

        触发条件：``config.tts.enabled`` 为真、``tts_synthesize`` 工具实际
        注册（未被工具开关禁用）、且 ONNX 模型已就绪（避免指令促使模型在
        模型缺失时触发数分钟的自动下载阻塞回合）。

        指令由当前回合在首次请求前暂存，拼接到 system prompt；首次请求
        完成后清除，因此工具结果后的后续请求不会重复注入，也不会改变
        TTS 工具的可用性。
        """

        config = getattr(self, "config", None)
        tts = getattr(config, "tts", None)
        if tts is None or not getattr(tts, "enabled", False):
            return ""
        tools = getattr(self, "_tools", None) or {}
        if "tts_synthesize" not in tools:
            return ""
        try:
            from omnicrawl.tts import models_ready
        except Exception:  # noqa: BLE001 - 依赖缺失按模型未就绪处理
            return ""
        try:
            model_dir = tts.resolved_model_dir()
        except Exception:  # noqa: BLE001
            model_dir = None
        try:
            if not models_ready(model_dir):
                return ""
        except Exception:  # noqa: BLE001
            return ""
        return _TTS_SPEAK_INSTRUCTION

    @staticmethod
    def _is_continue_last_task_request(text: str) -> bool:
        normalized = re.sub(r"[\s，。.!！?？]+", "", text.strip()).lower()
        return normalized in _CONTINUE_LAST_TASK_TEXTS

    @staticmethod
    def _is_turn_cancel_exception(exc: Exception) -> bool:
        """识别 UI 主动取消异常，避免把用户停止生成误记为异常中断。

        优先依据统一取消错误码（``ModelErrorCode.CANCELLED``）判断，
        同时兼容基于类名的旧取消约定；包装异常沿因果链检查，确保
        Provider/协议层包装后的取消仍然进入 ``turn_cancelled`` 收尾。
        """

        for candidate in (exc, *TurnLoopMixin._exception_causes(exc)):
            name = candidate.__class__.__name__.casefold()
            if "cancel" in name:
                return True
            if (
                isinstance(candidate, ModelError)
                and candidate.code == ModelErrorCode.CANCELLED
            ):
                return True
        return False

    def _project_instructions_messages(self) -> list[dict[str, str]]:
        """构造项目规范上下文消息，保留给测试和兼容调用使用。

        这个消息不写入 `_history`。项目规范来自工作区文件，必须带来源和权限
        边界，避免被模型当作可覆盖 system 的高优先级规则。
        """

        return build_project_instructions_messages(self._load_agents_instructions())

    def _context_messages(self, *, turn_id: str | None = None) -> list[dict[str, str]]:
        """构造 system 之外的稳定/动态上下文消息。"""

        workspace_detection_summary = getattr(
            getattr(self, "config", None),
            "workspace_detection_summary",
            "",
        )
        # context.build.before 只允许附加上下文，不改写用户原始消息。
        context_payload = self._dispatch_plugin_hook(
            "context.build.before",
            {"additionalContext": []},
            turn_id=turn_id,
        ) or {"additionalContext": []}
        messages = build_context_messages(
            workspace_root=self.workspace_root,
            project_instructions=self._load_agents_instructions(),
            skill_manager=getattr(self, "_skill_manager", None),
            active_skills=getattr(self, "_active_skills", []),
            tools=self._provider_tools().values(),
            agent_temp_dir=self._agent_temp_dir_display(),
            workspace_detection_summary=workspace_detection_summary,
        )
        extra = context_payload.get("additionalContext") or []
        if isinstance(extra, list):
            for item in extra:
                if isinstance(item, str) and item.strip():
                    messages.append(
                        {
                            "role": "user",
                            "content": (
                                '<plugin_context source="hook:context.build.before">\n'
                                f"{item.strip()}\n"
                                "</plugin_context>"
                            ),
                        }
                    )
                elif isinstance(item, dict) and item.get("content"):
                    messages.append(
                        {
                            "role": str(item.get("role") or "user"),
                            "content": str(item.get("content")),
                        }
                    )
        self._dispatch_plugin_hook(
            "context.build.after",
            {"messageCount": len(messages)},
            turn_id=turn_id,
        )
        return messages

    def _load_agents_instructions(self) -> str:
        """合并用户级和项目级 AGENTS.md，项目级规则排在后面并优先。"""

        workspace_root = getattr(self, "workspace_root", None)
        paths: list[tuple[str, Path]] = [("用户级", global_agents_path())]
        if workspace_root is not None:
            paths.append(("项目级", workspace_root / AGENTS_INSTRUCTIONS_FILE))

        sections: list[str] = []
        for scope, path in paths:
            if not path.is_file():
                continue
            try:
                content = path.read_text(encoding="utf-8").strip()
            except UnicodeDecodeError as exc:
                raise AgentError(f"{scope} {AGENTS_INSTRUCTIONS_FILE} 必须是 UTF-8 文本。") from exc
            except OSError as exc:
                raise AgentError(f"读取{scope} {AGENTS_INSTRUCTIONS_FILE} 失败：{exc}") from exc
            if content:
                sections.append(f"【{scope} AGENTS.md】\n{content}")

        if not sections:
            return ""
        return (
            "用户级规则提供默认协作约束；项目级规则针对当前工作区，项目级规则优先。\n\n"
            + "\n\n".join(sections)
        )

    def _request_agent_reply(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        on_retry_status: Callable[[str], None],
        on_stream_rollback: Callable[[], None] | None = None,
    ) -> AgentModelReply:
        """请求模型给出下一步：要么返回 tool_calls，要么输出最终回答。"""

        # model.request.before：可改 messages content / 采样参数；不暴露凭据。
        request_payload = self._dispatch_plugin_hook(
            "model.request.before",
            {
                "messages": messages,
                "model": self.config.llm.model,
            },
        )
        if request_payload is None:
            raise AgentError("model.request.before 被插件拒绝。")
        if isinstance(request_payload.get("messages"), list):
            messages = request_payload["messages"]  # type: ignore[assignment]

        # 自动审查保留消息快照：仅用于提取最近一条用户消息摘要，供审查者
        # 理解任务意图（不再复用完整上下文，避免污染）。按线程隔离，子代理
        # 审查使用各自上下文。
        review_local = getattr(self, "_review_context_local", None)
        if review_local is not None:
            review_local.messages = list(messages)

        try:
            reply = self._llm_protocol().request_reply(
                messages,
                on_delta,
                on_token_usage,
                on_protocol_wait,
                on_retry_status,
                getattr(self, "_cancel_check", None),
                getattr(self, "_reasoning_delta_callback", None),
                getattr(self, "_active_runtime_snapshot", None),
                on_stream_rollback,
            )
        except AgentProtocolError as exc:
            self._dispatch_plugin_hook(
                "model.request.error",
                {"error": str(exc), "model": self.config.llm.model},
            )
            raise AgentError(str(exc)) from exc

        # V1 model.response.after 仅 observe，避免流式 UI 与历史分叉。
        self._dispatch_plugin_hook(
            "model.response.after",
            {
                "model": self.config.llm.model,
                "content": reply.content,
                "toolCallCount": len(reply.tool_calls),
            },
        )
        return reply

    def _request_agent_reply_once(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        cancel_check: Callable[[], None] | None = None,
        on_reasoning_delta: Callable[[str], None] | None = None,
    ) -> AgentModelReply:
        try:
            return self._llm_protocol().request_reply_once(
                messages,
                on_delta,
                on_token_usage,
                on_protocol_wait,
                cancel_check,
                on_reasoning_delta
                if on_reasoning_delta is not None
                else getattr(self, "_reasoning_delta_callback", None),
                getattr(self, "_active_runtime_snapshot", None),
            )
        except AgentProtocolError as exc:
            raise AgentError(str(exc)) from exc

    def _llm_protocol(self) -> AgentLLMProtocol:
        """按当前运行态创建轻量协议对象，便于测试替换回调方法。"""

        provider_tools = self._provider_tools()
        return AgentLLMProtocol(
            client=self._llm_client(),
            model=self.config.llm.model,
            request_timeout_seconds=self.config.request_timeout_seconds,
            request_retry_count=getattr(self.config, "request_retry_count", 1),
            workspace_root=getattr(self, "workspace_root", Path.cwd()),
            system_prompt_provider=self._system_prompt,
            prompt_cache_identity_provider=self._prompt_cache_identity,
            tools_provider=self._chat_completion_tools,
            extra_body_provider=self._build_extra_body,
            tool_name_from_function_name=lambda function_name: tool_name_from_function_name(
                function_name,
                provider_tools,
            ),
            function_name_for_tool=function_name_for_tool,
            runtime_manager=self._runtime_manager_for_protocol(),
            reasoning_guard_config=getattr(self, "_active_run_guard_config", None),
            guard_retry_state=getattr(self, "_active_guard_retry_state", None),
            reasoning_effort_provider=lambda: getattr(
                self.config.llm,
                "reasoning_effort",
                "medium",
            ),
        )

    def _runtime_manager_for_protocol(self) -> ModelRuntimeManager | None:
        """完整 LLMConfig 使用统一 Runtime；遗留最小夹具保留直连 client。"""

        llm = getattr(self.config, "llm", None)
        if llm is None or not hasattr(llm, "profile_id"):
            return None
        return self._ensure_runtime_manager()

    def _ensure_runtime_manager(self) -> ModelRuntimeManager:
        """惰性初始化统一模型 Runtime，并与当前 llm 配置对齐。"""

        manager = getattr(self, "_runtime_manager", None)
        if manager is None:
            manager = ModelRuntimeManager()
            self._runtime_manager = manager
            self._runtime_model_id = ""

        current_model = self.config.llm.model
        if (
            manager.active_snapshot is None
            or getattr(self, "_runtime_model_id", "") != current_model
        ):
            profile, descriptor = llm_config_to_profile_and_descriptor(self.config.llm)
            if manager.active_snapshot is None:
                manager.bootstrap(profile, descriptor)
            else:
                manager.switch(profile, descriptor)
            self._runtime_model_id = current_model
            # base_url/api_key 可能随 profile 变化，丢弃旧 client。
            self.__dict__.pop("_client", None)
        return manager

    def _llm_client(self) -> Any:
        """首次请求模型时再创建 OpenAI SDK 客户端。

        OpenAI SDK 导入链较重，放在 Agent 构造期会明显拖慢服务或 TUI 启动。
        客户端只在模型请求或自动审查时需要，因此惰性创建不会减少能力，
        还能让启动阶段先把 UI 呈现给用户。
        统一 Runtime 路径下 client 主要供自动审查等遗留调用复用。
        """

        client = getattr(self, "_client", None)
        if client is not None:
            return client

        try:
            import httpx
            from openai import OpenAI
        except ImportError as exc:
            raise AgentError(
                "缺少 openai/httpx 依赖，请先执行：pip install -r requirements.txt"
            ) from exc

        http_client = httpx.Client(trust_env=False, follow_redirects=True)
        openai_kwargs: dict[str, Any] = {
            "api_key": self.config.llm.api_key,
            "base_url": self.config.llm.base_url,
            "http_client": http_client,
        }
        user_agent = getattr(self.config.llm, "user_agent", "").strip()
        if user_agent:
            openai_kwargs["default_headers"] = {"User-Agent": user_agent}
        try:
            client = OpenAI(**openai_kwargs)
        except Exception:
            http_client.close()
            raise
        self._client = client
        return client

    def _build_extra_body(self) -> dict[str, Any]:
        return build_extra_body(self.config.llm)

    def _provider_tools(self) -> dict[str, ToolDefinition]:
        """返回 Provider 顶层工具面：所有可见工具的压缩声明。"""

        return build_provider_tools(self._host_tool_catalog())

    def _host_tool_catalog(self) -> HostToolCatalog:
        """构造当前 Agent 的完整 Host 工具目录。"""

        return HostToolCatalog(getattr(self, "_tools", {}))

    def _chat_completion_tools(self) -> list[dict[str, Any]]:
        return chat_completion_tools(
            self._provider_tools().values(),
            function_name_for_tool=function_name_for_tool,
        )

    def _prompt_cache_identity(self) -> dict[str, str]:
        """返回只包含稳定上下文 hash 的 prompt cache 身份。"""

        return build_prompt_cache_identity(
            system_prompt=self._system_prompt(),
            workspace_root=getattr(self, "workspace_root", Path.cwd()),
            project_instructions=self._load_agents_instructions(),
            skill_manager=getattr(self, "_skill_manager", None),
            active_skills=getattr(self, "_active_skills", []),
            chat_tools=self._chat_completion_tools(),
        ).as_payload(model=self.config.llm.model)

    def _model_supports_vision(self, snapshot: Any | None = None) -> bool:
        active_snapshot = (
            snapshot
            if snapshot is not None
            else getattr(self, "_active_runtime_snapshot", None)
        )
        runtime = getattr(active_snapshot, "runtime", None)
        capabilities = getattr(runtime, "capabilities", None)
        return bool(getattr(capabilities, "vision", False))

    def _active_model_supports_vision(self) -> bool:
        """兼容旧调用方：判断当前主 Agent Runtime 是否支持视觉。"""

        return self._model_supports_vision()

    @staticmethod
    def _assistant_message(assistant_text: str, reasoning: str = "") -> dict[str, Any]:
        # 思考模式下网关要求历史 assistant 消息必须回传 reasoning_content，
        # 否则二次请求会被拒绝（HTTP 400：reasoning_content must be passed back）。
        message: dict[str, Any] = {"role": "assistant", "content": assistant_text}
        if reasoning:
            message["reasoning_content"] = reasoning
        return message

    @staticmethod
    def _cancelled_turn_summary(snapshot: _ActiveTurnSnapshot | None) -> str:
        """生成被取消回合的历史摘要（已执行工具名 + 次数）。

        取消时没有最终回复可写，但任务文本与已执行工具对后续回合延续上下文
        至关重要。刻意用纯文本 assistant 消息而非未配对的 tool_calls，避免
        破坏 chat/responses 协议的“assistant tool_calls 必须紧跟 tool 结果”约束。
        """

        if snapshot is None:
            return "（上一回合被取消，未生成最终回复）"
        counts: dict[str, int] = {}
        for name in snapshot.executed_tools:
            counts[name] = counts.get(name, 0) + 1
        if not counts:
            return "（上一回合被取消，未生成最终回复，未执行任何工具）"
        summary = "，".join(
            f"{name}×{count}" if count > 1 else name
            for name, count in counts.items()
        )
        return f"（上一回合被取消，未生成最终回复）已执行工具：{summary}"

    def _can_recover_context_overflow(
        self,
        exc: Exception,
        *,
        visible_output_seen: bool,
    ) -> bool:
        """只识别未产生可见输出的明确上下文容量失败。"""
        if visible_output_seen:
            return False
        if getattr(self.config, "context_compaction", None) is None:
            return False
        candidates = (exc, *self._exception_causes(exc))
        # 先扫描完整异常链，避免外层包装错误的 token 文案掩盖内层限流原因。
        for candidate in candidates:
            if isinstance(candidate, ModelError) and candidate.code == ModelErrorCode.RATE_LIMITED:
                return False
            status_code = getattr(candidate, "status_code", None)
            if status_code == 429:
                return False
            if any(marker in str(candidate).casefold() for marker in _RATE_LIMIT_ERROR_MARKERS):
                return False

        for candidate in candidates:
            if isinstance(candidate, ModelError):
                if candidate.code == ModelErrorCode.CONTEXT_LENGTH_EXCEEDED:
                    return True
                if candidate.code != ModelErrorCode.INVALID_REQUEST:
                    continue
                message = candidate.message
            else:
                message = str(candidate)
            lowered = message.casefold()
            if any(marker in lowered for marker in _CONTEXT_OVERFLOW_ERROR_MARKERS):
                return True
        return False

    @staticmethod
    def _exception_causes(exc: Exception) -> tuple[BaseException, ...]:
        """以有界链遍历包装异常，避免第三方异常构造环导致恢复逻辑失控。"""
        causes: list[BaseException] = []
        current = exc.__cause__ or exc.__context__
        while current is not None and len(causes) < 4 and current not in causes:
            causes.append(current)
            current = current.__cause__ or current.__context__
        return tuple(causes)

    def _recover_context_overflow_for_retry(
        self,
        *,
        status: Callable[[str], None],
        check_cancelled: Callable[[], None],
    ) -> list[dict[str, Any]] | None:
        """为当前未完成回合生成摘要，并返回仅含摘要和续接指令的重试历史。"""
        check_cancelled()
        config = self.config.context_compaction
        service = self._context_compaction_service()
        try:
            outcome = service.recover_from_context_overflow(
                source_events=self._context_compaction_source_events(),
                target_summary_tokens=config.target_summary_tokens,
                reasoning_effort=config.reasoning_effort,
                preserve_exact_evidence=config.preserve_exact_evidence,
            )
        except Exception:
            LOGGER.warning("上下文超限后的模型压缩失败，无法自动续接当前回合。", exc_info=True)
            return None
        if outcome.compact_payload is None or outcome.history_projection is None:
            diagnostic = outcome.diagnostic or "模型摘要未生成可用投影。"
            self._append_session_event(
                "context_overflow_recovery_failed",
                {"reason": diagnostic},
            )
            return None
        compact_payload = dict(outcome.compact_payload)
        archive_id = self._archive_compacted_events(compact_payload)
        if archive_id:
            compact_payload["archive_id"] = archive_id
        self._append_session_event("compact_summary", compact_payload)
        before_tokens = estimate_json_tokens(self._history)
        self._history = list(outcome.history_projection)
        self._write_compaction_memories(compact_payload)
        self._auto_recall_compaction_memory(compact_payload)
        self._append_session_event(
            "context_overflow_recovery",
            {
                "mode": "model_summary",
                "decision_reason": "context_overflow_recovery",
                "single_large_turn": bool(compact_payload.get("single_large_turn")),
                "archive_id": archive_id,
            },
        )
        self._append_session_event(
            "user_message",
            {"content": _CONTEXT_OVERFLOW_RECOVERY_PROMPT},
        )
        self._history.append(
            {"role": "user", "content": _CONTEXT_OVERFLOW_RECOVERY_PROMPT}
        )
        after_tokens = estimate_json_tokens(self._history)
        notice = self._format_compaction_notice(before_tokens, after_tokens)
        self._last_compaction_notice = notice or ""
        status(
            f"{notice}（检测到上下文超限，已压缩当前任务上下文并自动继续。）"
            if notice
            else "检测到上下文超限，已压缩当前任务上下文并自动继续。"
        )
        return list(self._history)

    def _append_history(self, user_text: str, assistant_text: str, reasoning: str = "") -> None:
        """写入完整回合，先记录可选预算快照，再执行现有确定性压缩。"""

        self._history.extend(
            [
                {"role": "user", "content": user_text},
                self._assistant_message(assistant_text, reasoning),
            ]
        )
        config = getattr(self.config, "context_compaction", None)
        status = getattr(self, "_turn_context_compaction_status", None)
        if config is None:
            self._compact_history(force=False, status=status)
            return
        self._run_context_compaction_after_turn(
            context_messages=getattr(
                self,
                "_turn_context_compaction_context_messages",
                (),
            ),
            usage=getattr(
                self,
                "_turn_context_compaction_usage",
                TokenUsageSample(),
            ),
            status=status,
        )

    @staticmethod
    def _confirm_in_terminal(tool_name: str, arguments: dict[str, Any]) -> bool:
        print("\nAgent 请求执行受限工具：")
        print(f"工具：{tool_name}")
        print("参数：")
        print(json.dumps(arguments, ensure_ascii=False, indent=2))
        answer = input("是否允许执行？直接回车=YES，输入 n/no/否=NO：").strip().lower()
        return answer not in {"n", "no", "否", "false"}
