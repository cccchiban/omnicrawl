"""统一模型错误分类与用户可读提示。"""

from __future__ import annotations

import re
from dataclasses import dataclass
from enum import Enum
from typing import Any, Mapping


_HTML_ERROR_RE = re.compile(
    r"<!doctype\s+html|<html\b|<head\b|<body\b|<h1\b|</html>",
    re.IGNORECASE,
)
_HTTP_STATUS_RE = re.compile(r"\b([45]\d{2})\b")


class ModelErrorCode(str, Enum):
    AUTHENTICATION_FAILED = "AUTHENTICATION_FAILED"
    PERMISSION_DENIED = "PERMISSION_DENIED"
    MODEL_NOT_FOUND = "MODEL_NOT_FOUND"
    RATE_LIMITED = "RATE_LIMITED"
    REQUEST_TIMEOUT = "REQUEST_TIMEOUT"
    CONNECTION_FAILED = "CONNECTION_FAILED"
    SERVICE_UNAVAILABLE = "SERVICE_UNAVAILABLE"
    INVALID_REQUEST = "INVALID_REQUEST"
    UNSUPPORTED_CAPABILITY = "UNSUPPORTED_CAPABILITY"
    STREAM_INTERRUPTED = "STREAM_INTERRUPTED"
    MODEL_DISCOVERY_FAILED = "MODEL_DISCOVERY_FAILED"
    CONFIGURATION_ERROR = "CONFIGURATION_ERROR"
    EMPTY_RESPONSE = "EMPTY_RESPONSE"
    CANCELLED = "CANCELLED"
    UNKNOWN = "UNKNOWN"


@dataclass
class ModelError(RuntimeError):
    """Provider 无关的模型运行时错误。"""

    code: ModelErrorCode
    message: str
    retryable: bool = False
    status_code: int | None = None
    provider: str = ""
    protocol: str = ""

    def __str__(self) -> str:  # pragma: no cover - dataclass 默认 repr 不适合 UI
        return self.message


