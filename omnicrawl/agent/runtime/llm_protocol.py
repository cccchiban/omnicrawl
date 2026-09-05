"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping

from ..types import AgentModelReply, ToolCall, ToolDefinition
from ...llm import OpenAIResponseLLM, VALID_REASONING_EFFORTS
from ...llm.stream_registry import registered_stream_events, stream_owner_for
from .run_guard import (
    ConfiguredAutoRetryError,
    GuardRetryState,
    ReasoningGuardTriggered,
    configured_retry_code,
    wrap_reasoning_callback,
    pause_requested,
)


class AgentProtocolError(RuntimeError):
    """LLM 协议层失败；调用方负责转换成对外的 AgentError。"""


class EmptyAgentReply(AgentProtocolError):
    """网关请求成功但没有返回可用文本，交由上层按策略重试。"""


class RetryableAgentRequestError(AgentProtocolError):
    """模型请求遇到临时连接或服务端错误，可按请求重试策略重新发起。"""


class StreamInterruptedAfterOutputError(RetryableAgentRequestError):
    """模型流在已输出可见内容后中断。

    仅当调用方提供 ``on_stream_rollback`` 回调时，上层才会在重试前撤销
    已展示的内容再重新发起；未提供回调时保持既有行为（直接失败）。
    """


# Provider 以“正常结束”形态表达输出被截断的 finish_reason 值：
# - openai chat: length（max_tokens 用尽）/ content_filter；
# - openai responses: incomplete；
# - anthropic: max_tokens。
# 这些值必须进入重试/回滚路径，绝不能静默当正常完成（否则半截回复会
# 直接结束回合且不提示用户）。
_TRUNCATED_FINISH_REASONS = frozenset(
    {"length", "incomplete", "max_tokens", "content_filter", "failed"}
)


def _raise_if_finish_truncated(
    finish_reason: str,
    *,
    has_visible_output: bool,
) -> None:
    """finish_reason 表示截断时抛对应协议错误，触发上层重试/回滚。"""

    if finish_reason not in _TRUNCATED_FINISH_REASONS:
        return
    message = (
        f"模型输出被截断（finish_reason={finish_reason}），"
        "可能是输出长度或上下文窗口上限导致。"
    )
    if has_visible_output:
        raise StreamInterruptedAfterOutputError(message)
    raise RetryableAgentRequestError(message)


