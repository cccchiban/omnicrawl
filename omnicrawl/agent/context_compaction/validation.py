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
    "failed_attempts",
    "excluded_approaches",
    "key_concepts",
    "problem_solving_process",
    "user_messages",
    "next_steps",
)
# 新增的“过程与负信息”字段：缺失时按空数组宽容处理，不破坏旧摘要兼容；
# 完整性校验（_validate_completeness）仍会在需要覆盖时强制补齐。
_OPTIONAL_REFERENCED_FIELDS = frozenset(
    {
        "failed_attempts",
        "excluded_approaches",
        "key_concepts",
        "problem_solving_process",
        "user_messages",
        "next_steps",
    }
)
# 写类工具：与 ui/fullscreen/tool_diff.py 的 FILE_CHANGE_TOOLS 保持一致，
# 用于“该记的没记”完整性校验（被压缩窗口内成功写入的文件必须进摘要）。
_FILE_CHANGE_TOOLS = frozenset({"write_file", "replace_text"})
_FILE_LIST_FIELDS = ("read_files", "modified_files")
# 超过该长度的用户消息事件会被 summary 分块发送给摘要模型，模型拿不到完整
# 原文，因此 user_messages 强校验对这些事件豁免（与 summary 切分阈值对齐）。
_LARGE_EVENT_SPLIT_EXEMPT_CHARS = 60_000


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
        completeness_events: Sequence[SourceEvent] | None = None,
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
            # 新增字段缺失时按空数组处理（旧摘要兼容）；旧字段仍要求显式提供。
            if value is None and field in _OPTIONAL_REFERENCED_FIELDS:
                normalized[field] = normalized_items
                continue
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

        for field in _FILE_LIST_FIELDS:
            value = structured.get(field)
            normalized_items: list[dict[str, Any]] = []
            # 新增字段缺失时按空数组处理（旧摘要兼容）；完整性校验再强制覆盖。
            if value is None:
                normalized[field] = normalized_items
                continue
            if not isinstance(value, list):
                errors.append(f"{field} 必须是数组。")
                normalized[field] = normalized_items
                continue
            for index, item in enumerate(value):
                if not isinstance(item, dict):
                    errors.append(f"{field}[{index}] 必须是对象。")
                    continue
                path = item.get("path")
                description = item.get("description")
                refs = item.get("source_event_ids")
                if not isinstance(path, str) or not path.strip():
                    errors.append(f"{field}[{index}].path 必须是非空字符串。")
                    continue
                if not isinstance(description, str) or not description.strip():
                    errors.append(f"{field}[{index}].description 必须是非空字符串。")
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
                        "path": path.strip(),
                        "description": description.strip(),
                        "source_event_ids": list(dict.fromkeys(refs)),
                    }
                )
            normalized[field] = normalized_items

        self._validate_previous_constraints(previous_summary, normalized, errors)
        if preserve_exact_evidence:
            self._validate_exact_evidence(normalized, event_by_id, errors)
        else:
            normalized["exact_evidence"] = []
        self._validate_user_messages(normalized, event_by_id, errors)
        self._validate_completed_tool_chains(normalized, event_by_id, errors)
        self._validate_completeness(normalized, completeness_events, errors)
        # target_summary_tokens <= 0 表示无摘要预算上限：不因摘要长度拒绝。
        if target_summary_tokens > 0 and estimate_json_tokens(normalized) > target_summary_tokens:
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
    def _validate_user_messages(
        normalized: Mapping[str, Any],
        event_by_id: Mapping[str, SourceEvent],
        errors: list[str],
    ) -> None:
        """用户消息原文必须逐字保留：引用的事件必须是 user_message 且原文一致。"""

        for index, item in enumerate(normalized.get("user_messages", [])):
            text = str(item.get("text") or "").strip()
            refs = item.get("source_event_ids", [])
            if not text:
                errors.append(f"user_messages[{index}].text 必须是非空字符串。")
                continue
            for ref in refs:
                event = event_by_id.get(ref)
                if event is None:
                    continue
                if event.type != "user_message":
                    errors.append(
                        f"user_messages[{index}] 引用了非用户消息事件：{ref}（{event.type}）。"
                    )
                    continue
                content = event.payload.get("content")
                if (
                    isinstance(content, str)
                    and content.strip()
                    and len(content) < _LARGE_EVENT_SPLIT_EXEMPT_CHARS
                    and text != content.strip()
                ):
                    errors.append(
                        f"user_messages[{index}] 不是用户消息原文（事件 {ref}）。"
                    )

    @staticmethod
    def _validate_completeness(
        normalized: Mapping[str, Any],
        completeness_events: Sequence[SourceEvent] | None,
        errors: list[str],
    ) -> None:
        """完整性把关：被压缩窗口内成功写入的文件和失败的工具调用必须进摘要。

        只针对 ``completeness_events``（本次被压缩的事件）做检查，避免误伤
        保留窗口内的事件；未提供时跳过（向后兼容）。
        """

        if not completeness_events:
            return
        call_events: dict[str, SourceEvent] = {}
        result_by_call_id: dict[str, SourceEvent] = {}
        for event in completeness_events:
            if event.type == "tool_call_requested":
                call_events[event.event_id] = event
            elif event.type == "tool_result":
                call_id = str(event.payload.get("tool_call_id") or "")
                if call_id:
                    result_by_call_id[call_id] = event

        # 1) 成功写入的文件必须被 modified_files 覆盖（引用事件 ID 或路径匹配任一）。
        modified_items = normalized.get("modified_files", [])
        modified_refs = {
            ref for item in modified_items for ref in item.get("source_event_ids", [])
        }
        modified_paths = [
            str(item.get("path") or "").strip() for item in modified_items
        ]
        for event_id, event in call_events.items():
            if str(event.payload.get("tool") or "") not in _FILE_CHANGE_TOOLS:
                continue
            call_id = str(event.payload.get("tool_call_id") or "")
            result = result_by_call_id.get(call_id)
            if result is None or not bool(result.payload.get("ok", False)):
                # 未执行或失败：不算已修改，不要求覆盖。
                continue
            if event_id in modified_refs:
                continue
            arguments = event.payload.get("arguments")
            path = (
                str(arguments.get("path") or "").strip()
                if isinstance(arguments, Mapping)
                else ""
            )
            if path and any(
                _path_equivalent(path, candidate) for candidate in modified_paths
            ):
                continue
            errors.append(
                f"modified_files 缺少对成功写入文件的覆盖：{path or event_id}。"
            )

        # 2) 失败的工具调用必须被 failed_attempts 覆盖（引用调用或结果事件 ID）。
        failed_refs = {
            ref
            for item in normalized.get("failed_attempts", [])
            for ref in item.get("source_event_ids", [])
        }
        for event_id, event in call_events.items():
            call_id = str(event.payload.get("tool_call_id") or "")
            result = result_by_call_id.get(call_id)
            if result is None or bool(result.payload.get("ok", False)):
                continue
            if event_id in failed_refs or result.event_id in failed_refs:
                continue
            tool = str(event.payload.get("tool") or "")
            errors.append(
                f"failed_attempts 缺少对失败工具调用的覆盖：{tool}（事件 {event_id}）。"
            )

        # 3) 被压缩窗口内的用户消息必须被 user_messages 原文覆盖（引用事件 ID）。
        #    超大用户消息会被切成多个块发送给摘要模型，模型拿不到完整原文，
        #    因此长度超过切分阈值的消息不要求覆盖（与 tool 输出落盘同理）。
        user_refs = {
            ref
            for item in normalized.get("user_messages", [])
            for ref in item.get("source_event_ids", [])
        }
        for event in completeness_events:
            if event.type != "user_message":
                continue
            content = event.payload.get("content")
            if not isinstance(content, str) or not content.strip():
                # 空消息不要求覆盖。
                continue
            if len(content) >= _LARGE_EVENT_SPLIT_EXEMPT_CHARS:
                # 超过切分阈值的超大消息：模型只收到分块，无法逐字保留。
                continue
            if event.event_id in user_refs:
                continue
            errors.append(
                f"user_messages 缺少对用户消息的覆盖：{content.strip()[:40]}…（事件 {event.event_id}）。"
            )

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


def _path_equivalent(left: str, right: str) -> bool:
    """宽松路径等价：统一分隔符、去掉 ./ 前缀后比较，允许相对/绝对差异。"""

    def normalize(value: str) -> str:
        result = value.replace("\\", "/").strip()
        while result.startswith("./"):
            result = result[2:]
        return result.rstrip("/")

    left_normalized = normalize(left)
    right_normalized = normalize(right)
    if not left_normalized or not right_normalized:
        return False
    if left_normalized == right_normalized:
        return True
    # 允许一方是另一方以 / 分隔的后缀（如 a.py vs src/a.py）。
    return left_normalized.endswith("/" + right_normalized) or right_normalized.endswith(
        "/" + left_normalized
    )


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
