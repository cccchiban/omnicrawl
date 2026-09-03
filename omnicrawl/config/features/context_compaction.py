"""上下文压缩与测量模式的运行配置。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

from ..core.runtime import RuntimeConfigError, get_section, load_config_data


class ContextCompactionConfigError(RuntimeConfigError):
    """上下文压缩配置缺失、类型错误或跨字段不合法。"""


@dataclass(frozen=True)
class ContextCompactionConfig:
    """上下文压缩配置。

    上下文压缩始终作为 Agent 基础能力运行。完整回合结束时由模型摘要 service
    接管自动压缩；普通 ``/compact`` 仍保持本地确定性，``/compact --model``
    显式产生摘要模型调用。``summary_profile`` 接受 models.toml key/alias、裸模型 ID
    或 ``profile/model_id``，跨供应商仍需单独允许。

    ``target_summary_tokens`` 为 0 时表示不设摘要预算上限：摘要以完整性优先，
    不再被 token 预算卡住或校验拒绝（建议同时保持较大的触发阈值，避免过早压缩）。

    ``archive_compacted_events`` 开启时，被压缩窗口的原始事件会归档到
    ``.agent_sessions/archive/compacted/<session>/``，形成摘要之外的二级存储；
    ``auto_memory_recall`` 开启时，压缩完成后自动检索长期记忆并把命中结果注入
    后续上下文，帮助恢复“之前做过什么”。
    """

    # 保存用户选择的百分比，使上下文窗口变化时能在运行态实时重算阈值。
    # None 表示显式使用固定 Token 阈值；默认按上下文窗口的 80% 计算。
    trigger_context_percent: int | None = 80
    trigger_context_tokens: int = 100_000
    next_user_reserve_tokens: int = 10_240
    minimum_turns_between_model_compactions: int = 4
    emergency_context_ratio: float = 0.85
    summary_profile: str = ""
    reasoning_effort: str = "low"
    recent_turns: int = 6
    recent_context_ratio: float = 0.25
    target_summary_tokens: int = 6_000
    preserve_exact_evidence: bool = True
    archive_compacted_events: bool = True
    auto_memory_recall: bool = True
    allow_cross_provider: bool = False
    failure_fallback: str = "deterministic"

    def __post_init__(self) -> None:
        for name in (
            "trigger_context_tokens",
            "next_user_reserve_tokens",
            "minimum_turns_between_model_compactions",
            "recent_turns",
            "target_summary_tokens",
        ):
            # target_summary_tokens 允许 0（0 = 无摘要预算上限），其余必须为正整数。
            if name == "target_summary_tokens":
                _require_non_negative_int(name, getattr(self, name))
            else:
                _require_positive_int(name, getattr(self, name))
        if self.trigger_context_percent is not None:
            _require_positive_int(
                "trigger_context_percent",
                self.trigger_context_percent,
            )
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
        if not isinstance(self.archive_compacted_events, bool):
            raise ContextCompactionConfigError(
                "context_compaction.archive_compacted_events 必须是布尔值。"
            )
        if not isinstance(self.auto_memory_recall, bool):
            raise ContextCompactionConfigError(
                "context_compaction.auto_memory_recall 必须是布尔值。"
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
    """从 ``config.toml`` 读取并严格校验 ``context_compaction`` 段。"""

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


def _require_non_negative_int(name: str, value: Any) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ContextCompactionConfigError(
            f"context_compaction.{name} 必须是非负整数（0 表示无预算限制）。"
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
