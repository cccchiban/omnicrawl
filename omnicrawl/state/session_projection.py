"""将持久化会话事件投影为模型上下文消息。"""

from __future__ import annotations

import json
from typing import Any

from .session_models import COMPACT_SUMMARY_PREFIX, SessionEvent, clean_title


TOOL_CALL_CONTEXT_PREFIX = "工具调用请求："
TOOL_RESULT_CONTEXT_PREFIX = "工具执行结果："
TURN_UNDONE_EVENT_TYPE = "turn_undone"
RUN_GUARD_TODO_TOOL_NAME = "update_todos"
_RUN_GUARD_PENDING_EVENTS = frozenset(
    {
        "run_guard_continue",
        "run_guard_continue_exhausted",
        "run_guard_paused",
        "turn_cancelled",
        "session_interrupted",
    }
)


def active_session_events(events: list[SessionEvent]) -> list[SessionEvent]:
    """投影回退后的有效事件，保留 JSONL 的仅追加审计特性。

    `turn_undone` 只记录被回退轮次的事件 ID。读取时统一过滤这些事件，
    使模型恢复、会话列表和客户端转录共享同一套逻辑视图。
    """

    hidden_event_ids: set[str] = set()
    for event in events:
        if event.type != TURN_UNDONE_EVENT_TYPE:
            continue
        event_ids = event.payload.get("event_ids", [])
        if not isinstance(event_ids, list):
            continue
        hidden_event_ids.update(
            event_id.strip()
            for event_id in event_ids
            if isinstance(event_id, str) and event_id.strip()
        )
    return [
        event
        for event in events
        if event.type != TURN_UNDONE_EVENT_TYPE and event.event_id not in hidden_event_ids
    ]


def session_title_from_events(
    events: list[SessionEvent],
    *,
    fallback: str = "新会话",
) -> str:
    """按现有自动标题与显式重命名规则投影当前会话标题。"""

    title = fallback
    first_user_title_applied = False
    for event in events:
        if event.type == "session_started":
            started_title = event.payload.get("title", "")
            if isinstance(started_title, str) and started_title.strip():
                title = clean_title(started_title) or title
        if not first_user_title_applied and event.type == "user_message":
            content = event.payload.get("content", "")
            if isinstance(content, str) and content.strip():
                title = clean_title(content)
                first_user_title_applied = True
        elif event.type == "session_renamed":
            renamed_title = event.payload.get("title", "")
            if isinstance(renamed_title, str) and renamed_title.strip():
                title = clean_title(renamed_title)
    return title


CANCELLED_TURN_DEFAULT_SUMMARY = "（上一回合被取消，未生成最终回复）"


def recover_run_guard_state(
    events: list[SessionEvent],
) -> tuple[str, tuple[dict[str, Any], ...]]:
    """从有效事件流恢复运行护栏需要的 pending 任务和最后一份 Todo。

    运行中的临时字段不写入 ``index.json``，而是随关键事件做 last-write-wins
    投影。这样崩溃发生在 ``pause_work``、自动续跑或 Provider 中断之后时，
    新进程仍能知道“继续”应恢复哪个任务；普通 assistant_message 没有待续
    字段时会清除 pending，避免把已经完成的任务误认为未完成。
    """

    pending_user_text = ""
    todo_items: tuple[dict[str, Any], ...] = ()
    for event in events:
        pending_user_text, todo_items = apply_run_guard_event(
            pending_user_text,
            todo_items,
            event.type,
            event.payload if isinstance(event.payload, dict) else {},
        )
    return pending_user_text, todo_items


def apply_run_guard_event(
    pending_user_text: str,
    todo_items: tuple[dict[str, Any], ...],
    event_type: str,
    payload: dict[str, Any],
) -> tuple[str, tuple[dict[str, Any], ...]]:
    """把一条新事件应用到运行护栏恢复投影。

    该增量版本与 ``recover_run_guard_state`` 共用同一规则，供 Agent 在当前
    进程追加事件后立即更新 ``SessionState``，避免每个工具事件都重新扫描整份
    JSONL。只有显式保存的 pending 字段/用户任务会改变 pending；助手最终回复
    的 ``content`` 绝不能被误当成待续任务。
    """

    if event_type == "user_message":
        pending = _pending_text_from_payload(payload, include_content=True)
        raw_todos = payload.get("todo_items")
        return pending, _normalize_todo_items(raw_todos) if isinstance(raw_todos, list) else ()

    if event_type == "tool_call_requested":
        if str(payload.get("tool") or "").strip() == RUN_GUARD_TODO_TOOL_NAME:
            arguments = payload.get("arguments")
            if isinstance(arguments, dict):
                return pending_user_text, _normalize_todo_items(arguments.get("todos"))
        return pending_user_text, todo_items

    if event_type == "tool_result":
        if str(payload.get("tool") or "").strip() == RUN_GUARD_TODO_TOOL_NAME:
            recovered = _todo_items_from_tool_result(payload)
            if recovered is not None:
                return pending_user_text, recovered
        return pending_user_text, todo_items

    if event_type in _RUN_GUARD_PENDING_EVENTS:
        candidate = _pending_text_from_payload(payload)
        next_todos = todo_items
        if isinstance(payload.get("todo_items"), list):
            next_todos = _normalize_todo_items(payload.get("todo_items"))
        return candidate or pending_user_text, next_todos

    if event_type == "assistant_message":
        # 只有未来显式提供 pending_user_text 时才保留待续状态；当前正常
        # assistant_message 的 content 是回复正文，不是任务来源。
        candidate = _pending_text_from_payload(payload, include_content=False)
        return candidate, todo_items

    return pending_user_text, todo_items