def map_openai_exception(
    exc: Exception,
    *,
    known_models: tuple[str, ...] = (),
) -> ModelError:
    """把 OpenAI SDK / 网关异常映射为统一错误。"""

    message = str(exc).strip()
    # OpenAI SDK / 兼容网关有时只在异常 response JSON 中放业务错误码，
    # 而 str(exc) 仅包含通用的 "422 Unprocessable Entity"。结构化内容只用于
    # 内部分流，绝不拼回公开提示或持久化记录，避免泄露网关原始响应。
    detail_text = _extract_structured_error_text(exc)
    lowered = "\n".join(part for part in (message, detail_text) if part).lower()
    status_code = _extract_http_status_code(exc, message)

    if "model not found" in lowered or "invalid_model" in lowered:
        models = "、".join(known_models) if known_models else "请检查模型名与账号权限"
        return ModelError(
            code=ModelErrorCode.MODEL_NOT_FOUND,
            message=(
                "模型不存在或当前账号无权使用该模型。"
                f"可用模型示例：{models}。"
            ),
            status_code=status_code or 404,
            provider="openai",
        )

    if _looks_like_html_error(message):
        if status_code is not None:
            return _http_status_error(status_code, provider="openai")
        return ModelError(
            code=ModelErrorCode.SERVICE_UNAVAILABLE,
            message=(
                "模型服务返回了非 JSON 错误页面，可能是网关、反向代理或上游服务异常。"
                "请稍后重试，或检查 base_url 对应的服务状态。"
            ),
            retryable=True,
            provider="openai",
        )

    if status_code is not None:
        return _http_status_error(status_code, provider="openai")

    if _contains_any(lowered, ("peer closed connection", "incomplete chunked read")):
        return ModelError(
            code=ModelErrorCode.CONNECTION_FAILED,
            message="模型服务连接提前断开。系统会按重试策略重新请求；如果持续失败，请稍后重试或检查网关稳定性。",
            retryable=True,
            provider="openai",
        )

    if _contains_any(
        lowered,
        (
            "remote protocol error",
            "server disconnected",
            "connection reset",
            "connection aborted",
            "broken pipe",
        ),
    ):
        return ModelError(
            code=ModelErrorCode.CONNECTION_FAILED,
            message="模型服务连接被中途断开。请稍后重试；如果频繁出现，请检查网络或模型网关稳定性。",
            retryable=True,
            provider="openai",
        )

    if _contains_any(lowered, ("timeout", "timed out", "readtimeout", "connecttimeout")):
        return ModelError(
            code=ModelErrorCode.REQUEST_TIMEOUT,
            message="模型服务请求超时。请稍后重试，或适当调大 AGENT_REQUEST_TIMEOUT_SECONDS。",
            retryable=True,
            provider="openai",
        )

    if _contains_any(
        lowered,
        ("rate limit", "too many requests", "insufficient_quota", "quota", "429"),
    ):
        return ModelError(
            code=ModelErrorCode.RATE_LIMITED,
            message="模型服务限流或额度不足。请稍后重试，或检查账号额度和并发限制。",
            retryable=True,
            status_code=429,
            provider="openai",
        )

    if _contains_any(lowered, ("invalid_api_key", "authentication", "unauthorized", "401")):
        return ModelError(
            code=ModelErrorCode.AUTHENTICATION_FAILED,
            message="模型服务鉴权失败。请检查 API Key 是否正确、是否过期，以及当前网关是否接受该 Key。",
            status_code=401,
            provider="openai",
        )

    if _contains_any(lowered, ("permission", "forbidden", "403")):
        return ModelError(
            code=ModelErrorCode.PERMISSION_DENIED,
            message="当前 API Key 没有访问该模型或接口的权限。请检查模型权限、账号权限或网关配置。",
            status_code=403,
            provider="openai",
        )

    if _contains_any(
        lowered,
        (
            "connection",
            "connecterror",
            "dns",
            "name resolution",
            "temporary failure",
            "failed to resolve",
            "nodename",
        ),
    ):
        return ModelError(
            code=ModelErrorCode.CONNECTION_FAILED,
            message="无法连接模型服务。请检查网络、代理配置和 base_url 是否可达。",
            retryable=True,
            provider="openai",
        )

    if _contains_any(lowered, ("ssl", "certificate", "tls")):
        return ModelError(
            code=ModelErrorCode.CONNECTION_FAILED,
            message="模型服务 TLS/证书校验失败。请检查网关证书、代理或本机证书配置。",
            retryable=True,
            provider="openai",
        )

    return ModelError(
        code=ModelErrorCode.UNKNOWN,
        message=(
            "模型请求失败，但未能识别具体原因。"
            f"请检查网络、模型服务地址和本地配置。错误类型：{type(exc).__name__}。"
        ),
        provider="openai",
    )


def is_retryable_model_error(error: ModelError | Exception) -> bool:
    if isinstance(error, ModelError):
        return error.retryable
    mapped = map_openai_exception(error if isinstance(error, Exception) else Exception(str(error)))
    return mapped.retryable


