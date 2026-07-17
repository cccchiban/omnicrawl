"""从父 Session additive 事件重建跨进程 SubAgent 任务快照。

恢复语义刻意收窄：

1. 只恢复控制面可查询的安全任务快照（list/get）；
2. 终态 ``completed/failed/cancelled`` 按事件原文恢复；
3. 进程崩溃时仍停留在 ``queued/running/waiting_approval`` 的任务标记为
   ``failed`` + ``SUBAGENT_INTERRUPTED``，绝不自动重跑；
4. 不恢复审批请求、模型 Runtime、Fork 上下文、原始 prompt 或隐藏推理。
"""

from __future__ import annotations

import re
from datetime import datetime
from typing import Any, Mapping, Sequence

from ...state.session_artifacts import redact_sensitive_text

_TASK_ID_PATTERN = re.compile(r"^task-[a-f0-9]{12}$")
_BATCH_ID_PATTERN = re.compile(r"^batch-[a-f0-9]{12}$")
_TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled"})
_ACTIVE_STATUSES = frozenset({"queued", "running", "waiting_approval", "cancelling"})
_EVENT_STATUS_MAP = {
    "subagent_task_queued": "queued",
    "subagent_task_started": "running",
    "subagent_task_waiting_approval": "waiting_approval",
    "subagent_task_completed": "completed",
    "subagent_task_failed": "failed",
    "subagent_task_cancelled": "cancelled",
    "subagent_task_partial": "failed",
    "subagent_task_timed_out": "failed",
}


def rebuild_task_snapshots_from_session_events(
    events: Sequence[Any],
    *,
    owner_id: str,
    session_id: str,
) -> list[dict[str, Any]]:
    """把会话事件折叠为可导入 TaskManager 的终态快照列表。

    同一 ``task_id`` 以最后一条合法生命周期事件为准；非 SubAgent 事件、坏
    ``task_id`` 与缺字段 payload 被静默忽略，避免损坏转录阻断会话恢复。
    """

    records: dict[str, dict[str, Any]] = {}
    for event in events:
        event_type = str(getattr(event, "type", "") or "").strip()
        if event_type not in _EVENT_STATUS_MAP:
            continue
        payload = getattr(event, "payload", None)
        if not isinstance(payload, Mapping):
            continue
        task_id = str(payload.get("task_id", "") or "").strip()
        if not _TASK_ID_PATTERN.fullmatch(task_id):
            continue
        created_at = _event_timestamp(event)
        status = _normalize_status(
            payload.get("status"),
            fallback=_EVENT_STATUS_MAP[event_type],
        )
        current = records.get(task_id)
        if current is None:
            records[task_id] = _new_record(
                task_id=task_id,
                payload=payload,
                status=status,
                created_at=created_at,
                owner_id=owner_id,
                session_id=session_id,
            )
            continue
        current["updated_at"] = created_at
        current["status"] = status
        description = _safe_text(payload.get("description"), limit=120)
        if description:
            current["description"] = description
        agent_type = _safe_text(payload.get("agent_type"), limit=80)
        if agent_type:
            current["agent_type"] = agent_type
        batch_id = str(payload.get("batch_id", "") or "").strip()
        if _BATCH_ID_PATTERN.fullmatch(batch_id):
            current["batch_id"] = batch_id
        if status in _TERMINAL_STATUSES:
            current["result"] = _terminal_result(payload, status=status)
            current["error"] = _bound_error(payload.get("error"), status=status)
        else:
            current["result"] = None
            current["error"] = None

    snapshots: list[dict[str, Any]] = []
    for record in records.values():
        status = str(record["status"])
        if status not in _TERMINAL_STATUSES:
            record["status"] = "failed"
            record["result"] = {
                "status": "failed",
                "summary": "任务在进程重启前未完成。",
                "artifacts": [],
                "usage": {},
                "recovered": True,
            }
            record["error"] = {
                "code": "SUBAGENT_INTERRUPTED",
                "message": "任务在进程重启时中断，未自动重跑。",
            }
        elif record.get("result") is None and status == "completed":
            record["result"] = _terminal_result({}, status="completed")
        snapshots.append(record)

    snapshots.sort(key=lambda item: (float(item["created_at"]), str(item["task_id"])))
    return snapshots


