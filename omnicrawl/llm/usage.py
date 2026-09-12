"""Token usage 归一化。"""

from __future__ import annotations

from typing import Any

from .protocol import TokenUsage


def usage_from_openai_payload(payload: Any) -> TokenUsage | None:
    """从 OpenAI Responses / Chat Completions 响应或流事件中提取 usage。"""

    usage = _find_usage_payload(payload)
    if usage is None:
        return None

    input_tokens = _read_usage_int(usage, ("input_tokens", "prompt_tokens"))
    if input_tokens is None:
        input_tokens = _read_deepseek_input_tokens(usage)
    output_tokens = _read_usage_int(usage, ("output_tokens", "completion_tokens"))
    if input_tokens is None and output_tokens is None:
        return None
    cached_input_tokens = _read_cached_input_tokens(usage) or 0
    reasoning_tokens = _read_usage_int(usage, ("reasoning_tokens", "output_reasoning_tokens")) or 0
    if reasoning_tokens == 0:
        # Responses API 用 output_tokens_details，Chat Completions 用
        # completion_tokens_details；两种协议都要认，否则 Chat Completions
        # 路径上的 reasoning_tokens 永远是 0。
        details: Any = None
        for attribute in ("output_tokens_details", "completion_tokens_details"):
            details = getattr(usage, attribute, None)
            if details is not None:
                break
        if details is None:
            mapping = _to_mapping(usage)
            if isinstance(mapping, dict):
                details = mapping.get("output_tokens_details") or mapping.get(
                    "completion_tokens_details"
                )
        reasoning_tokens = _read_usage_int(details, ("reasoning_tokens",)) or 0
    return TokenUsage(
        input_tokens=input_tokens or 0,
        output_tokens=output_tokens or 0,
        cached_input_tokens=cached_input_tokens,
        reasoning_tokens=reasoning_tokens,
    )


def usage_from_anthropic_payload(payload: Any) -> TokenUsage | None:
    usage = getattr(payload, "usage", None)
    if usage is None and isinstance(payload, dict):
        usage = payload.get("usage")
    if usage is None:
        return None
    input_tokens = _read_usage_int(usage, ("input_tokens",)) or 0
    output_tokens = _read_usage_int(usage, ("output_tokens",)) or 0
    cached = _read_usage_int(usage, ("cache_read_input_tokens", "cached_input_tokens")) or 0
    return TokenUsage(
        input_tokens=input_tokens,
        output_tokens=output_tokens,
        cached_input_tokens=cached,
    )


def usage_from_gemini_payload(payload: Any) -> TokenUsage | None:
    meta = getattr(payload, "usage_metadata", None)
    if meta is None:
        meta = getattr(payload, "usageMetadata", None)
    data = _to_mapping(payload)
    if meta is None and isinstance(data, dict):
        meta = data.get("usage_metadata") or data.get("usageMetadata")
    if meta is None:
        return None
    input_tokens = _read_usage_int(
        meta,
        ("prompt_token_count", "promptTokenCount", "input_tokens"),
    ) or 0
    output_tokens = _read_usage_int(
        meta,
        ("candidates_token_count", "candidatesTokenCount", "output_tokens"),
    ) or 0
    total = _read_usage_int(meta, ("total_token_count", "totalTokenCount"))
    if input_tokens == 0 and output_tokens == 0 and total:
        return TokenUsage(input_tokens=total, output_tokens=0)
    return TokenUsage(input_tokens=input_tokens, output_tokens=output_tokens)


def _to_mapping(value: Any) -> dict[str, Any]:
    if hasattr(value, "model_dump"):
        data = value.model_dump()
        return data if isinstance(data, dict) else {}
    return value if isinstance(value, dict) else {}


def _find_usage_payload(value: Any) -> Any | None:
    usage = getattr(value, "usage", None)
    if usage is not None:
        return usage

    response = getattr(value, "response", None)
    response_usage = getattr(response, "usage", None) if response is not None else None
    if response_usage is not None:
        return response_usage

    data = _to_mapping(value)
    usage = data.get("usage")
    if usage is not None:
        return usage

    response_data = data.get("response")
    if isinstance(response_data, dict):
        return response_data.get("usage")
    return None


def _read_usage_int(usage: Any, keys: tuple[str, ...]) -> int | None:
    if usage is None:
        return None
    for key in keys:
        value = getattr(usage, key, None)
        if isinstance(value, int) and not isinstance(value, bool):
            return value
    data = _to_mapping(usage)
    for key in keys:
        value = data.get(key)
        if isinstance(value, int) and not isinstance(value, bool):
            return value
    return None


def _read_deepseek_input_tokens(usage: Any) -> int | None:
    hit_tokens = _read_usage_int(usage, ("prompt_cache_hit_tokens",))
    miss_tokens = _read_usage_int(usage, ("prompt_cache_miss_tokens",))
    if hit_tokens is None or miss_tokens is None:
        return None
    return hit_tokens + miss_tokens


def _read_cached_input_tokens(usage: Any) -> int | None:
    for key in (
        "cached_tokens",
        "cached_input_tokens",
        "input_cached_tokens",
        "prompt_cache_hit_tokens",
    ):
        value = getattr(usage, key, None)
        if isinstance(value, int) and not isinstance(value, bool):
            return value

    for details_key in ("input_tokens_details", "prompt_tokens_details"):
        details = getattr(usage, details_key, None)
        value = _read_usage_int(details, ("cached_tokens",)) if details is not None else None
        if value is not None:
            return value

    data = _to_mapping(usage)
    for key in (
        "cached_tokens",
        "cached_input_tokens",
        "input_cached_tokens",
        "prompt_cache_hit_tokens",
    ):
        value = data.get(key)
        if isinstance(value, int) and not isinstance(value, bool):
            return value

    for details_key in ("input_tokens_details", "prompt_tokens_details"):
        details = data.get(details_key)
        value = _read_usage_int(details, ("cached_tokens",)) if details is not None else None
        if value is not None:
            return value
    return None
