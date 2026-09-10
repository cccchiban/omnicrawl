"""Agent 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from ...session import COMPACT_SUMMARY_PREFIX
from ...state.session_projection import complete_tool_pairing


COMPACT_SNIPPET_CHARS = 360
COMPACT_MAX_BULLETS = 4


@dataclass(frozen=True)
class CompactHistoryResult:
    summary: str
    compacted_message_count: int
    recent_messages: list[dict[str, Any]]


def restore_history_window(
    messages: list[dict[str, Any]],
    *,
    max_history_turns: int,
) -> list[dict[str, Any]]:
    """恢复会话历史窗口：完整继承，不再按轮硬裁剪。

    会话内多轮对话要求完整继承（含 assistant tool_calls 与 tool 结果原文），
    且历史必须「只追加、不改写前缀」才能持续命中 Provider 前缀缓存。因此这里
    只做协议层规范化：为缺失结果的工具调用补「已中断」占位、丢弃没有配对调用
    来源的孤立 tool 消息；压缩摘要边界保持首位由投影层保证，不做任何裁剪。

    ``max_history_turns`` 仍保留，仅用于确定性压缩的窗口判断（见
    ``compact_history``），不再参与恢复裁剪：任何按轮截断都会让重启后的
    历史前缀缩短，使已发送消息的缓存前缀周期性失效。
    """

    if not messages:
        return []
    del max_history_turns  # 显式声明：恢复不再使用该参数裁剪历史
    return complete_tool_pairing([dict(message) for message in messages])


def compact_history(
    messages: list[dict[str, Any]],
    *,
    max_history_turns: int,
    force: bool = False,
) -> CompactHistoryResult | None:
    """计算需要压缩的历史窗口和确定性摘要；不负责写会话事件。

    切分点必须落在「轮」边界（user 消息）上：历史现在包含 assistant
    tool_calls 与 tool 结果消息，按消息条数硬切会把一次工具调用与其结果拆开，
    产生 Provider 会拒收的非法协议。

    自动压缩（``force=False``）保留最后 ``max_history_turns`` 轮，轮数不足
    时不压缩；手动压缩（``force=True``）保留最后一轮，确保用户显式请求一定
    生效。首条为压缩摘要时，摘要会并入新的压缩窗口，其内容通过
    ``previous_summary`` 继续保留。
    """

    if len(messages) <= 1:
        return None
    has_leading_summary = bool(
        messages and str(messages[0].get("content") or "").strip().startswith(COMPACT_SUMMARY_PREFIX)
    )
    body_start = 1 if has_leading_summary else 0
    turn_starts = [
        index
        for index in range(body_start, len(messages))
        if messages[index].get("role") == "user"
    ]
    if not turn_starts:
        return None
    keep_turns = max_history_turns if max_history_turns > 0 else 1
    keep_index = (len(turn_starts) - 1) if force else (len(turn_starts) - keep_turns)
    if keep_index <= 0:
        return None
    compact_count = turn_starts[keep_index]
    if compact_count <= body_start:
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