def _new_record(
    *,
    task_id: str,
    payload: Mapping[str, Any],
    status: str,
    created_at: float,
    owner_id: str,
    session_id: str,
) -> dict[str, Any]:
    batch_id = str(payload.get("batch_id", "") or "").strip()
    if not _BATCH_ID_PATTERN.fullmatch(batch_id):
        batch_id = f"batch-{task_id[5:]}"
    record: dict[str, Any] = {
        "task_id": task_id,
        "batch_id": batch_id,
        "owner_id": str(owner_id),
        "session_id": str(session_id),
        "description": _safe_text(payload.get("description"), limit=120)
        or "SubAgent 任务",
        "agent_type": _safe_text(payload.get("agent_type"), limit=80) or "unknown",
        "status": status,
        "result": None,
        "error": None,
        "created_at": created_at,
        "updated_at": created_at,
    }
    if status in _TERMINAL_STATUSES:
        record["result"] = _terminal_result(payload, status=status)
        record["error"] = _bound_error(payload.get("error"), status=status)
    return record


def _terminal_result(payload: Mapping[str, Any], *, status: str) -> dict[str, Any]:
    artifacts = payload.get("artifacts")
    if not isinstance(artifacts, (list, tuple)):
        artifacts = []
    usage = payload.get("usage")
    if not isinstance(usage, Mapping):
        usage = {}
    result = payload.get("result")
    summary = ""
    if isinstance(result, Mapping):
        summary = _safe_text(result.get("summary"), limit=6000)
        if not artifacts and isinstance(result.get("artifacts"), (list, tuple)):
            artifacts = list(result.get("artifacts") or [])
        if not usage and isinstance(result.get("usage"), Mapping):
            usage = dict(result.get("usage") or {})
    if not summary:
        summary = _safe_text(payload.get("summary"), limit=6000)
    return {
        "status": status,
        "summary": summary,
        "artifacts": list(artifacts)[:16],
        "usage": dict(usage),
        "recovered": True,
    }


def _bound_error(error: Any, *, status: str) -> dict[str, str] | None:
    if status == "completed":
        return None
    if isinstance(error, Mapping):
        code = _safe_text(error.get("code"), limit=80) or "SUBAGENT_ERROR"
        message = _safe_text(error.get("message"), limit=500) or "子任务失败。"
        return {"code": code, "message": message}
    if status == "cancelled":
        return {"code": "SUBAGENT_CANCELLED", "message": "任务已取消。"}
    if status == "failed":
        return {"code": "SUBAGENT_ERROR", "message": "子任务失败。"}
    return None


def _normalize_status(raw: Any, *, fallback: str) -> str:
    status = str(raw or "").strip().casefold()
    if status in _TERMINAL_STATUSES or status in _ACTIVE_STATUSES:
        return status
    if fallback == "running" and status == "started":
        return "running"
    return fallback


def _safe_text(value: Any, *, limit: int) -> str:
    text = redact_sensitive_text(str(value or "")).strip()
    if len(text) > limit:
        return text[:limit]
    return text


def _event_timestamp(event: Any) -> float:
    created_at = getattr(event, "created_at", None)
    if isinstance(created_at, datetime):
        try:
            return float(created_at.timestamp())
        except (OverflowError, OSError, ValueError):
            pass
    payload = getattr(event, "payload", None)
    if isinstance(payload, Mapping):
        raw = payload.get("timestamp")
        if isinstance(raw, (int, float)) and not isinstance(raw, bool):
            return float(raw)
    return 0.0


__all__ = ["rebuild_task_snapshots_from_session_events"]
