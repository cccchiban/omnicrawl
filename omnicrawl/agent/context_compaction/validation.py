"""结构化摘要的 Schema、来源、精确证据和长度预算校验。"""

from __future__ import annotations

from typing import Any, Iterable, Mapping, Sequence

from .models import SourceEvent, SummaryValidationResult
from .policy import estimate_json_tokens


_PLAIN_LIST_FIELDS = ("objective", "current_state")
_REFERENCED_LIST_FIELDS = (
    "constraints",
    "decisions",
    "completed",
    "open_issues",
    "artifacts",
    "exact_evidence",
)


class SummaryValidator:
    """拒绝不可恢复、无来源或把未完成工具链写成完成的摘要。"""

    def validate(
        self,
        structured: Mapping[str, Any],
        *,
        source_events: Sequence[SourceEvent],
        target_summary_tokens: int,
        previous_summary: Mapping[str, Any] | None = None,
        preserve_exact_evidence: bool = True,
    ) -> SummaryValidationResult:
        errors: list[str] = []
        normalized: dict[str, Any] = {}
        event_by_id = {event.event_id: event for event in source_events}

        for field in _PLAIN_LIST_FIELDS:
            value = structured.get(field)
            if not isinstance(value, list) or any(
                not isinstance(item, str) or not item.strip() for item in value
            ):
                errors.append(f"{field} 必须是非空字符串数组。")
                normalized[field] = []
            else:
                normalized[field] = [item.strip() for item in value]
        if not normalized.get("objective"):
            errors.append("objective 至少需要一项目标。")

        for field in _REFERENCED_LIST_FIELDS:
            value = structured.get(field)
            normalized_items: list[dict[str, Any]] = []
            if not isinstance(value, list):
                errors.append(f"{field} 必须是数组。")
                normalized[field] = normalized_items
                continue
            for index, item in enumerate(value):
                if not isinstance(item, dict):
                    errors.append(f"{field}[{index}] 必须是对象。")
                    continue
                text = item.get("text")
                refs = item.get("source_event_ids")
                if not isinstance(text, str) or not text.strip():
                    errors.append(f"{field}[{index}].text 必须是非空字符串。")
                    continue
                if not isinstance(refs, list) or not refs or any(
                    not isinstance(ref, str) or not ref for ref in refs
                ):
                    errors.append(
                        f"{field}[{index}].source_event_ids 必须是非空字符串数组。"
                    )
                    continue
                unknown = [ref for ref in refs if ref not in event_by_id]
                if unknown:
                    errors.append(
                        f"{field}[{index}] 引用了不存在的事件：{', '.join(unknown)}。"
                    )
                normalized_items.append(
                    {
                        "text": text.strip(),
                        "source_event_ids": list(dict.fromkeys(refs)),
                    }
                )
            normalized[field] = normalized_items

        self._validate_previous_constraints(previous_summary, normalized, errors)
        if preserve_exact_evidence:
            self._validate_exact_evidence(normalized, event_by_id, errors)
        else:
            normalized["exact_evidence"] = []
        self._validate_completed_tool_chains(normalized, event_by_id, errors)
        if estimate_json_tokens(normalized) > target_summary_tokens:
            errors.append("结构化摘要超过 target_summary_tokens。")
        return SummaryValidationResult(
            valid=not errors,
            errors=tuple(errors),
            normalized=normalized if not errors else None,
        )

    @staticmethod
    def _validate_previous_constraints(
        previous_summary: Mapping[str, Any] | None,
        normalized: Mapping[str, Any],
        errors: list[str],
    ) -> None:
        if previous_summary is None:
            return
        previous_structured = previous_summary.get("structured")
        if not isinstance(previous_structured, dict):
            return
        old_constraints = previous_structured.get("constraints", [])
        current_text = {
            str(item.get("text") or "")
            for item in normalized.get("constraints", [])
            if isinstance(item, dict)
        }
        decisions_text = "\n".join(
            str(item.get("text") or "")
            for item in normalized.get("decisions", [])
            if isinstance(item, dict)
        )
        for item in old_constraints if isinstance(old_constraints, list) else []:
            if not isinstance(item, dict):
                continue
            text = str(item.get("text") or "").strip()
            if text and text not in current_text and text not in decisions_text:
                errors.append(f"既有约束未保留或说明失效：{text}")

    @staticmethod
    def _validate_exact_evidence(
        normalized: Mapping[str, Any],
        event_by_id: Mapping[str, SourceEvent],
        errors: list[str],
    ) -> None:
        for index, item in enumerate(normalized.get("exact_evidence", [])):
            text = str(item.get("text") or "")
            refs = item.get("source_event_ids", [])
            source_text = "\n".join(
                value
                for ref in refs
                if ref in event_by_id
                for value in _string_values(event_by_id[ref].payload)
            )
            if text and text not in source_text:
                errors.append(f"exact_evidence[{index}] 与来源事件原文不一致。")

    @staticmethod
    def _validate_completed_tool_chains(
        normalized: Mapping[str, Any],
        event_by_id: Mapping[str, SourceEvent],
        errors: list[str],
    ) -> None:
        completed_refs = {
            ref
            for item in normalized.get("completed", [])
            for ref in item.get("source_event_ids", [])
            if isinstance(item, dict)
        }
        result_call_ids = {
            str(event.payload.get("tool_call_id") or "")
            for event in event_by_id.values()
            if event.type == "tool_result"
        }
        for ref in completed_refs:
            event = event_by_id.get(ref)
            if event is None or event.type != "tool_call_requested":
                continue
            call_id = str(event.payload.get("tool_call_id") or "")
            if call_id and call_id not in result_call_ids:
                errors.append(f"completed 引用了未完成工具调用：{call_id}。")


def _string_values(value: Any) -> Iterable[str]:
    if isinstance(value, Mapping):
        for item in value.values():
            yield from _string_values(item)
        return
    if isinstance(value, (list, tuple)):
        for item in value:
            yield from _string_values(item)
        return
    if value is not None:
        yield str(value)
