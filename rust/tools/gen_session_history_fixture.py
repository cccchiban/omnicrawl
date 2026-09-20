#!/usr/bin/env python3
"""生成有状态投影对照数据集，供 Rust 侧 `omnicrawl-session::history` 的 parity 测试使用。

期望值来自 Python 真实现 `omnicrawl/state/session_projection.py` 的
`TurnHistoryProjector` 与 `project_history_messages` / `project_session_history` /
`project_compaction_boundary_history`。

用法：``python rust/tools/gen_session_history_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/session_history_parity.json``
"""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/session_history_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state import session_projection as P  # noqa: E402
from omnicrawl.state.session_models import SessionEvent  # noqa: E402

if not Path(P.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

SESSION_ID = "20260918-030529-abcdef"
CREATED_AT = "2026-09-18T03:05:29.123456+00:00"


def event(event_id: str, event_type: str, payload: dict | None = None) -> SessionEvent:
    return SessionEvent.from_dict(
        {
            "version": 1,
            "session_id": SESSION_ID,
            "event_id": event_id,
            "parent_id": None,
            "type": event_type,
            "created_at": CREATED_AT,
            "payload": payload or {},
        }
    )


def event_dict(event_id: str, event_type: str, payload: dict | None = None) -> dict:
    return {
        "version": 1,
        "session_id": SESSION_ID,
        "event_id": event_id,
        "parent_id": None,
        "type": event_type,
        "created_at": CREATED_AT,
        "payload": payload or {},
    }


def build(specs: list[tuple[str, str, dict]]) -> list[SessionEvent]:
    return [event(event_id, event_type, payload) for event_id, event_type, payload in specs]


def entries_to_json(entries) -> list:
    return [[anchor, message] for anchor, message in entries]


def history_case(name: str, specs: list[tuple[str, str, dict]]) -> dict:
    return {
        "name": name,
        "events": [event_dict(*spec) for spec in specs],
        "expected": entries_to_json(P.project_history_messages(build(specs))),
    }


def session_case(name: str, specs: list[tuple[str, str, dict]]) -> dict:
    return {
        "name": name,
        "events": [event_dict(*spec) for spec in specs],
        "expected": entries_to_json(P.project_session_history(build(specs))),
    }


def call(call_id: str, tool: str, arguments: dict, **extra) -> dict:
    payload = {
        "tool": tool,
        "tool_call_id": call_id,
        "function_name": extra.pop("function_name", tool),
        "arguments": arguments,
    }
    payload.update(extra)
    return payload


def provider_case(name: str, specs: list[tuple[str, str, dict]], provider: dict) -> dict:
    """带「arguments 原文」提供者的投影：provider 表描述每个 call_id 的返回形态。"""

    def stub(call_id: str, _tool: str):
        entry = provider.get(call_id)
        if not isinstance(entry, dict):
            return None
        kind = entry.get("kind")
        if kind == "raise":
            raise RuntimeError("提供者取原文失败")
        if kind == "empty":
            return ""
        if kind == "text":
            return entry.get("text", "")
        return None

    projector = P.TurnHistoryProjector(raw_arguments_provider=stub)
    for item in build(specs):
        projector.feed(item)
    return {
        "name": name,
        "events": [event_dict(*spec) for spec in specs],
        "provider": provider,
        "expected": entries_to_json(projector.take()),
    }


CASES = [
    history_case(
        "plain_messages",
        [
            ("e1", "user_message", {"content": "第一个问题"}),
            ("e2", "assistant_message", {"content": "第一个回答", "reasoning_content": "思考"}),
            ("e3", "session_started", {}),
        ],
    ),
    history_case(
        "tool_batch_then_reply",
        [
            ("e1", "user_message", {"content": "看一下文件"}),
            (
                "e2",
                "tool_call_requested",
                call("c1", "read_file", {"path": "a.py"}, assistant_content="", assistant_reasoning_content="先读"),
            ),
            (
                "e3",
                "tool_call_requested",
                call("c2", "grep", {"pattern": "todo"}),
            ),
            ("e4", "tool_result", {"tool": "read_file", "tool_call_id": "c1", "ok": True, "output": "内容"}),
            ("e5", "tool_result", {"tool": "grep", "tool_call_id": "c2", "ok": False, "output": "没找到"}),
            ("e6", "assistant_message", {"content": "读完了"}),
        ],
    ),
    history_case(
        "missing_result_becomes_interrupted",
        [
            ("e1", "tool_call_requested", call("c1", "read_file", {"path": "a.py"})),
            ("e2", "tool_call_requested", call("c2", "grep", {"pattern": "x"})),
            ("e3", "tool_result", {"tool": "read_file", "tool_call_id": "c1", "ok": True, "output": "内容"}),
            ("e4", "user_message", {"content": "下一个问题"}),
        ],
    ),
    history_case(
        "result_without_call_id_falls_back_to_tool_name",
        [
            ("e1", "tool_call_requested", call("c1", "read_file", {"path": "a.py"})),
            ("e2", "tool_result", {"tool": "read_file", "ok": True, "output": "内容"}),
        ],
    ),
    history_case(
        "orphan_result_is_skipped",
        [("e1", "tool_result", {"tool": "read_file", "tool_call_id": "c9", "ok": True, "output": "孤儿"})],
    ),
    history_case(
        "new_batch_after_flush",
        [
            ("e1", "tool_call_requested", call("c1", "read_file", {"path": "a.py"})),
            ("e2", "tool_result", {"tool": "read_file", "tool_call_id": "c1", "ok": True, "output": "内容"}),
            ("e3", "tool_call_requested", call("c2", "grep", {"pattern": "x"})),
            ("e4", "tool_result", {"tool": "grep", "tool_call_id": "c2", "ok": True, "output": "结果"}),
        ],
    ),
    history_case(
        "denied_and_interrupted",
        [
            ("e1", "tool_call_denied", {"tool": "rm", "reason": "不允许"}),
            ("e2", "tool_call_requested", call("c1", "read_file", {"path": "a.py"})),
            ("e3", "session_interrupted", {}),
        ],
    ),
    history_case(
        "cancel_and_pause",
        [
            ("e1", "turn_cancelled", {"summary": "被取消"}),
            ("e2", "run_guard_paused", {"message": "暂停中"}),
            ("e3", "assistant_message", {"content": "恢复后继续"}),
        ],
    ),
    history_case(
        "arguments_json_preferred_and_content_null",
        [
            (
                "e1",
                "tool_call_requested",
                call("c1", "read_file", {"path": "a.py"}, arguments_json='{"path":"a.py"}'),
            ),
            ("e2", "tool_result", {"tool": "read_file", "tool_call_id": "c1", "ok": True, "output": "内容"}),
        ],
    ),
    history_case(
        "compact_summary_with_remaining_ids",
        [
            ("e1", "user_message", {"content": "旧问题"}),
            ("e2", "assistant_message", {"content": "旧回答"}),
            ("e3", "user_message", {"content": "保留的问题"}),
            ("e4", "compact_summary", {"content": "摘要", "remaining_event_ids": ["e3"]}),
            ("e5", "user_message", {"content": "压缩后的新问题"}),
        ],
    ),
    history_case(
        "compact_summary_with_remaining_count",
        [
            ("e1", "user_message", {"content": "旧问题"}),
            ("e2", "assistant_message", {"content": "旧回答"}),
            ("e3", "user_message", {"content": "保留的问题"}),
            ("e4", "compact_summary", {"content": "摘要", "remaining_message_count": 1}),
        ],
    ),
    session_case(
        "session_history_with_boundary",
        [
            ("e1", "user_message", {"content": "被压缩掉的问题"}),
            ("e2", "assistant_message", {"content": "被压缩掉的回答"}),
            ("e3", "user_message", {"content": "保留窗口里的问题"}),
            ("e4", "compact_summary", {"content": "摘要", "remaining_event_ids": ["e3"]}),
            ("e5", "assistant_message", {"content": "压缩后的回答"}),
            ("e6", "user_message", {"content": "压缩后的新问题"}),
        ],
    ),
    session_case(
        "session_history_without_boundary",
        [
            ("e1", "user_message", {"content": "问题"}),
            ("e2", "assistant_message", {"content": "回答"}),
        ],
    ),
    session_case(
        "session_history_ignores_older_boundary",
        [
            ("e1", "user_message", {"content": "第一轮"}),
            ("e2", "compact_summary", {"content": "旧摘要", "remaining_event_ids": ["e1"]}),
            ("e3", "user_message", {"content": "第二轮"}),
            ("e4", "compact_summary", {"content": "新摘要"}),
            ("e5", "assistant_message", {"content": "第三轮回答"}),
        ],
    ),
    provider_case(
        "provider_prefers_raw_arguments",
        [
            ("e1", "tool_call_requested", call("c1", "read_file", {"path": "a.py"})),
            ("e2", "tool_call_requested", call("c2", "grep", {"pattern": "x"})),
            (
                "e3",
                "tool_result",
                {"tool": "read_file", "tool_call_id": "c1", "ok": True, "output": "内容"},
            ),
            ("e4", "tool_result", {"tool": "grep", "tool_call_id": "c2", "ok": True, "output": "结果"}),
        ],
        {
            # 原文与落盘 arguments 不同：投影必须用原文。
            "c1": {"kind": "text", "text": '{"path": "a.py", "encoding": "utf-8"}'},
            # 提供者返回空串 → 回落 arguments_json。
            "c2": {"kind": "empty"},
        },
    ),
    provider_case(
        "provider_raise_and_absent_fall_back",
        [
            (
                "e1",
                "tool_call_requested",
                call("c3", "read_file", {"path": "b.py"}, arguments_json='{"path":"b.py"}'),
            ),
            ("e2", "tool_call_requested", call("c4", "grep", {"pattern": "y"})),
        ],
        {
            "c3": {"kind": "raise"},
            "c4": {"kind": "absent"},
        },
    ),
]


def main() -> None:
    boundary_payloads = [
        {"content": "摘要", "remaining_event_ids": ["e3"]},
        {"content": "摘要", "remaining_message_count": 2},
        {"content": "摘要"},
    ]
    boundary_events = [
        ("e1", "user_message", {"content": "旧问题"}),
        ("e2", "assistant_message", {"content": "旧回答"}),
        ("e3", "user_message", {"content": "保留的问题"}),
    ]
    fixture = {
        "source": ["omnicrawl/state/session_projection.py"],
        "session_id": SESSION_ID,
        "history": [case for case in CASES if "expected" in case and "events" in case and case["name"].startswith(("plain", "tool", "missing", "result", "orphan", "new_batch", "denied", "cancel", "arguments", "compact"))],
        "session_history": [case for case in CASES if case["name"].startswith("session_history")],
        "history_with_provider": [
            case for case in CASES if case["name"].startswith("provider_")
        ],
        "boundary": [
            {
                "name": "with_remaining_ids",
                "payload": boundary_payloads[0],
                "events": [event_dict(*spec) for spec in boundary_events],
                "expected": P.project_compaction_boundary_history(
                    boundary_payloads[0], build(boundary_events)
                ),
            },
            {
                "name": "with_remaining_count",
                "payload": boundary_payloads[1],
                "events": [event_dict(*spec) for spec in boundary_events],
                "expected": P.project_compaction_boundary_history(
                    boundary_payloads[1], build(boundary_events)
                ),
            },
            {
                "name": "without_window",
                "payload": boundary_payloads[2],
                "events": [event_dict(*spec) for spec in boundary_events],
                "expected": P.project_compaction_boundary_history(
                    boundary_payloads[2], build(boundary_events)
                ),
            },
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
