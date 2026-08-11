"""将持久化会话事件投影为模型上下文消息。"""

from __future__ import annotations

import json
from typing import Any

from .session_models import COMPACT_SUMMARY_PREFIX, SessionEvent, clean_title


TOOL_CALL_CONTEXT_PREFIX = "工具调用请求："
TOOL_RESULT_CONTEXT_PREFIX = "工具执行结果："
TURN_UNDONE_EVENT_TYPE = "turn_undone"


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
