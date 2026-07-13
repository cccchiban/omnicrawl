"""LLM 配置模型、读写与规范化。

网络请求见 `config.llm_client`；多模型解析见 `config.llm_multi`。
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data

DEFAULT_THINKING_TYPE = "disabled"
DEFAULT_REASONING_EFFORT = ""
KNOWN_AVAILABLE_MODELS = ("qwen3.6-plus", "glm-5.1", "deepseek-v4-flash", "gpt-5.2", "gpt-5.4-mini")
VALID_REASONING_EFFORTS = {"none", "low", "medium", "high", "xhigh", "max"}
_REASONING_EFFORT_ALIASES = {
    "": DEFAULT_REASONING_EFFORT, "disabled": "disabled", "off": "disabled",
    "none": "none", "low": "low", "medium": "medium", "med": "medium",
    "high": "high", "xhigh": "xhigh", "x_high": "xhigh", "extra_high": "xhigh",
    "very_high": "xhigh", "max": "max", "maximum": "max",
}


class LLMError(RuntimeError):
    """大模型请求或响应解析失败时抛出。"""


@dataclass
class LLMConfig:
    """面向 Agent / UI 的当前模型运行视图。"""

    api_key: str = field(default_factory=lambda: os.getenv("OPENAI_API_KEY", ""))
    base_url: str = field(default_factory=lambda: os.getenv("OPENAI_BASE_URL", ""))
    model: str = field(default_factory=lambda: os.getenv("OPENAI_MODEL", ""))
    thinking_type: str = field(
        default_factory=lambda: os.getenv("OPENAI_THINKING_TYPE", DEFAULT_THINKING_TYPE)
    )
    reasoning_effort: str = field(
        default_factory=lambda: os.getenv("REASONING_EFFORT", DEFAULT_REASONING_EFFORT)
    )
    context_window_tokens: int = 128_000
    max_output_tokens: int = 0
    temperature: float | None = None
    system_prompt: str = (
        "你是一个通过语音和用户对话的中文 AI 助手。"
        "回答要自然、简洁、适合被朗读；遇到不确定内容要明确说明。"
    )
    max_history_turns: int = 8
    profile_id: str = ""
    provider: str = "openai"
    protocol: str = "openai_chat_completions"
    catalog_key: str = ""
    model_source: str = "legacy"  # legacy | custom | detected
    api_key_env: str = ""
    request_timeout_seconds: int = 180
    request_retry_count: int = 5
    provider_options: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.api_key = self.api_key.strip()
        self.base_url = self.base_url.strip()
        self.model = self.model.strip()
        self.thinking_type = (self.thinking_type.strip() or DEFAULT_THINKING_TYPE).lower()
        self.reasoning_effort = normalize_reasoning_effort(self.reasoning_effort)
        if isinstance(self.context_window_tokens, bool) or not isinstance(
            self.context_window_tokens, int
        ):
            raise LLMError("配置项 llm.context_window_tokens 必须是正整数。")
        if self.context_window_tokens <= 0:
            raise LLMError("配置项 llm.context_window_tokens 必须大于 0。")
        if self.model_source == "legacy":
            _require_non_empty("api_key", self.api_key, "OPENAI_API_KEY")
            _require_non_empty("base_url", self.base_url, "OPENAI_BASE_URL")
            _require_non_empty("model", self.model, "OPENAI_MODEL")
        else:
            if not self.model.strip():
                raise LLMError("缺少当前模型 model_id。")
            if not self.api_key.strip():
                env_name = self.api_key_env or "OPENAI_API_KEY"
                raise LLMError(
                    f"缺少 API Key，请设置环境变量 {env_name} 或在 Profile 中配置 api_key。"
                )

    @property
    def thinking_enabled(self) -> bool:
        if self.reasoning_effort in {"none", "disabled"}:
            return False
        if self.reasoning_effort:
            return True
        return self.thinking_type not in {"", "disabled"}


@dataclass(frozen=True)
class ActiveModelRef:
    source: str  # custom | detected
    key: str = ""
    profile: str = ""
    model_id: str = ""
    protocol: str = ""

    def to_dict(self) -> dict[str, str]:
        if self.source == "custom":
            return {"source": "custom", "key": self.key}
        return {
            "source": "detected",
            "profile": self.profile,
            "model_id": self.model_id,
            "protocol": self.protocol,
        }


def _require_non_empty(key: str, value: str, env_name: str) -> None:
    if not isinstance(value, str) or not value.strip():
        raise LLMError(
            f"缺少配置 llm.{key}，请在 config.yaml/config.json 中填写 llm.{key}，"
            f"或设置环境变量 {env_name}。"
        )


def normalize_reasoning_effort(value: Any) -> str:
    if not isinstance(value, str):
        raise LLMError("配置项 llm.reasoning_effort 必须是字符串。")
    normalized = value.strip().lower().replace("-", "_").replace(" ", "_")
    effort = _REASONING_EFFORT_ALIASES.get(normalized)
    if effort is None:
        allowed = ", ".join(sorted([*VALID_REASONING_EFFORTS, "disabled"]))
        raise LLMError(f"llm.reasoning_effort 仅支持 {allowed}，当前值：{value}。")
    return effort


def _read_required_config_text(section: dict[str, Any], key: str, env_name: str) -> str:
    env_value = os.getenv(env_name)
    if env_value is not None and env_value.strip():
        return env_value
    value = section.get(key)
    if value is None or (isinstance(value, str) and not value.strip()):
        raise LLMError(
            f"缺少配置 llm.{key}，请在配置文件中填写 llm.{key}，"
            f"或设置环境变量 {env_name}。"
        )
    if not isinstance(value, str):
        raise LLMError(f"配置项 llm.{key} 必须是字符串。")
    return value.strip()


def _read_optional_config_text(
    section: dict[str, Any], key: str, env_name: str, default: str
) -> str:
    env_value = os.getenv(env_name)
    if env_value is not None and env_value.strip():
        return env_value
    value = section.get(key, default)
    if value is None:
        return default
    if not isinstance(value, str):
        raise LLMError(f"配置项 llm.{key} 必须是字符串。")
    return value.strip() or default


def _read_context_window_tokens(section: dict[str, Any], default: int = 128_000) -> int:
    value = section.get("context_window_tokens", default)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise LLMError("配置项 llm.context_window_tokens 必须是正整数。")
    return value


def load_llm_config() -> LLMConfig:
    """从本地配置文件和环境变量创建当前 LLM 运行视图。"""

    try:
        data = load_config_data()
        llm_section = get_section(data, "llm")
    except RuntimeConfigError as exc:
        raise LLMError(str(exc)) from exc

    from .llm_multi import is_multi_model_section, load_multi_model_llm_config

    if is_multi_model_section(llm_section):
        return load_multi_model_llm_config(llm_section)

    return LLMConfig(
        api_key=_read_required_config_text(llm_section, "api_key", "OPENAI_API_KEY"),
        base_url=_read_required_config_text(llm_section, "base_url", "OPENAI_BASE_URL"),
        model=_read_required_config_text(llm_section, "model", "OPENAI_MODEL"),
        thinking_type=_read_optional_config_text(
            llm_section, "thinking_type", "OPENAI_THINKING_TYPE", DEFAULT_THINKING_TYPE
        ),
        reasoning_effort=_read_optional_config_text(
            llm_section, "reasoning_effort", "REASONING_EFFORT", DEFAULT_REASONING_EFFORT
        ),
        context_window_tokens=_read_context_window_tokens(llm_section),
        model_source="legacy",
        provider="openai",
        protocol="openai_chat_completions",
    )


def save_reasoning_effort(effort: str, config_path: str | Path | None = None) -> Path:
    """把推理强度写回配置文件，并同步 thinking_type。"""

    normalized = normalize_reasoning_effort(effort)
    try:
        data = load_config_data(config_path)
        llm_section = get_section(data, "llm")
        from .llm_multi import is_multi_model_section

        if is_multi_model_section(llm_section):
            defaults = llm_section.get("defaults")
            if not isinstance(defaults, dict):
                defaults = {}
            defaults = dict(defaults)
            defaults["reasoning_effort"] = normalized
            defaults["thinking_type"] = (
                "disabled" if normalized in {"none", "disabled"} else "enabled"
            )
            llm_section["defaults"] = defaults
        else:
            llm_section["reasoning_effort"] = normalized
            llm_section["thinking_type"] = (
                "disabled" if normalized in {"none", "disabled"} else "enabled"
            )
        data["llm"] = llm_section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise LLMError(str(exc)) from exc


def save_active_model_ref(
    ref: ActiveModelRef, config_path: str | Path | None = None
) -> Path:
    """持久化当前模型选择。"""
    try:
        data = load_config_data(config_path)
        llm_section = get_section(data, "llm")
        from .llm_multi import is_multi_model_section
        if not is_multi_model_section(llm_section):
            llm_section["model"] = ref.model_id or ref.key
        else:
            llm_section["active_model"] = ref.to_dict()
        data["llm"] = llm_section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise LLMError(str(exc)) from exc


from .llm_client import OpenAIResponseLLM  # 兼容旧导入路径

__all__ = [
    "DEFAULT_REASONING_EFFORT",
    "DEFAULT_THINKING_TYPE",
    "KNOWN_AVAILABLE_MODELS",
    "ActiveModelRef",
    "LLMConfig",
    "LLMError",
    "OpenAIResponseLLM",
    "VALID_REASONING_EFFORTS",
    "load_llm_config",
    "normalize_reasoning_effort",
    "save_active_model_ref",
    "save_reasoning_effort",
]
