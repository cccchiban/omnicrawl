"""下一请求预算、完整回合批次选择和自动压缩策略。"""

from __future__ import annotations

import json
import math
import re
from typing import Any, Iterable, Mapping, Sequence

from .models import (
    AutoCompactionDecision,
    CompactionBatch,
    ContextBudgetSnapshot,
    SourceEvent,
    TokenUsageSample,
)


_COMPACT_SUMMARY_PREFIX = "会话压缩摘要：\n"
_MODEL_CONTEXT_EVENT_TYPES = {
    "user_message",
    "assistant_message",
    "tool_call_requested",
    "tool_call_denied",
    "tool_result",
}
_CJK_PATTERN = re.compile(
    r"[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\u3040-\u30ff\uac00-\ud7af]"
)


class ContextBudgetManager:
    """本地、确定性且无第三方 tokenizer 依赖的上下文预算管理器。"""

    def measure(
        self,
        *,
        system_prompt: str,
        context_messages: Sequence[Mapping[str, Any]],
        history_messages: Sequence[Mapping[str, Any]],
        tool_schemas: Sequence[Mapping[str, Any]],
        recent_turns: int,
        target_summary_tokens: int,
        next_user_reserve_tokens: int,
        trigger_context_tokens: int,
        context_window_tokens: int,
        usage: TokenUsageSample,
        emergency_context_ratio: float = 0.85,
    ) -> ContextBudgetSnapshot:
        if recent_turns <= 0:
            raise ValueError("recent_turns 必须是正整数。")

        history = list(history_messages)
        summary_messages: list[Mapping[str, Any]] = []
        if history and _is_compact_summary(history[0]):
            summary_messages.append(history.pop(0))

        cutoff = _recent_turn_cutoff(history, recent_turns)
        cold_messages = history[:cutoff]
        recent_messages = history[cutoff:]
        stable_context_tokens = estimate_text_tokens(system_prompt)
        stable_context_tokens += estimate_messages_tokens(context_messages)
        stable_context_tokens += estimate_json_tokens(tool_schemas)

        return self.measure_from_token_counts(
            stable_context_tokens=stable_context_tokens,
            existing_summary_tokens=estimate_messages_tokens(summary_messages),
            cold_history_tokens=estimate_messages_tokens(cold_messages),
            recent_history_tokens=estimate_messages_tokens(recent_messages),
            next_user_reserve_tokens=next_user_reserve_tokens,
            target_summary_tokens=target_summary_tokens,
            trigger_context_tokens=trigger_context_tokens,
            context_window_tokens=context_window_tokens,
            usage=usage,
            emergency_context_ratio=emergency_context_ratio,
        )

    def measure_from_token_counts(
        self,
        *,
        stable_context_tokens: int,
        existing_summary_tokens: int,
        cold_history_tokens: int,
        recent_history_tokens: int,
        next_user_reserve_tokens: int,
        target_summary_tokens: int,
        trigger_context_tokens: int,
        context_window_tokens: int,
        usage: TokenUsageSample,
        emergency_context_ratio: float = 0.85,
    ) -> ContextBudgetSnapshot:
        counts = {
            "stable_context_tokens": stable_context_tokens,
            "existing_summary_tokens": existing_summary_tokens,
            "cold_history_tokens": cold_history_tokens,
            "recent_history_tokens": recent_history_tokens,
            "next_user_reserve_tokens": next_user_reserve_tokens,
            "target_summary_tokens": target_summary_tokens,
            "trigger_context_tokens": trigger_context_tokens,
            "context_window_tokens": context_window_tokens,
        }
        for name, value in counts.items():
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise ValueError(f"{name} 必须是非负整数。")
        if trigger_context_tokens <= 0 or context_window_tokens <= 0:
            raise ValueError("触发阈值和上下文窗口必须是正整数。")
        if not 0 < emergency_context_ratio < 1:
            raise ValueError("emergency_context_ratio 必须满足 0 < value < 1。")

        estimated_next_input_tokens = (
            stable_context_tokens
            + existing_summary_tokens
            + cold_history_tokens
            + recent_history_tokens
            + next_user_reserve_tokens
        )
        compactable_tokens = existing_summary_tokens + cold_history_tokens
        simulated_summary_tokens = min(compactable_tokens, target_summary_tokens)
        potential_retired_tokens = max(0, compactable_tokens - simulated_summary_tokens)
        cache_hit_ratio = (
            min(usage.cached_input_tokens, usage.input_tokens) / usage.input_tokens
            if usage.input_tokens > 0
            else 0.0
        )
        emergency_tokens = math.ceil(context_window_tokens * emergency_context_ratio)
        return ContextBudgetSnapshot(
            stable_context_tokens=stable_context_tokens,
            existing_summary_tokens=existing_summary_tokens,
            cold_history_tokens=cold_history_tokens,
            recent_history_tokens=recent_history_tokens,
            next_user_reserve_tokens=next_user_reserve_tokens,
            target_summary_tokens=target_summary_tokens,
            estimated_next_input_tokens=estimated_next_input_tokens,
            simulated_compacted_input_tokens=(
                estimated_next_input_tokens - potential_retired_tokens
            ),
            potential_retired_tokens=potential_retired_tokens,
            trigger_context_tokens=trigger_context_tokens,
            context_window_tokens=context_window_tokens,
            trigger_reached=estimated_next_input_tokens >= trigger_context_tokens,
            emergency_ratio_reached=estimated_next_input_tokens >= emergency_tokens,
            cache_hit_ratio=cache_hit_ratio,
        )

    def select_batch(
        self,
        events: Sequence[SourceEvent],
        *,
        recent_turns: int,
        recent_token_budget: int | None = None,
        manual: bool = False,
        allow_single_large_turn: bool = False,
    ) -> CompactionBatch | None:
        """从最后摘要边界之后选择完整回合，不截断工具链或未完成回合。"""

        if recent_turns <= 0:
            raise ValueError("recent_turns 必须是正整数。")
        boundary_index = -1
        previous_summary: Mapping[str, Any] | None = None
        previous_covered: tuple[str, ...] = ()
        for index, event in enumerate(events):
            if event.type != "compact_summary":
                continue
            boundary_index = index
            previous_summary = event.payload
            covered = event.payload.get("covered_event_ids", [])
            if isinstance(covered, list):
                previous_covered = tuple(
                    item for item in covered if isinstance(item, str) and item
                )

        carried_events: list[SourceEvent] = []
        if boundary_index >= 0 and previous_summary is not None:
            remaining_ids = previous_summary.get("remaining_event_ids", [])
            if isinstance(remaining_ids, list) and remaining_ids:
                remaining_set = {
                    item for item in remaining_ids if isinstance(item, str) and item
                }
                carried_events = [
                    event for event in events[:boundary_index] if event.event_id in remaining_set
                ]
            else:
                remaining_count = previous_summary.get("remaining_message_count", 0)
                if isinstance(remaining_count, int) and remaining_count > 0:
                    model_events = [
                        event
                        for event in events[:boundary_index]
                        if event.type in _MODEL_CONTEXT_EVENT_TYPES
                    ]
                    carried_events = model_events[-remaining_count:]
        turns = _complete_turns([*carried_events, *events[boundary_index + 1 :]])
        if manual:
            if len(turns) < 2:
                return None
            compact_turns = turns[:-1]
            recent = turns[-1:]
            single_large_turn = False
        else:
            retain_count = min(len(turns), recent_turns)
            if recent_token_budget is not None:
                if recent_token_budget <= 0:
                    raise ValueError("recent_token_budget 必须是正整数。")
                retained_tokens = 0
                retain_count = 0
                for turn in reversed(turns[-recent_turns:]):
                    turn_tokens = estimate_json_tokens(
                        [event.to_prompt_dict() for event in turn]
                    )
                    if retain_count > 0 and retained_tokens + turn_tokens > recent_token_budget:
                        break
                    if retain_count == 0 and turn_tokens > recent_token_budget:
                        break
                    retained_tokens += turn_tokens
                    retain_count += 1
            compact_count = len(turns) - retain_count
            if compact_count > 0:
                compact_turns = turns[:compact_count]
                recent = turns[compact_count:]
                single_large_turn = len(turns) == 1 and retain_count == 0
            elif allow_single_large_turn and len(turns) == 1:
                compact_turns = turns
                recent = []
                single_large_turn = True
            else:
                return None

        compact_events = tuple(event for turn in compact_turns for event in turn)
        if not compact_events:
            return None
        return CompactionBatch(
            events=compact_events,
            recent_events=tuple(event for turn in recent for event in turn),
            previous_summary=previous_summary,
            previous_covered_event_ids=previous_covered,
            single_large_turn=single_large_turn,
        )

    @staticmethod
    def decide_auto_compaction(
        snapshot: ContextBudgetSnapshot,
        batch: CompactionBatch | None,
        *,
        turns_since_last_model_compaction: int | None,
        minimum_turns_between_model_compactions: int,
    ) -> AutoCompactionDecision:
        if not snapshot.trigger_reached:
            return AutoCompactionDecision(False, "below_trigger_threshold")
        if batch is None:
            return AutoCompactionDecision(False, "no_complete_batch")
        if turns_since_last_model_compaction is None:
            return AutoCompactionDecision(True, "first_model_compaction")
        if turns_since_last_model_compaction >= minimum_turns_between_model_compactions:
            return AutoCompactionDecision(True, "cooldown_satisfied")
        if snapshot.emergency_ratio_reached:
            return AutoCompactionDecision(True, "emergency_bypass", True)
        return AutoCompactionDecision(False, "cooldown_active")

    @staticmethod
    def turns_since_last_model_compaction(
        events: Sequence[SourceEvent],
    ) -> int | None:
        """返回距上次自动模型尝试的完整回合数。

        成功摘要和自动模型失败都建立冷却边界，避免摘要服务异常时每个回合
        连续产生付费重试；手动 `/compact --model` 失败不改变自动冷却。
        """

        last_index: int | None = None
        for index, event in enumerate(events):
            model_summary = event.type == "compact_summary" and bool(
                event.payload.get("model_generated", False)
            )
            automatic_failure = event.type == "context_compaction_failed" and (
                event.payload.get("mode") == "automatic_model"
            )
            if model_summary or automatic_failure:
                last_index = index
        if last_index is None:
            return None
        return len(_complete_turns(events[last_index + 1 :]))