def _http_status_error(status_code: int, *, provider: str) -> ModelError:
    if status_code == 400:
        return ModelError(
            code=ModelErrorCode.INVALID_REQUEST,
            message="模型服务拒绝了请求参数（HTTP 400）。请检查模型名称、思考配置、消息格式或网关兼容性。",
            status_code=status_code,
            provider=provider,
        )
    if status_code == 401:
        return ModelError(
            code=ModelErrorCode.AUTHENTICATION_FAILED,
            message="模型服务鉴权失败（HTTP 401）。请检查 API Key 是否正确、是否过期，以及当前网关是否接受该 Key。",
            status_code=status_code,
            provider=provider,
        )
    if status_code == 403:
        return ModelError(
            code=ModelErrorCode.PERMISSION_DENIED,
            message="当前 API Key 没有访问该模型或接口的权限（HTTP 403）。请检查模型权限、账号权限或网关配置。",
            status_code=status_code,
            provider=provider,
        )
    if status_code == 404:
        return ModelError(
            code=ModelErrorCode.MODEL_NOT_FOUND,
            message="模型或接口地址不存在（HTTP 404）。请检查 base_url 和 model。",
            status_code=status_code,
            provider=provider,
        )
    if status_code == 408:
        return ModelError(
            code=ModelErrorCode.REQUEST_TIMEOUT,
            message="模型服务请求超时（HTTP 408）。请稍后重试，或适当调大 AGENT_REQUEST_TIMEOUT_SECONDS。",
            retryable=True,
            status_code=status_code,
            provider=provider,
        )
    if status_code == 409:
        return ModelError(
            code=ModelErrorCode.SERVICE_UNAVAILABLE,
            message="模型服务暂时无法处理该请求（HTTP 409）。请稍后重试。",
            retryable=True,
            status_code=status_code,
            provider=provider,
        )
    if status_code == 422:
        return ModelError(
            code=ModelErrorCode.INVALID_REQUEST,
            message="模型服务无法处理当前请求内容（HTTP 422）。请检查模型参数、消息格式或网关兼容性。",
            status_code=status_code,
            provider=provider,
        )
    if status_code == 429:
        return ModelError(
            code=ModelErrorCode.RATE_LIMITED,
            message="模型服务限流或额度不足（HTTP 429）。请稍后重试，或检查账号额度和并发限制。",
            retryable=True,
            status_code=status_code,
            provider=provider,
        )
    if status_code in {500, 502, 503, 504} or 500 <= status_code <= 599:
        return ModelError(
            code=ModelErrorCode.SERVICE_UNAVAILABLE,
            message=f"模型服务网关暂时不可用（HTTP {status_code}）。请稍后重试；如果持续出现，请检查网关或上游模型服务状态。",
            retryable=True,
            status_code=status_code,
            provider=provider,
        )
    return ModelError(
        code=ModelErrorCode.UNKNOWN,
        message=f"模型服务返回错误状态（HTTP {status_code}）。请检查模型配置、网络和网关状态。",
        status_code=status_code,
        provider=provider,
    )


def _extract_structured_error_text(exc: Exception) -> str:
    """从 SDK 已解析的错误 body 中提取有限分类线索，不向调用方暴露原文。"""

    payloads: list[Any] = [getattr(exc, "body", None)]
    response = getattr(exc, "response", None)
    if response is not None:
        try:
            payloads.append(response.json())
        except Exception:
            pass

    fragments: list[str] = []
    for payload in payloads:
        _collect_error_text_fragments(payload, fragments, depth=0)
    return "\n".join(fragments)


def _collect_error_text_fragments(value: Any, fragments: list[str], *, depth: int) -> None:
    """只保留标准错误字段，避免把响应的任意内容用于分类或日志。"""

    if depth > 4 or len(fragments) >= 12:
        return
    if isinstance(value, str):
        fragments.append(value[:512])
        return
    if not isinstance(value, Mapping):
        return
    for key in ("message", "type", "code", "error", "detail"):
        item = value.get(key)
        if isinstance(item, str):
            fragments.append(item[:512])
        elif isinstance(item, Mapping):
            _collect_error_text_fragments(item, fragments, depth=depth + 1)
        elif isinstance(item, list):
            for nested in item[:4]:
                _collect_error_text_fragments(nested, fragments, depth=depth + 1)


def _extract_http_status_code(exc: Exception, message: str) -> int | None:
    for value in (
        getattr(exc, "status_code", None),
        getattr(getattr(exc, "response", None), "status_code", None),
    ):
        if isinstance(value, int) and 400 <= value <= 599:
            return value
    match = _HTTP_STATUS_RE.search(message)
    if match is None:
        return None
    return int(match.group(1))


def _looks_like_html_error(message: str) -> bool:
    return bool(_HTML_ERROR_RE.search(message))


def _contains_any(text: str, needles: tuple[str, ...]) -> bool:
    return any(needle in text for needle in needles)


def read_exception_status(exc: Exception) -> int | None:
    value = getattr(exc, "status_code", None)
    if isinstance(value, int):
        return value
    response = getattr(exc, "response", None)
    status = getattr(response, "status_code", None)
    return status if isinstance(status, int) else None


def as_user_message(error: Any) -> str:
    if isinstance(error, ModelError):
        return error.message
    if isinstance(error, Exception):
        return str(error)
    return str(error)
