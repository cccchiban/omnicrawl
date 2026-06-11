from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable

from .runtime_config import RuntimeConfigError, get_section, load_config_data


DEFAULT_THINKING_TYPE = "disabled"
DEFAULT_REASONING_EFFORT = ""
KNOWN_AVAILABLE_MODELS = (
    "qwen3.6-plus",
    "glm-5.1",
    "deepseek-v4-flash",
    "gpt-5.2",
    "gpt-5.4-mini",
)
VALID_REASONING_EFFORTS = {"none", "low", "medium", "high", "xhigh", "max"}
_HTML_ERROR_RE = re.compile(
    r"<!doctype\s+html|<html\b|<head\b|<body\b|<h1\b|</html>",
    re.IGNORECASE,
)
_HTTP_STATUS_RE = re.compile(r"\b([45]\d{2})\b")
_REASONING_EFFORT_ALIASES = {
    "": DEFAULT_REASONING_EFFORT,
    "disabled": "disabled",
    "off": "disabled",
    "none": "none",
    "low": "low",
    "medium": "medium",
    "med": "medium",
    "high": "high",
    "xhigh": "xhigh",
    "x_high": "xhigh",
    "extra_high": "xhigh",
    "very_high": "xhigh",
    "max": "max",
    "maximum": "max",
}


class LLMError(RuntimeError):
    """大模型请求或响应解析失败时抛出。"""


@dataclass
class LLMConfig:
    """OpenAI Responses API 兼容接口配置。

    API Key、接口地址和模型从本地 `config.json` 或环境变量读取，
    避免把运行配置写入源码。
    """

    api_key: str = field(default_factory=lambda: os.getenv("OPENAI_API_KEY", ""))
    base_url: str = field(default_factory=lambda: os.getenv("OPENAI_BASE_URL", ""))
    model: str = field(default_factory=lambda: os.getenv("OPENAI_MODEL", ""))
    thinking_type: str = field(
        default_factory=lambda: os.getenv("OPENAI_THINKING_TYPE", DEFAULT_THINKING_TYPE)
    )
    reasoning_effort: str = field(
        default_factory=lambda: os.getenv("REASONING_EFFORT", DEFAULT_REASONING_EFFORT)
    )
    system_prompt: str = (
        "你是一个通过语音和用户对话的中文 AI 助手。"
        "回答要自然、简洁、适合被朗读；遇到不确定内容要明确说明。"
    )
    max_history_turns: int = 8

    def __post_init__(self) -> None:
        self.api_key = self.api_key.strip()
        self.base_url = self.base_url.strip()
        self.model = self.model.strip()
        self.thinking_type = (self.thinking_type.strip() or DEFAULT_THINKING_TYPE).lower()
        self.reasoning_effort = normalize_reasoning_effort(self.reasoning_effort)
        _require_non_empty("api_key", self.api_key, "OPENAI_API_KEY")
        _require_non_empty("base_url", self.base_url, "OPENAI_BASE_URL")
        _require_non_empty("model", self.model, "OPENAI_MODEL")

    @property
    def thinking_enabled(self) -> bool:
        """推理强度非空且不是 none/disabled 时启用思考模式。"""
        if self.reasoning_effort in {"none", "disabled"}:
            return False
        if self.reasoning_effort:
            return True
        return self.thinking_type not in {"", "disabled"}


def _require_non_empty(key: str, value: str, env_name: str) -> None:
    if not isinstance(value, str) or not value.strip():
        raise LLMError(
            f"缺少配置 llm.{key}，请在 config.json 中填写 llm.{key}，"
            f"或设置环境变量 {env_name}。"
        )


def normalize_reasoning_effort(value: Any) -> str:
    """把配置里的思考深度规范化为网关可识别的字符串。

    课程网关常见写法是 `low`、`medium`、`high`、`xhigh`、`max`；这里同时
    兼容大小写、连字符和下划线，避免用户在 config.json 或环境变量中写成
    `X-HIGH` / `x_high` 后静默丢失推理强度参数。
    """

    if not isinstance(value, str):
        raise LLMError("配置项 llm.reasoning_effort 必须是字符串。")

    normalized = value.strip().lower().replace("-", "_").replace(" ", "_")
    effort = _REASONING_EFFORT_ALIASES.get(normalized)
    if effort is None:
        allowed = ", ".join(sorted([*VALID_REASONING_EFFORTS, "disabled"]))
        raise LLMError(f"llm.reasoning_effort 仅支持 {allowed}，当前值：{value}。")
    return effort