def estimate_messages_tokens(messages: Iterable[Mapping[str, Any]]) -> int:
    total = 0
    for message in messages:
        total += 4
        total += estimate_text_tokens(str(message.get("role") or ""))
        total += estimate_value_tokens(message.get("content", ""))
        for key in ("name", "tool_call_id", "tool_calls"):
            if key in message:
                total += estimate_value_tokens(message[key])
    return total


def estimate_value_tokens(value: Any) -> int:
    return estimate_text_tokens(value) if isinstance(value, str) else estimate_json_tokens(value)


def estimate_json_tokens(value: Any) -> int:
    try:
        serialized = json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    except (TypeError, ValueError):
        serialized = str(value)
    return estimate_text_tokens(serialized)


def estimate_text_tokens(text: str) -> int:
    if not text:
        return 0
    cjk_count = len(_CJK_PATTERN.findall(text))
    return cjk_count + math.ceil((len(text) - cjk_count) / 4)


def _recent_turn_cutoff(messages: Sequence[Mapping[str, Any]], recent_turns: int) -> int:
    user_indexes = [
        index for index, message in enumerate(messages) if message.get("role") == "user"
    ]
    if len(user_indexes) <= recent_turns:
        return 0
    return user_indexes[-recent_turns]


def _complete_turns(events: Sequence[SourceEvent]) -> list[tuple[SourceEvent, ...]]:
    turns: list[tuple[SourceEvent, ...]] = []
    pending: list[SourceEvent] = []
    for event in events:
        if event.type == "user_message":
            pending = [event]
            continue
        if not pending:
            continue
        pending.append(event)
        if event.type == "assistant_message":
            turns.append(tuple(pending))
            pending = []
    return turns


def _is_compact_summary(message: Mapping[str, Any]) -> bool:
    return str(message.get("content") or "").strip().startswith(
        _COMPACT_SUMMARY_PREFIX.strip()
    )
