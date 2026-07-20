"""上下文压缩与测量模式的运行配置。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

from .runtime import RuntimeConfigError, get_section, load_config_data


class ContextCompactionConfigError(RuntimeConfigError):
    """上下文压缩配置缺失、类型错误或跨字段不合法。"""


@dataclass(frozen=True)
class ContextCompactionConfig:
    """上下文压缩配置。

    ``enabled`` 默认关闭。开启后，完整回合结束时由模型摘要 service 接管自动
    压缩；普通 ``/compact`` 仍保持本地确定性，``/compact --model`` 才显式
    产生摘要模型调用。``summary_profile`` 接受 models.yaml key/alias、裸模型 ID
    或 ``profile/model_id``，跨供应商仍需单独允许。
    """

    enabled: bool = False
    trigger_context_tokens: int = 70_000
    next_user_reserve_tokens: int = 4_096
    minimum_turns_between_model_compactions: int = 4
    emergency_context_ratio: float = 0.85
    summary_profile: str = ""
    reasoning_effort: str = "low"
    recent_turns: int = 6
    recent_context_ratio: float = 0.25
    target_summary_tokens: int = 6_000
    preserve_exact_evidence: bool = True
    allow_cross_provider: bool = False
    failure_fallback: str = "deterministic"

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise ContextCompactionConfigError("context_compaction.enabled 必须是布尔值。")
        for name in (
            "trigger_context_tokens",
            "next_user_reserve_tokens",
            "minimum_turns_between_model_compactions",
            "recent_turns",
            "target_summary_tokens",
        ):
            _require_positive_int(name, getattr(self, name))
        _require_ratio("emergency_context_ratio", self.emergency_context_ratio, upper=1.0)
        _require_ratio("recent_context_ratio", self.recent_context_ratio, upper=1.0)
        if not isinstance(self.summary_profile, str):
            raise ContextCompactionConfigError(
                "context_compaction.summary_profile 必须是字符串。"
            )
        if not isinstance(self.reasoning_effort, str) or not self.reasoning_effort.strip():
            raise ContextCompactionConfigError(
                "context_compaction.reasoning_effort 必须是非空字符串。"
            )
        if not isinstance(self.preserve_exact_evidence, bool):
            raise ContextCompactionConfigError(
                "context_compaction.preserve_exact_evidence 必须是布尔值。"
            )
        if not isinstance(self.allow_cross_provider, bool):
            raise ContextCompactionConfigError(
                "context_compaction.allow_cross_provider 必须是布尔值。"
            )
        if self.failure_fallback != "deterministic":
            raise ContextCompactionConfigError(
                "context_compaction.failure_fallback 当前仅支持 deterministic。"
            )


def load_context_compaction_config(
    config_path: str | Path | None = None,
) -> ContextCompactionConfig:
    """从 ``config.yaml`` 读取并严格校验 ``context_compaction`` 段。"""

    try:
        section = get_section(load_config_data(config_path), "context_compaction")
    except RuntimeConfigError as exc:
        raise ContextCompactionConfigError(str(exc)) from exc

    defaults = ContextCompactionConfig()
    values: dict[str, Any] = {}
    for name in defaults.__dataclass_fields__:
        values[name] = section.get(name, getattr(defaults, name))
    _reject_unknown_fields(section, allowed=set(values))
    return ContextCompactionConfig(**values)


def _require_positive_int(name: str, value: Any) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ContextCompactionConfigError(
            f"context_compaction.{name} 必须是正整数。"
        )


def _require_ratio(name: str, value: Any, *, upper: float) -> None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ContextCompactionConfigError(
            f"context_compaction.{name} 必须是数字。"
        )
    numeric = float(value)
    if numeric <= 0 or numeric > upper or (upper == 1.0 and numeric >= 1.0):
        comparator = "< 1" if upper == 1.0 else f"<= {upper}"
        raise ContextCompactionConfigError(
            f"context_compaction.{name} 必须满足 0 < value {comparator}。"
        )


def _reject_unknown_fields(section: Mapping[str, Any], *, allowed: set[str]) -> None:
    unknown = sorted(set(section) - allowed)
    if unknown:
        raise ContextCompactionConfigError(
            "context_compaction 包含未知配置项：" + ", ".join(unknown)
        )
