"""OpenAI Responses API 网络客户端与响应解析。

配置读写位于 `config.llm`；本模块只负责 SDK 请求、流式解析、usage 提取和错误归一化。
"""

from __future__ import annotations

import re
from typing import Any, Callable, Iterable

from .llm import (
    KNOWN_AVAILABLE_MODELS,
    LLMConfig,
    LLMError,
    load_llm_config,
)


_HTML_ERROR_RE = re.compile(
    r"<!doctype\s+html|<html\b|<head\b|<body\b|<h1\b|</html>",
    re.IGNORECASE,
)
_HTTP_STATUS_RE = re.compile(r"\b([45]\d{2})\b")


class OpenAIResponseLLM:
    """使用 OpenAI Python SDK 调用 Responses API，并维护简短对话上下文。"""

    def __init__(self, config: LLMConfig | None = None) -> None:
        self.config = config or load_llm_config()
        if not self.config.api_key.strip():
            raise LLMError("缺少 API Key，请在 config.toml 的 llm 配置 中配置，或设置 OPENAI_API_KEY。")

        try:
            import httpx
            from openai import OpenAI
        except ImportError as exc:
            raise LLMError(
                "缺少 openai/httpx 依赖，请先执行：pip install -r requirements.txt"
            ) from exc

        http_client = httpx.Client(trust_env=False, follow_redirects=True)
        openai_kwargs: dict[str, Any] = {
            "api_key": self.config.api_key,
            "base_url": self.config.base_url,
            "http_client": http_client,
        }
        user_agent = getattr(self.config, "user_agent", "").strip()
        if user_agent:
            openai_kwargs["default_headers"] = {"User-Agent": user_agent}
        try:
            self._client = OpenAI(**openai_kwargs)
        except Exception:
            http_client.close()
            raise
        self._history: list[dict[str, str]] = []

    def ask(self, user_text: str) -> str:
        """发送用户文字到 Responses API，返回完整 AI 回复。"""

        user_text = user_text.strip()
        if not user_text:
            raise LLMError("用户输入为空，无法发送给 AI。")

        input_messages = self._build_input_messages(user_text)
        try:
            response = self._client.responses.create(
                model=self.config.model,
                instructions=self.config.system_prompt,
                input=input_messages,
                extra_body=self._build_extra_body(),
            )
        except Exception as exc:  # SDK 会根据网络、鉴权、服务端错误抛出不同异常。
            raise LLMError(f"AI 接口请求失败：{self.format_request_error(exc)}") from exc

        reply = self._extract_text(response)
        if not reply:
            raise LLMError("AI 返回内容为空或不是可解析的 OpenAI Responses 格式。")

        reasoning = self._extract_reasoning(response) if self.config.thinking_enabled else ""
        self._append_history(user_text, reply, reasoning)
        self._trim_history()
        return reply

    def ask_stream(self, user_text: str, on_delta: Callable[[str], None]) -> str:
        """流式发送用户文字，边收到边回调输出，并返回完整 AI 回复。

        on_delta 只负责显示增量文字；方法内部仍会拼接完整回复并写入历史，
        这样后续语音播报和多轮对话上下文不会丢失。
        """

        user_text = user_text.strip()
        if not user_text:
            raise LLMError("用户输入为空，无法发送给 AI。")

        input_messages = self._build_input_messages(user_text)
        try:
            stream = self._client.responses.create(
                model=self.config.model,
                instructions=self.config.system_prompt,
                input=input_messages,
                stream=True,
                extra_body=self._build_extra_body(),
            )
        except Exception as exc:  # SDK 会根据网络、鉴权、服务端错误抛出不同异常。
            raise LLMError(f"AI 接口请求失败：{self.format_request_error(exc)}") from exc

        chunks: list[str] = []
        reasoning_chunks: list[str] = []
        from ..llm.stream_registry import registered_stream_events, stream_owner_for

        try:
            for event in registered_stream_events(
                stream,
                owner=stream_owner_for(),
            ):
                for delta in self.extract_stream_text(event):
                    chunks.append(delta)
                    on_delta(delta)
                if self.config.thinking_enabled:
                    for delta in self.extract_stream_reasoning(event):
                        reasoning_chunks.append(delta)
        except Exception as exc:
            raise LLMError(f"AI 流式回复中断：{self.format_request_error(exc)}") from exc

        reply = "".join(chunks).strip()
        if not reply:
            raise LLMError("AI 返回内容为空或不是可解析的 OpenAI Responses 流式格式。")

        reasoning = "".join(reasoning_chunks).strip()
        self._append_history(user_text, reply, reasoning)
        self._trim_history()
        return reply

    def _append_history(self, user_text: str, assistant_text: str, reasoning: str = "") -> None:
        """写入历史；思考模式下保存思维链，随历史原样回传给网关。"""

        assistant_msg: dict[str, str] = {"role": "assistant", "content": assistant_text}
        if reasoning:
            assistant_msg["reasoning_content"] = reasoning
        self._history.extend(
            [
                {"role": "user", "content": user_text},
                assistant_msg,
            ]
        )

    def _build_input_messages(self, user_text: str) -> list[dict[str, str]]:
        """组合历史消息和本轮用户输入，原样回传 reasoning_content。

        思考模式网关（如 Console Go）要求历史 assistant 消息必须回传
        reasoning_content，剔除会导致二次请求被拒（HTTP 400）。
        """

        return [*self._history, {"role": "user", "content": user_text}]

    def _build_extra_body(self) -> dict[str, Any]:
        """构造网关扩展参数；统一使用标准 Responses 思考参数 reasoning.effort。

        axo 网关实测忽略 ``thinking.type=disabled``（仍输出思考摘要）；标准参数
        ``reasoning.effort=none`` 是唯一可靠关闭思考的方式，其余档位同样生效。
        """

        effort = str(self.config.reasoning_effort or "").strip()
        if (
            not effort
            or effort in {"none", "disabled"}
            or not self.config.thinking_enabled
        ):
            effort = "none"
        return {"reasoning": {"effort": effort}}

    def _trim_history(self) -> None:
        """只保留最近若干轮，避免语音对话运行时间长后请求体过大。"""

        max_messages = self.config.max_history_turns * 2
        if len(self._history) > max_messages:
            self._history = self._history[-max_messages:]

    @staticmethod
    def _extract_text(response: Any, *, include_reasoning: bool = False) -> str:
        """兼容 SDK 对象与字典两种 Responses API 返回形态。

        ``include_reasoning=True`` 时，在常规输出文本为空的情况下，从
        reasoning item 的 summary 提取文本作为兜底。部分网关在思考模式下只
        返回 reasoning item（content 为 encrypted_content，summary 为明文摘要），
        常规提取会得到空串；默认 False 保持纯输出语义，避免 ask/ask_stream
        把思考内容当作正式回复写入历史。
        """

        output_text = getattr(response, "output_text", None)
        if isinstance(output_text, str) and output_text.strip():
            return output_text.strip()

        if hasattr(response, "model_dump"):
            data = response.model_dump()
        elif isinstance(response, dict):
            data = response
        else:
            data = {}

        texts: list[str] = []
        for item in data.get("output") or []:
            if not isinstance(item, dict):
                continue
            for content in item.get("content") or []:
                if not isinstance(content, dict):
                    continue
                text = content.get("text")
                if isinstance(text, str) and text.strip():
                    texts.append(text.strip())

        joined = "\n".join(texts).strip()
        if joined or not include_reasoning:
            return joined
        # 兜底：reasoning item 的 summary 明文摘要（encrypted_text 无法解密）。
        for item in data.get("output") or []:
            if not isinstance(item, dict) or item.get("type") != "reasoning":
                continue
            for summary in item.get("summary") or []:
                if not isinstance(summary, dict):
                    continue
                text = summary.get("summary_text") or summary.get("text")
                if isinstance(text, str) and text.strip():
                    return text.strip()
            for content in item.get("content") or []:
                if not isinstance(content, dict):
                    continue
                text = content.get("text")
                if isinstance(text, str) and text.strip():
                    return text.strip()
        return ""

    @staticmethod
    def _has_reasoning_output(response: Any) -> bool:
        """判断响应是否只含 reasoning item（无任何可见输出文本）。

        供审查路径诊断“思考模式未返回文本”场景：区分真·空响应与
        思考-only 响应，让拒绝原因可操作。
        """

        if hasattr(response, "model_dump"):
            data = response.model_dump()
        elif isinstance(response, dict):
            data = response
        else:
            data = {}
        return any(
            isinstance(item, dict) and item.get("type") == "reasoning"
            for item in data.get("output") or []
        )

    @staticmethod
    def extract_stream_text(event: Any) -> Iterable[str]:
        """从 Responses API 流式事件中提取文本增量。

        只接收 response.output_text.delta，避免把 reasoning_summary 等非最终回答事件
        显示到命令行，或拼接进后续语音播报文本。
        """

        event_type = getattr(event, "type", None)
        delta = getattr(event, "delta", None)
        if event_type == "response.output_text.delta" and isinstance(delta, str):
            yield delta
            return

        if hasattr(event, "model_dump"):
            data = event.model_dump()
        elif isinstance(event, dict):
            data = event
        else:
            data = {}

        if data.get("type") == "response.output_text.delta":
            text = data.get("delta")
            if isinstance(text, str) and text:
                yield text

    @staticmethod
    def extract_stream_reasoning(event: Any) -> Iterable[str]:
        """从 Responses API 流式事件中提取思维链文本增量。"""

        event_type = getattr(event, "type", None)
        delta = getattr(event, "delta", None)
        if event_type == "response.reasoning_text.delta" and isinstance(delta, str):
            yield delta
            return

        # Dict / model_dump 格式兼容
        if hasattr(event, "model_dump"):
            data = event.model_dump()
        elif isinstance(event, dict):
            data = event
        else:
            data = {}

        if data.get("type") == "response.reasoning_text.delta":
            text = data.get("delta")
            if isinstance(text, str) and text:
                yield text

    @staticmethod
    def extract_token_usage(payload: Any) -> tuple[int, int, int] | None:
        """从 Responses 响应或流式事件中提取输入/输出 token 数。

        不同网关会把 usage 放在事件自身、`response` 字段或 SDK 对象属性上；
        这里只做结构兼容，不假设某一种固定返回形态。返回值为
        (input_tokens, output_tokens, cached_input_tokens)。找不到 usage 时返回 None，
        由 UI 保留最近一次已知统计。
        """

        usage = _find_usage_payload(payload)
        if usage is None:
            return None

        input_tokens = _read_usage_int(usage, ("input_tokens", "prompt_tokens"))
        if input_tokens is None:
            input_tokens = _read_deepseek_input_tokens(usage)
        output_tokens = _read_usage_int(usage, ("output_tokens", "completion_tokens"))
        if input_tokens is None and output_tokens is None:
            return None
        cached_input_tokens = _read_cached_input_tokens(usage)
        return input_tokens or 0, output_tokens or 0, cached_input_tokens or 0

    @staticmethod
    def _extract_reasoning(response: Any) -> str:
        """从非流式 Responses API 响应中提取思维链文本。"""

        if hasattr(response, "output"):
            for item in response.output:
                if getattr(item, "type", None) == "reasoning":
                    content = getattr(item, "content", None)
                    if isinstance(content, str):
                        return content.strip()
                    if isinstance(content, list):
                        return "".join(
                            getattr(c, "text", "") if hasattr(c, "text") else str(c)
                            for c in content
                        ).strip()

        if hasattr(response, "model_dump"):
            data = response.model_dump()
        elif isinstance(response, dict):
            data = response
        else:
            return ""

        for item in data.get("output", []):
            if item.get("type") == "reasoning":
                content = item.get("content")
                if isinstance(content, str):
                    return content.strip()
                if isinstance(content, list):
                    return "".join(
                        c.get("text", "") if isinstance(c, dict) else str(c)
                        for c in content
                    ).strip()

        return ""

    @staticmethod
    def format_request_error(exc: Exception) -> str:
        """把 SDK、网关和网络异常归一化为用户可读提示。

        模型网关失败时常把 HTML 错误页、JSON 原始响应体或底层网络异常直接塞进
        exception message。这里不回显原文，只保留可操作的错误类别、HTTP 状态码
        和下一步建议，避免终端暴露大段响应正文或服务端实现细节。
        """

        message = str(exc).strip()
        lowered = message.lower()
        status_code = _extract_http_status_code(exc, message)

        if "model not found" in lowered or "invalid_model" in lowered:
            models = "、".join(KNOWN_AVAILABLE_MODELS)
            return (
                "模型不存在或当前账号无权使用该模型。"
                f"当前网关可用模型示例：{models}。"
                "可在 config.toml 中配置模型，或设置 OPENAI_MODEL 切换。"
            )

        if _looks_like_html_error(message):
            if status_code is not None:
                return _format_http_status_error(status_code)
            return (
                "模型服务返回了非 JSON 错误页面，可能是网关、反向代理或上游服务异常。"
                "请稍后重试，或检查 OPENAI_BASE_URL 对应的服务状态。"
            )

        if status_code is not None:
            return _format_http_status_error(status_code)

        if _contains_any(lowered, ("peer closed connection", "incomplete chunked read")):
            return "模型服务连接提前断开。系统会按重试策略重新请求；如果持续失败，请稍后重试或检查网关稳定性。"

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
            return "模型服务连接被中途断开。请稍后重试；如果频繁出现，请检查网络或模型网关稳定性。"

        if _contains_any(lowered, ("timeout", "timed out", "readtimeout", "connecttimeout")):
            return "模型服务请求超时。请稍后重试，或适当调大 AGENT_REQUEST_TIMEOUT_SECONDS。"

        if _contains_any(
            lowered,
            (
                "rate limit",
                "too many requests",
                "insufficient_quota",
                "quota",
                "429",
            ),
        ):
            return "模型服务限流或额度不足。请稍后重试，或检查账号额度和并发限制。"

        if _contains_any(lowered, ("invalid_api_key", "authentication", "unauthorized", "401")):
            return "模型服务鉴权失败。请检查 API Key 是否正确、是否过期，以及当前网关是否接受该 Key。"

        if _contains_any(lowered, ("permission", "forbidden", "403")):
            return "当前 API Key 没有访问该模型或接口的权限。请检查模型权限、账号权限或网关配置。"

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
            return "无法连接模型服务。请检查网络、代理配置和 OPENAI_BASE_URL 是否可达。"

        if _contains_any(lowered, ("ssl", "certificate", "tls")):
            return "模型服务 TLS/证书校验失败。请检查网关证书、代理或本机证书配置。"

        return f"模型请求失败，但未能识别具体原因。请检查网络、模型服务地址和本地配置。错误类型：{type(exc).__name__}。"


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


