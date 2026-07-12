"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from ..session import COMPACT_SUMMARY_PREFIX


COMPACT_SNIPPET_CHARS = 360
COMPACT_MAX_BULLETS = 4


@dataclass(frozen=True)
class CompactHistoryResult:
    summary: str
    compacted_message_count: int
    recent_messages: list[dict[str, Any]]


def restore_history_window(
    messages: list[dict[str, str]],
    *,
    max_history_turns: int,
) -> list[dict[str, str]]:
    """恢复最近上下文；如果首条是摘要边界，则固定保留摘要。"""

    max_messages = max_history_turns * 2
    if not messages:
        return []
    if str(messages[0].get("content") or "").strip().startswith(COMPACT_SUMMARY_PREFIX):
        if len(messages) <= max_messages:
            return list(messages)
        recent_messages = messages[1:]
        recent_window = recent_messages[-max_messages:]
        if recent_window and recent_window[0].get("role") != "user":
            recent_window = recent_window[1:]
        return [messages[0], *recent_window]
    return messages[-max_messages:]


def compact_history(
    messages: list[dict[str, Any]],
    *,
    max_history_turns: int,
    force: bool = False,
) -> CompactHistoryResult | None:
    """计算需要压缩的历史窗口和确定性摘要；不负责写会话事件。"""

    max_messages = max_history_turns * 2
    has_leading_summary = bool(
        messages and str(messages[0].get("content") or "").strip().startswith(COMPACT_SUMMARY_PREFIX)
    )
    max_compactable = max(0, len(messages) - 2)
    if len(messages) <= max_messages:
        if not force:
            return None
        compact_count = max_compactable
        if compact_count < 2:
            return None
    else:
        compact_count = len(messages) - max_messages

    compact_count = min(compact_count, max_compactable)
    if has_leading_summary:
        if compact_count % 2 == 0:
            compact_count -= 1
        if compact_count < 3:
            return None
    else:
        if compact_count % 2 == 1:
            compact_count -= 1
        if compact_count < 2:
            return None
    if compact_count > max_compactable:
        return None

    compacted_messages = messages[:compact_count]
    recent_messages = messages[compact_count:]
    previous_summary = extract_existing_compact_summary(compacted_messages)
    summary = build_compact_summary(
        compacted_messages,
        previous_summary=previous_summary,
    )
    if not summary:
        return None
    return CompactHistoryResult(
        summary=summary,
        compacted_message_count=compact_count,
        recent_messages=recent_messages,
    )


def build_compact_summary(
    messages: list[dict[str, Any]],
    *,
    previous_summary: str = "",
) -> str:
    """按时间顺序生成可恢复摘要，保留目标、进展和最近状态。"""

    user_items: list[str] = []
    assistant_items: list[str] = []
    for message in messages:
        content = str(message.get("content") or "").strip()
        if not content:
            continue
        if content.startswith(COMPACT_SUMMARY_PREFIX):
            continue
        snippet = compact_snippet(content)
        if message.get("role") == "user":
            user_items.append(snippet)
        elif message.get("role") == "assistant":
            assistant_items.append(snippet)

    lines = ["## 会话压缩摘要"]
    if previous_summary:
        lines.append(f"- 既有摘要：{compact_snippet(previous_summary)}")
    if user_items:
        lines.append(f"- 原始目标：{user_items[0]}")
    if len(user_items) > 1:
        lines.append("- 已压缩的用户后续要求：" + format_compact_items(user_items[1:]))
    if assistant_items:
        lines.append("- 已完成/已回复要点：" + format_compact_items(assistant_items))
    if user_items or assistant_items:
        latest = assistant_items[-1] if assistant_items else user_items[-1]
        lines.append(f"- 压缩前状态：最近一条可见进展为「{latest}」。")
    lines.append("- 下一步：继续以用户最新输入为最高优先级，并结合本摘要后的最近对话。")
    return "\n".join(lines)


def extract_existing_compact_summary(messages: list[dict[str, Any]]) -> str:
    for message in messages:
        content = str(message.get("content") or "").strip()
        if content.startswith(COMPACT_SUMMARY_PREFIX):
            return content[len(COMPACT_SUMMARY_PREFIX) :].strip()
    return ""


def format_compact_items(items: list[str]) -> str:
    selected = items[:COMPACT_MAX_BULLETS]
    suffix = f"；另有 {len(items) - len(selected)} 条已省略" if len(items) > len(selected) else ""
    return "；".join(selected) + suffix


def compact_snippet(content: str) -> str:
    text = " ".join(content.split())
    if len(text) <= COMPACT_SNIPPET_CHARS:
        return text
    return text[: COMPACT_SNIPPET_CHARS - 3] + "..."
