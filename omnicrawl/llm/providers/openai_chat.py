"""OpenAI Chat Completions Adapter（原生 SDK）。"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable, Iterator

from ..capabilities import (
    ModelCapabilities,
    conservative_openai_chat_capabilities,
    merge_capabilities,
)
from ..errors import ModelError, ModelErrorCode
from ..protocol import (
    PROTOCOL_OPENAI_CHAT_COMPLETIONS,
    ConversationMessage,
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
                    raise ModelError(
                        code=ModelErrorCode.INVALID_REQUEST,
                        message=f"Agent 请求失败：{format_openai_error(retry_exc)}",
                        retryable=is_retryable_model_request_error(retry_exc),
                    ) from retry_exc
            else:
                raise ModelError(
                    code=ModelErrorCode.INVALID_REQUEST
                    if not is_retryable_model_request_error(exc)
                    else ModelErrorCode.CONNECTION_FAILED,
                    message=f"Agent 请求失败：{format_openai_error(exc)}",
                    retryable=is_retryable_model_request_error(exc),
                ) from exc

        buffers: dict[int, dict[str, Any]] = {}
        started: set[int] = set()
        finish_reason = "stop"
        try:
            for event in stream:
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

        # 将缓冲的完整 tool call 收尾为 Completed 事件
        for idx in sorted(buffers.keys()):
            buf = buffers[idx]
            name = str(buf.get("name") or "").strip()
            if not name:
                continue
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
            except Exception:
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
                except Exception:
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
            if tool_calls:
                payload["tool_calls"] = tool_calls
            result.append(payload)
            continue
        # user / system
        text = message.text
        result.append({"role": message.role, "content": text})
    return result


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