def _pending_text_from_payload(
    payload: dict[str, Any],
    *,
    include_content: bool = False,
) -> str:
    keys = ("pending_user_text", "user_text", "content") if include_content else (
        "pending_user_text",
        "user_text",
    )
    for key in keys:
        value = payload.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ""


def _normalize_todo_items(raw_todos: Any) -> tuple[dict[str, Any], ...]:
    if not isinstance(raw_todos, list):
        return ()
    normalized: list[dict[str, Any]] = []
    for index, raw_item in enumerate(raw_todos[:20], start=1):
        if not isinstance(raw_item, dict):
            continue
        step = str(
            raw_item.get("step")
            or raw_item.get("description")
            or raw_item.get("title")
            or ""
        ).strip()
        if not step:
            continue
        status = str(raw_item.get("status") or "").strip().casefold()
        completed = bool(raw_item.get("completed")) or status in {
            "completed",
            "done",
            "complete",
        }
        item_id = str(raw_item.get("id") or index).strip()[:80] or str(index)
        normalized.append(
            {
                "id": item_id,
                "step": step[:240],
                "completed": completed,
            }
        )
    return tuple(normalized)


def _todo_items_from_tool_result(payload: dict[str, Any]) -> tuple[dict[str, Any], ...] | None:
    for key in ("model_output", "output"):
        raw_output = payload.get(key)
        if not isinstance(raw_output, str) or not raw_output.strip():
            continue
        try:
            decoded = json.loads(raw_output)
        except (TypeError, ValueError):
            continue
        if isinstance(decoded, dict) and "todos" in decoded:
            return _normalize_todo_items(decoded.get("todos"))
    return None


def event_to_model_message(event: SessionEvent) -> dict[str, str] | None:
    if event.type == "user_message":
        content = event.payload.get("content", "")
        return {"role": "user", "content": content} if isinstance(content, str) and content.strip() else None
    if event.type == "assistant_message":
        content = event.payload.get("content", "")
        return {"role": "assistant", "content": content} if isinstance(content, str) and content.strip() else None
    if event.type == "turn_cancelled":
        # 取消回合必须投影为可恢复的 assistant 摘要：仅靠 user_message
        # 无法让 /resume 复现进程内历史，后续提问会丢失取消上下文。
        summary = event.payload.get("summary", "")
        if not isinstance(summary, str) or not summary.strip():
            summary = CANCELLED_TURN_DEFAULT_SUMMARY
        return {"role": "assistant", "content": summary}
    if event.type == "run_guard_paused":
        # pause_work 不产生正常最终回答，但仍需要向恢复后的模型说明上一轮
        # 在哪里停下；工具调用/结果事件会在此前分别投影，避免留下未配对协议。
        message = event.payload.get("message", "")
        if not isinstance(message, str) or not message.strip():
            message = "（上一任务已暂停，用户发送“继续”后恢复。）"
        return {"role": "assistant", "content": message.strip()}
    if event.type == "compact_summary":
        content = event.payload.get("content", "")
        if isinstance(content, str) and content.strip():
            return {"role": "assistant", "content": f"{COMPACT_SUMMARY_PREFIX}{content}"}
    if event.type == "tool_call_requested":
        content = _tool_call_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    if event.type == "tool_call_denied":
        content = _tool_denied_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    if event.type == "tool_result":
        content = _tool_result_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    return None


def _tool_call_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    arguments = payload.get("arguments", {})
    safe_arguments = arguments if isinstance(arguments, dict) else {}
    arguments_text = json.dumps(safe_arguments, ensure_ascii=False, sort_keys=True)
    return f"{TOOL_CALL_CONTEXT_PREFIX}{tool.strip()} 参数：{arguments_text}"


def _tool_denied_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    reason = payload.get("reason", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    reason_text = reason.strip() if isinstance(reason, str) and reason.strip() else "未批准。"
    return f"{TOOL_RESULT_CONTEXT_PREFIX}{tool.strip()} 失败，原因：{reason_text}"


def _tool_result_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    status = "成功" if bool(payload.get("ok", False)) else "失败"
    output = payload.get("model_output")
    if not isinstance(output, str) or not output.strip():
        output = payload.get("output_preview")
    if not isinstance(output, str) or not output.strip():
        output = payload.get("output", "")
    if not isinstance(output, str):
        output = ""

    artifact_path = payload.get("artifact_path", "")
    artifact_hint = ""
    if isinstance(artifact_path, str) and artifact_path.strip():
        artifact_hint = f"\n完整输出 artifact：{artifact_path.strip()}"
    return f"{TOOL_RESULT_CONTEXT_PREFIX}{tool.strip()} {status}\n{output.strip()}{artifact_hint}".strip()
