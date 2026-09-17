#!/usr/bin/env python3
"""生成会话投影对照数据集，供 Rust 侧 `omnicrawl-session::projection` 的 parity 测试使用。

期望值来自 Python 真实现 `omnicrawl/state/session_projection.py` 的纯函数部分：
回退过滤、标题投影、运行护栏状态、事件 → 模型消息、工具结果消息与协议配对补全。

用法：``python rust/tools/gen_session_projection_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/session_projection_parity.json``
"""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-session/tests/fixtures/session_projection_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.state import session_projection as P  # noqa: E402
from omnicrawl.state.session_models import SessionEvent  # noqa: E402

if not Path(P.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

SESSION_ID = "20260918-030529-abcdef"
CREATED_AT = "2026-09-18T03:05:29.123456+00:00"


def event(event_id: str, event_type: str, payload: dict) -> SessionEvent:
    return SessionEvent(
        version=1,
        session_id=SESSION_ID,
        event_id=event_id,
        parent_id=None,
        type=event_type,
        created_at=datetime(2026, 9, 18, 3, 5, 29, 123456, tzinfo=timezone.utc),
        payload=payload,
    )


def event_dict(event_id: str, event_type: str, payload: dict) -> dict:
    """同一份事件在 fixture 里以字典形式记录，Rust 侧据此重建。"""
    return {
        "version": 1,
        "session_id": SESSION_ID,
        "event_id": event_id,
        "parent_id": None,
        "type": event_type,
        "created_at": CREATED_AT,
        "payload": payload,
    }


def build(specs: list[tuple[str, str, dict]]) -> list[SessionEvent]:
    return [event(event_id, event_type, payload) for event_id, event_type, payload in specs]


def active_case(name: str, specs: list[tuple[str, str, dict]]) -> dict:
    events = build(specs)
    return {
        "name": name,
        "events": [event_dict(*spec) for spec in specs],
        "expected_ids": [item.event_id for item in P.active_session_events(events)],
    }


def title_case(name: str, specs: list[tuple[str, str, dict]]) -> dict:
    return {
        "name": name,
        "events": [event_dict(*spec) for spec in specs],
        "expected": P.session_title_from_events(build(specs)),
    }


def run_guard_case(name: str, specs: list[tuple[str, str, dict]]) -> dict:
    pending, todos = P.recover_run_guard_state(build(specs))
    return {
        "name": name,
        "events": [event_dict(*spec) for spec in specs],
        "expected_pending": pending,
        "expected_todos": list(todos),
    }


def message_case(name: str, event_type: str, payload: dict) -> dict:
    return {
        "name": name,
        "event": event_dict("e1", event_type, payload),
        "expected": P.event_to_model_message(event("e1", event_type, payload)),
    }


PAIRING_CASES = [
    {
        "name": "paired",
        "messages": [
            {"role": "user", "content": "看一下"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [P.function_tool_call("c1", "read_file", '{"path": "a.py"}')],
            },
            P.tool_result_message("read_file", True, "内容", "c1"),
        ],
    },
    {
        "name": "missing_result",
        "messages": [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    P.function_tool_call("c1", "read_file", "{}"),
                    P.function_tool_call("c2", "grep", "{}"),
                ],
            },
            P.tool_result_message("read_file", True, "内容", "c1"),
        ],
    },
    {
        "name": "orphan_and_mismatched",
        "messages": [
            P.tool_result_message("read_file", True, "孤儿", "c9"),
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [P.function_tool_call("c1", "read_file", "{}")],
            },
            P.tool_result_message("grep", True, "错配", "c7"),
            P.tool_result_message("read_file", True, "正确", "c1"),
        ],
    },
    {
        "name": "call_without_id",
        "messages": [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"type": "function", "function": {"name": "grep", "arguments": "{}"}}
                ],
            }
        ],
    },
]


def payload_text_case(name: str, payload: dict) -> dict:
    return {
        "name": name,
        "payload": payload,
        "expected": P.tool_result_output_text(payload),
    }


