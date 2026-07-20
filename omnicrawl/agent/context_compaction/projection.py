"""结构化摘要、最近原文和工具事件的模型历史投影。"""

from __future__ import annotations

import json
from typing import Any, Mapping, Sequence

from .models import SourceEvent


COMPACT_SUMMARY_PREFIX = "会话压缩摘要：\n"


class ContextAssembler:
    """构建摘要在前、最近原文在后的稳定模型上下文。"""

    def assemble(
        self,
        structured: Mapping[str, Any],
        recent_events: Sequence[SourceEvent],
    ) -> tuple[dict[str, Any], ...]:
        summary = render_summary_markdown(structured)
        messages = [
            {"role": "assistant", "content": f"{COMPACT_SUMMARY_PREFIX}{summary}"}
        ]
        messages.extend(
            message
            for event in recent_events
            if (message := event_to_model_message(event)) is not None
        )
        return tuple(messages)

    @staticmethod
    def recent_message_count(events: Sequence[SourceEvent]) -> int:
        return sum(event_to_model_message(event) is not None for event in events)


def render_summary_markdown(structured: Mapping[str, Any]) -> str:
    lines = ["## 结构化工作摘要"]
    _append_plain(lines, "当前目标", structured.get("objective", []))
    _append_referenced(lines, "约束", structured.get("constraints", []))
    _append_referenced(lines, "已确认决策", structured.get("decisions", []))
    _append_referenced(lines, "已完成与验证", structured.get("completed", []))
    _append_plain(lines, "当前状态", structured.get("current_state", []))
    _append_referenced(lines, "未完成事项与风险", structured.get("open_issues", []))
    _append_referenced(lines, "文件、命令与产物", structured.get("artifacts", []))
    _append_referenced(lines, "精确证据", structured.get("exact_evidence", []))
    return "\n".join(lines)


def event_to_model_message(event: SourceEvent) -> dict[str, Any] | None:
    payload = event.payload
    if event.type in {"user_message", "assistant_message"}:
        content = payload.get("content", "")
        if isinstance(content, str) and content.strip():
            return {
                "role": "user" if event.type == "user_message" else "assistant",
                "content": content,
            }
        return None
    if event.type == "tool_call_requested":
        tool = str(payload.get("tool") or "").strip()
        if not tool:
            return None
        arguments = payload.get("arguments", {})
        arguments = arguments if isinstance(arguments, dict) else {}
        return {
            "role": "assistant",
            "content": (
                f"工具调用请求：{tool} 参数："
                f"{json.dumps(arguments, ensure_ascii=False, sort_keys=True)}"
            ),
        }
    if event.type in {"tool_result", "tool_call_denied"}:
        tool = str(payload.get("tool") or "").strip()
        if not tool:
            return None
        if event.type == "tool_call_denied":
            reason = str(payload.get("reason") or "未批准。").strip()
            return {"role": "assistant", "content": f"工具执行结果：{tool} 失败，原因：{reason}"}
        output = payload.get("model_output")
        if not isinstance(output, str) or not output.strip():
            output = payload.get("output_preview")
        if not isinstance(output, str) or not output.strip():
            output = payload.get("output", "")
        status = "成功" if bool(payload.get("ok", False)) else "失败"
        return {
            "role": "assistant",
            "content": f"工具执行结果：{tool} {status}\n{str(output).strip()}".strip(),
        }
    return None


def _append_plain(lines: list[str], title: str, items: Any) -> None:
    lines.append(f"### {title}")
    values = items if isinstance(items, list) else []
    if not values:
        lines.append("- 无")
        return
    lines.extend(f"- {str(item).strip()}" for item in values)


def _append_referenced(lines: list[str], title: str, items: Any) -> None:
    lines.append(f"### {title}")
    values = items if isinstance(items, list) else []
    if not values:
        lines.append("- 无")
        return
    for item in values:
        if not isinstance(item, dict):
            continue
        text = str(item.get("text") or "").strip()
        refs = item.get("source_event_ids", [])
        ref_text = ", ".join(str(ref) for ref in refs) if isinstance(refs, list) else ""
        lines.append(f"- {text}（来源：{ref_text}）")
