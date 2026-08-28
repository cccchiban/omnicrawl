"""OpenAI Responses API Adapter（原生 SDK）。"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from typing import Any, Callable, Iterator

from ..capabilities import (
    ModelCapabilities,
    conservative_openai_responses_capabilities,
    merge_capabilities,
)
from ..errors import ModelError, ModelErrorCode
from ..protocol import (
    PROTOCOL_OPENAI_RESPONSES,
    ConversationMessage,
    ImageBlock,
    ModelIdentity,
    ModelStreamEvent,
    ModelTurnRequest,
    ProviderWarning,
    ReasoningDelta,
    ResponseCompleted,
    TextBlock,
    TextDelta,
    ToolCallBlock,
    ToolCallCompleted,
    ToolCallStarted,
    ToolResultBlock,
    UsageUpdated,
    tools_from_conversation_messages,
)
from ..registry import DiscoveryModel, DiscoveryResult, ModelDescriptor, ProviderProfile
from ..stream_registry import registered_stream_events, stream_owner_for
from ..usage import usage_from_openai_payload
from .openai_common import (
    build_prompt_cache_key,
    create_openai_client,
    format_openai_error,
    is_retryable_model_request_error,
    is_unsupported_prompt_cache_error,
    parse_tool_arguments,
    resolve_api_key,
    sanitize_provider_options,
)


@dataclass
class OpenAIResponsesRuntime:
    identity: ModelIdentity
    capabilities: ModelCapabilities
    client: Any
    profile: ProviderProfile
    descriptor: ModelDescriptor
    _owns_client: bool = True
    _closed: bool = False
    # 已确认当前网关/模型组合不支持工具调用历史 item（HTTP 400）。置位后
    # 后续请求直接展平工具历史为纯文本，避免每轮先发一次必然 400 的请求。
    _tool_history_unsupported: bool = False

    def stream_turn(
        self,
        request: ModelTurnRequest,
        *,
        cancel_check: Callable[[], None] | None = None,
    ) -> Iterator[ModelStreamEvent]:
        if self._closed:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message="Responses Runtime 已关闭。",
            )
        options = request.generation_options
        extra_body = dict(sanitize_provider_options(options.provider_options))
        if options.reasoning_effort and options.reasoning_effort not in {"none", "disabled", ""}:
            effort = options.reasoning_effort
        else:
            effort = "none"
        # 统一走标准 Responses 思考参数 reasoning.effort（各档位含 none 实测
        # 均被网关接受且 none 能真正关闭思考）。旧 chat 风格扩展字段
        # thinking / reasoning_effort 在 Responses 网关不被识别，移除避免冗余。
        extra_body.pop("thinking", None)
        extra_body.pop("reasoning_effort", None)
        extra_body.setdefault("reasoning", {"effort": effort})

        tools = _tools_for_responses(request)
        input_items = messages_to_responses_input(request.messages)
        # Responses API 要求工具观察后继续生成时，输入末项保持为 user 角色；
        # 工具历史转换后追加明确的收尾指令，避免兼容网关把空的下一轮误判为新对话。
        if input_items and input_items[-1].get("type") == "function_call_output":
            input_items.append(
                _text_message(
                    "user",
                    "请基于以上工具结果给出最终总结，不要向用户发起新的开场问候。",
                    "input_text",
                )
            )
        # 已确认当前网关/模型不支持工具调用历史 item：直接展平，避免每轮先发
        # 一次必然 400 的请求（见下方策略 3 说明）。
        if self._tool_history_unsupported and _has_tool_history_items(input_items):
            input_items = _flatten_tool_history_to_text(input_items)
        kwargs: dict[str, Any] = {
            "model": self.identity.model_id,
            "instructions": request.system_prompt,
            "input": input_items,
            "stream": True,
            "extra_body": extra_body,
            "timeout": options.request_timeout_seconds or self.profile.request_timeout_seconds,
        }
        if tools:
            kwargs["tools"] = tools
        if options.max_output_tokens:
            kwargs["max_output_tokens"] = options.max_output_tokens
        if options.temperature is not None:
            kwargs["temperature"] = options.temperature

        prompt_cache_key = build_prompt_cache_key(
            request.prompt_cache_identity,
            model=self.identity.model_id,
        )
        if prompt_cache_key:
            kwargs["prompt_cache_key"] = prompt_cache_key
        prompt_cache_warning = False
        tool_history_warning = False

        try:
            stream = self.client.responses.create(**kwargs)
        except Exception as exc:
            # 依次尝试可自动修复的请求问题；全部失败才抛出最终错误。
            last_error: Exception | None = exc
            # 策略 1：网关不认 prompt_cache_key → 移除该参数后重试一次。
            if "prompt_cache_key" in kwargs and is_unsupported_prompt_cache_error(last_error):
                kwargs.pop("prompt_cache_key", None)
                try:
                    stream = self.client.responses.create(**kwargs)
                    prompt_cache_warning = True
                    last_error = None
                except Exception as retry_exc:
                    last_error = retry_exc
            # 策略 3：网关不支持工具调用历史 item。部分兼容网关对个别模型在
            # Responses 协议下不接受 function_call / function_call_output 输入
            # item（即使已追加 user 结尾仍返回 HTTP 400，错误体回显
            # {"model": ...} 极具误导性）。此时把工具历史展平为纯文本后重试
            # 一次，并记忆该组合以便后续直接适配。
            if last_error is not None and _is_tool_history_rejection(last_error, input_items):
                kwargs["input"] = _flatten_tool_history_to_text(input_items)
                try:
                    stream = self.client.responses.create(**kwargs)
                    self._tool_history_unsupported = True
                    tool_history_warning = True
                    last_error = None
                except Exception as retry_exc:
                    last_error = retry_exc
            if last_error is not None:
                retryable = is_retryable_model_request_error(last_error)
                raise ModelError(
                    code=(
                        ModelErrorCode.CONNECTION_FAILED
                        if retryable
                        else ModelErrorCode.INVALID_REQUEST
                    ),
                    message=f"Responses 请求失败：{format_openai_error(last_error)}",
                    retryable=retryable,
                ) from last_error

        # 累积 function_call 参数分片，并记录已完成 call_id，避免 SDK 在
        # output_item.done 与 response.completed.output 重复报告同一调用。
        call_buffers: dict[str, dict[str, str]] = {}
        emitted_call_ids: set[str] = set()
        started_call_ids: set[str] = set()
        # 兼容网关把 output item 的 id（item_id）与真正回传工具结果所需的
        # call_id 分开传递：参数 delta 可能同时携带二者，后续 done 事件却只
        # 携带 item_id。始终以 call_id 作为上层工具循环的稳定标识。
        call_id_aliases: dict[str, str] = {}
        finish_reason = "stop"
        # 是否已收到 response.completed 完成事件：网关可能以“优雅关闭连接
        # （EOF）”包装断流，SDK 层迭代看似“正常耗尽”却从未到达完成事件，
        # 仅靠 finish_reason 无法区分正常完成与截断，需要显式跟踪该信号。
        stream_completed_seen = False
        # 兼容部分中转站：普通文本已经产生增量后，可能直接以 EOF 结束，
        # 丢弃 response.completed（甚至连 output_text.done 也一起丢弃）。
        # 只要没有未完成的工具调用，可将该文本流视为完整回复；工具流仍
        # 必须等待结构化收尾，避免半截参数被误执行。
        output_text_delta_seen = False
        try:
            for event in registered_stream_events(
                stream,
                owner=stream_owner_for(cancel_check),
            ):
                if cancel_check is not None:
                    cancel_check()
                usage = usage_from_openai_payload(event)
                if usage is not None:
                    yield UsageUpdated(
                        input_tokens=usage.input_tokens,
                        output_tokens=usage.output_tokens,
                        cached_input_tokens=usage.cached_input_tokens,
                        reasoning_tokens=usage.reasoning_tokens,
                    )

                event_type = getattr(event, "type", None)
                if event_type is None and isinstance(event, dict):
                    event_type = event.get("type")
                delta = getattr(event, "delta", None)
                if delta is None and isinstance(event, dict):
                    delta = event.get("delta")

                if event_type == "response.output_text.delta" and isinstance(delta, str):
                    output_text_delta_seen = True
                    yield TextDelta(text=delta)
                elif event_type in {
                    "response.reasoning_text.delta",
                    "response.reasoning_summary_text.delta",
                } and isinstance(delta, str):
                    yield ReasoningDelta(text=delta)
                elif event_type == "response.output_item.added":
                    # 部分兼容网关只在 output_item.added 事件里携带函数名，
                    # 之后直接给参数 delta 并在 response.completed 前结束流；
                    # 若忽略该事件，完整工具调用会被误判为“名称截断”。
                    item = getattr(event, "item", None)
                    if item is None and isinstance(event, dict):
                        item = event.get("item")
                    yield from _capture_function_call_added(
                        item,
                        call_buffers,
                        started_call_ids,
                        call_id_aliases,
                    )
                elif event_type == "response.function_call_arguments.delta":
                    item_id = str(
                        getattr(event, "item_id", None)
                        or (event.get("item_id") if isinstance(event, dict) else "")
                        or ""
                    )
                    provider_call_id = str(
                        getattr(event, "call_id", None)
                        or (event.get("call_id") if isinstance(event, dict) else "")
                        or ""
                    )
                    call_id = _canonical_call_id(
                        item_id,
                        provider_call_id,
                        call_buffers,
                        call_id_aliases,
                        started_call_ids,
                    )
                    if call_id and isinstance(delta, str):
                        buf = call_buffers.setdefault(call_id, {"name": "", "arguments": ""})
                        if not buf["name"]:
                            name = getattr(event, "name", None) or (
                                event.get("name") if isinstance(event, dict) else ""
                            )
                            if name:
                                buf["name"] = str(name)
                        if buf["name"] and call_id not in started_call_ids:
                            started_call_ids.add(call_id)
                            yield ToolCallStarted(call_id=call_id, name=buf["name"])
                        buf["arguments"] += delta
                elif event_type == "response.function_call_arguments.done":
                    # 标准 Responses 事件在该事件顶层携带 name + 最终 arguments
                    # （SDK 类型 ResponseFunctionCallArgumentsDoneEvent），并且它
                    # 没有 item 字段；旧实现只看 event.item 会丢掉名称，导致兼容
                    # 网关把完整工具调用误判为“名称截断”。
                    item = getattr(event, "item", None)
                    if item is None and isinstance(event, dict):
                        item = event.get("item")
                    if item is not None:
                        yield from _emit_function_call_item(
                            item,
                            call_buffers,
                            emitted_call_ids,
                            started_call_ids,
                            call_id_aliases,
                        )
                    else:
                        done_item_id = str(
                            getattr(event, "item_id", None)
                            or (event.get("item_id") if isinstance(event, dict) else "")
                            or ""
                        )
                        done_provider_call_id = str(
                            getattr(event, "call_id", None)
                            or (event.get("call_id") if isinstance(event, dict) else "")
                            or ""
                        )
                        done_id = _canonical_call_id(
                            done_item_id,
                            done_provider_call_id,
                            call_buffers,
                            call_id_aliases,
                            started_call_ids,
                        )
                        done_name = str(
                            getattr(event, "name", None)
                            or (event.get("name") if isinstance(event, dict) else "")
                            or ""
                        ).strip()
                        done_args = str(
                            getattr(event, "arguments", None)
                            or (event.get("arguments") if isinstance(event, dict) else "")
                            or ""
                        )
                        if done_id:
                            buf = call_buffers.setdefault(done_id, {"name": "", "arguments": ""})
                            if done_name:
                                if not buf["name"] and done_id not in started_call_ids:
                                    started_call_ids.add(done_id)
                                    yield ToolCallStarted(call_id=done_id, name=done_name)
                                buf["name"] = done_name
                            if done_args:
                                buf["arguments"] = done_args
                            final_name = done_name or buf.get("name", "")
                            if final_name and done_id not in emitted_call_ids:
                                final_args = done_args or buf.get("arguments", "")
                                if _arguments_json_complete(final_args):
                                    yield ToolCallCompleted(
                                        call_id=done_id,
                                        name=final_name,
                                        arguments=parse_tool_arguments(final_args),
                                    )
                                    emitted_call_ids.add(done_id)
                                    # arguments.done 已明确宣告该调用完整；移除
                                    # 缓冲区，允许兼容网关在随后 EOF 丢失
                                    # response.completed 时仍把调用交给上层执行。
                                    call_buffers.pop(done_id, None)
                elif event_type == "response.output_item.done":
                    item = getattr(event, "item", None)
                    if item is None and isinstance(event, dict):
                        item = event.get("item")
                    yield from _emit_function_call_item(
                        item,
                        call_buffers,
                        emitted_call_ids,
                        started_call_ids,
                        call_id_aliases,
                    )
                elif event_type == "response.completed":
                    stream_completed_seen = True
                    response = getattr(event, "response", None)
                    if response is None and isinstance(event, dict):
                        response = event.get("response")
                    status = getattr(response, "status", None) if response is not None else None
                    if isinstance(status, str) and status:
                        finish_reason = status
                    # 扫描最终 output 中的 function_call
                    output = getattr(response, "output", None) if response is not None else None
                    if output is None and isinstance(response, dict):
                        output = response.get("output")
                    if isinstance(output, list):
                        for item in output:
                            yield from _emit_function_call_item(
                                item,
                                call_buffers,
                                emitted_call_ids,
                                started_call_ids,
                                call_id_aliases,
                            )
        except Exception as exc:
            if cancel_check is not None:
                try:
                    cancel_check()
                except Exception:
                    raise
            raise ModelError(
                code=ModelErrorCode.STREAM_INTERRUPTED,
                message=f"Responses 流式回复中断：{format_openai_error(exc)}",
                retryable=is_retryable_model_request_error(exc),
            ) from exc

        # 流迭代器正常耗尽后仍要确认取消状态：外层主动 close() 的流可能
        # 以“正常结束、无可见内容”返回，此时必须优先处理取消，不能把
        # 结果继续转换为空响应或可重试请求。
        if cancel_check is not None:
            cancel_check()

        # 兼容部分 Responses 中转站：普通文本或完整工具调用流已经产生
        # 可验证的内容后，网关会直接以 EOF 结束并丢弃 response.completed。
        # 没有结构化工具调用时，已收到的文本就是该类网关唯一可用的完成
        # 信号；工具调用则要求至少有一个已完整解析的调用，且不能残留未
        # 完成的缓冲区，防止半截参数被当成真实调用。
        complete_buffered_calls = bool(call_buffers) and all(
            bool(buf.get("name")) and _arguments_json_complete(buf.get("arguments", ""))
            for buf in call_buffers.values()
        )
        eof_has_complete_output = (
            (not call_buffers and (output_text_delta_seen or bool(emitted_call_ids)))
            or complete_buffered_calls
        )
        if not stream_completed_seen and not eof_has_complete_output:
            raise ModelError(
                code=ModelErrorCode.STREAM_INTERRUPTED,
                message="Responses 流在收到 response.completed 前提前耗尽，疑似连接被网关截断。",
                retryable=True,
            )

        # Responses 的降级提示放在正常流事件之后，避免在模型首个文本增量前
        # 插入非内容事件，保持 UI 首屏输出和旧 Provider 的事件顺序稳定。
        if prompt_cache_warning:
            yield ProviderWarning(
                code="prompt_cache_unsupported",
                message="当前网关不支持 prompt_cache_key，已自动移除后重试。",
            )
        if tool_history_warning:
            yield ProviderWarning(
                code="tool_history_flattened",
                message="当前网关不支持工具调用历史 item（HTTP 400），已自动转为纯文本后重试；后续请求将直接使用该适配。",
            )

        # 收尾未完成的 function call。收到过 arguments delta 但 name 从未
        # 到达（或调用已 emit 过）的残留条目都是截断信号：绝不能静默跳过
        # （否则半截回复直接结束回合且不提示用户），交由上层回滚并重试。
        for call_id, buf in list(call_buffers.items()):
            name = buf.get("name") or ""
            if call_id in emitted_call_ids:
                continue
            if not name:
                raise ModelError(
                    code=ModelErrorCode.STREAM_INTERRUPTED,
                    message="Responses 流在工具调用名称完整到达前结束，疑似连接被网关截断。",
                    retryable=True,
                )
            if not _arguments_json_complete(buf.get("arguments", "")):
                raise ModelError(
                    code=ModelErrorCode.STREAM_INTERRUPTED,
                    message="Responses 流在工具调用参数完整到达前结束，疑似连接被网关截断。",
                    retryable=True,
                )
            yield ToolCallCompleted(
                call_id=call_id,
                name=name,
                arguments=parse_tool_arguments(buf.get("arguments", "")),
            )
            emitted_call_ids.add(call_id)
            call_buffers.pop(call_id, None)
        yield ResponseCompleted(finish_reason=finish_reason)

    def close(self) -> None:
        self._closed = True
        client = self.client
        self.client = None
        if not self._owns_client or client is None:
            return
        close = getattr(client, "close", None)
        if callable(close):
            try:
                close()
            except Exception:
                pass


class OpenAIResponsesAdapter:
    provider_type = PROTOCOL_OPENAI_RESPONSES

    def create_runtime(
        self,
        profile: ProviderProfile,
        model: ModelDescriptor,
    ) -> OpenAIResponsesRuntime:
        capabilities = merge_capabilities(
            conservative_openai_responses_capabilities(),
            model.capabilities,
        )
        if model.context_window_tokens > 0:
            from dataclasses import replace as _replace

            capabilities = _replace(
                capabilities,
                context_window_tokens=model.context_window_tokens,
            )
        identity = ModelIdentity(
            profile_id=profile.id,
            provider=profile.provider,
            protocol=PROTOCOL_OPENAI_RESPONSES,
            model_id=model.model_id,
            catalog_key=model.identity.catalog_key,
        )
        return OpenAIResponsesRuntime(
            identity=identity,
            capabilities=capabilities,
            client=create_openai_client(profile),
            profile=profile,
            descriptor=model,
        )

    def discover_models(
        self,
        profile: ProviderProfile,
        *,
        timeout_seconds: float,
    ) -> DiscoveryResult:
        # 与 Chat 共用 OpenAI models.list；协议标记为 Profile 默认或 responses。
        if not resolve_api_key(profile):
            return DiscoveryResult(
                profile_id=profile.id,
                status="unavailable",
                message="缺少 API Key，无法发现模型。",
            )
        client = create_openai_client(profile)
        try:
            response = client.models.list(timeout=timeout_seconds)
            items = list(getattr(response, "data", None) or response)
            models: list[DiscoveryModel] = []
            seen: set[str] = set()
            protocol = profile.resolve_protocol(PROTOCOL_OPENAI_RESPONSES)
            for item in items[:500]:
                model_id = str(
                    getattr(item, "id", None)
                    or (item.get("id") if isinstance(item, dict) else "")
                    or ""
                ).strip()
                if not model_id or model_id in seen:
                    continue
                seen.add(model_id)
                models.append(
                    DiscoveryModel(
                        profile_id=profile.id,
                        provider=profile.provider,
                        protocol=protocol,
                        model_id=model_id,
                        display_name=model_id,
                        capabilities=conservative_openai_responses_capabilities(),
                    )
                )
            return DiscoveryResult(profile_id=profile.id, models=tuple(models), status="ok")
        except Exception as exc:
            return DiscoveryResult(
                profile_id=profile.id,
                status="unavailable",
                message=f"模型列表发现失败：{format_openai_error(exc)}",
            )
        finally:
            close = getattr(client, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:
                    pass


def _tools_for_responses(request: ModelTurnRequest) -> list[dict[str, Any]]:
    # Responses 没有“消息内 tools”概念：把 system 消息携带的动态声明
    # 与顶层全局工具合并为请求级 tools，语义退化为全局可见。
    all_tools = (*request.tools, *tools_from_conversation_messages(request.messages))
    tools: list[dict[str, Any]] = []
    for tool in all_tools:
        tools.append(
            {
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters or {"type": "object", "properties": {}},
            }
        )
    return tools


def _status_code_of(exc: Exception) -> int | None:
    """从 SDK/网关异常中提取 HTTP 状态码（兼容 exc.status_code 与 exc.response.status_code）。"""

    for value in (
        getattr(exc, "status_code", None),
        getattr(getattr(exc, "response", None), "status_code", None),
    ):
        if isinstance(value, int) and 400 <= value <= 599:
            return value
    return None


def _has_tool_history_items(items: list[dict[str, Any]]) -> bool:
    """判断 Responses input items 中是否包含工具调用历史（function_call / function_call_output）。"""

    return any(
        item.get("type") in {"function_call", "function_call_output"}
        for item in items
    )


def _is_tool_history_rejection(exc: Exception, items: list[dict[str, Any]]) -> bool:
    """识别“网关不支持工具调用历史”类 400 错误。

    部分兼容网关（如 axo.chibanban.de）对 deepseek 等模型在 Responses 协议下
    不支持 function_call / function_call_output 输入 item，直接返回 HTTP 400，
    且错误体把 {"model": ...} 回显为错误消息（极具误导性，实际与模型名无关）。
    判定口径：请求确含工具历史 + 状态码 400，避免误伤参数错误等其他 400。
    """

    if not _has_tool_history_items(items):
        return False
    return _status_code_of(exc) == 400


def _text_message(role: str, text: str, block_type: str) -> dict[str, Any]:
    """构造单文本块的纯文本消息（展平工具历史时使用）。"""

    return {"role": role, "content": [{"type": block_type, "text": text}]}


def _append_message_text(message: dict[str, Any], text: str, block_type: str) -> None:
    """把文本追加到既有消息的 content（同类文本块尾部），避免产生多条连续消息。"""

    content = message.get("content")
    if (
        isinstance(content, list)
        and content
        and isinstance(content[0], dict)
        and content[0].get("type") == block_type
    ):
        content[0]["text"] = f"{content[0].get('text', '')}\n{text}"
    else:
        message["content"] = [{"type": block_type, "text": text}]


def _flatten_tool_history_to_text(items: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """把工具调用历史展平为纯文本，保持消息顺序与上下文语义。

    兼容网关不支持 function_call / function_call_output item 时降级为文本，
    避免丢失上下文（工具名、参数、结果仍可见）：
      function_call        -> 追加到前一条 assistant 文本 "[工具调用: name(args)]"
      function_call_output -> 追加到前一条 user 文本 "[工具结果: output]"
    reasoning 与普通消息原样保留。注意：本函数只改历史格式，不改请求级
    tools 声明，因此模型仍能继续调用工具。
    """

    flat: list[dict[str, Any]] = []
    for item in items:
        itype = item.get("type")
        if itype == "function_call":
            text = "[工具调用: {}({})]".format(
                item.get("name", ""),
                item.get("arguments", ""),
            )
            # 反向查找最近的 assistant 文本：该工具调用属于它的输出轮次
            # （中间可能隔着 reasoning item，不能只看 flat[-1]）。
            target: dict[str, Any] | None = None
            for msg in reversed(flat):
                if msg.get("role") == "assistant":
                    target = msg
                    break
            if target is not None:
                _append_message_text(target, text, "output_text")
            else:
                flat.append(_text_message("assistant", text, "output_text"))
        elif itype == "function_call_output":
            # 独立 user 消息，保证顺序严格正确（工具结果紧跟工具调用之后，
            # 不并入可能在前的普通 user 指令，避免破坏语义）。
            flat.append(
                _text_message(
                    "user",
                    "[工具结果: {}]".format(item.get("output", "")),
                    "input_text",
                )
            )
        else:
            flat.append(item)
    return flat


def messages_to_responses_input(
    messages: tuple[ConversationMessage, ...],
) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    for message in messages:
        if message.role == "system" and message.tools:
            # 动态工具声明已合并进请求级 tools，不再作为输入 item 下发。
            continue
        if message.role == "tool":
            for block in message.blocks:
                if isinstance(block, ToolResultBlock):
                    items.append(
                        {
                            "type": "function_call_output",
                            "call_id": block.call_id,
                            "output": block.content,
                        }
                    )
            continue
        if message.role == "assistant":
            text_parts: list[str] = []
            tool_call_items: list[dict[str, Any]] = []
            for block in message.blocks:
                if isinstance(block, TextBlock):
                    text_parts.append(block.text)
                elif isinstance(block, ToolCallBlock):
                    tool_call_items.append(
                        {
                            "type": "function_call",
                            "call_id": block.call_id or block.provider_call_id,
                            "name": block.name,
                            "arguments": json.dumps(block.arguments, ensure_ascii=False),
                        }
                    )
            text = "".join(text_parts)
            if text:
                items.append(
                    {
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }
                )
            # 思考模式 + 工具调用历史：上游（Console Go）要求回传 reasoning_content，
            # 但该字段是 chat completions 专用字段，Responses API 携带会导致网关
            # decode 失败（HTTP 400）。标准 Responses 格式是 reasoning item：
            # 实测带工具调用历史时必须携带（否则上游 400 "reasoning_content must
            # be passed back"），且无工具调用时不能携带（上游 400 invalid message）。
            # reasoning 为空时用占位文本兜底（上游只检查存在性，不校验内容）。
            if tool_call_items:
                reasoning_text = message.reasoning or "…"
                items.append(
                    {
                        "type": "reasoning",
                        "id": "rs_"
                        + hashlib.sha1(reasoning_text.encode("utf-8")).hexdigest()[:16],
                        "summary": [{"type": "summary_text", "text": reasoning_text}],
                    }
                )
            items.extend(tool_call_items)
            continue
        content: list[dict[str, Any]] = []
        for block in message.blocks:
            if isinstance(block, TextBlock) and block.text:
                content.append({"type": "input_text", "text": block.text})
            elif isinstance(block, ImageBlock):
                content.append(
                    {
                        "type": "input_image",
                        "image_url": block.data_url,
                        "detail": block.detail,
                    }
                )
        items.append(
            {
                "role": message.role if message.role in {"user", "system"} else "user",
                "content": content or [{"type": "input_text", "text": ""}],
            }
        )
    return items


def _canonical_call_id(
    item_id: str,
    provider_call_id: str,
    call_buffers: dict[str, dict[str, str]],
    call_id_aliases: dict[str, str],
    started_call_ids: set[str],
) -> str:
    """统一 Responses 流中 item_id 与 provider call_id 的别名。

    某些 OpenAI 兼容网关在 ``output_item.added`` 使用输出项 id，随后在
    参数 delta 同时给出 ``item_id`` 和真正用于 ``function_call_output`` 的
    ``call_id``。工具循环必须保留后者，但已收集的名称/参数缓冲又位于前者。
    """

    item_id = str(item_id or "")
    provider_call_id = str(provider_call_id or "")
    if item_id and provider_call_id and item_id != provider_call_id:
        call_id_aliases[item_id] = provider_call_id
        existing = call_buffers.pop(item_id, None)
        if existing is not None:
            current = call_buffers.setdefault(
                provider_call_id, {"name": "", "arguments": ""}
            )
            if not current.get("name"):
                current["name"] = existing.get("name", "")
            if not current.get("arguments"):
                current["arguments"] = existing.get("arguments", "")
        if item_id in started_call_ids:
            started_call_ids.discard(item_id)
            started_call_ids.add(provider_call_id)
        return provider_call_id
    candidate = provider_call_id or item_id
    return call_id_aliases.get(candidate, candidate)


def _capture_function_call_added(
    item: Any,
    call_buffers: dict[str, dict[str, str]],
    started_call_ids: set[str],
    call_id_aliases: dict[str, str],
) -> Iterator[ModelStreamEvent]:
    """从 response.output_item.added 事件提取 function_call 的名称。

    部分兼容网关只在 added 事件里携带函数名，后续参数 delta 不再附带；
    若一直等到 output_item.done/response.completed 才取名称，这类网关的
    完整工具调用会被误判为“名称截断”。
    """
    if item is None:
        return
    data = item if isinstance(item, dict) else None
    if data is None and hasattr(item, "model_dump"):
        data = item.model_dump()
    if not isinstance(data, dict) or data.get("type") != "function_call":
        return
    call_id = str(data.get("id") or data.get("call_id") or "")
    name = str(data.get("name") or "").strip()
    if not call_id or not name:
        return
    buf = call_buffers.setdefault(call_id, {"name": "", "arguments": ""})
    if not buf["name"]:
        buf["name"] = name
    arguments = str(data.get("arguments") or "")
    if arguments and not buf["arguments"]:
        buf["arguments"] = arguments
    if call_id not in started_call_ids:
        started_call_ids.add(call_id)
        yield ToolCallStarted(call_id=call_id, name=buf["name"])


def _arguments_json_complete(raw_arguments: Any) -> bool:
    """工具调用参数是否为完整可解析的 JSON。

    空字符串视为完整（模型可不带参数）；非空字符串必须能被 json.loads
    解析，否则说明参数流在半途被网关截断（半截 JSON），不能当正常调用
    收尾，否则会被 parse_tool_arguments 静默降级为 {} 并以空参数误执行。
    """

    if not isinstance(raw_arguments, str) or not raw_arguments.strip():
        return True
    try:
        json.loads(raw_arguments)
        return True
    except json.JSONDecodeError:
        return False


def _emit_function_call_item(
    item: Any,
    call_buffers: dict[str, dict[str, str]],
    emitted_call_ids: set[str],
    started_call_ids: set[str],
    call_id_aliases: dict[str, str],
) -> Iterator[ModelStreamEvent]:
    if item is None:
        return
    item_type = getattr(item, "type", None)
    if item_type is None and isinstance(item, dict):
        item_type = item.get("type")
    if item_type not in {"function_call", "function_call_output"}:
        # 也可能是 output item 内嵌
        if item_type != "function_call":
            data = item if isinstance(item, dict) else None
            if data is None and hasattr(item, "model_dump"):
                data = item.model_dump()
            if not isinstance(data, dict) or data.get("type") != "function_call":
                return
            item = data
            item_type = "function_call"
    if item_type != "function_call":
        return

    raw_call_id = str(
        getattr(item, "call_id", None)
        or getattr(item, "id", None)
        or (item.get("call_id") if isinstance(item, dict) else "")
        or (item.get("id") if isinstance(item, dict) else "")
        or ""
    )
    call_id = _canonical_call_id(
        raw_call_id,
        "",
        call_buffers,
        call_id_aliases,
        started_call_ids,
    )
    name = str(
        getattr(item, "name", None)
        or (item.get("name") if isinstance(item, dict) else "")
        or ""
    )
    arguments = (
        getattr(item, "arguments", None)
        or (item.get("arguments") if isinstance(item, dict) else "")
        or ""
    )
    if not call_id or not name or call_id in emitted_call_ids:
        return
    if call_id in call_buffers:
        call_buffers.pop(call_id, None)
    if call_id not in started_call_ids:
        started_call_ids.add(call_id)
        yield ToolCallStarted(call_id=call_id, name=name)
    yield ToolCallCompleted(
        call_id=call_id,
        name=name,
        arguments=parse_tool_arguments(arguments),
    )
    emitted_call_ids.add(call_id)
