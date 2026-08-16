"""完整回合后的测量、模型摘要、校验、投影和降级编排。"""

from __future__ import annotations

from typing import Any, Mapping, Sequence

from .ledger import UsageLedger
from .models import (
    CompactionBatch,
    ContextCompactionOutcome,
    SourceEvent,
    TokenUsageSample,
)
from .policy import ContextBudgetManager, estimate_json_tokens
from .projection import ContextAssembler, event_to_model_message, render_summary_markdown
from .summary import ModelSummaryCompactor, SummaryGenerationError
from .validation import SummaryValidator


class ContextCompactionService:
    """编排一次压缩流程，不直接读写 Session 或依赖 Agent。"""

    def __init__(
        self,
        *,
        compactor: ModelSummaryCompactor | None = None,
        budget_manager: ContextBudgetManager | None = None,
        validator: SummaryValidator | None = None,
        assembler: ContextAssembler | None = None,
        ledger: UsageLedger | None = None,
    ) -> None:
        self._compactor = compactor
        self._budget_manager = budget_manager or ContextBudgetManager()
        self._validator = validator or SummaryValidator()
        self._assembler = assembler or ContextAssembler()
        self._ledger = ledger or UsageLedger()

    def measure_after_complete_turn(
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
        emergency_context_ratio: float,
        usage: TokenUsageSample,
    ) -> ContextMeasurementResult:
        snapshot = self._budget_manager.measure(
            system_prompt=system_prompt,
            context_messages=context_messages,
            history_messages=history_messages,
            tool_schemas=tool_schemas,
            recent_turns=recent_turns,
            target_summary_tokens=target_summary_tokens,
            next_user_reserve_tokens=next_user_reserve_tokens,
            trigger_context_tokens=trigger_context_tokens,
            context_window_tokens=context_window_tokens,
            emergency_context_ratio=emergency_context_ratio,
            usage=usage,
        )
        return ContextMeasurementResult(
            snapshot=snapshot,
            event_payload=self._ledger.measurement_payload(snapshot, usage),
        )

    def after_complete_turn(
        self,
        *,
        source_events: Sequence[SourceEvent],
        system_prompt: str,
        context_messages: Sequence[Mapping[str, Any]],
        history_messages: Sequence[Mapping[str, Any]],
        tool_schemas: Sequence[Mapping[str, Any]],
        recent_turns: int,
        recent_context_ratio: float,
        target_summary_tokens: int,
        next_user_reserve_tokens: int,
        trigger_context_tokens: int,
        context_window_tokens: int,
        emergency_context_ratio: float,
        minimum_turns_between_model_compactions: int,
        reasoning_effort: str,
        preserve_exact_evidence: bool,
        usage: TokenUsageSample,
    ) -> ContextCompactionOutcome:
        measured = self.measure_after_complete_turn(
            system_prompt=system_prompt,
            context_messages=context_messages,
            history_messages=history_messages,
            tool_schemas=tool_schemas,
            recent_turns=recent_turns,
            target_summary_tokens=target_summary_tokens,
            next_user_reserve_tokens=next_user_reserve_tokens,
            trigger_context_tokens=trigger_context_tokens,
            context_window_tokens=context_window_tokens,
            emergency_context_ratio=emergency_context_ratio,
            usage=usage,
        )
        batch = self._budget_manager.select_batch(
            source_events,
            recent_turns=recent_turns,
            recent_token_budget=max(1, int(context_window_tokens * recent_context_ratio)),
            allow_single_large_turn=measured.snapshot.trigger_reached,
        )
        turns_since = self._budget_manager.turns_since_last_model_compaction(source_events)
        decision = self._budget_manager.decide_auto_compaction(
            measured.snapshot,
            batch,
            turns_since_last_model_compaction=turns_since,
            minimum_turns_between_model_compactions=(
                minimum_turns_between_model_compactions
            ),
        )
        measurement_payload = {
            **measured.event_payload,
            "auto_decision": decision.reason,
            "turns_since_last_model_compaction": turns_since,
            "cooldown_bypassed": decision.bypassed_cooldown,
        }
        if not decision.should_compact or batch is None:
            return ContextCompactionOutcome(measurement_payload=measurement_payload)
        return self._compact_batch(
            batch,
            source_events=source_events,
            target_summary_tokens=target_summary_tokens,
            reasoning_effort=reasoning_effort,
            preserve_exact_evidence=preserve_exact_evidence,
            retired_token_estimate=max(
                measured.snapshot.potential_retired_tokens,
                estimate_json_tokens([event.to_prompt_dict() for event in batch.events]),
            ),
            manual=False,
            decision_reason=decision.reason,
            measurement_payload=measurement_payload,
        )

    def recover_from_context_overflow(
        self,
        *,
        source_events: Sequence[SourceEvent],
        target_summary_tokens: int,
        reasoning_effort: str,
        preserve_exact_evidence: bool,
    ) -> ContextCompactionOutcome:
        """压缩当前未完成回合，并返回可用于同回合重试的上下文投影。"""
        batch = self._budget_manager.select_recovery_batch(source_events)
        if batch is None:
            return ContextCompactionOutcome(
                measurement_payload={},
                fallback_required=True,
                diagnostic="没有可安全压缩的上下文超限回合。",
            )
        return self._compact_batch(
            batch,
            source_events=source_events,
            target_summary_tokens=target_summary_tokens,
            reasoning_effort=reasoning_effort,
            preserve_exact_evidence=preserve_exact_evidence,
            retired_token_estimate=estimate_json_tokens(
                [event.to_prompt_dict() for event in batch.events]
            ),
            manual=False,
            decision_reason="context_overflow_recovery",
            measurement_payload={},
        )

    def manual_compact(
        self,
        *,
        source_events: Sequence[SourceEvent],
        target_summary_tokens: int,
        reasoning_effort: str,
        preserve_exact_evidence: bool,
    ) -> ContextCompactionOutcome:
        batch = self._budget_manager.select_batch(
            source_events,
            recent_turns=1,
            manual=True,
        )
        if batch is None:
            return ContextCompactionOutcome(
                measurement_payload={},
                diagnostic="当前会话内容太少，暂不需要模型压缩。",
            )
        return self._compact_batch(
            batch,
            source_events=source_events,
            target_summary_tokens=target_summary_tokens,
            reasoning_effort=reasoning_effort,
            preserve_exact_evidence=preserve_exact_evidence,
            retired_token_estimate=estimate_json_tokens(
                [event.to_prompt_dict() for event in batch.events]
            ),
            manual=True,
            decision_reason="manual_model_compaction",
            measurement_payload={},
        )

    def _compact_batch(
        self,
        batch: CompactionBatch,
        *,
        source_events: Sequence[SourceEvent],
        target_summary_tokens: int,
        reasoning_effort: str,
        preserve_exact_evidence: bool,
        retired_token_estimate: int,
        manual: bool,
        decision_reason: str,
        measurement_payload: Mapping[str, Any],
    ) -> ContextCompactionOutcome:
        if self._compactor is None:
            return ContextCompactionOutcome(
                measurement_payload=measurement_payload,
                fallback_required=True,
                diagnostic="摘要模型调用器不可用。",
            )

        feedback: tuple[str, ...] = ()
        generation = None
        validation = None
        for _attempt in range(2):
            try:
                generation = self._compactor.compact(
                    batch,
                    target_summary_tokens=target_summary_tokens,
                    validation_feedback=feedback,
                )
            except SummaryGenerationError as exc:
                return ContextCompactionOutcome(
                    measurement_payload=measurement_payload,
                    fallback_required=True,
                    diagnostic=str(exc),
                )
            validation = self._validator.validate(
                generation.structured,
                source_events=source_events,
                target_summary_tokens=target_summary_tokens,
                previous_summary=batch.previous_summary,
                preserve_exact_evidence=preserve_exact_evidence,
                completeness_events=batch.events,
            )
            if validation.valid:
                break
            feedback = validation.errors
        if generation is None or validation is None or not validation.valid:
            return ContextCompactionOutcome(
                measurement_payload=measurement_payload,
                fallback_required=True,
                diagnostic=(
                    "结构化摘要校验失败：" + "; ".join(feedback)
                    if feedback
                    else "结构化摘要校验失败。"
                ),
            )

        assert validation.normalized is not None
        structured = validation.normalized
        content = render_summary_markdown(structured)
        projection = self._assembler.assemble(structured, batch.recent_events)
        recent_count = self._assembler.recent_message_count(batch.recent_events)
        compacted_count = sum(
            event_to_model_message(event) is not None for event in batch.events
        )
        coverage = self._coverage_metrics(batch, structured, retired_token_estimate)
        compact_payload = {
            "schema_version": 2,
            "content": content,
            "structured": structured,
            "covered_event_ids": list(batch.covered_event_ids),
            "compacted_event_ids": [event.event_id for event in batch.events],
            "coverage": coverage,
            "retired_token_estimate": retired_token_estimate,
            "summary_input_tokens": generation.usage.input_tokens,
            "summary_output_tokens": generation.usage.output_tokens,
            "cached_input_tokens": generation.usage.cached_input_tokens,
            "summary_profile": generation.profile,
            "summary_provider": generation.provider,
            "reasoning_effort": reasoning_effort,
            "quality": {
                "schema_valid": True,
                "source_refs_valid": True,
                "critical_facts_checked": True,
                "attempts": generation.attempts,
            },
            "model_generated": True,
            "manual": manual,
            "single_large_turn": batch.single_large_turn,
            "decision_reason": decision_reason,
            "compacted_message_count": compacted_count,
            "remaining_message_count": recent_count,
            "remaining_event_ids": [event.event_id for event in batch.recent_events],
        }
        return ContextCompactionOutcome(
            measurement_payload=measurement_payload,
            compact_payload=compact_payload,
            history_projection=projection,
        )

    @staticmethod
    def _coverage_metrics(
        batch: CompactionBatch,
        structured: Mapping[str, Any],
        retired_token_estimate: int,
    ) -> dict[str, Any]:
        """计算本次压缩的覆盖度指标：本批事件有多少被摘要显式引用。

        coverage_ratio 反映“该记的没记”风险：引用越少，后续恢复越依赖
        归档/证据工具。字段计数用于确认完整性校验确实产出了条目。
        """

        compacted_ids = [event.event_id for event in batch.events]
        compacted_id_set = set(compacted_ids)
        covered_set = set(batch.covered_event_ids)

        referenced: set[str] = set()

        def visit(value: Any) -> None:
            if isinstance(value, Mapping):
                refs = value.get("source_event_ids", [])
                if isinstance(refs, list):
                    referenced.update(
                        event_id
                        for event_id in refs
                        if isinstance(event_id, str) and event_id in compacted_id_set
                    )
                for nested in value.values():
                    visit(nested)
            elif isinstance(value, list):
                for nested in value:
                    visit(nested)

        visit(structured)
        compacted_count = len(compacted_ids)
        referenced_count = len(referenced)
        return {
            "compacted_event_count": compacted_count,
            "covered_event_count": sum(
                1 for event_id in compacted_ids if event_id in covered_set
            ),
            "referenced_event_count": referenced_count,
            "coverage_ratio": round(
                referenced_count / compacted_count, 4
            )
            if compacted_count
            else 0.0,
            "retired_token_estimate": retired_token_estimate,
            "field_counts": {
                field: len(
                    structured.get(field, [])
                    if isinstance(structured.get(field, []), list)
                    else []
                )
                for field in (
                    "constraints",
                    "decisions",
                    "completed",
                    "open_issues",
                    "artifacts",
                    "read_files",
                    "modified_files",
                    "failed_attempts",
                    "excluded_approaches",
                    "key_concepts",
                    "problem_solving_process",
                    "user_messages",
                    "next_steps",
                    "exact_evidence",
                )
            },
        }


# 第一阶段公开名保持兼容；实现已经扩展为完整 service。
ContextCompactionMeasurementService = ContextCompactionService


# 避免 models.py 反向依赖 service，仅在这里提供旧测试使用的小结果类型。
class ContextMeasurementResult:
    def __init__(self, *, snapshot: Any, event_payload: Mapping[str, Any]) -> None:
        self.snapshot = snapshot
        self.event_payload = dict(event_payload)