def _format_http_status_error(status_code: int) -> str:
    if status_code == 400:
        return "模型服务拒绝了请求参数（HTTP 400）。请检查模型名称、思考配置、消息格式或网关兼容性。"
    if status_code == 401:
        return "模型服务鉴权失败（HTTP 401）。请检查 API Key 是否正确、是否过期，以及当前网关是否接受该 Key。"
    if status_code == 403:
        return "当前 API Key 没有访问该模型或接口的权限（HTTP 403）。请检查模型权限、账号权限或网关配置。"
    if status_code == 404:
        models = "、".join(KNOWN_AVAILABLE_MODELS)
        return f"模型或接口地址不存在（HTTP 404）。请检查 OPENAI_BASE_URL 和 llm.model。当前网关可用模型示例：{models}。"
    if status_code == 408:
        return "模型服务请求超时（HTTP 408）。请稍后重试，或适当调大 AGENT_REQUEST_TIMEOUT_SECONDS。"
    if status_code == 409:
        return "模型服务暂时无法处理该请求（HTTP 409）。请稍后重试。"
    if status_code == 422:
        return "模型服务无法处理当前请求内容（HTTP 422）。请检查模型参数、消息格式或网关兼容性。"
    if status_code == 429:
        return "模型服务限流或额度不足（HTTP 429）。请稍后重试，或检查账号额度和并发限制。"
    if status_code in {500, 502, 503, 504}:
        return f"模型服务网关暂时不可用（HTTP {status_code}）。请稍后重试；如果持续出现，请检查网关或上游模型服务状态。"
    if 500 <= status_code <= 599:
        return f"模型服务端异常（HTTP {status_code}）。请稍后重试；如果持续出现，请检查网关或上游模型服务状态。"
    return f"模型服务返回错误状态（HTTP {status_code}）。请检查模型配置、网络和网关状态。"


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


__all__ = ["OpenAIResponseLLM"]