def main() -> None:
    fixture = {
        "source": ["omnicrawl/state/session_projection.py"],
        "session_id": SESSION_ID,
        "active_events": [
            active_case(
                "undo_hides_ids",
                [
                    ("e1", "user_message", {"content": "第一轮"}),
                    ("e2", "assistant_message", {"content": "回复"}),
                    ("e3", "turn_undone", {"event_ids": ["e1", "e2", "  ", 5, ""]}),
                    ("e4", "user_message", {"content": "第二轮"}),
                ],
            ),
            active_case(
                "undo_without_list",
                [
                    ("e1", "user_message", {"content": "问题"}),
                    ("e2", "turn_undone", {"event_ids": "e1"}),
                ],
            ),
            active_case("no_undo", [("e1", "user_message", {"content": "只有一条"})]),
        ],
        "titles": [
            title_case(
                "started_then_user",
                [
                    ("e1", "session_started", {"title": "  初始   标题 "}),
                    ("e2", "user_message", {"content": "第一个问题"}),
                    ("e3", "user_message", {"content": "第二个问题"}),
                ],
            ),
            title_case(
                "renamed_after_user",
                [
                    ("e1", "session_started", {"title": "标题"}),
                    ("e2", "user_message", {"content": "问题"}),
                    ("e3", "session_renamed", {"title": "  改名后  "}),
                ],
            ),
            title_case("empty_stream", []),
            title_case(
                "blank_user_message",
                [
                    ("e1", "user_message", {"content": "   "}),
                    ("e2", "user_message", {"content": "有效问题"}),
                ],
            ),
        ],
        "run_guard": [
            run_guard_case(
                "user_message_sets_pending_and_todos",
                [
                    (
                        "e1",
                        "user_message",
                        {
                            "content": "做点事",
                            "todo_items": [
                                {"id": 1, "step": " 第一步 ", "completed": True},
                                {"description": "第二步", "status": "DONE"},
                                {"title": "第三步"},
                                {"step": "   "},
                                "不是对象",
                            ],
                        },
                    )
                ],
            ),
            run_guard_case(
                "assistant_message_keeps_pending_but_clears_content",
                [
                    ("e1", "assistant_message", {"content": "回复正文", "user_text": " 待续 "}),
                    ("e2", "assistant_message", {"content": "只有正文"}),
                ],
            ),
            run_guard_case(
                "tool_call_then_tool_result",
                [
                    (
                        "e1",
                        "tool_call_requested",
                        {
                            "tool": "update_todos",
                            "arguments": {"todos": [{"step": "甲", "completed": False}]},
                        },
                    ),
                    (
                        "e2",
                        "tool_result",
                        {
                            "tool": "update_todos",
                            "ok": True,
                            "model_output": json.dumps({"todos": [{"step": "乙"}]}),
                        },
                    ),
                ],
            ),
            run_guard_case(
                "other_tool_result_does_not_change_todos",
                [
                    ("e1", "tool_result", {"tool": "read_file", "output": "{}"}),
                    ("e2", "tool_result", {"tool": "update_todos", "output": "不是 JSON"}),
                ],
            ),
            run_guard_case(
                "pending_events_override",
                [
                    ("e1", "user_message", {"content": "原始任务"}),
                    ("e2", "run_guard_paused", {"pending_user_text": " 暂停时的待续 "}),
                    ("e3", "run_guard_continue", {"user_text": "继续"}),
                    ("e4", "turn_cancelled", {"summary": "被取消"}),
                ],
            ),
        ],
        "messages": [
            message_case("user", "user_message", {"content": "问题"}),
            message_case("user_blank", "user_message", {"content": "  "}),
            message_case(
                "assistant_with_reasoning",
                "assistant_message",
                {"content": "回复", "reasoning_content": "想了下"},
            ),
            message_case("assistant_empty_reasoning", "assistant_message", {"content": "回复", "reasoning_content": ""}),
            message_case("cancelled_with_summary", "turn_cancelled", {"summary": "  自定义总结 "}),
            message_case("cancelled_default", "turn_cancelled", {}),
            message_case("paused", "run_guard_paused", {"message": "  已暂停 "}),
            message_case("paused_default", "run_guard_paused", {}),
            message_case("compact", "compact_summary", {"content": "摘要正文"}),
            message_case("compact_blank", "compact_summary", {"content": " "}),
            message_case("tool_call", "tool_call_requested", {"tool": "read_file", "arguments": {"b": 2, "a": 1}}),
            message_case("tool_call_no_args", "tool_call_requested", {"tool": "read_file"}),
            message_case("tool_call_no_tool", "tool_call_requested", {"arguments": {}}),
            message_case("denied", "tool_call_denied", {"tool": "rm", "reason": "  不允许 "}),
            message_case("denied_default_reason", "tool_call_denied", {"tool": "rm"}),
            message_case(
                "result_with_artifact",
                "tool_result",
                {
                    "tool": "read_file",
                    "ok": False,
                    "model_output": "  模型可见输出  ",
                    "output": "原始输出",
                    "artifact_path": " artifacts/x.txt ",
                },
            ),
            message_case(
                "result_preview_fallback",
                "tool_result",
                {"tool": "grep", "ok": True, "output_preview": "预览", "output": "原始"},
            ),
            message_case("result_output_only", "tool_result", {"tool": "grep", "output": "原始"}),
            message_case("ignored_type", "todo_update", {"todos": []}),
        ],
        "payload_text": [
            payload_text_case("model_output_first", {"model_output": "M", "output_preview": "P", "output": "O"}),
            payload_text_case("preview_second", {"output_preview": "P", "output": "O"}),
            payload_text_case("output_only", {"output": "O"}),
            payload_text_case("blank_values", {"model_output": "  ", "output_preview": ""}),
            payload_text_case("nothing", {}),
        ],
        "tool_messages": [
            {
                "name": "format_ok",
                "kind": "format",
                "tool": "read_file",
                "ok": True,
                "output": "内容",
                "expected": P.format_tool_result_content("read_file", True, "内容"),
            },
            {
                "name": "format_failed",
                "kind": "format",
                "tool": "grep",
                "ok": False,
                "output": "",
                "expected": P.format_tool_result_content("grep", False, ""),
            },
            {
                "name": "result_message",
                "kind": "result_message",
                "tool": "read_file",
                "ok": True,
                "output": "内容",
                "tool_call_id": "c1",
                "expected": P.tool_result_message("read_file", True, "内容", "c1"),
            },
            {
                "name": "interrupted",
                "kind": "interrupted",
                "tool": "read_file",
                "tool_call_id": "c1",
                "expected": P.interrupted_tool_result_message("read_file", "c1"),
            },
            {
                "name": "function_call",
                "kind": "function_call",
                "call_id": "c1",
                "function_name": "read_file",
                "arguments": chr(123) + chr(34) + "a" + chr(34) + ": 1" + chr(125),
                "expected": P.function_tool_call(
                    "c1", "read_file", chr(123) + chr(34) + "a" + chr(34) + ": 1" + chr(125)
                ),
            },
        ],
        "pairing": [
            {"name": case["name"], "messages": case["messages"], "expected": P.complete_tool_pairing(case["messages"])}
            for case in PAIRING_CASES
        ],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    counts = {key: len(value) for key, value in fixture.items() if isinstance(value, list)}
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：{counts}")


if __name__ == "__main__":
    main()
