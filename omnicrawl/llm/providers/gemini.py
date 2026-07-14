"""Google Gemini Generate Content Adapter（google-genai 原生 SDK）。"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Callable, Iterator

from ..capabilities import (
    ModelCapabilities,
    conservative_gemini_capabilities,
    merge_capabilities,
)
from ..errors import ModelError, ModelErrorCode
from ..protocol import (
    PROTOCOL_GEMINI_GENERATE_CONTENT,
    ConversationMessage,
    ModelIdentity,
    ModelStreamEvent,
    ModelTurnRequest,
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
from ..usage import usage_from_gemini_payload
from .openai_common import parse_tool_arguments, resolve_api_key


_GEMINI_OPTION_ALLOWLIST = frozenset(
    {
        "top_p",
        "top_k",
        "candidate_count",
        "stop_sequences",
        "response_mime_type",
        "safety_settings",
    }
)
_FORBIDDEN = frozenset(
    {
        "model",
        "contents",
        "tools",
        "system_instruction",
        "stream",
        "api_key",
        "timeout",
    }
)


@dataclass
class GeminiGenerateContentRuntime:
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
                message="Gemini Runtime 已关闭。",
            )
        if request.tools and not self.capabilities.tools:
            raise ModelError(
                code=ModelErrorCode.UNSUPPORTED_CAPABILITY,
                message=f"模型 {self.identity.model_id} 不支持工具调用。",
            )

        options = request.generation_options
        provider_options = _sanitize_options(options.provider_options)
        config = _build_generate_config(request, options, provider_options)
        contents = _to_gemini_contents(request.messages)

        try:
            # google-genai: client.models.generate_content_stream(...)
            models_api = getattr(self.client, "models", None)
            if models_api is None:
                raise ModelError(
                    code=ModelErrorCode.CONFIGURATION_ERROR,
                    message="google-genai 客户端缺少 models 接口。",
                )
            stream = models_api.generate_content_stream(
                model=self.identity.model_id,
                contents=contents,
                config=config,
            )
        except ModelError:
            raise
        except Exception as exc:
            raise ModelError(
                code=ModelErrorCode.INVALID_REQUEST,
                message=f"Gemini 请求失败：{_format_gemini_error(exc)}",
            ) from exc

        # 避免 SDK 自动函数执行：我们只消费 function_call part，不注册本地 callable。
        emitted_calls: set[str] = set()
        finish_reason = "stop"
        try:
            for chunk in stream:
                if cancel_check is not None:
                    cancel_check()
                usage = usage_from_gemini_payload(chunk)
                if usage is not None:
                    yield UsageUpdated(
                        input_tokens=usage.input_tokens,
                        output_tokens=usage.output_tokens,
                        cached_input_tokens=usage.cached_input_tokens,
                    )
                yield from _emit_chunk_parts(chunk, emitted_calls)
                fr = _read_finish_reason(chunk)
                if fr:
                    finish_reason = fr
        except Exception as exc:
            if cancel_check is not None:
                try:
                    cancel_check()
                except Exception:
                    raise
            raise ModelError(
                code=ModelErrorCode.STREAM_INTERRUPTED,
                message=f"Gemini 流式回复中断：{_format_gemini_error(exc)}",
            ) from exc

        yield ResponseCompleted(finish_reason=finish_reason)

    def close(self) -> None:
        self._closed = True
        # google-genai Client 通常无需显式 close；保留钩子以兼容未来版本。
        client = self.client
        self.client = None
        close = getattr(client, "close", None) if client is not None else None
        if callable(close):
            try:
                close()
            except Exception:
                pass


class GeminiGenerateContentAdapter:
    provider_type = PROTOCOL_GEMINI_GENERATE_CONTENT

    def create_runtime(
        self,
        profile: ProviderProfile,
        model: ModelDescriptor,
    ) -> GeminiGenerateContentRuntime:
        capabilities = merge_capabilities(
            conservative_gemini_capabilities(),
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
            protocol=PROTOCOL_GEMINI_GENERATE_CONTENT,
            model_id=model.model_id,
            catalog_key=model.identity.catalog_key,
        )
        return GeminiGenerateContentRuntime(
            identity=identity,
            capabilities=capabilities,
            client=_create_gemini_client(profile),
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
        client = _create_gemini_client(profile)
        try:
            models_api = getattr(client, "models", None)
            list_fn = getattr(models_api, "list", None)
            if not callable(list_fn):
                return DiscoveryResult(
                    profile_id=profile.id,
                    status="unsupported",
                    message="当前 Google Gen AI SDK 不支持模型列表发现，请使用 models.yaml 自定义模型。",
                )
            response = list_fn()
            items = list(response) if response is not None else []
            models: list[DiscoveryModel] = []
            seen: set[str] = set()
            for item in items[:500]:
                name = str(
                    getattr(item, "name", None)
                    or getattr(item, "model", None)
                    or (item.get("name") if isinstance(item, dict) else "")
                    or ""
                ).strip()
                # name 常见为 models/gemini-...
                model_id = name.split("/", 1)[-1] if name else ""
                if not model_id or model_id in seen:
                    continue
                seen.add(model_id)
                models.append(
                    DiscoveryModel(
                        profile_id=profile.id,
                        provider=profile.provider,
                        protocol=PROTOCOL_GEMINI_GENERATE_CONTENT,
                        model_id=model_id,
                        display_name=model_id,
                        capabilities=conservative_gemini_capabilities(),
                    )
                )
            return DiscoveryResult(profile_id=profile.id, models=tuple(models), status="ok")
        except Exception as exc:
            return DiscoveryResult(
                profile_id=profile.id,
                status="unavailable",
                message=f"Gemini 模型列表发现失败：{_format_gemini_error(exc)}",
            )


def _create_gemini_client(profile: ProviderProfile) -> Any:
    api_key = resolve_api_key(profile)
    if not api_key:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message=f"Profile {profile.id} 缺少 Gemini API Key。",
        )
    try:
        from google import genai
    except ImportError as exc:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message="缺少 google-genai 依赖，请先执行：pip install google-genai",
        ) from exc

    kwargs: dict[str, Any] = {"api_key": api_key}
    # http_options 可用于自定义 base_url；首版仅在 profile.base_url 非空时尝试。
    if profile.base_url.strip():
        try:
            from google.genai import types as genai_types

            kwargs["http_options"] = genai_types.HttpOptions(base_url=profile.base_url.strip())
        except Exception:
            # 旧版本 SDK 可能不支持；忽略自定义 base_url 并依赖默认官方地址。
            pass
    return genai.Client(**kwargs)


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
        if key not in _GEMINI_OPTION_ALLOWLIST:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message=f"Gemini provider_options 不支持字段：{key}",
            )
        result[key] = value
    return result


def _build_generate_config(
    request: ModelTurnRequest,
    options: Any,
    provider_options: dict[str, Any],
) -> Any:
    """构建 GenerateContentConfig；优先使用 SDK types，失败则退回 dict。"""

    config: dict[str, Any] = {}
    if request.system_prompt.strip():
        config["system_instruction"] = request.system_prompt
    if options.max_output_tokens:
        config["max_output_tokens"] = options.max_output_tokens
    if options.temperature is not None:
        config["temperature"] = options.temperature
    if request.tools:
        # 显式禁用自动函数执行：只声明 schema，不绑定 Python callable。
        config["tools"] = [
            {
                "function_declarations": [
                    {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters or {"type": "object", "properties": {}},
                    }
                    for tool in request.tools
                ]
            }
        ]
        config["automatic_function_calling"] = {"disable": True}
    config.update(provider_options)

    try:
        from google.genai import types as genai_types

        return genai_types.GenerateContentConfig(**config)
    except Exception:
        return config


def _to_gemini_contents(
    messages: tuple[ConversationMessage, ...],
) -> list[dict[str, Any]]:
    contents: list[dict[str, Any]] = []
    call_names: dict[str, str] = {}
    for message in messages:
        if message.role == "tool":
            parts = []
            for block in message.blocks:
                if isinstance(block, ToolResultBlock):
                    response_payload: dict[str, Any] = {"result": block.content}
                    if not block.ok:
                        response_payload["error"] = block.content
                    parts.append(
                        {
                            "function_response": {
                                "name": call_names.get(block.call_id, "tool"),
                                "response": response_payload,
                            }
                        }
                    )
            if parts:
                contents.append({"role": "user", "parts": parts})
            continue

        role = "model" if message.role == "assistant" else "user"
        parts = []
        for block in message.blocks:
            if isinstance(block, TextBlock) and block.text:
                parts.append({"text": block.text})
            elif isinstance(block, ToolCallBlock):
                if block.call_id:
                    call_names[block.call_id] = block.name
                parts.append(
                    {
                        "function_call": {
                            "name": block.name,
                            "args": block.arguments or {},
                        }
                    }
                )
        if not parts and message.text:
            parts.append({"text": message.text})
        if parts:
            contents.append({"role": role, "parts": parts})
    return contents


def _emit_chunk_parts(
    chunk: Any,
    emitted_calls: set[str],
) -> Iterator[ModelStreamEvent]:
    candidates = getattr(chunk, "candidates", None)
    if candidates is None and isinstance(chunk, dict):
        candidates = chunk.get("candidates")
    if not candidates:
        # 兼容 chunk.text
        text = getattr(chunk, "text", None)
        if isinstance(text, str) and text:
            yield TextDelta(text=text)
        return

    for candidate in candidates:
        content = getattr(candidate, "content", None) or (
            candidate.get("content") if isinstance(candidate, dict) else None
        )
        parts = getattr(content, "parts", None) if content is not None else None
        if parts is None and isinstance(content, dict):
            parts = content.get("parts")
        if not parts:
            continue
        for part in parts:
            text = getattr(part, "text", None)
            if text is None and isinstance(part, dict):
                text = part.get("text")
            if isinstance(text, str) and text:
                yield TextDelta(text=text)
                continue
            function_call = getattr(part, "function_call", None)
            if function_call is None and isinstance(part, dict):
                function_call = part.get("function_call") or part.get("functionCall")
            if function_call is None:
                continue
            name = str(
                getattr(function_call, "name", None)
                or (function_call.get("name") if isinstance(function_call, dict) else "")
                or ""
            )
            args = (
                getattr(function_call, "args", None)
                or (function_call.get("args") if isinstance(function_call, dict) else None)
                or {}
            )
            if isinstance(args, str):
                args = parse_tool_arguments(args)
            if not isinstance(args, dict):
                args = {}
            call_id = f"{name}:{json.dumps(args, sort_keys=True, ensure_ascii=False)}"
            if not name or call_id in emitted_calls:
                continue
            emitted_calls.add(call_id)
            # Gemini 不总提供稳定 call_id，Host 侧会再规范化。
            stable_id = f"gemini_{len(emitted_calls)}"
            yield ToolCallStarted(call_id=stable_id, name=name)
            yield ToolCallCompleted(call_id=stable_id, name=name, arguments=args)


def _read_finish_reason(chunk: Any) -> str:
    candidates = getattr(chunk, "candidates", None)
    if candidates is None and isinstance(chunk, dict):
        candidates = chunk.get("candidates")
    if not candidates:
        return ""
    first = candidates[0]
    reason = getattr(first, "finish_reason", None) or (
        first.get("finish_reason") if isinstance(first, dict) else None
    )
    if reason is None:
        reason = getattr(first, "finishReason", None) or (
            first.get("finishReason") if isinstance(first, dict) else None
        )
    return str(reason) if reason else ""


def _format_gemini_error(exc: Exception) -> str:
    message = str(exc).strip()
    lowered = message.lower()
    if "api key" in lowered or "401" in lowered or "unauthenticated" in lowered:
        return "Gemini 鉴权失败。请检查 GEMINI_API_KEY。"
    if "permission" in lowered or "403" in lowered:
        return "当前 API Key 没有访问该 Gemini 模型的权限。"
    if "not found" in lowered or "404" in lowered:
        return "Gemini 模型不存在或路径错误。"
    if "resource exhausted" in lowered or "429" in lowered or "quota" in lowered:
        return "Gemini 服务限流或额度不足，请稍后重试。"
    if "timeout" in lowered:
        return "Gemini 请求超时，请稍后重试。"
    return f"Gemini 请求失败：{type(exc).__name__}"