@dataclass(frozen=True)
class AgentLLMProtocol:
    """封装模型请求协议与 tool call 聚合逻辑。

    优先通过 ModelRuntimeManager 走统一 Runtime；若未提供 runtime_manager，
    则回退到直接 OpenAI Chat Completions client（兼容旧测试/调用）。
    """

    client: Any
    model: str
    request_timeout_seconds: int
    request_retry_count: int
    workspace_root: Path
    system_prompt_provider: Callable[[], str]
    prompt_cache_identity_provider: Callable[[], dict[str, str]]
    tools_provider: Callable[[], list[dict[str, Any]]]
    extra_body_provider: Callable[[], dict[str, Any]]
    tool_name_from_function_name: Callable[[str], str]
    function_name_for_tool: Callable[[str], str]
    runtime_manager: Any = None
    reasoning_effort_provider: Callable[[], str] | None = None
    # 运行节奏配置由 Agent 注入；缺省保持旧协议测试/嵌入调用兼容。
    reasoning_guard_config: Any = None
    # 同一主回合内的 Guard/白名单错误共享该计数，避免每次工具观察
    # 请求都重新获得完整护栏重试额度。
    guard_retry_state: GuardRetryState | None = None

    def request_reply(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        on_retry_status: Callable[[str], None],
        cancel_check: Callable[[], None] | None = None,
        on_reasoning_delta: Callable[[str], None] | None = None,
        runtime_snapshot: Any = None,
        on_stream_rollback: Callable[[], None] | None = None,
    ) -> AgentModelReply:
        """请求模型给出下一步：要么返回 tool_calls，要么输出最终回答。

        同一次请求的所有重试必须绑定同一个 Runtime 快照；Agent 工具循环可传入
        更外层获取的快照，使工具执行前后的多次模型请求也保持同一 Provider/协议。

        ``on_stream_rollback`` 可选：当流式输出已展示部分内容后中断并决定重试时，
        先调用该回调撤销已显示内容（例如删除 UI 中半截回复），再重新发起请求，
        避免重试成功后旧内容与新回复拼接错乱。
        """

        owned_snapshot = None
        if self.runtime_manager is not None and runtime_snapshot is None:
            owned_snapshot = self.runtime_manager.acquire_turn()
            runtime_snapshot = owned_snapshot
        try:
            request_attempt = 0
            guard_config = self.reasoning_guard_config
            retry_state = self.guard_retry_state
            if retry_state is None:
                retry_state = GuardRetryState(
                    max_retries=(
                        int(getattr(guard_config, "max_guard_retries", 0))
                        if guard_config is not None
                        and bool(getattr(guard_config, "enabled", False))
                        else 0
                    )
                )
            while True:
                # 开始请求前必须确认当前回合未被取消，避免在取消竞态中
                # 发起新的模型请求或继续重试（ESC 取消后流被关闭的场景）。
                if cancel_check is not None:
                    cancel_check()
                try:
                    return self.request_reply_once(
                        messages,
                        on_delta,
                        on_token_usage,
                        on_protocol_wait,
                        cancel_check,
                        on_reasoning_delta,
                        runtime_snapshot,
                    )
                except ReasoningGuardTriggered as exc:
                    if pause_requested():
                        raise AgentProtocolError(
                            "当前回合已暂停，已停止推理护栏自动重试。"
                        ) from exc
                    retry_number = retry_state.consume()
                    if retry_number is None:
                        raise AgentProtocolError(
                            "推理护栏已达到本回合自动重试上限，已停止本次请求。\n"
                            f"{exc.message}"
                        ) from exc
                    if on_stream_rollback is not None:
                        on_stream_rollback()
                    on_retry_status(
                        f"推理护栏已触发，正在自动重试（第{retry_number}次）"
                    )
                    if cancel_check is not None:
                        cancel_check()
                    continue
                except ConfiguredAutoRetryError as exc:
                    if pause_requested():
                        raise AgentProtocolError(
                            "当前回合已暂停，已停止上游错误自动重试。"
                        ) from exc
                    retry_number = retry_state.consume()
                    if retry_number is None:
                        raise AgentProtocolError(
                            f"上游错误 {exc.code} 已达到本回合自动重试上限，已停止本次请求。"
                        ) from exc
                    if on_stream_rollback is not None:
                        on_stream_rollback()
                    on_retry_status(
                        f"请求失败（{exc.code}），正在自动重试（第{retry_number}次）"
                    )
                    if cancel_check is not None:
                        cancel_check()
                    continue
                except EmptyAgentReply as exc:
                    request_attempt += 1
                    if request_attempt < self.request_retry_count:
                        if cancel_check is not None:
                            cancel_check()
                        on_retry_status(f"正在重试(第{request_attempt}次)")
                        continue
                    if cancel_check is not None:
                        cancel_check()
                    raise AgentProtocolError(
                        f"Agent 连续 {self.request_retry_count} 次返回空响应，已停止本轮请求。"
                    ) from exc
                except StreamInterruptedAfterOutputError as exc:
                    request_attempt += 1
                    if request_attempt < self.request_retry_count and on_stream_rollback is not None:
                        on_stream_rollback()
                        on_retry_status(f"正在重试(第{request_attempt}次)")
                        if cancel_check is not None:
                            cancel_check()
                        continue
                    raise AgentProtocolError(f"Agent 模型流中断：{exc}") from exc
                except RetryableAgentRequestError as exc:
                    request_attempt += 1
                    if request_attempt < self.request_retry_count:
                        on_retry_status(f"正在重试(第{request_attempt}次)")
                        if cancel_check is not None:
                            cancel_check()
                        continue
                    raise AgentProtocolError(f"Agent 模型请求中断：{exc}") from exc
        finally:
            if owned_snapshot is not None:
                self.runtime_manager.release_turn(owned_snapshot)

    def request_reply_once(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        cancel_check: Callable[[], None] | None = None,
        on_reasoning_delta: Callable[[str], None] | None = None,
        runtime_snapshot: Any = None,
    ) -> AgentModelReply:
        """执行一次模型请求；Runtime 优先，否则回退 Chat Completions。"""

        if self.runtime_manager is not None:
            return self._request_via_runtime(
                messages,
                on_delta,
                on_token_usage,
                on_protocol_wait,
                cancel_check,
                on_reasoning_delta,
                runtime_snapshot,
                self.reasoning_guard_config,
            )
        return self._request_via_openai_client(
            messages,
            on_delta,
            on_token_usage,
            on_protocol_wait,
            cancel_check,
            on_reasoning_delta,
            self.reasoning_guard_config,
        )

    def _request_via_runtime(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        cancel_check: Callable[[], None] | None,
        on_reasoning_delta: Callable[[str], None] | None,
        runtime_snapshot: Any = None,
        reasoning_guard_config: Any = None,
    ) -> AgentModelReply:
        from ...llm.errors import ModelError, ModelErrorCode
        from ...llm.protocol import (
            ModelTurnRequest,
            ReasoningDelta,
            ResponseCompleted,
            TextDelta,
            ToolCallCompleted,
            ToolCallStarted,
            UsageUpdated,
            conversation_from_openai_messages,
        )

        owned_snapshot = None
        snapshot = runtime_snapshot
        if snapshot is None:
            owned_snapshot = self.runtime_manager.acquire_turn()
            snapshot = owned_snapshot
        try:
            system_prompt = self.system_prompt_provider()
            tools_payload = self.tools_provider() or []
            tool_specs = tuple(_tool_specs_from_openai_tools(tools_payload))
            extra_body = dict(self.extra_body_provider() or {})
            effort = ""
            if self.reasoning_effort_provider is not None:
                effort = str(self.reasoning_effort_provider() or "").strip()
            if not effort:
                effort = str(extra_body.get("reasoning_effort") or "").strip()

            # 模型默认生成参数：描述符 / provider_options / extra_body 合并。
            # 优先级：extra_body 显式 > 描述符字段 > provider_options 默认。
            generation_options = _build_generation_options(
                descriptor=snapshot.descriptor,
                extra_body=extra_body,
                reasoning_effort=effort,
                request_timeout_seconds=float(self.request_timeout_seconds),
                request_retry_count=int(self.request_retry_count),
            )

            request = ModelTurnRequest(
                identity=snapshot.runtime.identity,
                system_prompt=system_prompt,
                messages=conversation_from_openai_messages(messages),
                tools=tool_specs,
                generation_options=generation_options,
                prompt_cache_identity=self.prompt_cache_identity_provider(),
            )

            content_parts: list[str] = []
            reasoning_parts: list[str] = []
            completed_calls: list[ToolCall] = []
            guarded_reasoning_delta = wrap_reasoning_callback(
                on_reasoning_delta,
                reasoning_guard_config,
            )
            has_streamed_visible = False
            protocol_wait_sent = False
            latest_usage: tuple[int, int, int] | None = None
            cancellation_error: Exception | None = None
            finish_reason = "stop"

            try:
                for event in snapshot.runtime.stream_turn(request, cancel_check=cancel_check):
                    if isinstance(event, TextDelta) and event.text:
                        content_parts.append(event.text)
                        on_delta(event.text)
                        has_streamed_visible = True
                    elif isinstance(event, ReasoningDelta):
                        if event.text:
                            reasoning_parts.append(event.text)
                        if guarded_reasoning_delta is not None:
                            # 即使 Provider 给出空 reasoning 分片也交给护栏，
                            # 以便 block 计数与真实流分片保持一致。
                            guarded_reasoning_delta(event.text)
                    elif isinstance(event, ToolCallStarted):
                        if has_streamed_visible and not protocol_wait_sent:
                            on_protocol_wait()
                            protocol_wait_sent = True
                    elif isinstance(event, ToolCallCompleted):
                        if has_streamed_visible and not protocol_wait_sent:
                            on_protocol_wait()
                            protocol_wait_sent = True
                        function_name = event.name
                        completed_calls.append(
                            ToolCall(
                                name=self.tool_name_from_function_name(function_name),
                                arguments=dict(event.arguments or {}),
                                id=event.call_id,
                                function_name=function_name,
                            )
                        )
                    elif isinstance(event, UsageUpdated):
                        latest_usage = (
                            event.input_tokens,
                            event.output_tokens,
                            event.cached_input_tokens,
                        )
                    elif isinstance(event, ResponseCompleted):
                        # Provider 用 finish_reason 表达“正常结束但输出被截断”
                        # （length/incomplete/max_tokens/content_filter），
                        # 必须读取并在循环后检查，避免半截回复静默结束回合。
                        finish_reason = event.finish_reason or "stop"
            except ReasoningGuardTriggered:
                # 护栏错误必须穿透协议层，由 request_reply 统一消耗本回合
                # 共享重试额度；不能被包装成普通流中断后重复计数。
                raise
            except ModelError as exc:
                configured_code = configured_retry_code(exc, reasoning_guard_config)
                if configured_code is not None:
                    raise ConfiguredAutoRetryError(
                        configured_code,
                        str(exc),
                        cause=exc,
                    ) from exc
                # 取消是用户主动行为：无论取消来自 Provider 内部检查还是
                # 外层关闭流后的重新检查，都必须原样传播，不得包装成
                # 可重试错误或普通协议错误，否则取消历史收尾会丢失。
                if exc.code == ModelErrorCode.CANCELLED:
                    raise
                if exc.code == ModelErrorCode.EMPTY_RESPONSE:
                    raise EmptyAgentReply(str(exc)) from exc
                if exc.retryable or exc.code in {
                    ModelErrorCode.STREAM_INTERRUPTED,
                    ModelErrorCode.CONNECTION_FAILED,
                    ModelErrorCode.REQUEST_TIMEOUT,
                    ModelErrorCode.SERVICE_UNAVAILABLE,
                    ModelErrorCode.RATE_LIMITED,
                }:
                    if has_streamed_visible or completed_calls:
                        raise StreamInterruptedAfterOutputError(str(exc)) from exc
                    raise RetryableAgentRequestError(str(exc)) from exc
                raise AgentProtocolError(str(exc)) from exc
            except Exception as exc:
                if isinstance(exc, ReasoningGuardTriggered):
                    raise
                configured_code = configured_retry_code(exc, reasoning_guard_config)
                if configured_code is not None:
                    raise ConfiguredAutoRetryError(
                        configured_code,
                        str(exc),
                        cause=exc,
                    ) from exc
                if cancel_check is not None:
                    try:
                        cancel_check()
                    except Exception as cancel_exc:
                        cancellation_error = cancel_exc
                if cancellation_error is not None:
                    raise cancellation_error
                if is_retryable_model_request_error(exc):
                    formatted = OpenAIResponseLLM.format_request_error(exc)
                    if has_streamed_visible or completed_calls:
                        raise StreamInterruptedAfterOutputError(formatted) from exc
                    raise RetryableAgentRequestError(formatted) from exc
                raise AgentProtocolError(
                    f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(exc)}"
                ) from exc

            if cancel_check is not None:
                # 流迭代器正常耗尽后仍要确认取消状态：外层主动 close() 的流
                # 可能以“正常结束、无可见内容”返回，此时必须优先处理取消，
                # 不能把结果继续转换为空响应或可重试请求。
                cancel_check()

            # 截断检查必须先于空响应判定：截断（length/incomplete/...）是
            # 可恢复的限时/容量问题，应走重试；空响应则可能是模型行为。
            _raise_if_finish_truncated(
                finish_reason,
                has_visible_output=has_streamed_visible or bool(completed_calls),
            )

            if latest_usage is not None:
                on_token_usage(*latest_usage)

            content = "".join(content_parts)
            reasoning = "".join(reasoning_parts).strip()
            if not content.strip() and not completed_calls and not reasoning:
                raise EmptyAgentReply("Agent 返回内容为空，且未返回工具调用或推理。")

            message = assistant_tool_call_message(
                content,
                completed_calls,
                reasoning,
                function_name_for_tool=self.function_name_for_tool,
            )
            return AgentModelReply(
                message=message,
                content=content,
                tool_calls=completed_calls,
                reasoning=reasoning,
                content_streamed=has_streamed_visible,
            )
        finally:
            if owned_snapshot is not None:
                self.runtime_manager.release_turn(owned_snapshot)

    def _request_via_openai_client(
        self,
        messages: list[dict[str, Any]],
        on_delta: Callable[[str], None],
        on_token_usage: Callable[[int, int, int], None],
        on_protocol_wait: Callable[[], None],
        cancel_check: Callable[[], None] | None,
        on_reasoning_delta: Callable[[str], None] | None,
        reasoning_guard_config: Any = None,
    ) -> AgentModelReply:
        """旧路径：直接调用 OpenAI Chat Completions 流式接口。"""

        system_prompt = self.system_prompt_provider()
        extra_body = dict(self.extra_body_provider() or {})
        # 遗留 Chat 路径：把模型默认 max_output/temperature 提升为顶层请求参数。
        max_output = _coerce_positive_int(extra_body.pop("max_output_tokens", None))
        if max_output is None:
            max_output = _coerce_positive_int(extra_body.pop("max_tokens", None))
        temperature = _coerce_optional_float(extra_body.pop("temperature", None))
        request_kwargs: dict[str, Any] = {
            "model": self.model,
            "messages": [{"role": "system", "content": system_prompt}, *messages],
            "tools": self.tools_provider(),
            "tool_choice": "auto",
            "stream": True,
            "extra_body": extra_body,
            "timeout": self.request_timeout_seconds,
        }
        if max_output is not None:
            request_kwargs["max_tokens"] = max_output
        if temperature is not None:
            request_kwargs["temperature"] = temperature
        prompt_cache_key = build_prompt_cache_key(
            self.prompt_cache_identity_provider(),
            model=self.model,
        )
        if prompt_cache_key:
            request_kwargs["prompt_cache_key"] = prompt_cache_key

        try:
            stream = self.client.chat.completions.create(**request_kwargs)
        except Exception as exc:
            configured_code = configured_retry_code(exc, reasoning_guard_config)
            if configured_code is not None:
                raise ConfiguredAutoRetryError(
                    configured_code,
                    str(exc),
                    cause=exc,
                ) from exc
            if "prompt_cache_key" in request_kwargs and is_unsupported_prompt_cache_error(exc):
                request_kwargs.pop("prompt_cache_key", None)
                try:
                    stream = self.client.chat.completions.create(**request_kwargs)
                except Exception as retry_exc:
                    configured_code = configured_retry_code(
                        retry_exc,
                        reasoning_guard_config,
                    )
                    if configured_code is not None:
                        raise ConfiguredAutoRetryError(
                            configured_code,
                            str(retry_exc),
                            cause=retry_exc,
                        ) from retry_exc
                    raise AgentProtocolError(
                        f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(retry_exc)}"
                    ) from retry_exc
            elif is_retryable_model_request_error(exc):
                raise RetryableAgentRequestError(OpenAIResponseLLM.format_request_error(exc)) from exc
            else:
                raise AgentProtocolError(
                    f"Agent 请求失败：{OpenAIResponseLLM.format_request_error(exc)}"
                ) from exc

        content_parts: list[str] = []
        reasoning_parts: list[str] = []
        guarded_reasoning_delta = wrap_reasoning_callback(
            on_reasoning_delta,
            reasoning_guard_config,
        )
        tool_call_delta_buffers: dict[int, dict[str, Any]] = {}
        latest_usage: tuple[int, int, int] | None = None
        has_streamed_visible = False
        protocol_wait_sent = False
        cancellation_error: Exception | None = None
        finish_reason = "stop"

        try:
            for event in registered_stream_events(
                stream,
                owner=stream_owner_for(cancel_check),
            ):
                if cancel_check is not None:
                    try:
                        cancel_check()
                    except Exception as exc:
                        cancellation_error = exc
                        raise
                usage = OpenAIResponseLLM.extract_token_usage(event)
                if usage is not None:
                    latest_usage = usage
                # 与 Runtime 路径一致：记录 Provider 的 finish_reason，循环后
                # 检查截断，避免半截回复被当作正常完成静默结束回合。
                finish = _read_finish_reason(event)
                if finish:
                    finish_reason = finish

                delta = extract_stream_delta(event)
                if delta is None:
                    continue

                delta_content = read_attr_or_key(delta, "content")
                if isinstance(delta_content, str) and delta_content:
                    content_parts.append(delta_content)
                    on_delta(delta_content)
                    has_streamed_visible = True

                delta_reasoning = read_attr_or_key(delta, "reasoning_content")
                if isinstance(delta_reasoning, str):
                    if delta_reasoning:
                        reasoning_parts.append(delta_reasoning)
                    if guarded_reasoning_delta is not None:
                        guarded_reasoning_delta(delta_reasoning)

                tc_deltas = read_attr_or_key(delta, "tool_calls")
                if isinstance(tc_deltas, list) and tc_deltas:
                    if has_streamed_visible and not protocol_wait_sent:
                        on_protocol_wait()
                        protocol_wait_sent = True
                    accumulate_tool_call_deltas(tc_deltas, tool_call_delta_buffers)
        except ReasoningGuardTriggered:
            raise
        except Exception as exc:
            if cancellation_error is not None:
                raise cancellation_error
            configured_code = configured_retry_code(exc, reasoning_guard_config)
            if configured_code is not None:
                raise ConfiguredAutoRetryError(
                    configured_code,
                    str(exc),
                    cause=exc,
                ) from exc
            formatted = OpenAIResponseLLM.format_request_error(exc)
            if has_streamed_visible or tool_call_delta_buffers:
                raise StreamInterruptedAfterOutputError(formatted) from exc
            raise RetryableAgentRequestError(formatted) from exc

        if cancel_check is not None:
            # 与 Runtime 路径一致：流迭代器正常耗尽后必须确认取消状态，
            # 防止外层 close() 的流以空响应形式继续进入重试循环。
            cancel_check()

        # 截断检查必须先于空响应判定，与 Runtime 路径保持一致。
        _raise_if_finish_truncated(
            finish_reason,
            has_visible_output=has_streamed_visible or bool(tool_call_delta_buffers),
        )

        # 收到过 tool_calls delta 但函数名从未完整到达：与 Runtime 路径一致，
        # 这也是网关以“正常结束”形态包装断流的截断信号，绝不能静默丢弃
        # （否则半截回复直接结束回合且不提示用户），必须进入回滚/重试路径。
        if any(
            not str(buf.get("function", {}).get("name") or "").strip()
            for buf in tool_call_delta_buffers.values()
        ):
            message = "Agent 流在工具调用名称完整到达前结束，疑似连接被网关截断。"
            if has_streamed_visible or tool_call_delta_buffers:
                raise StreamInterruptedAfterOutputError(message)
            raise RetryableAgentRequestError(message)

        if latest_usage is not None:
            on_token_usage(*latest_usage)

        content = "".join(content_parts)
        reasoning = "".join(reasoning_parts).strip()
        tool_calls = build_tool_calls_from_deltas(
            tool_call_delta_buffers,
            tool_name_from_function_name=self.tool_name_from_function_name,
        )

        if not content.strip() and not tool_calls and not reasoning:
            raise EmptyAgentReply("Agent 返回内容为空，且未返回工具调用或推理。")

        message = assistant_tool_call_message(
            content,
            tool_calls,
            reasoning,
            function_name_for_tool=self.function_name_for_tool,
        )
        return AgentModelReply(
            message=message,
            content=content,
            tool_calls=tool_calls,
            reasoning=reasoning,
            content_streamed=has_streamed_visible,
        )


