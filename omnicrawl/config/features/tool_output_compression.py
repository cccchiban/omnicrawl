"""工具输出压缩配置：默认关闭，显式启用并选择模型后才生效。

对应 config.toml 的 ``[tool_output_compression]`` 段：

.. code-block:: yaml

    [tool_output_compression]
    enabled = true                      # 显式启用后才压缩工具结果
    model_key = "qwen2.5-3b-instruct"   # 压缩模型：models.toml key/alias 或 profile/model_id
    thinking_enabled = false            # 压缩模型思考开关
    reasoning_effort = "low"            # 思考深度：low/medium/high/xhigh/max
    min_chars = 1200                    # 模型可见文本短于此值不压缩
    max_input_chars = 24000             # 超长输出按头尾采样后再交给压缩模型
    max_output_chars = 1500             # 压缩结果硬上限
    timeout_seconds = 60                # 单个工具结果的压缩超时
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

from ..core.runtime import RuntimeConfigError, get_section, load_config_data, save_config_data

from ..models.llm import VALID_REASONING_EFFORTS

DEFAULT_MIN_CHARS = 1200
DEFAULT_MAX_INPUT_CHARS = 24_000
DEFAULT_MAX_OUTPUT_CHARS = 1_500
DEFAULT_TIMEOUT_SECONDS = 60
# 思考深度：关闭思考由 thinking_enabled=false 表达，因此这里不含 none。
DEFAULT_THINKING_EFFORT = "low"
THINKING_EFFORT_OPTIONS = tuple(
    effort
    for effort in ("low", "medium", "high", "xhigh", "max")
    if effort in VALID_REASONING_EFFORTS
)


class ToolOutputCompressionConfigError(RuntimeConfigError):
    """工具输出压缩配置无效或无法写回。"""


@dataclass(frozen=True)
class ToolOutputCompressionConfig:
    """工具输出压缩的开关、模型选择与预算。

    ``enabled`` 为 False 时工具结果完全走原路径（零成本）；``model_key``
    空串表示未选择压缩模型（等价禁用），语义与 advisor 一致。
    """

    enabled: bool = False
    model_key: str = ""
    # 思考开关默认关闭：小模型通常不需要推理，关闭也最省时延。
    thinking_enabled: bool = False
    reasoning_effort: str = DEFAULT_THINKING_EFFORT
    min_chars: int = DEFAULT_MIN_CHARS
    max_input_chars: int = DEFAULT_MAX_INPUT_CHARS
    max_output_chars: int = DEFAULT_MAX_OUTPUT_CHARS
    timeout_seconds: int = DEFAULT_TIMEOUT_SECONDS

    @property
    def active(self) -> bool:
        """是否真正可用：显式启用且已选择压缩模型。"""

        return self.enabled and bool((self.model_key or "").strip())

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise ToolOutputCompressionConfigError(
                "tool_output_compression.enabled 必须是布尔值。"
            )
        if not isinstance(self.model_key, str):
            raise ToolOutputCompressionConfigError(
                "tool_output_compression.model_key 必须是字符串。"
            )
        if not isinstance(self.thinking_enabled, bool):
            raise ToolOutputCompressionConfigError(
                "tool_output_compression.thinking_enabled 必须是布尔值。"
            )
        if self.reasoning_effort not in THINKING_EFFORT_OPTIONS:
            allowed = ", ".join(THINKING_EFFORT_OPTIONS)
            raise ToolOutputCompressionConfigError(
                f"tool_output_compression.reasoning_effort 仅支持 {allowed}，"
                f"当前值：{self.reasoning_effort}；关闭思考请设 thinking_enabled = false。"
            )
        for name in (
            "min_chars",
            "max_input_chars",
            "max_output_chars",
            "timeout_seconds",
        ):
            _require_positive_int(name, getattr(self, name))


def load_tool_output_compression_config(
    config_path: str | Path | None = None,
) -> ToolOutputCompressionConfig:
    """读取 config.toml 的 ``[tool_output_compression]`` 段；缺失即默认关闭。"""

    try:
        data = load_config_data(config_path)
        section = get_section(data, "tool_output_compression")
    except RuntimeConfigError as exc:
        raise ToolOutputCompressionConfigError(str(exc)) from exc
    return _parse_section(section)


def _parse_section(section: Mapping[str, Any]) -> ToolOutputCompressionConfig:
    return ToolOutputCompressionConfig(
        enabled=_bool_field(section, "enabled", False),
        model_key=str(section.get("model_key") or "").strip(),
        thinking_enabled=_bool_field(section, "thinking_enabled", False),
        reasoning_effort=str(
            section.get("reasoning_effort") or DEFAULT_THINKING_EFFORT
        ).strip(),
        min_chars=_int_field(section, "min_chars", DEFAULT_MIN_CHARS),
        max_input_chars=_int_field(section, "max_input_chars", DEFAULT_MAX_INPUT_CHARS),
        max_output_chars=_int_field(
            section, "max_output_chars", DEFAULT_MAX_OUTPUT_CHARS
        ),
        timeout_seconds=_int_field(
            section, "timeout_seconds", DEFAULT_TIMEOUT_SECONDS
        ),
    )


def save_tool_output_compression_config(
    config: ToolOutputCompressionConfig,
    config_path: str | Path | None = None,
) -> Path:
    """把工具输出压缩配置写回 config.toml（保留其他段）。"""

    if not isinstance(config, ToolOutputCompressionConfig):
        raise ToolOutputCompressionConfigError("工具输出压缩配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["tool_output_compression"] = {
            "enabled": config.enabled,
            "model_key": config.model_key,
            "thinking_enabled": config.thinking_enabled,
            "reasoning_effort": config.reasoning_effort,
            "min_chars": config.min_chars,
            "max_input_chars": config.max_input_chars,
            "max_output_chars": config.max_output_chars,
            "timeout_seconds": config.timeout_seconds,
        }
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise ToolOutputCompressionConfigError(str(exc)) from exc


def clear_tool_output_compression_config(
    config_path: str | Path | None = None,
) -> Path:
    """关闭工具输出压缩：置 enabled=False 并清空 model_key。"""

    return save_tool_output_compression_config(
        ToolOutputCompressionConfig(enabled=False), config_path
    )


def _require_positive_int(name: str, value: Any) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ToolOutputCompressionConfigError(
            f"tool_output_compression.{name} 必须是正整数。"
        )


def _bool_field(section: Mapping[str, Any], name: str, default: bool) -> bool:
    value = section.get(name, default)
    if not isinstance(value, bool):
        raise ToolOutputCompressionConfigError(
            f"tool_output_compression.{name} 必须是布尔值。"
        )
    return value


def _int_field(section: Mapping[str, Any], name: str, default: int) -> int:
    value = section.get(name, default)
    if isinstance(value, bool) or not isinstance(value, int):
        raise ToolOutputCompressionConfigError(
            f"tool_output_compression.{name} 必须是整数。"
        )
    return value


__all__ = [
    "ToolOutputCompressionConfig",
    "ToolOutputCompressionConfigError",
    "DEFAULT_MAX_INPUT_CHARS",
    "DEFAULT_MAX_OUTPUT_CHARS",
    "DEFAULT_MIN_CHARS",
    "DEFAULT_THINKING_EFFORT",
    "DEFAULT_TIMEOUT_SECONDS",
    "THINKING_EFFORT_OPTIONS",
    "clear_tool_output_compression_config",
    "load_tool_output_compression_config",
    "save_tool_output_compression_config",
]
