"""OpenAI Chat Completions Adapter（原生 SDK）。"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Callable, Iterable, Iterator

from ..capabilities import (
    ModelCapabilities,
    conservative_openai_chat_capabilities,
    merge_capabilities,
)
from ..errors import ModelError, ModelErrorCode, map_openai_exception
from ..protocol import (
    PROTOCOL_OPENAI_CHAT_COMPLETIONS,
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
    ToolCallArgumentsDelta,
    ToolCallBlock,
    ToolCallCompleted,
    ToolCallStarted,
    ToolResultBlock,
    UsageUpdated,
)
from ..registry import DiscoveryModel, DiscoveryResult, ModelDescriptor, ProviderProfile
from ..stream_registry import (
    register_stream,
    registered_stream_events,
    stream_owner_for,
    unregister_stream,
)
from ..usage import usage_from_openai_payload
from .openai_common import (
    build_prompt_cache_key,
    create_openai_client,
    extract_chat_stream_delta,
    format_openai_error,
    is_retryable_model_request_error,
    is_unsupported_prompt_cache_error,
    parse_tool_arguments,
    read_attr_or_key,
    resolve_api_key,
    sanitize_provider_options,
    tool_specs_to_openai_functions,
)


@dataclass
class OpenAIChatCompletionsRuntime:
    identity: ModelIdentity
    capabilities: ModelCapabilities
    client: Any
    profile: ProviderProfile
    descriptor: ModelDescriptor
    _owns_client: bool = True
    _closed: bool = False

    def stream_turn(
        self,
        request: ModelTurnRequest,
        *,
        cancel_check: Callable[[], None] | None = None,
    ) -> Iterator[ModelStreamEvent]:
        if self._closed:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message="Chat Completions Runtime 已关闭。",
            )
        if not self.capabilities.streaming:
            raise ModelError(
                code=ModelErrorCode.UNSUPPORTED_CAPABILITY,
                message=f"模型 {self.identity.model_id} 不支持流式输出。",
            )
        if request.tools and not self.capabilities.tools:
            raise ModelError(
                code=ModelErrorCode.UNSUPPORTED_CAPABILITY,
                message=f"模型 {self.identity.model_id} 不支持工具调用。",
            )

        messages = _to_openai_messages(request.system_prompt, request.messages)
        tools = tool_specs_to_openai_functions(request.tools) if request.tools else None
        options = request.generation_options
        extra_body = dict(sanitize_provider_options(options.provider_options))
        # 兼容课程网关 thinking / reasoning_effort 扩展。
        if options.reasoning_effort and options.reasoning_effort not in {"none", "disabled", ""}:
            extra_body.setdefault("thinking", {"type": "enabled"})
            extra_body.setdefault("reasoning_effort", options.reasoning_effort)
        elif "thinking" not in extra_body:
            extra_body["thinking"] = {"type": "disabled"}

        request_kwargs: dict[str, Any] = {
            "model": self.identity.model_id,
            "messages": messages,
            "stream": True,
            "timeout": options.request_timeout_seconds or self.profile.request_timeout_seconds,
        }
        if tools:
            request_kwargs["tools"] = tools
            request_kwargs["tool_choice"] = "auto"
        if options.max_output_tokens:
            request_kwargs["max_tokens"] = options.max_output_tokens
        if options.temperature is not None:
            request_kwargs["temperature"] = options.temperature
        if extra_body:
            request_kwargs["extra_body"] = extra_body

        prompt_cache_key = build_prompt_cache_key(
            request.prompt_cache_identity,
            model=self.identity.model_id,
        )
        if prompt_cache_key and self.capabilities.prompt_cache:
            request_kwargs["prompt_cache_key"] = prompt_cache_key
        elif prompt_cache_key and is_openai_gpt_like(self.identity.model_id):
            # 与旧 AgentLLMProtocol 一致：仅 GPT 系列尝试 prompt_cache_key。
            request_kwargs["prompt_cache_key"] = prompt_cache_key

        try:
            stream = self.client.chat.completions.create(**request_kwargs)
        except Exception as exc:
            if "prompt_cache_key" in request_kwargs and is_unsupported_prompt_cache_error(exc):
                request_kwargs.pop("prompt_cache_key", None)
                try:
                    stream = self.client.chat.completions.create(**request_kwargs)
                    yield ProviderWarning(
                        code="prompt_cache_unsupported",
                        message="当前网关不支持 prompt_cache_key，已自动移除后重试。",
                    )
                except Exception as retry_exc:
                    mapped = map_openai_exception(retry_exc)
                    raise ModelError(
                        code=mapped.code,
                        message=f"Agent 请求失败：{mapped.message}",
                        retryable=mapped.retryable,
                        status_code=mapped.status_code,
                        provider=mapped.provider,
                        protocol=PROTOCOL_OPENAI_CHAT_COMPLETIONS,
                    ) from retry_exc
            else:
                mapped = map_openai_exception(exc)
                raise ModelError(
                    code=mapped.code,
                    message=f"Agent 请求失败：{mapped.message}",
                    retryable=mapped.retryable,
                    status_code=mapped.status_code,
                    provider=mapped.provider,
                    protocol=PROTOCOL_OPENAI_CHAT_COMPLETIONS,
                ) from exc

        buffers: dict[int, dict[str, Any]] = {}
        started: set[int] = set()
        finish_reason = "stop"
        try:
            for event in _iter_stream_events(stream, cancel_check):
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

                choice0 = _first_choice(event)
                if choice0 is not None:
                    fr = read_attr_or_key(choice0, "finish_reason")
                    if isinstance(fr, str) and fr:
                        finish_reason = fr

                delta = extract_chat_stream_delta(event)
                if delta is None:
                    continue

                content = read_attr_or_key(delta, "content")
                if isinstance(content, str) and content:
                    yield TextDelta(text=content)

                reasoning = read_attr_or_key(delta, "reasoning_content")
                if isinstance(reasoning, str) and reasoning:
                    yield ReasoningDelta(text=reasoning)

                tool_deltas = read_attr_or_key(delta, "tool_calls")
                if isinstance(tool_deltas, list):
                    yield from _emit_tool_call_deltas(tool_deltas, buffers, started)
        except Exception as exc:
            if cancel_check is not None:
                # cancel_check 抛出的异常原样上抛
                try:
                    cancel_check()
                except Exception:
                    raise
            raise ModelError(
                code=ModelErrorCode.STREAM_INTERRUPTED,
                message=f"Agent 流式回复中断：{format_openai_error(exc)}",
                retryable=is_retryable_model_request_error(exc),
            ) from exc

        # 流迭代器正常耗尽后仍要确认取消状态：外层主动 close() 的流可能
        # 以“正常结束、无可见内容”返回，此时必须优先处理取消，不能把
        # 结果继续转换为空响应或可重试请求。
        if cancel_check is not None:
            cancel_check()

        # 将缓冲的完整 tool call 收尾为 Completed 事件。流在此正常耗尽并不
        # 代表调用一定完整：网关可能以“正常结束”形态包装断流，此时 name
        # 缺失或 arguments 是半截 JSON 都是截断信号，绝不能静默丢弃（否则
        # 半截回复会直接结束回合且不提示用户），交由上层回滚并重试。
        for idx in sorted(buffers.keys()):
            buf = buffers[idx]
            name = str(buf.get("name") or "").strip()
            if not name:
                raise ModelError(
                    code=ModelErrorCode.STREAM_INTERRUPTED,
                    message="Chat Completions 流在工具调用名称完整到达前结束，疑似连接被网关截断。",
                    retryable=True,
                )
            if not _arguments_json_complete(buf.get("arguments", "")):
                raise ModelError(
                    code=ModelErrorCode.STREAM_INTERRUPTED,
                    message="Chat Completions 流在工具调用参数完整到达前结束，疑似连接被网关截断。",
                    retryable=True,
                )
            call_id = str(buf.get("id") or f"call_{idx}")
            if idx not in started:
                yield ToolCallStarted(call_id=call_id, name=name)
            yield ToolCallCompleted(
                call_id=call_id,
                name=name,
                arguments=parse_tool_arguments(buf.get("arguments", "")),
            )
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
            except Exception:  # noqa: BLE001 - SDK 客户端关闭失败不影响已释放的运行时
                pass


class OpenAIChatCompletionsAdapter:
    provider_type = PROTOCOL_OPENAI_CHAT_COMPLETIONS

    def create_runtime(
        self,
        profile: ProviderProfile,
        model: ModelDescriptor,
    ) -> OpenAIChatCompletionsRuntime:
        capabilities = merge_capabilities(
            conservative_openai_chat_capabilities(),
            model.capabilities,
        )
        # 仅补窗口大小时用 replace，避免稀疏 ModelCapabilities 覆盖 tools 等开关。
        if model.context_window_tokens > 0:
            from dataclasses import replace as _replace

            capabilities = _replace(
                capabilities,
                context_window_tokens=model.context_window_tokens,
            )
        identity = ModelIdentity(
            profile_id=profile.id,
            provider=profile.provider,
            protocol=PROTOCOL_OPENAI_CHAT_COMPLETIONS,
            model_id=model.model_id,
            catalog_key=model.identity.catalog_key,
        )
        client = create_openai_client(profile)
        return OpenAIChatCompletionsRuntime(
            identity=identity,
            capabilities=capabilities,
            client=client,
            profile=profile,
            descriptor=model,
        )

    def discover_models(
        self,
        profile: ProviderProfile,
        *,
        timeout_seconds: float,
    ) -> DiscoveryResult:
        if not resolve_api_key(profile):
            return DiscoveryResult(
                profile_id=profile.id,
                status="unavailable",
                message="缺少 API Key，无法发现模型。",
            )
        client = create_openai_client(profile)
        try:
            # OpenAI SDK models.list；timeout 通过 request options 传递
            response = client.models.list(timeout=timeout_seconds)
            items = list(getattr(response, "data", None) or response)
            models: list[DiscoveryModel] = []
            seen: set[str] = set()
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
                        protocol=profile.resolve_protocol(PROTOCOL_OPENAI_CHAT_COMPLETIONS)
                        if profile.default_protocol
                        else PROTOCOL_OPENAI_CHAT_COMPLETIONS,
                        model_id=model_id,
                        display_name=model_id,
                        capabilities=conservative_openai_chat_capabilities(),
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
                except Exception:  # noqa: BLE001 - 模型发现结束后关闭临时客户端，失败不影响发现结果
                    pass


def is_openai_gpt_like(model: str) -> bool:
    from .openai_common import is_openai_gpt_model

    return is_openai_gpt_model(model)


def _to_openai_messages(
    system_prompt: str,
    messages: tuple[ConversationMessage, ...],
) -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    if system_prompt.strip():
        result.append({"role": "system", "content": system_prompt})
    # 动态工具声明跨多条 system 消息按工具名去重，避免重复函数声明。
    seen_dynamic_tools: set[str] = set()
    for message in messages:
        if message.role == "tool":
            for block in message.blocks:
                if isinstance(block, ToolResultBlock):
                    result.append(
                        {
                            "role": "tool",
                            "tool_call_id": block.call_id,
                            "content": block.content,
                        }
                    )
            continue
        if message.role == "assistant":
            content_parts: list[str] = []
            tool_calls: list[dict[str, Any]] = []
            for block in message.blocks:
                if isinstance(block, TextBlock):
                    content_parts.append(block.text)
                elif isinstance(block, ToolCallBlock):
                    tool_calls.append(
                        {
                            "id": block.call_id or block.provider_call_id,
                            "type": "function",
                            "function": {
                                "name": block.name,
                                "arguments": json.dumps(block.arguments, ensure_ascii=False),
                            },
                        }
                    )
            payload: dict[str, Any] = {
                "role": "assistant",
                "content": "".join(content_parts) or None,
            }
            # 思考模式上游（DeepSeek V4 thinking 等）要求历史 assistant 消息原样
            # 回传 reasoning_content，否则二次请求会被拒绝：
            # HTTP 400 "The `reasoning_content` in the thinking mode must be passed back"。
            # 含 tool_calls 的消息属上游强制校验对象：即使推理为空也带空串占位
            # （上游只检查字段存在性），避免工具历史出现后第二次请求必然失败；
            # 纯文本消息仅在确有推理时携带，与运行期构造保持一致。
            if tool_calls:
                payload["reasoning_content"] = message.reasoning or ""
                payload["tool_calls"] = tool_calls
            elif message.reasoning:
                payload["reasoning_content"] = message.reasoning
            result.append(payload)
            continue
        if message.role == "system" and message.tools:
            # 动态加载工具协议：声明通过 system 消息的 tools 字段下发，
            # 该消息不能再带 content 字段，否则网关返回 400。
            fresh_tools = [
                tool for tool in message.tools if tool.name not in seen_dynamic_tools
            ]
            if not fresh_tools:
                continue
            seen_dynamic_tools.update(tool.name for tool in fresh_tools)
            result.append(
                {
                    "role": "system",
                    "tools": tool_specs_to_openai_functions(tuple(fresh_tools)),
                }
            )
            continue
        # user / system。视觉图片只由 Host 生成的 user 观察消息携带。
        content: list[dict[str, Any]] = []
        for block in message.blocks:
            if isinstance(block, TextBlock) and block.text:
                content.append({"type": "text", "text": block.text})
            elif isinstance(block, ImageBlock):
                content.append(
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": block.data_url,
                            "detail": block.detail,
                        },
                    }
                )
        if content and any(item.get("type") == "image_url" for item in content):
            payload_content: Any = content
        else:
            payload_content = "".join(
                str(item.get("text") or "") for item in content if item.get("type") == "text"
            )
        result.append({"role": message.role, "content": payload_content})
    return result


def _iter_stream_events(
    stream: Any,
    cancel_check: Callable[[], None] | None,
) -> Iterator[Any]:
    """迭代模型流事件，优先走跳过 pydantic 建模的原始 SSE 路径。

    SDK 的 ``Stream.__stream__`` 对**每个 chunk** 调一次
    ``construct_type(ChatCompletionChunk, ...)``，递归走完整类型树（实测约 11 次
    ``construct_type`` + 约 50 次 ``get_origin`` 每 chunk）。对 546 条真实 chunk
    离线回放：200.5µs/chunk → 12.4µs/chunk（-93.8%），且两条路径下游输出完全一致
    —— 因为本模块取值全部经由 ``read_attr_or_key`` / ``isinstance(x, dict)``，
    本来就兼容原生 dict。

    ``Stream._iter_events`` 是 SDK 私有 API：缺失时自动回退到 SDK 自身迭代路径，
    只损失性能、不影响正确性。
    """

    if not _raw_sse_available(stream):
        yield from registered_stream_events(
            stream,
            owner=stream_owner_for(cancel_check),
        )
        return
    # 注册真实 Stream（而非包装后的迭代器）：取消时 close_active_streams 要关闭的
    # 是底层 HTTP 连接。
    register_stream(stream, owner=stream_owner_for(cancel_check))
    try:
        yield from _iter_raw_sse_events(stream)
    finally:
        unregister_stream(stream)


def _raw_sse_available(stream: Any) -> bool:
    """SDK 是否提供了可供绕过的 SSE 迭代入口。"""

    return callable(getattr(stream, "_iter_events", None)) and getattr(
        stream, "response", None
    ) is not None


def _decode_sse_data(sse: Any) -> Any:
    """取出单个 SSE 事件的 JSON 负载；不可解析时返回 None。"""

    json_method = getattr(sse, "json", None)
    if callable(json_method):
        try:
            return json_method()
        except Exception:  # noqa: BLE001 - 非 JSON 事件按“跳过”处理
            return None
    raw = getattr(sse, "data", "")
    if isinstance(raw, str) and raw.strip():
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            return None
    return None


def _iter_raw_sse_events(stream: Any) -> Iterator[dict[str, Any]]:
    """直接消费 SDK 的 SSE 层，产出原生 JSON dict。

    与 ``Stream.__stream__`` 保持相同的可观察语义：``[DONE]`` 终止、``error``
    负载抛 ``APIError``、无论正常结束、异常还是提前返回都关闭 HTTP 响应。
    """

    from openai import APIError

    response = stream.response
    try:
        for sse in stream._iter_events():
            raw = getattr(sse, "data", "")
            if isinstance(raw, str) and raw.startswith("[DONE]"):
                return
            data = _decode_sse_data(sse)
            if not isinstance(data, dict):
                continue
            error = data.get("error")
            if error:
                message = error.get("message") if isinstance(error, dict) else None
                if not message or not isinstance(message, str):
                    message = "An error occurred during streaming"
                raise APIError(
                    message=message,
                    request=getattr(response, "request", None),
                    body=error,
                )
            yield data
    finally:
        # SDK 的 __stream__ 在 finally 里关闭响应。绕过它就必须自己关，否则
        # 提前退出（[DONE] / 取消 / 消费方 break）会泄漏底层连接。
        close = getattr(response, "close", None)
        if callable(close):
            close()


def _first_choice(event: Any) -> Any | None:
    choices = getattr(event, "choices", None)
    if isinstance(choices, list) and choices:
        return choices[0]
    if isinstance(event, dict):
        choices = event.get("choices")
        if isinstance(choices, list) and choices:
            return choices[0]
    return None


def _emit_tool_call_deltas(
    tc_deltas: list[Any],
    buffers: dict[int, dict[str, Any]],
    started: set[int],
) -> Iterable[ModelStreamEvent]:
    for tc in tc_deltas:
        idx = read_attr_or_key(tc, "index")
        if not isinstance(idx, int):
            idx = 0
        if idx not in buffers:
            buffers[idx] = {"id": "", "name": "", "arguments": ""}
        buf = buffers[idx]
        tc_id = read_attr_or_key(tc, "id")
        if tc_id:
            buf["id"] = str(tc_id)
        func = read_attr_or_key(tc, "function")
        name_delta = ""
        args_delta = ""
        if isinstance(func, dict):
            if func.get("name"):
                name_delta = str(func["name"])
                buf["name"] += name_delta
            if func.get("arguments"):
                args_delta = str(func["arguments"])
                buf["arguments"] += args_delta
        elif func is not None:
            fn_name = getattr(func, "name", None)
            if fn_name:
                name_delta = str(fn_name)
                buf["name"] += name_delta
            fn_args = getattr(func, "arguments", None)
            if fn_args:
                args_delta = str(fn_args)
                buf["arguments"] += args_delta
        call_id = str(buf["id"] or f"call_{idx}")
        if buf["name"] and idx not in started:
            started.add(idx)
            yield ToolCallStarted(call_id=call_id, name=buf["name"])
        if args_delta:
            yield ToolCallArgumentsDelta(call_id=call_id, delta=args_delta)


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