def is_openai_gpt_model(model: str) -> bool:
    """只为 OpenAI GPT 系列模型启用官方 prompt_cache_key 参数。"""

    return model.startswith("gpt-") or model.startswith("chatgpt-") or bool(re.match(r"^o\d", model))


def is_unsupported_prompt_cache_error(exc: Exception) -> bool:
    """兼容网关不认识 prompt_cache_key 时，自动移除该参数重试一次。"""

    message = str(exc).lower()
    return (
        "prompt_cache_key" in message
        and any(
            marker in message
            for marker in (
                "unknown",
                "unsupported",
                "unexpected",
                "unrecognized",
                "extra",
                "invalid",
                "not permitted",
            )
        )
    )


# 与 errors.map_openai_exception 的消息正则保持同一提取口径：
# SDK/网关异常可能把状态码放在 exc.status_code、exc.response.status_code
# 或仅出现在消息文本中（如 "Error code: 400 - Bad Request"）。
_HTTP_STATUS_CODE_RE = re.compile(r"\b([45]\d{2})\b")


def _http_status_code_of(exc: Exception) -> int | None:
    """从 SDK/网关异常中提取 HTTP 状态码（与 errors 模块口径一致）。"""

    for value in (
        getattr(exc, "status_code", None),
        getattr(getattr(exc, "response", None), "status_code", None),
    ):
        if isinstance(value, int) and 400 <= value <= 599:
            return value
    match = _HTTP_STATUS_CODE_RE.search(str(exc))
    if match is None:
        return None
    return int(match.group(1))


