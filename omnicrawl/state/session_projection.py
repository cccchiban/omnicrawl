"""将持久化会话事件投影为模型上下文消息。"""

from __future__ import annotations

import json
from typing import Any

from .session_models import COMPACT_SUMMARY_PREFIX, SessionEvent


TOOL_CALL_CONTEXT_PREFIX = "工具调用请求："
TOOL_RESULT_CONTEXT_PREFIX = "工具执行结果："


def event_to_model_message(event: SessionEvent) -> dict[str, str] | None:
    if event.type == "user_message":
        content = event.payload.get("content", "")
        return {"role": "user", "content": content} if isinstance(content, str) and content.strip() else None
    if event.type == "assistant_message":
        content = event.payload.get("content", "")
        return {"role": "assistant", "content": content} if isinstance(content, str) and content.strip() else None
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
