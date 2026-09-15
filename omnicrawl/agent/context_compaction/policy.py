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
        provider_input_tokens: int = 0,
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
            provider_input_tokens=provider_input_tokens,
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
        provider_input_tokens: int = 0,
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
        if (
            isinstance(provider_input_tokens, bool)
            or not isinstance(provider_input_tokens, int)
            or provider_input_tokens < 0
        ):
            raise ValueError("provider_input_tokens 必须是非负整数。")

        # 预估下一请求输入必须包含全部历史：压缩前冷历史仍原样进入下一次模型
        # 请求（working_messages = 稳定上下文 + 完整 _history + 下一条用户消息）。
        # 漏算冷历史会让预估远低于真实请求，长会话里触发阈值永远达不到，上下文
        # 越过窗口也不压缩，只能等上下文超限再走溢出恢复。
        # 已存在的压缩摘要单独计入 existing_summary_tokens，不会重复累加。
        estimated_next_input_tokens = (
            stable_context_tokens
            + existing_summary_tokens
            + cold_history_tokens
            + recent_history_tokens
            + next_user_reserve_tokens
        )
        # compactable_tokens 是真正会被本次压缩替换的“可压缩体积”：既有摘要与冷
        # 历史都会被新摘要取代；近回合与稳定上下文不在压缩窗口内。
        compactable_tokens = existing_summary_tokens + cold_history_tokens
        # target_summary_tokens <= 0 表示无摘要预算上限：模拟阶段无法预估实际摘要
        # 大小，保守按“不承诺节省”处理（potential_retired_tokens=0），避免
        # simulated_compacted_input_tokens 给出过分乐观的压缩后预算。
        if target_summary_tokens <= 0:
            simulated_summary_tokens = compactable_tokens
        else:
            simulated_summary_tokens = min(compactable_tokens, target_summary_tokens)
        potential_retired_tokens = max(0, compactable_tokens - simulated_summary_tokens)
        cache_hit_ratio = (
            min(usage.cached_input_tokens, usage.input_tokens) / usage.input_tokens
            if usage.input_tokens > 0
            else 0.0
        )
        # 紧急比仍按预估的下一次请求（含预留）判断，作为窗口溢出前的兜底。
        emergency_tokens = math.ceil(context_window_tokens * emergency_context_ratio)
        # 自动压缩触发口径＝回合结束后实际上下文（稳定上下文 + 既有摘要 + 全部
        # 历史），不含下一轮用户预留：预留只影响下一请求预估与紧急比。
        # 本地估算把非 CJK 文本按 4 字符 1 token 折算，代码/JSON 密集的工具结果
        # 会低估近一半，因此与供应商回报的最近一次请求输入取大值：后者是同一
        # 口径的真实值（已含系统提示、工具定义与缓存命中），0 表示没有可用数据。
        post_turn_context_tokens = max(
            stable_context_tokens
            + existing_summary_tokens
            + cold_history_tokens
            + recent_history_tokens,
            provider_input_tokens,
        )
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
            post_turn_context_tokens=post_turn_context_tokens,
            trigger_reached=post_turn_context_tokens >= trigger_context_tokens,
            emergency_ratio_reached=estimated_next_input_tokens >= emergency_tokens,
            cache_hit_ratio=cache_hit_ratio,
            provider_input_tokens=provider_input_tokens,
        )

    def select_recovery_batch(
        self,
        events: Sequence[SourceEvent],
    ) -> CompactionBatch | None:
        """选择由上下文超限中断的未完成回合，供一次性恢复重试使用。

        与常规自动压缩不同，这里允许最后一条 ``user_message`` 尚未配对
        ``assistant_message``。调用方必须确保失败发生在任何工具执行和可见输出之前，
        否则重试可能重复外部副作用或拼接两段回复。
        """
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

        candidates = [*carried_events, *events[boundary_index + 1 :]]
        recoverable_events = [
            event for event in candidates if event.type in _MODEL_CONTEXT_EVENT_TYPES
        ]
        if not recoverable_events or recoverable_events[-1].type != "user_message":
            return None
        return CompactionBatch(
            events=tuple(recoverable_events),
            recent_events=(),
            previous_summary=previous_summary,
            previous_covered_event_ids=previous_covered,
            single_large_turn=True,
        )

    def select_batch(
        self,
        events: Sequence[SourceEvent],
    ) -> CompactionBatch | None:
        """从最后摘要边界之后选择全部完整回合，不截断工具链或未完成回合。

        压缩即丢弃：摘要必须覆盖整个窗口，投影只保留「摘要 + 最终回复锚点」，
        因此这里不再保留任何原文回合。若仍保留尾部若干回合，它们既不在摘要
        覆盖范围内，也不会被下次压缩重新纳入，等于在上下文里留下第二份无人
        负责的历史。``recent_turns`` 仍参与触发预估的热窗口划分（``measure``），
        但不再决定压缩后保留哪些原文。
        """

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
        if not turns:
            return None
        compact_events = tuple(event for turn in turns for event in turn)
        if not compact_events:
            return None
        return CompactionBatch(
            events=compact_events,
            recent_events=(),
            previous_summary=previous_summary,
            previous_covered_event_ids=previous_covered,
            single_large_turn=len(turns) == 1,
        )

    @staticmethod
    def decide_auto_compaction(
        snapshot: ContextBudgetSnapshot,
        batch: CompactionBatch | None,
    ) -> AutoCompactionDecision:
        if not snapshot.trigger_reached:
            return AutoCompactionDecision(False, "below_trigger_threshold")
        if batch is None:
            return AutoCompactionDecision(False, "no_complete_batch")
        return AutoCompactionDecision(True, "trigger_reached")


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
