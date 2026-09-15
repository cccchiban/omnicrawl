"""上下文压缩使用的配置无关数据模型和注入协议。"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Callable, Mapping, Sequence


@dataclass(frozen=True)
class TokenUsageSample:
    """一次完整回合或摘要调用内累计的供应商 Token 用量。"""

    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0

    def __post_init__(self) -> None:
        for name in ("input_tokens", "output_tokens", "cached_input_tokens"):
            value = getattr(self, name)
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise ValueError(f"{name} 必须是非负整数。")

    def add(
        self,
        input_tokens: int,
        output_tokens: int,
        cached_input_tokens: int,
    ) -> "TokenUsageSample":
        return TokenUsageSample(
            input_tokens=self.input_tokens + max(0, int(input_tokens)),
            output_tokens=self.output_tokens + max(0, int(output_tokens)),
            cached_input_tokens=self.cached_input_tokens
            + max(0, int(cached_input_tokens)),
        )

    def to_dict(self) -> dict[str, int]:
        return {
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cached_input_tokens": self.cached_input_tokens,
        }


@dataclass(frozen=True)
class ContextBudgetSnapshot:
    """完整回合结束后的上下文预算与下一请求估算。"""

    stable_context_tokens: int
    existing_summary_tokens: int
    cold_history_tokens: int
    recent_history_tokens: int
    next_user_reserve_tokens: int
    target_summary_tokens: int
    estimated_next_input_tokens: int
    post_turn_context_tokens: int
    simulated_compacted_input_tokens: int
    potential_retired_tokens: int
    trigger_context_tokens: int
    context_window_tokens: int
    trigger_reached: bool
    emergency_ratio_reached: bool
    cache_hit_ratio: float

    def to_dict(self) -> dict[str, Any]:
        return {
            "stable_context_tokens": self.stable_context_tokens,
            "existing_summary_tokens": self.existing_summary_tokens,
            "cold_history_tokens": self.cold_history_tokens,
            "recent_history_tokens": self.recent_history_tokens,
            "next_user_reserve_tokens": self.next_user_reserve_tokens,
            "target_summary_tokens": self.target_summary_tokens,
            "estimated_next_input_tokens": self.estimated_next_input_tokens,
            "post_turn_context_tokens": self.post_turn_context_tokens,
            "simulated_compacted_input_tokens": self.simulated_compacted_input_tokens,
            "potential_retired_tokens": self.potential_retired_tokens,
            "trigger_context_tokens": self.trigger_context_tokens,
            "context_window_tokens": self.context_window_tokens,
            "trigger_reached": self.trigger_reached,
            "emergency_ratio_reached": self.emergency_ratio_reached,
            "cache_hit_ratio": self.cache_hit_ratio,
        }


@dataclass(frozen=True)
class SourceEvent:
    """从 Session 事实源复制出的只读事件。"""

    event_id: str
    type: str
    payload: Mapping[str, Any]

    def to_prompt_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "type": self.type,
            "payload": dict(self.payload),
        }

    def to_index_dict(self, *, preview_chars: int = 200) -> dict[str, Any]:
        """ID、类型与短预览：正文留在复用的原文上下文里。"""

        serialized = json.dumps(
            dict(self.payload), ensure_ascii=False, separators=(",", ":")
        )
        if 0 < preview_chars < len(serialized):
            serialized = serialized[:preview_chars] + "…"
        return {"event_id": self.event_id, "type": self.type, "preview": serialized}


@dataclass(frozen=True)
class CompactionBatch:
    """本次交给摘要模型的完整事件批次。

    ``recent_events`` 是压缩后仍以原文保留的回合。当前策略「压缩即丢弃」不保留
    任何原文（投影只留摘要与最终回复锚点），批量选择器恒返回空元组；字段与
    ``ContextAssembler`` 的对应入参仅为兼容既有调用方保留。
    """

    events: tuple[SourceEvent, ...]
    recent_events: tuple[SourceEvent, ...]
    previous_summary: Mapping[str, Any] | None = None
    previous_covered_event_ids: tuple[str, ...] = ()
    single_large_turn: bool = False

    @property
    def covered_event_ids(self) -> tuple[str, ...]:
        ordered = [*self.previous_covered_event_ids]
        ordered.extend(event.event_id for event in self.events)
        return tuple(dict.fromkeys(item for item in ordered if item))


@dataclass(frozen=True)
class AutoCompactionDecision:
    should_compact: bool
    reason: str


@dataclass(frozen=True)
class SummaryModelResponse:
    """模型调用适配器返回的最小供应商无关结果。"""

    content: str
    usage: TokenUsageSample = field(default_factory=TokenUsageSample)
    profile: str = ""
    provider: str = ""
    tool_calls: int = 0


SummaryModelCall = Callable[[Sequence[Mapping[str, Any]]], SummaryModelResponse]


@dataclass(frozen=True)
class ModelSummaryResult:
    """已解析但尚未通过来源校验的模型结构化结果。"""

    structured: Mapping[str, Any]
    usage: TokenUsageSample
    profile: str
    provider: str
    attempts: int = 1


@dataclass(frozen=True)
class SummaryValidationResult:
    valid: bool
    errors: tuple[str, ...] = ()
    normalized: Mapping[str, Any] | None = None


@dataclass(frozen=True)
class ContextCompactionOutcome:
    """service 的一次完整编排结果；Session I/O 仍由组合根执行。"""

    measurement_payload: Mapping[str, Any]
    compact_payload: Mapping[str, Any] | None = None
    history_projection: tuple[dict[str, Any], ...] | None = None
    fallback_required: bool = False
    diagnostic: str = ""
