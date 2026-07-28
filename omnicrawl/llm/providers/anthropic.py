"""Anthropic Claude Messages Adapter（原生 SDK）。"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Callable, Iterator

from ..capabilities import (
    ModelCapabilities,
    conservative_anthropic_capabilities,
    merge_capabilities,
)
from ..errors import ModelError, ModelErrorCode
from ..protocol import (
    PROTOCOL_ANTHROPIC_MESSAGES,
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
from ..usage import usage_from_anthropic_payload
from .openai_common import parse_tool_arguments, resolve_api_key, user_agent_headers


_ANTHROPIC_OPTION_ALLOWLIST = frozenset(
    {
        "top_p",
        "top_k",
        "metadata",
        "stop_sequences",
        "thinking",
    }
)
_FORBIDDEN = frozenset(
    {
        "model",
        "messages",
        "tools",
        "system",
        "stream",
        "max_tokens",
        "api_key",
        "base_url",
        "timeout",
    }
)


@dataclass
class AnthropicMessagesRuntime:
    identity: ModelIdentity
    capabilities: ModelCapabilities
    client: Any
    profile: ProviderProfile
    descriptor: ModelDescriptor
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
                message="Anthropic Runtime 已关闭。",
            )
        if request.tools and not self.capabilities.tools:
            raise ModelError(
                code=ModelErrorCode.UNSUPPORTED_CAPABILITY,
                message=f"模型 {self.identity.model_id} 不支持工具调用。",
            )

        options = request.generation_options
        provider_options = _sanitize_options(options.provider_options)
        max_tokens = options.max_output_tokens or self.descriptor.max_output_tokens or 4096
        kwargs: dict[str, Any] = {
            "model": self.identity.model_id,
            "max_tokens": max_tokens,
            "messages": _to_anthropic_messages(request.messages),
            "stream": True,
        }
        if request.system_prompt.strip():
            kwargs["system"] = request.system_prompt
        if request.tools:
            kwargs["tools"] = [
                {
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters or {"type": "object", "properties": {}},
                }
                for tool in request.tools
            ]
        if options.temperature is not None:
            kwargs["temperature"] = options.temperature
        if options.reasoning_effort and options.reasoning_effort not in {"none", "disabled", ""}:
            # 抽象 reasoning_effort 映射为 Claude thinking 配置时保持保守：仅在用户显式给 provider_options.thinking 时启用。
            if "thinking" not in provider_options and self.capabilities.reasoning:
                pass
        kwargs.update(provider_options)

        try:
            stream = self.client.messages.create(**kwargs)
        except Exception as exc:
            raise ModelError(
                code=ModelErrorCode.INVALID_REQUEST,
                message=f"Claude 请求失败：{_format_anthropic_error(exc)}",
            ) from exc

        # tool_use 缓冲：index -> {id, name, input_json}
        tool_buffers: dict[int, dict[str, Any]] = {}
        finish_reason = "stop"
        try:
            for event in stream:
                if cancel_check is not None:
                    cancel_check()
                event_type = getattr(event, "type", None) or (
                    event.get("type") if isinstance(event, dict) else None
                )
                if event_type == "message_start":
                    message = getattr(event, "message", None) or (
                        event.get("message") if isinstance(event, dict) else None
                    )
                    usage = usage_from_anthropic_payload(message)
                    if usage is not None:
                        yield UsageUpdated(
                            input_tokens=usage.input_tokens,
                            output_tokens=usage.output_tokens,
                            cached_input_tokens=usage.cached_input_tokens,
                        )
                elif event_type == "content_block_start":
                    index = getattr(event, "index", 0)
                    block = getattr(event, "content_block", None) or (
                        event.get("content_block") if isinstance(event, dict) else None
                    )
                    block_type = getattr(block, "type", None) or (
                        block.get("type") if isinstance(block, dict) else None
                    )
                    if block_type == "tool_use":
                        call_id = str(
                            getattr(block, "id", None)
                            or (block.get("id") if isinstance(block, dict) else "")
                            or f"toolu_{index}"
                        )
                        name = str(
                            getattr(block, "name", None)
                            or (block.get("name") if isinstance(block, dict) else "")
                            or ""
                        )
                        tool_buffers[index] = {
                            "id": call_id,
                            "name": name,
                            "input_json": "",
                        }
                        if name:
                            yield ToolCallStarted(call_id=call_id, name=name)
                elif event_type == "content_block_delta":
                    index = getattr(event, "index", 0)
                    delta = getattr(event, "delta", None) or (
                        event.get("delta") if isinstance(event, dict) else None
                    )
                    delta_type = getattr(delta, "type", None) or (
                        delta.get("type") if isinstance(delta, dict) else None
                    )
                    if delta_type == "text_delta":
                        text = getattr(delta, "text", None) or (
                            delta.get("text") if isinstance(delta, dict) else None
                        )
                        if isinstance(text, str) and text:
                            yield TextDelta(text=text)
                    elif delta_type == "thinking_delta":
                        text = getattr(delta, "thinking", None) or (
                            delta.get("thinking") if isinstance(delta, dict) else None
                        )
                        if isinstance(text, str) and text:
                            yield ReasoningDelta(text=text)
                    elif delta_type == "input_json_delta":
                        partial = getattr(delta, "partial_json", None) or (
                            delta.get("partial_json") if isinstance(delta, dict) else None
                        )
                        if isinstance(partial, str) and index in tool_buffers:
                            tool_buffers[index]["input_json"] += partial
                elif event_type == "content_block_stop":
                    index = getattr(event, "index", 0)
                    buf = tool_buffers.pop(index, None)
                    if buf and buf.get("name"):
                        yield ToolCallCompleted(
                            call_id=str(buf["id"]),
                            name=str(buf["name"]),
                            arguments=parse_tool_arguments(buf.get("input_json", "")),
                        )
                elif event_type == "message_delta":
                    delta = getattr(event, "delta", None) or (
                        event.get("delta") if isinstance(event, dict) else None
                    )
                    stop_reason = getattr(delta, "stop_reason", None) or (
                        delta.get("stop_reason") if isinstance(delta, dict) else None
                    )
                    if isinstance(stop_reason, str) and stop_reason:
                        finish_reason = stop_reason
                    usage = getattr(event, "usage", None) or (
                        event.get("usage") if isinstance(event, dict) else None
                    )
                    if usage is not None:
                        mapped = usage_from_anthropic_payload({"usage": usage})
                        if mapped is not None:
                            yield UsageUpdated(
                                input_tokens=mapped.input_tokens,
                                output_tokens=mapped.output_tokens,
                                cached_input_tokens=mapped.cached_input_tokens,
                            )
                elif event_type == "message_stop":
                    pass
        except Exception as exc:
            if cancel_check is not None:
                try:
                    cancel_check()
                except Exception:
                    raise
            raise ModelError(
                code=ModelErrorCode.STREAM_INTERRUPTED,
                message=f"Claude 流式回复中断：{_format_anthropic_error(exc)}",
            ) from exc

        for buf in tool_buffers.values():
            if buf.get("name"):
                yield ToolCallCompleted(
                    call_id=str(buf["id"]),
                    name=str(buf["name"]),
                    arguments=parse_tool_arguments(buf.get("input_json", "")),
                )
        yield ResponseCompleted(finish_reason=finish_reason)

    def close(self) -> None:
        self._closed = True
        client = self.client
        self.client = None
        if client is None:
            return
        close = getattr(client, "close", None)
        if callable(close):
            try:
                close()
            except Exception:
                pass


class AnthropicMessagesAdapter:
    provider_type = PROTOCOL_ANTHROPIC_MESSAGES

    def create_runtime(
        self,
        profile: ProviderProfile,
        model: ModelDescriptor,
    ) -> AnthropicMessagesRuntime:
        capabilities = merge_capabilities(
            conservative_anthropic_capabilities(),
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
            protocol=PROTOCOL_ANTHROPIC_MESSAGES,
            model_id=model.model_id,
            catalog_key=model.identity.catalog_key,
        )
        return AnthropicMessagesRuntime(
            identity=identity,
            capabilities=capabilities,
            client=_create_anthropic_client(profile),
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
        client = _create_anthropic_client(profile)
        try:
            # Anthropic SDK 部分版本提供 models.list
            list_fn = getattr(getattr(client, "models", None), "list", None)
            if not callable(list_fn):
                return DiscoveryResult(
                    profile_id=profile.id,
                    status="unsupported",
                    message="当前 Anthropic SDK/账号不支持模型列表发现，请使用 models.yaml 自定义模型。",
                )
            response = list_fn()
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
                        protocol=PROTOCOL_ANTHROPIC_MESSAGES,
                        model_id=model_id,
                        display_name=model_id,
                        capabilities=conservative_anthropic_capabilities(),
                    )
                )
            return DiscoveryResult(profile_id=profile.id, models=tuple(models), status="ok")
        except Exception as exc:
            return DiscoveryResult(
                profile_id=profile.id,
                status="unavailable",
                message=f"Claude 模型列表发现失败：{_format_anthropic_error(exc)}",
            )
        finally:
            close = getattr(client, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:
                    pass


def _create_anthropic_client(profile: ProviderProfile) -> Any:
    api_key = resolve_api_key(profile)
    if not api_key:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message=f"Profile {profile.id} 缺少 Anthropic API Key。",
        )
    try:
        import anthropic
    except ImportError as exc:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message="缺少 anthropic 依赖，请先执行：pip install anthropic",
        ) from exc

    kwargs: dict[str, Any] = {"api_key": api_key}
    if profile.base_url.strip():
        kwargs["base_url"] = profile.base_url.strip()
    headers = user_agent_headers(profile)
    if headers:
        kwargs["default_headers"] = headers
    timeout = profile.request_timeout_seconds
    if timeout:
        kwargs["timeout"] = timeout
    return anthropic.Anthropic(**kwargs)


def _sanitize_options(options: Any) -> dict[str, Any]:
    if not options:
        return {}
    if not isinstance(options, dict):
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message="provider_options 必须是对象。",
        )
    result: dict[str, Any] = {}
    for key, value in options.items():
        if key in _FORBIDDEN:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message=f"provider_options 不允许覆盖 Host 字段：{key}",
            )
        if key not in _ANTHROPIC_OPTION_ALLOWLIST:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message=f"Anthropic provider_options 不支持字段：{key}",
            )
        result[key] = value
    return result


def _to_anthropic_messages(
    messages: tuple[ConversationMessage, ...],
) -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    pending_tool_results: list[dict[str, Any]] = []

    def flush_tool_results() -> None:
        nonlocal pending_tool_results
        if pending_tool_results:
            result.append({"role": "user", "content": pending_tool_results})
            pending_tool_results = []

    for message in messages:
        if message.role == "tool":
            for block in message.blocks:
                if isinstance(block, ToolResultBlock):
                    pending_tool_results.append(
                        {
                            "type": "tool_result",
                            "tool_use_id": block.call_id,
                            "content": block.content,
                            "is_error": not block.ok,
                        }
                    )
            continue

        if message.role == "assistant":
            flush_tool_results()
            content: list[dict[str, Any]] = []
            for block in message.blocks:
                if isinstance(block, TextBlock) and block.text:
                    content.append({"type": "text", "text": block.text})
                elif isinstance(block, ToolCallBlock):
                    content.append(
                        {
                            "type": "tool_use",
                            "id": block.call_id or block.provider_call_id,
                            "name": block.name,
                            "input": block.arguments or {},
                        }
                    )
            result.append({"role": "assistant", "content": content or [{"type": "text", "text": ""}]})
            continue

        # user：截图以原生 Base64 image source 发送，不把临时路径交给远端读取。
        content: list[dict[str, Any]] = []
        for block in message.blocks:
            if isinstance(block, TextBlock) and block.text:
                content.append({"type": "text", "text": block.text})
            elif isinstance(block, ImageBlock):
                content.append(
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": block.media_type,
                            "data": block.data_base64,
                        },
                    }
                )
        if pending_tool_results:
            # 工具结果和紧随其后的截图属于同一个 user turn；合并可避免连续
            # user 消息，并符合 Anthropic 对 tool_result 紧邻 tool_use 的约束。
            content = [*pending_tool_results, *content]
            pending_tool_results = []
        result.append({"role": "user", "content": content or ""})

    flush_tool_results()
    return result


def _format_anthropic_error(exc: Exception) -> str:
    message = str(exc).strip()
    lowered = message.lower()
    if "authentication" in lowered or "api key" in lowered or "401" in lowered:
        return "Claude 鉴权失败。请检查 ANTHROPIC_API_KEY。"
    if "permission" in lowered or "403" in lowered:
        return "当前 API Key 没有访问该 Claude 模型的权限。"
    if "not_found" in lowered or "404" in lowered:
        return "Claude 模型不存在或路径错误。"
    if "rate" in lowered or "429" in lowered:
        return "Claude 服务限流，请稍后重试。"
    if "timeout" in lowered:
        return "Claude 请求超时，请稍后重试。"
    return f"Claude 请求失败：{type(exc).__name__}"
