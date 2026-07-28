"""OpenAI SDK 公共 Client 工厂、错误映射与工具转换。"""

from __future__ import annotations

import hashlib
import json
import re
from typing import Any, Mapping

from ..errors import ModelError, ModelErrorCode, map_openai_exception
from ..protocol import ToolSpec
from ..registry import ProviderProfile


KNOWN_AVAILABLE_MODELS = (
    "qwen3.6-plus",
    "glm-5.1",
    "deepseek-v4-flash",
    "gpt-5.2",
    "gpt-5.4-mini",
)

# Host 权威字段：禁止 provider_options 覆盖。
_FORBIDDEN_PROVIDER_OPTION_KEYS = frozenset(
    {
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
    }
)

# OpenAI Profile 白名单扩展参数。
_OPENAI_PROVIDER_OPTION_ALLOWLIST = frozenset(
    {
        "thinking",
        "reasoning_effort",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "user",
        "seed",
        "response_format",
        "metadata",
        "store",
        "service_tier",
    }
)


def create_openai_client(profile: ProviderProfile) -> Any:
    api_key = resolve_api_key(profile)
    if not api_key:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message=(
                f"Profile {profile.id} 缺少 API Key。"
                "请配置 api_key_env 环境变量或 profile.api_key。"
            ),
        )
    try:
        import httpx
        from openai import OpenAI
    except ImportError as exc:
        raise ModelError(
            code=ModelErrorCode.CONFIGURATION_ERROR,
            message="缺少 openai/httpx 依赖，请先执行：pip install -r requirements.txt",
        ) from exc

    kwargs: dict[str, Any] = {"api_key": api_key}
    base_url = profile.base_url.strip()
    if base_url:
        kwargs["base_url"] = base_url
    # OpenAI SDK 默认 trust_env=True，会在 Windows 上读取系统代理注册表。
    # 本地代理常把 HTTPS 代理地址声明为 https://127.0.0.1:port，但实际只
    # 支持明文 HTTP CONNECT，HTTPX 随后会在代理握手阶段抛出 SSLEOFError。
    # OmniCrawl 当前没有 Provider 代理配置，因此默认直连 Provider；需要代理
    # 时应在 Provider 层显式增加受控配置，而不是隐式继承系统代理。
    http_client = httpx.Client(trust_env=False, follow_redirects=True)
    headers = user_agent_headers(profile)
    if headers:
        kwargs["default_headers"] = headers
    try:
        return OpenAI(**kwargs, http_client=http_client)
    except Exception:
        http_client.close()
        raise


def user_agent_headers(profile: ProviderProfile) -> dict[str, str]:
    user_agent = getattr(profile, "user_agent", "").strip()
    return {"User-Agent": user_agent} if user_agent else {}


def resolve_api_key(profile: ProviderProfile) -> str:
    import os

    if profile.api_key_env:
        env_value = os.getenv(profile.api_key_env, "").strip()
        if env_value:
            return env_value
    return (profile.api_key or "").strip()


def format_openai_error(exc: Exception) -> str:
    return map_openai_exception(exc, known_models=KNOWN_AVAILABLE_MODELS).message


def raise_openai_error(exc: Exception) -> ModelError:
    error = map_openai_exception(exc, known_models=KNOWN_AVAILABLE_MODELS)
    raise error from exc


def sanitize_provider_options(options: Mapping[str, Any] | None) -> dict[str, Any]:
    if not options:
        return {}
    result: dict[str, Any] = {}
    for key, value in options.items():
        if not isinstance(key, str):
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message="provider_options 的键必须是字符串。",
            )
        if key in _FORBIDDEN_PROVIDER_OPTION_KEYS:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message=f"provider_options 不允许覆盖 Host 字段：{key}",
            )
        if key not in _OPENAI_PROVIDER_OPTION_ALLOWLIST:
            raise ModelError(
                code=ModelErrorCode.CONFIGURATION_ERROR,
                message=f"OpenAI provider_options 不支持字段：{key}",
            )
        result[key] = value
    return result


def tool_specs_to_openai_functions(tools: tuple[ToolSpec, ...]) -> list[dict[str, Any]]:
    return [
        {
            "type": "function",
            "function": {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters or {"type": "object", "properties": {}},
            },
        }
        for tool in tools
    ]


def is_openai_gpt_model(model: str) -> bool:
    return model.startswith("gpt-") or model.startswith("chatgpt-") or bool(re.match(r"^o\d", model))


def is_unsupported_prompt_cache_error(exc: Exception) -> bool:
    message = str(exc).lower()
    return "prompt_cache_key" in message and any(
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


def is_retryable_model_request_error(exc: Exception) -> bool:
    status_code = getattr(exc, "status_code", None)
    if isinstance(status_code, int) and status_code in {408, 409, 429, 500, 502, 503, 504}:
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
            "rate limit",
            "too many requests",
        )
    )


def build_prompt_cache_key(identity: Mapping[str, str], *, model: str) -> str:
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


def extract_chat_stream_delta(event: Any) -> Any | None:
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