def _read_required_config_text(
    section: dict[str, Any],
    key: str,
    env_name: str,
) -> str:
    """按 环境变量 > JSON 配置 的顺序读取必填字符串配置。"""

    env_value = os.getenv(env_name)
    if env_value is not None and env_value.strip():
        return env_value

    value = section.get(key)
    if value is None:
        raise LLMError(
            f"缺少配置 llm.{key}，请在 config.json 中填写 llm.{key}，"
            f"或设置环境变量 {env_name}。"
        )
    if not isinstance(value, str):
        raise LLMError(f"配置项 llm.{key} 必须是字符串。")
    if not value.strip():
        raise LLMError(
            f"缺少配置 llm.{key}，请在 config.json 中填写 llm.{key}，"
            f"或设置环境变量 {env_name}。"
        )

    return value.strip()


def _read_optional_config_text(
    section: dict[str, Any],
    key: str,
    env_name: str,
    default: str,
) -> str:
    """按 环境变量 > JSON 配置 > 默认值 的顺序读取可选字符串配置。"""

    env_value = os.getenv(env_name)
    if env_value is not None and env_value.strip():
        return env_value

    value = section.get(key, default)
    if value is None:
        return default
    if not isinstance(value, str):
        raise LLMError(f"配置项 llm.{key} 必须是字符串。")

    return value.strip() or default


def load_llm_config() -> LLMConfig:
    """从本地 JSON 配置文件和环境变量创建 LLM 配置。

    默认读取项目根目录下的 `config.json`；也可以通过 `AI_CONFIG_FILE` 指定路径。
    环境变量优先级更高，便于临时覆盖本地配置。
    """

    try:
        data = load_config_data()
        llm_section = get_section(data, "llm")
    except RuntimeConfigError as exc:
        raise LLMError(str(exc)) from exc

    return LLMConfig(
        api_key=_read_required_config_text(llm_section, "api_key", "OPENAI_API_KEY"),
        base_url=_read_required_config_text(llm_section, "base_url", "OPENAI_BASE_URL"),
        model=_read_required_config_text(llm_section, "model", "OPENAI_MODEL"),
        thinking_type=_read_optional_config_text(
            llm_section,
            "thinking_type",
            "OPENAI_THINKING_TYPE",
            DEFAULT_THINKING_TYPE,
        ),
        reasoning_effort=_read_optional_config_text(
            llm_section,
            "reasoning_effort",
            "REASONING_EFFORT",
            DEFAULT_REASONING_EFFORT,
        ),
    )


class OpenAIResponseLLM:
    """使用 OpenAI Python SDK 调用 Responses API，并维护简短对话上下文。"""

    def __init__(self, config: LLMConfig | None = None) -> None:
        self.config = config or load_llm_config()
        if not self.config.api_key.strip():
            raise LLMError("缺少 API Key，请在 config.json 的 llm.api_key 中配置，或设置 OPENAI_API_KEY。")

        try:
            from openai import OpenAI
        except ImportError as exc:
            raise LLMError("缺少 openai 依赖，请先执行：pip install -r requirements.txt") from exc

        self._client = OpenAI(api_key=self.config.api_key, base_url=self.config.base_url)
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
        try:
            for event in stream:
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
        """写入历史；思考模式下保存思维链，但仅在工具调用场景中回传。"""

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
        """组合历史消息和本轮用户输入。

        简单对话（无工具调用）场景下，剔除 assistant 的 reasoning_content，
        因为 API 在两段 user 消息之间没有工具调用时会忽略它。
        """

        messages = [*self._history, {"role": "user", "content": user_text}]
        return [
            {k: v for k, v in msg.items() if k != "reasoning_content"}
            for msg in messages
        ]

    def _build_extra_body(self) -> dict[str, Any]:
        """构造网关扩展参数；根据 reasoning_effort 决定是否启用思考模式。"""

        thinking_type = "enabled" if self.config.thinking_enabled else "disabled"
        body: dict[str, Any] = {"thinking": {"type": thinking_type}}
        if self.config.thinking_enabled and self.config.reasoning_effort:
            if self.config.reasoning_effort in VALID_REASONING_EFFORTS:
                body["reasoning_effort"] = self.config.reasoning_effort
        return body

    def _trim_history(self) -> None:
        """只保留最近若干轮，避免语音对话运行时间长后请求体过大。"""

        max_messages = self.config.max_history_turns * 2
        if len(self._history) > max_messages:
            self._history = self._history[-max_messages:]

    @staticmethod
    def _extract_text(response: Any) -> str:
        """兼容 SDK 对象与字典两种 Responses API 返回形态。"""

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
        for item in data.get("output", []):
            for content in item.get("content", []):
                text = content.get("text")
                if isinstance(text, str) and text.strip():
                    texts.append(text.strip())

        return "\n".join(texts).strip()

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
                "可在 config.json 中配置 llm.model，或设置 OPENAI_MODEL 切换。"
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
