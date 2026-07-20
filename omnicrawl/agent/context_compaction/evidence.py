"""按当前有效摘要引用恢复 Session 精确证据。"""

from __future__ import annotations

import json
from collections.abc import Callable, Mapping, Sequence
from typing import Any

from .models import SourceEvent
from .policy import estimate_json_tokens, estimate_text_tokens


RECALL_SESSION_EVIDENCE_TOOL_NAME = "recall_session_evidence"
DEFAULT_EVIDENCE_MAX_ITEMS = 8
DEFAULT_EVIDENCE_MAX_OUTPUT_TOKENS = 4_000
_MAX_EVENT_ID_CHARS = 128
_ARTIFACT_METADATA_KEYS = {
    "type",
    "title",
    "size_chars",
    "output_size_chars",
    "html_size_chars",
    "sha256",
    "output_sha256",
    "html_sha256",
    "truncated",
    "artifact_truncated",
    "redacted",
    "storage",
}

ArtifactReader = Callable[[str], str]


class SessionEvidenceRecallService:
    """只恢复最后一个有效摘要授权的当前 Session 事件。"""

    def __init__(
        self,
        *,
        max_items: int = DEFAULT_EVIDENCE_MAX_ITEMS,
        max_output_tokens: int = DEFAULT_EVIDENCE_MAX_OUTPUT_TOKENS,
    ) -> None:
        if isinstance(max_items, bool) or not isinstance(max_items, int) or max_items <= 0:
            raise ValueError("max_items 必须是正整数。")
        if (
            isinstance(max_output_tokens, bool)
            or not isinstance(max_output_tokens, int)
            or max_output_tokens <= 0
        ):
            raise ValueError("max_output_tokens 必须是正整数。")
        self.max_items = max_items
        self.max_output_tokens = max_output_tokens

    def recall(
        self,
        *,
        events: Sequence[SourceEvent],
        event_ids: Any,
        artifact_reader: ArtifactReader,
    ) -> dict[str, Any]:
        requested, diagnostics, request_truncated = self._normalize_event_ids(event_ids)
        latest_summary = next(
            (event for event in reversed(events) if event.type == "compact_summary"),
            None,
        )
        authorized_ids = (
            _summary_source_event_ids(latest_summary.payload)
            if latest_summary is not None
            else set()
        )
        event_by_id = {event.event_id: event for event in events}
        item_budget = max(
            64,
            (self.max_output_tokens - 600 - len(requested) * 80)
            // max(1, len(requested)),
        )

        items: list[dict[str, Any]] = []
        content_truncated = False
        for event_id in requested:
            if latest_summary is None:
                items.append(
                    _diagnostic_item(
                        event_id,
                        "no_active_summary",
                        "当前 Session 没有可用于证据恢复的有效摘要。",
                    )
                )
                continue
            if event_id not in authorized_ids:
                items.append(
                    _diagnostic_item(
                        event_id,
                        "unauthorized",
                        "事件未被当前有效摘要引用。",
                    )
                )
                continue
            event = event_by_id.get(event_id)
            if event is None:
                items.append(
                    _diagnostic_item(
                        event_id,
                        "missing",
                        "当前有效事件流中找不到该摘要来源事件。",
                    )
                )
                continue

            item = self._event_item(
                event,
                item_budget=item_budget,
                artifact_reader=artifact_reader,
            )
            items.append(item)
            content_truncated = content_truncated or bool(item["content_truncated"])
            content_truncated = content_truncated or any(
                bool(artifact.get("content_truncated", False))
                for artifact in item["artifacts"]
            )

        result: dict[str, Any] = {
            "schema_version": 1,
            "ok": any(item.get("status") == "ok" for item in items),
            "summary_event_id": latest_summary.event_id if latest_summary is not None else None,
            "requested_count": len(requested),
            "items": items,
            "diagnostics": diagnostics,
            "truncated": request_truncated or content_truncated,
            "budget": {
                "max_items": self.max_items,
                "max_output_tokens": self.max_output_tokens,
            },
            "estimated_tokens": 0,
        }
        self._fit_result_budget(result)
        result["estimated_tokens"] = estimate_json_tokens(result)
        # estimated_tokens 自身会使序列化长度变化几个字符，再做一次最终守卫。
        if result["estimated_tokens"] > self.max_output_tokens:
            self._fit_result_budget(result)
            result["estimated_tokens"] = estimate_json_tokens(result)
        return result

    def _normalize_event_ids(
        self,
        event_ids: Any,
    ) -> tuple[list[str], list[dict[str, Any]], bool]:
        diagnostics: list[dict[str, Any]] = []
        if not isinstance(event_ids, list):
            return [], [
                {
                    "code": "invalid_event_ids",
                    "message": "event_ids 必须是字符串数组。",
                }
            ], False

        normalized: list[str] = []
        seen: set[str] = set()
        for index, raw_event_id in enumerate(event_ids):
            if not isinstance(raw_event_id, str) or not raw_event_id.strip():
                diagnostics.append(
                    {
                        "code": "invalid_event_id",
                        "index": index,
                        "message": "事件 ID 必须是非空字符串。",
                    }
                )
                continue
            event_id = raw_event_id.strip()
            if len(event_id) > _MAX_EVENT_ID_CHARS:
                diagnostics.append(
                    {
                        "code": "invalid_event_id",
                        "index": index,
                        "message": f"事件 ID 长度不能超过 {_MAX_EVENT_ID_CHARS}。",
                    }
                )
                continue
            if event_id in seen:
                continue
            seen.add(event_id)
            normalized.append(event_id)

        truncated = len(normalized) > self.max_items
        if truncated:
            diagnostics.append(
                {
                    "code": "item_limit_exceeded",
                    "message": f"单次最多恢复 {self.max_items} 个事件，其余引用未处理。",
                    "omitted_count": len(normalized) - self.max_items,
                }
            )
            normalized = normalized[: self.max_items]
        return normalized, diagnostics, truncated

    def _event_item(
        self,
        event: SourceEvent,
        *,
        item_budget: int,
        artifact_reader: ArtifactReader,
    ) -> dict[str, Any]:
        artifact_references = _artifact_references(event.payload)
        event_content_budget = item_budget if not artifact_references else max(48, item_budget // 3)
        serialized_payload = json.dumps(
            event.payload,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )
        content, content_was_truncated = _truncate_to_token_budget(
            serialized_payload,
            event_content_budget,
        )

        artifacts: list[dict[str, Any]] = []
        artifact_budget = max(
            32,
            (item_budget - estimate_text_tokens(content) - 40)
            // max(1, len(artifact_references)),
        )
        for path, metadata in artifact_references:
            artifact: dict[str, Any] = {
                "path": path,
                "metadata": metadata,
            }
            try:
                artifact_text = artifact_reader(path)
            except UnicodeDecodeError:
                artifact.update(
                    {
                        "status": "metadata_only",
                        "diagnostic": {
                            "code": "artifact_not_text",
                            "message": "artifact 不是 UTF-8 文本，仅返回元数据。",
                        },
                    }
                )
            except Exception:  # 读取端负责路径与当前 Session 归属校验
                artifact.update(
                    {
                        "status": "metadata_only",
                        "diagnostic": {
                            "code": "artifact_unreadable",
                            "message": "artifact 不存在、越界或不可读取，仅返回元数据。",
                        },
                    }
                )
            else:
                artifact_content, artifact_truncated = _truncate_to_token_budget(
                    artifact_text,
                    artifact_budget,
                )
                artifact.update(
                    {
                        "status": "ok",
                        "content": artifact_content,
                        "content_truncated": artifact_truncated,
                    }
                )
            artifacts.append(artifact)

        return {
            "event_id": event.event_id,
            "event_type": event.type,
            "status": "ok",
            "content": content,
            "content_truncated": content_was_truncated,
            "artifacts": artifacts,
        }

    def _fit_result_budget(self, result: dict[str, Any]) -> None:
        """最终按实际 JSON Token 估算收紧内容，避免元数据挤破总预算。"""

        for _attempt in range(32):
            if estimate_json_tokens(result) <= self.max_output_tokens:
                return
            candidates: list[tuple[dict[str, Any], str]] = []
            for item in result.get("items", []):
                if isinstance(item.get("content"), str):
                    candidates.append((item, "content"))
                for artifact in item.get("artifacts", []):
                    if isinstance(artifact.get("content"), str):
                        candidates.append((artifact, "content"))
            if not candidates:
                return
            target, key = max(candidates, key=lambda candidate: len(candidate[0][candidate[1]]))
            current = target[key]
            if len(current) <= 32:
                return
            target[key] = current[: max(16, len(current) * 3 // 4)] + "\n... 证据已按总预算截断。"
            target["content_truncated"] = True
            result["truncated"] = True


def _summary_source_event_ids(payload: Mapping[str, Any]) -> set[str]:
    source_ids: set[str] = set()
    covered = payload.get("covered_event_ids", [])
    if isinstance(covered, list):
        source_ids.update(
            event_id.strip()
            for event_id in covered
            if isinstance(event_id, str) and event_id.strip()
        )

    def visit(value: Any) -> None:
        if isinstance(value, Mapping):
            refs = value.get("source_event_ids", [])
            if isinstance(refs, list):
                source_ids.update(
                    event_id.strip()
                    for event_id in refs
                    if isinstance(event_id, str) and event_id.strip()
                )
            for nested in value.values():
                visit(nested)
        elif isinstance(value, list):
            for nested in value:
                visit(nested)

    structured = payload.get("structured")
    if isinstance(structured, Mapping):
        visit(structured)
    return source_ids


def _artifact_references(payload: Mapping[str, Any]) -> list[tuple[str, dict[str, Any]]]:
    references: list[tuple[str, dict[str, Any]]] = []
    seen: set[str] = set()

    def visit(value: Any) -> None:
        if isinstance(value, Mapping):
            raw_path = value.get("artifact_path")
            if isinstance(raw_path, str) and raw_path.strip():
                path = raw_path.strip()
                if path not in seen:
                    seen.add(path)
                    metadata = {
                        key: nested
                        for key, nested in value.items()
                        if key in _ARTIFACT_METADATA_KEYS
                        and isinstance(nested, (str, int, float, bool))
                    }
                    references.append((path, metadata))
            for nested in value.values():
                visit(nested)
        elif isinstance(value, list):
            for nested in value:
                visit(nested)

    visit(payload)
    return references


def _diagnostic_item(event_id: str, status: str, message: str) -> dict[str, Any]:
    return {
        "event_id": event_id,
        "status": status,
        "diagnostic": {"code": status, "message": message},
    }


def _truncate_to_token_budget(text: str, max_tokens: int) -> tuple[str, bool]:
    value = str(text or "")
    if estimate_text_tokens(value) <= max_tokens:
        return value, False
    suffix = "\n... 证据已截断，可按更少的事件 ID 分批恢复。"
    low = 0
    high = len(value)
    while low < high:
        middle = (low + high + 1) // 2
        candidate = value[:middle] + suffix
        if estimate_text_tokens(candidate) <= max_tokens:
            low = middle
        else:
            high = middle - 1
    return value[:low] + suffix, True


__all__ = [
    "DEFAULT_EVIDENCE_MAX_ITEMS",
    "DEFAULT_EVIDENCE_MAX_OUTPUT_TOKENS",
    "RECALL_SESSION_EVIDENCE_TOOL_NAME",
    "SessionEvidenceRecallService",
]
