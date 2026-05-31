from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Any, Callable, Iterable

from runtime_config import RuntimeConfigError, get_section, load_config_data


DEFAULT_THINKING_TYPE = "disabled"
DEFAULT_REASONING_EFFORT = ""
KNOWN_AVAILABLE_MODELS = (
    "qwen3.6-plus",
    "glm-5.1",
    "deepseek-v4-flash",
    "gpt-5.2",
    "gpt-5.4-mini",
)
VALID_REASONING_EFFORTS = {"none", "low", "medium", "high", "max"}


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
        self.thinking_type = self.thinking_type.strip() or DEFAULT_THINKING_TYPE
        self.reasoning_effort = self.reasoning_effort.strip()
        _require_non_empty("api_key", self.api_key, "OPENAI_API_KEY")
        _require_non_empty("base_url", self.base_url, "OPENAI_BASE_URL")
        _require_non_empty("model", self.model, "OPENAI_MODEL")

    @property
    def thinking_enabled(self) -> bool:
        """推理强度非空且不是 none/disabled 时启用思考模式。"""
        if self.reasoning_effort and self.reasoning_effort not in {"none", "disabled"}:
            return True
        return self.thinking_type not in {"", "disabled"}


def _require_non_empty(key: str, value: str, env_name: str) -> None:
    if not isinstance(value, str) or not value.strip():
        raise LLMError(
            f"缺少配置 llm.{key}，请在 config.json 中填写 llm.{key}，"
            f"或设置环境变量 {env_name}。"
        )


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
        """给常见接口错误补充可操作提示，避免只看到 SDK 原始异常。"""

        message = str(exc)
        if "model not found" in message.lower() or "invalid_model" in message.lower():
            models = "、".join(KNOWN_AVAILABLE_MODELS)
            return f"{message}\n当前网关可用模型示例：{models}。可在 config.json 中配置 llm.model，或设置 OPENAI_MODEL 切换。"

        return message
