"""OpenAI Responses API Adapter（原生 SDK）。"""

from __future__ import annotations

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
    ReasoningDelta,
    ResponseCompleted,
    TextBlock,
    TextDelta,
    ToolCallBlock,
    ToolCallCompleted,
    ToolCallStarted,
    ToolResultBlock,
    UsageUpdated,
)
from ..registry import DiscoveryModel, DiscoveryResult, ModelDescriptor, ProviderProfile
from ..usage import usage_from_openai_payload
from .openai_common import (
    create_openai_client,
    format_openai_error,
    is_retryable_model_request_error,
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
            extra_body.setdefault("thinking", {"type": "enabled"})
            extra_body.setdefault("reasoning_effort", options.reasoning_effort)
        else:
            extra_body.setdefault("thinking", {"type": "disabled"})

        tools = _tools_for_responses(request)
        input_items = _messages_to_responses_input(request.messages)
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

        try:
            stream = self.client.responses.create(**kwargs)
        except Exception as exc:
            raise ModelError(
                code=ModelErrorCode.CONNECTION_FAILED
                if is_retryable_model_request_error(exc)
                else ModelErrorCode.INVALID_REQUEST,
                message=f"Responses 请求失败：{format_openai_error(exc)}",
                retryable=is_retryable_model_request_error(exc),
            ) from exc

        # 累积 function_call 参数分片，并记录已完成 call_id，避免 SDK 在
        # output_item.done 与 response.completed.output 重复报告同一调用。
        call_buffers: dict[str, dict[str, str]] = {}
        emitted_call_ids: set[str] = set()
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

                event_type = getattr(event, "type", None)
                if event_type is None and isinstance(event, dict):
                    event_type = event.get("type")
                delta = getattr(event, "delta", None)
                if delta is None and isinstance(event, dict):
                    delta = event.get("delta")

                if event_type == "response.output_text.delta" and isinstance(delta, str):
                    yield TextDelta(text=delta)
                elif event_type in {
                    "response.reasoning_text.delta",
                    "response.reasoning_summary_text.delta",
                } and isinstance(delta, str):
                    yield ReasoningDelta(text=delta)
                elif event_type == "response.function_call_arguments.delta":
                    call_id = str(
                        getattr(event, "item_id", None)
                        or getattr(event, "call_id", None)
                        or (event.get("item_id") if isinstance(event, dict) else "")
                        or (event.get("call_id") if isinstance(event, dict) else "")
                        or ""
                    )
                    if call_id and isinstance(delta, str):
                        buf = call_buffers.setdefault(call_id, {"name": "", "arguments": ""})
                        if not buf["name"]:
                            name = getattr(event, "name", None) or (
                                event.get("name") if isinstance(event, dict) else ""
                            )
                            if name:
                                buf["name"] = str(name)
                                yield ToolCallStarted(call_id=call_id, name=buf["name"])
                        buf["arguments"] += delta
                elif event_type in {
                    "response.output_item.done",
                    "response.function_call_arguments.done",
                }:
                    item = getattr(event, "item", None)
                    if item is None and isinstance(event, dict):
                        item = event.get("item")
                    yield from _emit_function_call_item(
                        item,
                        call_buffers,
                        emitted_call_ids,
                    )
                elif event_type == "response.completed":
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

        # 收尾未完成的 function call
        for call_id, buf in list(call_buffers.items()):
            name = buf.get("name") or ""
            if not name or call_id in emitted_call_ids:
                continue
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
    tools: list[dict[str, Any]] = []
    for tool in request.tools:
        tools.append(
            {
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters or {"type": "object", "properties": {}},
            }
        )
    return tools


def _messages_to_responses_input(
    messages: tuple[ConversationMessage, ...],
) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    for message in messages:
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
            for block in message.blocks:
                if isinstance(block, TextBlock):
                    text_parts.append(block.text)
                elif isinstance(block, ToolCallBlock):
                    items.append(
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


def _emit_function_call_item(
    item: Any,
    call_buffers: dict[str, dict[str, str]],
    emitted_call_ids: set[str],
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

    call_id = str(
        getattr(item, "call_id", None)
        or getattr(item, "id", None)
        or (item.get("call_id") if isinstance(item, dict) else "")
        or (item.get("id") if isinstance(item, dict) else "")
        or ""
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
    else:
        yield ToolCallStarted(call_id=call_id, name=name)
    yield ToolCallCompleted(
        call_id=call_id,
        name=name,
        arguments=parse_tool_arguments(arguments),
    )
    emitted_call_ids.add(call_id)
