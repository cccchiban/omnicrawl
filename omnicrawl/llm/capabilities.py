"""模型能力定义与合并规则。"""

from __future__ import annotations

from dataclasses import dataclass, fields, replace
from typing import Any, Mapping


@dataclass(frozen=True)
class ModelCapabilities:
    """描述模型是否支持流式、工具、推理等 Host 关心的能力。

    bool 字段使用 ``None`` 表示“本层未声明”，合并时不得覆盖下层已有值。
    对外读取时请用 :meth:`resolved` 或直接把缺省视为保守 False
    （``streaming`` 缺省 True）。
    """

    streaming: bool | None = None
    tools: bool | None = None
    parallel_tool_calls: bool | None = None
    reasoning: bool | None = None
    vision: bool | None = None
    model_discovery: bool | None = None
    prompt_cache: bool | None = None
    context_window_tokens: int = 0
    max_output_tokens: int = 0

    def resolved(self) -> ModelCapabilities:
        """把 None 填成对外可用的布尔默认值，便于 Runtime 门禁判断。"""

        return ModelCapabilities(
            streaming=True if self.streaming is None else self.streaming,
            tools=False if self.tools is None else self.tools,
            parallel_tool_calls=(
                False if self.parallel_tool_calls is None else self.parallel_tool_calls
            ),
            reasoning=False if self.reasoning is None else self.reasoning,
            vision=False if self.vision is None else self.vision,
            model_discovery=False if self.model_discovery is None else self.model_discovery,
            prompt_cache=False if self.prompt_cache is None else self.prompt_cache,
            context_window_tokens=self.context_window_tokens,
            max_output_tokens=self.max_output_tokens,
        )

    def to_dict(self) -> dict[str, Any]:
        resolved = self.resolved()
        return {
            "streaming": bool(resolved.streaming),
            "tools": bool(resolved.tools),
            "parallel_tool_calls": bool(resolved.parallel_tool_calls),
            "reasoning": bool(resolved.reasoning),
            "vision": bool(resolved.vision),
            "model_discovery": bool(resolved.model_discovery),
            "prompt_cache": bool(resolved.prompt_cache),
            "context_window_tokens": resolved.context_window_tokens,
            "max_output_tokens": resolved.max_output_tokens,
        }


_BOOL_FIELDS = frozenset(
    {
        "streaming",
        "tools",
        "parallel_tool_calls",
        "reasoning",
        "vision",
        "model_discovery",
        "prompt_cache",
    }
)


def capabilities_from_mapping(raw: Mapping[str, Any] | None) -> ModelCapabilities:
    """从 models.yaml / 发现结果中解析能力；未知字段忽略，缺失字段为 None。"""

    if not isinstance(raw, Mapping):
        return ModelCapabilities()

    def _bool(key: str) -> bool | None:
        if key not in raw:
            return None
        value = raw.get(key)
        return bool(value) if isinstance(value, bool) else None

    def _int(key: str, default: int = 0) -> int:
        value = raw.get(key, default)
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            return default
        return value

    return ModelCapabilities(
        streaming=_bool("streaming"),
        tools=_bool("tools"),
        parallel_tool_calls=_bool("parallel_tool_calls"),
        reasoning=_bool("reasoning"),
        vision=_bool("vision"),
        model_discovery=_bool("model_discovery"),
        prompt_cache=_bool("prompt_cache"),
        context_window_tokens=_int("context_window_tokens", 0),
        max_output_tokens=_int("max_output_tokens", 0),
    )


def merge_capabilities(
    *layers: ModelCapabilities | None,
) -> ModelCapabilities:
    """按优先级合并能力层（后者覆盖前者中“已声明”的字段）。

    设计优先级：
    models.yaml 用户显式配置 > Provider 自动发现 > Adapter 保守默认值。
    调用时按从低到高传入即可。

    - bool / Optional：仅当层值不是 None 时覆盖（可显式写 False）。
    - int：仅正数覆盖。
    最终返回 :meth:`ModelCapabilities.resolved` 结果，保证 Runtime 读到具体布尔值。
    """

    result = ModelCapabilities()
    for layer in layers:
        if layer is None:
            continue
        updates: dict[str, Any] = {}
        for item in fields(ModelCapabilities):
            value = getattr(layer, item.name)
            if item.name in _BOOL_FIELDS:
                if value is not None:
                    updates[item.name] = value
                continue
            if isinstance(value, int) and value > 0:
                updates[item.name] = value
        if updates:
            result = replace(result, **updates)
    return result.resolved()


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