def is_retryable_model_request_error(exc: Exception) -> bool:
    """识别请求建立阶段可直接重试的模型服务错误。

    400 不再视为通用可重试错误：它通常表示确定性的参数/协议问题，
    重试同一个请求只会放大问题；prompt_cache_key 的兼容重试由调用方单独处理。
    """

    status_code = _http_status_code_of(exc)
    if status_code is not None and status_code in {408, 409, 429, 500, 502, 503, 504}:
        return True

    message = str(exc).lower()
    return any(
        marker in message
        for marker in (
            "peer closed connection",
            "incomplete chunked read",
            "remote protocol error",
            "server disconnected",
            "connection reset",
            "connection aborted",
            "broken pipe",
            "timeout",
            "timed out",
            "readtimeout",
            "connecttimeout",
        )
    )


def build_prompt_cache_key(identity: dict[str, str], *, model: str) -> str:
    """为 GPT/OpenAI 请求提供稳定缓存路由 key。

    key 只来自稳定上下文身份：prompt 版本、模型、工作区、项目规范 hash、
    Skill 索引/手动 Skill hash 和工具 schema hash。它不读取当前 user、历史
    消息或工具结果，避免请求态内容打散缓存路由。
    """

    normalized_model = model.strip().lower()
    if not is_openai_gpt_model(normalized_model):
        return ""

    stable_identity = dict(identity)
    stable_identity["model"] = model.strip()
    digest = hashlib.sha256(
        json.dumps(
            stable_identity,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    ).hexdigest()[:32]
    return f"local-agent-{digest}"


def build_extra_body(llm_config: Any) -> dict[str, Any]:
    """构造网关扩展参数；根据 reasoning_effort 决定是否启用思考模式。"""

    thinking_type = "enabled" if llm_config.thinking_enabled else "disabled"
    body: dict[str, Any] = {"thinking": {"type": thinking_type}}
    if llm_config.thinking_enabled and llm_config.reasoning_effort:
        if llm_config.reasoning_effort in VALID_REASONING_EFFORTS:
            body["reasoning_effort"] = llm_config.reasoning_effort
    # 模型级默认生成参数也进入 extra_body，供遗留 Chat 路径与 Runtime 合并逻辑使用。
    max_output = int(getattr(llm_config, "max_output_tokens", 0) or 0)
    if max_output > 0:
        body["max_output_tokens"] = max_output
    temperature = getattr(llm_config, "temperature", None)
    if temperature is not None:
        body["temperature"] = temperature
    provider_options = getattr(llm_config, "provider_options", None)
    if isinstance(provider_options, dict):
        for key, value in provider_options.items():
            # thinking / reasoning_effort 仍由上面的 Host 逻辑控制。
            if key in {"thinking", "reasoning_effort", "max_output_tokens", "temperature", "max_tokens"}:
                continue
            body.setdefault(key, value)
    return body


def _build_generation_options(
    *,
    descriptor: Any,
    extra_body: dict[str, Any],
    reasoning_effort: str,
    request_timeout_seconds: float,
    request_retry_count: int,
):
    """合并模型默认生成参数到 GenerationOptions。

    优先级（高 → 低）：
    1. extra_body 中的显式 max_output_tokens / temperature
    2. ModelDescriptor 顶层字段
    3. descriptor.provider_options
    4. 厂商默认（None / 不传）
    """

    from ...llm.protocol import GenerationOptions

    provider_options = dict(getattr(descriptor, "provider_options", {}) or {})
    merged_options = dict(provider_options)
    # extra_body 覆盖同名字段，但 max/temperature 会提升为 GenerationOptions 顶级字段。
    merged_options.update(extra_body)

    max_output = _coerce_positive_int(
        merged_options.pop("max_output_tokens", None)
        if "max_output_tokens" in merged_options
        else None
    )
    if max_output is None:
        max_output = _coerce_positive_int(merged_options.pop("max_tokens", None))
    if max_output is None:
        max_output = _coerce_positive_int(getattr(descriptor, "max_output_tokens", 0))
    if max_output is None:
        capabilities = getattr(descriptor, "capabilities", None)
        max_output = _coerce_positive_int(getattr(capabilities, "max_output_tokens", 0))

    temperature = _coerce_optional_float(
        merged_options.pop("temperature", None)
        if "temperature" in merged_options
        else None
    )
    if temperature is None:
        temperature = _coerce_optional_float(getattr(descriptor, "temperature", None))

    # Host 权威字段不进入 provider_options。
    for host_key in (
        "model",
        "messages",
        "input",
        "tools",
        "tool_choice",
        "stream",
        "timeout",
        "api_key",
        "base_url",
        "instructions",
        "system",
        "max_output_tokens",
        "max_tokens",
        "temperature",
    ):
        merged_options.pop(host_key, None)

    return GenerationOptions(
        max_output_tokens=max_output,
        temperature=temperature,
        reasoning_effort=reasoning_effort,
        request_timeout_seconds=request_timeout_seconds,
        request_retry_count=request_retry_count,
        provider_options=merged_options,
    )


def _coerce_positive_int(value: Any) -> int | None:
    if value in (None, ""):
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return None
    return value


def _coerce_optional_float(value: Any) -> float | None:
    if value in (None, ""):
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def parse_tool_arguments(raw_arguments: Any) -> dict[str, Any]:
    if isinstance(raw_arguments, dict):
        return raw_arguments
    if isinstance(raw_arguments, str) and raw_arguments.strip():
        try:
            parsed = json.loads(raw_arguments)
        except json.JSONDecodeError:
            return {}
        return parsed if isinstance(parsed, dict) else {}
    return {}


def read_attr_or_key(value: Any, key: str) -> Any:
    if value is None:
        return None
    attr = getattr(value, key, None)
    if attr is not None:
        return attr
    if isinstance(value, dict):
        return value.get(key)
    if hasattr(value, "model_dump"):
        data = value.model_dump()
        return data.get(key) if isinstance(data, dict) else None
    return None


def _read_finish_reason(event: Any) -> str:
    """从 OpenAI 流式事件提取 choices[0].finish_reason，兼容 SDK 模型与字典。"""

    choices = getattr(event, "choices", None)
    if isinstance(choices, list) and choices:
        finish = read_attr_or_key(choices[0], "finish_reason")
        if isinstance(finish, str) and finish:
            return finish
    elif isinstance(event, dict):
        choices_data = event.get("choices")
        if isinstance(choices_data, list) and choices_data:
            finish = read_attr_or_key(choices_data[0], "finish_reason")
            if isinstance(finish, str) and finish:
                return finish
    return ""


def extract_stream_delta(event: Any) -> Any | None:
    """从流式事件中提取 choices[0].delta，兼容 SDK 模型与字典。"""

    choices = getattr(event, "choices", None)
    if isinstance(choices, list) and choices:
        delta = getattr(choices[0], "delta", None)
        if delta is not None:
            return delta
        first = choices[0]
        if isinstance(first, dict):
            return first.get("delta")
    elif isinstance(event, dict):
        choices_data = event.get("choices")
        if isinstance(choices_data, list) and choices_data:
            first = choices_data[0]
            if isinstance(first, dict):
                return first.get("delta")
    return None


def accumulate_tool_call_deltas(
    tc_deltas: list[Any],
    buffers: dict[int, dict[str, Any]],
) -> None:
    """把流式 tool_calls 增量块按 index 累积到缓冲区。"""

    for tc in tc_deltas:
        idx = read_attr_or_key(tc, "index")
        if not isinstance(idx, int):
            idx = 0
        if idx not in buffers:
            buffers[idx] = {
                "id": "",
                "function": {"name": "", "arguments": ""},
            }
        buf = buffers[idx]
        tc_id = read_attr_or_key(tc, "id")
        if tc_id:
            buf["id"] = str(tc_id)
        func = read_attr_or_key(tc, "function")
        if isinstance(func, dict):
            fn_name = func.get("name")
            if fn_name:
                buf["function"]["name"] += str(fn_name)
            fn_args = func.get("arguments")
            if fn_args:
                buf["function"]["arguments"] += str(fn_args)
        elif func is not None:
            fn_name = getattr(func, "name", None)
            if fn_name:
                buf["function"]["name"] += str(fn_name)
            fn_args = getattr(func, "arguments", None)
            if fn_args:
                buf["function"]["arguments"] += str(fn_args)


def build_tool_calls_from_deltas(
    buffers: dict[int, dict[str, Any]],
    *,
    tool_name_from_function_name: Callable[[str], str],
) -> list[ToolCall]:
    """把累积的流式 tool_call 增量块解析为结构化 ToolCall 列表。"""

    calls: list[ToolCall] = []
    for idx in sorted(buffers.keys()):
        buf = buffers[idx]
        fn_name = buf["function"]["name"].strip()
        if not fn_name:
            continue
        calls.append(
            ToolCall(
                name=tool_name_from_function_name(fn_name),
                arguments=parse_tool_arguments(buf["function"]["arguments"]),
                id=buf["id"],
                function_name=fn_name,
            )
        )
    return calls


def assistant_tool_call_message(
    content: str,
    tool_calls: list[ToolCall],
    reasoning: str,
    *,
    function_name_for_tool: Callable[[str], str],
) -> dict[str, Any]:
    message: dict[str, Any] = {"role": "assistant", "content": content or None}
    if reasoning:
        message["reasoning_content"] = reasoning
    if tool_calls:
        message["tool_calls"] = [
            {
                "id": tool_call.id,
                "type": "function",
                "function": {
                    "name": _assistant_tool_call_name(
                        tool_call,
                        function_name_for_tool=function_name_for_tool,
                    ),
                    "arguments": json.dumps(tool_call.arguments, ensure_ascii=False),
                },
            }
            for tool_call in tool_calls
        ]
    return message


def _assistant_tool_call_name(
    tool_call: ToolCall,
    *,
    function_name_for_tool: Callable[[str], str],
) -> str:
    """生成历史消息中的函数名：与模型当前看到的 tools 声明保持一致。

    默认保留模型原始回显名（动态加载工具按真实工具名注册，例如 MCP 的
    ``server.tool``，不能哈希化）。仅当回显名是旧式哈希函数名的变体时
    （如 ``tool_search_e960b0242f`` 而非 ``search_tools``），才按 digest
    反查并把历史消息规范化为当前注册名，避免截断哈希名污染会话历史。
    """

    raw = tool_call.function_name or ""
    if raw and resolve_tool_name_from_hashed_function_name(raw, [tool_call.name]):
        return function_name_for_tool(tool_call.name) or raw
    return raw or function_name_for_tool(tool_call.name)


def compact_tool_description(value: str) -> str:
    """折叠工具说明中的空白（多行拼接的换行与缩进归并为单空格）。

    顶层 ``tools`` 注册所有工具后，每次请求都会携带全部声明；空白折叠
    保证多行拼接的描述在 JSON 载荷中紧凑呈现，但不再限制长度，完整
    描述原样发送给 Provider。
    """

    return " ".join(str(value or "").split())


def compact_tool_schema(tool: ToolDefinition) -> dict[str, Any]:
    """删除长描述和默认值，只保留模型填写参数所需的 Schema 信息。

    顶层注册场景下 Schema 随每次请求全量发送，默认值/示例描述会被截断
    浪费；压缩后只保留类型、必填项、枚举与边界约束。Host 分发前仍用
    工具目录完整 Schema 二次校验，压缩声明不是安全边界。
    """

    try:
        schema = tool_parameters_schema(tool)
    except Exception:
        schema = {"type": "object", "properties": {}}
    return _compact_schema_node(schema, depth=0)


def _compact_schema_node(value: Any, *, depth: int) -> Any:
    if depth > 6:
        return {"type": "object"}
    if isinstance(value, Mapping):
        allowed = {
            "type",
            "properties",
            "required",
            "additionalProperties",
            "items",
            "enum",
            "const",
            "oneOf",
            "anyOf",
            "allOf",
            "minLength",
            "maxLength",
            "minimum",
            "maximum",
            "minItems",
            "maxItems",
            "minProperties",
            "maxProperties",
            "pattern",
        }
        compacted: dict[str, Any] = {}
        for key, child in value.items():
            if key not in allowed:
                continue
            if key == "enum":
                # enum 卸载压缩：取值集合是语义契约，浅拷贝后逐项原样保留，
                # 不递归、不截断、不改写。历史教训：通用列表截断 [:20] 曾把
                # git action 52 个枚举砍成 20 个，导致 status/log 等合法取值
                # 被误判为 invalid_arguments。枚举本身短小，全量发送的 token
                # 增量可忽略。
                compacted[str(key)] = (
                    list(child) if isinstance(child, list) else child
                )
            elif key == "properties" and isinstance(child, Mapping):
                compacted["properties"] = {
                    str(property_name): _compact_schema_node(
                        property_schema,
                        depth=depth + 1,
                    )
                    for property_name, property_schema in child.items()
                }
            else:
                compacted[str(key)] = _compact_schema_node(child, depth=depth + 1)
        return compacted
    if isinstance(value, list):
        # 不再对列表做任何长度截断：schema 中的列表字段（enum/required/
        # items/oneOf/anyOf/allOf）要么是语义契约要么是单元素描述，截断
        # 任何一项都可能改变契约语义（如 enum 被砍掉合法取值）。
        return [_compact_schema_node(item, depth=depth + 1) for item in value]
    return value


def chat_completion_tools(
    tools: Iterable[ToolDefinition],
    *,
    function_name_for_tool: Callable[[str], str],
) -> list[dict[str, Any]]:
    """把 Host 工具声明转成 Chat Completions ``tools`` 数组。

    顶层注册所有工具：每个工具都带完整字段（name、description、parameters），
    但 description 折叠空白后原样发送、parameters 使用压缩 Schema，避免
    长描述和默认值/示例在每次请求中重复发送。
    """

    return [
        {
            "type": "function",
            "function": {
                "name": function_name_for_tool(tool.name),
                "description": compact_tool_description(tool.description),
                "parameters": compact_tool_schema(tool),
            },
        }
        for tool in tools
    ]


def function_name_for_tool(tool_name: str) -> str:
    """把 Host 工具名映射为注册给 Provider 的函数名。

    方案 A（去哈希化）：对本身符合函数名规范的名称（字母/数字/下划线/连字符，
    长度 <=64）直接返回原名，让模型看到的函数名就是真实工具名（search_tools
    就是 search_tools），避免长哈希名被模型回显时截断/改写导致反查失败。
    仅对含非法字符的名称（如 MCP 的 "server.tool"）做归一化，并追加短哈希
    兜底保证网关侧合法且不与其他工具名冲突。
    """

    if re.fullmatch(r"[A-Za-z0-9_-]{1,64}", tool_name):
        return tool_name
    readable = re.sub(r"[^A-Za-z0-9_]+", "_", tool_name).strip("_").lower()
    readable = readable or "tool"
    digest = hashlib.sha1(tool_name.encode("utf-8")).hexdigest()[:10]
    return f"tool_{readable[:40]}_{digest}"


# 哈希函数名形态：tool_<可读段>_<sha1 前 10 位>。可读段最长 40 字符，
# digest 恒为 10 位十六进制。用于从模型回传的（可能被截断/改写的）哈希
# 函数名中按 digest 反查真实工具名。
_HASHED_FUNCTION_NAME_RE = re.compile(r"^tool_[a-z0-9_]{1,40}_([0-9a-f]{10})$")


def resolve_tool_name_from_hashed_function_name(
    function_name: str,
    tool_names: Iterable[str],
) -> str | None:
    """从形如 ``tool_<可读段>_<digest>`` 的函数名中按 digest 反查真实工具名。

    模型回显哈希函数名时可能截断可读段（例如把 ``tool_search_tools_e960b0242f``
    回显成 ``tool_search_e960b0242f``），但 digest 段（SHA-1 前 10 位）通常保留，
    据此反查可容忍这类改写。返回 None 表示不是哈希函数名或未命中任何工具。
    """

    match = _HASHED_FUNCTION_NAME_RE.fullmatch(function_name)
    if not match:
        return None
    digest = match.group(1)
    for tool_name in tool_names:
        if hashlib.sha1(tool_name.encode("utf-8")).hexdigest()[:10] == digest:
            return tool_name
    return None


def tool_name_from_function_name(
    function_name: str,
    tool_names: Iterable[str],
    *,
    function_name_for_tool_callback: Callable[[str], str] = function_name_for_tool,
) -> str:
    for tool_name in tool_names:
        if function_name_for_tool_callback(tool_name) == function_name:
            return tool_name
    # 方案 D 兜底：精确匹配失败后，尝试按哈希函数名的 digest 段宽容反查，
    # 容忍模型回显被截断/改写的旧式哈希函数名（如 tool_search_e960b0242f）。
    resolved = resolve_tool_name_from_hashed_function_name(function_name, tool_names)
    if resolved is not None:
        return resolved
    return function_name


def tool_parameters_schema(tool: ToolDefinition) -> dict[str, Any]:
    try:
        raw_schema = json.loads(tool.argument_schema)
    except json.JSONDecodeError:
        raw_schema = {}
    if not isinstance(raw_schema, dict):
        raw_schema = {}
    if raw_schema.get("type") == "object" and isinstance(raw_schema.get("properties"), dict):
        schema = dict(raw_schema)
    else:
        properties = {
            key: infer_tool_property_schema(value)
            for key, value in raw_schema.items()
            if isinstance(key, str)
        }
        schema = {
            "type": "object",
            "properties": properties,
        }
        # 示例值风格没有显式 required 声明，用 minProperties 兜底：
        # 只要工具声明了参数，就禁止空对象调用，避免模型发送空 arguments。
        if properties:
            schema["minProperties"] = 1
    schema.setdefault("type", "object")
    schema.setdefault("properties", {})
    return schema


def infer_tool_property_schema(example: Any) -> dict[str, Any]:
    if isinstance(example, bool):
        return {"type": "boolean"}
    if isinstance(example, int) and not isinstance(example, bool):
        return {"type": "integer"}
    if isinstance(example, (float, int)) and not isinstance(example, bool):
        return {"type": "number"}
    if isinstance(example, list):
        return {"type": "array", "items": {"type": "string"}}
    if isinstance(example, dict):
        return {"type": "object"}
    return {"type": "string"}


def _tool_specs_from_openai_tools(tools_payload: list[dict[str, Any]]) -> list[Any]:
    """把 Chat Completions tools 定义转成统一 ToolSpec。"""

    from ...llm.protocol import ToolSpec

    specs: list[ToolSpec] = []
    for item in tools_payload:
        if not isinstance(item, dict):
            continue
        function = item.get("function") if isinstance(item.get("function"), dict) else item
        if not isinstance(function, dict):
            continue
        name = str(function.get("name") or "").strip()
        if not name:
            continue
        description = str(function.get("description") or "")
        parameters = function.get("parameters")
        if not isinstance(parameters, dict):
            parameters = {"type": "object", "properties": {}}
        specs.append(
            ToolSpec(name=name, description=description, parameters=dict(parameters))
        )
    return specs
