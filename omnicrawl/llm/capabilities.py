"""模型能力定义与合并规则。"""

from __future__ import annotations

from dataclasses import dataclass, fields, replace
from typing import Any, Mapping


@dataclass(frozen=True)
class ModelCapabilities:
    """描述模型是否支持流式、工具、推理等 Host 关心的能力。"""

    streaming: bool = True
    tools: bool = False
    parallel_tool_calls: bool = False
    reasoning: bool = False
    vision: bool = False
    model_discovery: bool = False
    prompt_cache: bool = False
    context_window_tokens: int = 0
    max_output_tokens: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "streaming": self.streaming,
            "tools": self.tools,
            "parallel_tool_calls": self.parallel_tool_calls,
            "reasoning": self.reasoning,
            "vision": self.vision,
            "model_discovery": self.model_discovery,
            "prompt_cache": self.prompt_cache,
            "context_window_tokens": self.context_window_tokens,
            "max_output_tokens": self.max_output_tokens,
        }


def capabilities_from_mapping(raw: Mapping[str, Any] | None) -> ModelCapabilities:
    """从 models.yaml / 发现结果中解析能力；未知字段忽略，缺失字段用保守默认。"""

    if not isinstance(raw, Mapping):
        return ModelCapabilities()

    def _bool(key: str, default: bool) -> bool:
        value = raw.get(key, default)
        return bool(value) if isinstance(value, bool) else default

    def _int(key: str, default: int = 0) -> int:
        value = raw.get(key, default)
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            return default
        return value

    return ModelCapabilities(
        streaming=_bool("streaming", True),
        tools=_bool("tools", False),
        parallel_tool_calls=_bool("parallel_tool_calls", False),
        reasoning=_bool("reasoning", False),
        vision=_bool("vision", False),
        model_discovery=_bool("model_discovery", False),
        prompt_cache=_bool("prompt_cache", False),
        context_window_tokens=_int("context_window_tokens", 0),
        max_output_tokens=_int("max_output_tokens", 0),
    )


def merge_capabilities(
    *layers: ModelCapabilities | None,
) -> ModelCapabilities:
    """按“后者覆盖前者非默认值”合并能力。

    设计优先级：
    models.yaml 用户显式配置 > Provider 自动发现 > Adapter 保守默认值。
    调用时按从低到高传入即可。
    """

    result = ModelCapabilities()
    for layer in layers:
        if layer is None:
            continue
        updates: dict[str, Any] = {}
        for item in fields(ModelCapabilities):
            value = getattr(layer, item.name)
            default = getattr(ModelCapabilities(), item.name)
            # 仅当层给出“有意义”的值时覆盖：bool 直接覆盖，int 仅正数覆盖。
            if item.type is bool or item.name in {
                "streaming",
                "tools",
                "parallel_tool_calls",
                "reasoning",
                "vision",
                "model_discovery",
                "prompt_cache",
            }:
                updates[item.name] = value
            elif isinstance(value, int) and value > 0:
                updates[item.name] = value
            elif value != default:
                updates[item.name] = value
        result = replace(result, **updates)
    return result


def conservative_openai_chat_capabilities() -> ModelCapabilities:
    return ModelCapabilities(
        streaming=True,
        tools=True,
        parallel_tool_calls=True,
        reasoning=False,
        vision=False,
        model_discovery=True,
        prompt_cache=False,
    )


def conservative_openai_responses_capabilities() -> ModelCapabilities:
    return ModelCapabilities(
        streaming=True,
        tools=True,
        parallel_tool_calls=True,
        reasoning=True,
        vision=False,
        model_discovery=True,
        prompt_cache=False,
    )


def conservative_anthropic_capabilities() -> ModelCapabilities:
    return ModelCapabilities(
        streaming=True,
        tools=True,
        parallel_tool_calls=True,
        reasoning=True,
        vision=True,
        model_discovery=True,
        prompt_cache=True,
    )


def conservative_gemini_capabilities() -> ModelCapabilities:
    return ModelCapabilities(
        streaming=True,
        tools=True,
        parallel_tool_calls=True,
        reasoning=False,
        vision=True,
        model_discovery=True,
        prompt_cache=False,
    )
