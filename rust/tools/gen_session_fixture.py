#!/usr/bin/env python3
"""生成会话层模型对照数据集，供 Rust 侧 `omnicrawl-session` 的 parity 测试使用。

期望值来自 Python 真实现 `omnicrawl/state/session_models.py`：会话 id / 事件类型 / 相对路径的
校验、标题折叠、时间戳解析与格式化、会话事件与索引条目的字段校验，以及会话转录里一行的字节布局。

用法：``python rust/tools/gen_session_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/session_models_parity.json``
"""

from __future__ import annotations

import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-session/tests/fixtures/session_models_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.state import session_models as M  # noqa: E402

if not Path(M.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{M.__file__}")


SESSION_IDS = [
    "20260918-030529-abcdef",
    " 20260918-030529-abcdef ",
    "2026-09-18-030529-abcdef",
    "20260918-030529-ABCDEF",
    "20260918-030529-abcde",
    "20260918_030529_abcdef",
    "",
    "   ",
    12345678,
]

EVENT_TYPES = [
    "user_message",
    "assistant_message",
    "compact_summary",
    " a_b ",
    "a",
    "a" + "b" * 63,
    "a" + "b" * 64,
    "9bad",
    "Bad",
    "has-dash",
    "",
    "   ",
    42,
]

RELATIVE_PATHS = [
    "sessions/20260918-030529-abcdef.jsonl",
    "sessions\\20260918-030529-abcdef.jsonl",
    "sessions/upper.JSONL",
    "../escape.jsonl",
    "sessions/../../escape.jsonl",
    "/absolute/x.jsonl",
    "sessions/notes.txt",
    "sessions/plain",
    ".jsonl",
    "",
    "   ",
    42,
]

TITLES = [
    "  多个   空白\t 折叠  ",
    "短标题",
    "字" * 60,
    "字" * 61,
    "字" * 80,
    "\n换行\n标题\n",
    "",
]

DATETIMES = [
    "2026-09-18T03:05:29+00:00",
    "2026-09-18T11:05:29+08:00",
    "2026-09-18T03:05:29.123456+00:00",
    "2026-09-18T03:05:29.123+00:00",
    "2026-09-18T03:05:29Z",
    "2026-09-18 03:05:29",
    "2026-09-18t03:05:29",
    "2026-09-18T03:05:29",
    "2026-09-18",
    "2026-09-18T03:05",
    "2026-09-18T03:05:29.123456-05:00",
    "1969-12-31T23:59:59.500+00:00",
    "不是时间",
    "2026-13-45T00:00:00+00:00",
    "",
    None,
    1774000000,
]

BASE_EVENT = {
    "version": 1,
    "session_id": "20260918-030529-abcdef",
    "event_id": "0123456789abcdef01234567",
    "parent_id": None,
    "type": "user_message",
    "created_at": "2026-09-18T03:05:29.123456+00:00",
    "payload": {"text": "你好"},
}

EVENTS = [
    BASE_EVENT,
    {**BASE_EVENT, "parent_id": "0123456789abcdef01234566"},
    {**BASE_EVENT, "parent_id": "  "},
    {key: value for key, value in BASE_EVENT.items() if key != "payload"},
    {key: value for key, value in BASE_EVENT.items() if key != "parent_id"},
    {**BASE_EVENT, "version": 2},
    {**BASE_EVENT, "version": "1"},
    {**BASE_EVENT, "event_id": "  "},
    {**BASE_EVENT, "parent_id": 5},
    {**BASE_EVENT, "payload": "不是对象"},
    {**BASE_EVENT, "payload": [1, 2]},
    {**BASE_EVENT, "created_at": "bad"},
    {**BASE_EVENT, "session_id": "bad-id"},
    {**BASE_EVENT, "type": "Bad"},
    {key: value for key, value in BASE_EVENT.items() if key != "type"},
]

BASE_INDEX = {
    "session_id": "20260918-030529-abcdef",
    "title": "会话标题",
    "workspace_root": "D:/work/demo",
    "path": "sessions/20260918-030529-abcdef.jsonl",
    "created_at": "2026-09-18T03:05:29.123456+00:00",
    "updated_at": "2026-09-18T04:00:00+00:00",
    "event_count": 12,
    "message_count": 4,
    "last_event_type": "assistant_message",
    "archived_at": None,
}

INDEX_ENTRIES = [
    BASE_INDEX,
    {key: value for key, value in BASE_INDEX.items() if key != "archived_at"},
    {key: value for key, value in BASE_INDEX.items() if key != "event_count"},
    {key: value for key, value in BASE_INDEX.items() if key != "last_event_type"},
    {key: value for key, value in BASE_INDEX.items() if key != "title"},
    {key: value for key, value in BASE_INDEX.items() if key != "path"},
    {**BASE_INDEX, "archived_at": "2026-09-18T05:00:00+00:00"},
    {**BASE_INDEX, "title": 5},
    {**BASE_INDEX, "workspace_root": "   "},
    {**BASE_INDEX, "event_count": -1},
    {**BASE_INDEX, "event_count": 1.5},
    {**BASE_INDEX, "event_count": True},
    {**BASE_INDEX, "message_count": "3"},
    {**BASE_INDEX, "last_event_type": 5},
    {**BASE_INDEX, "session_id": "nope"},
    {**BASE_INDEX, "path": "../x.jsonl"},
    {**BASE_INDEX, "updated_at": "bad"},
]

PAYLOAD_COUNTS = [0, 12, -1, True, "5", 1.5, None, {"a": 1}, [1]]

LINE_EVENTS = [
    BASE_EVENT,
    {
        **BASE_EVENT,
        "type": "assistant_message",
        "payload": {
            "blocks": [{"Text": {"text": "中文与 emoji 🙂"}}],
            "count": 3,
            "ratio": 0.5,
            "flag": True,
            "nested": {"list": [1, 2, {"deep": None}]},
            "empty": "",
        },
    },
    {
        **BASE_EVENT,
        "type": "tool_result",
        "created_at": "2026-09-18T03:05:29+00:00",
        "payload": {
            "call_id": "call_1",
            "ok": False,
            "output": "第一行\n第二行 \"引号\" 与 \\ 反斜杠",
        },
    },
]


def attempt(func, value):
    try:
        result = func(value)
    except M.SessionStoreError as exc:
        return {"input": value, "error": str(exc)}
    return {"input": value, "expected": result}


def datetime_case(value):
    try:
        parsed = M.parse_datetime(value)
    except M.SessionStoreError as exc:
        return {"input": value, "error": str(exc)}
    return {
        "input": value,
        "expected_iso": M.format_datetime(parsed),
        "expected_millis": M.datetime_to_millis(parsed),
    }


def event_case(value):
    try:
        event = M.SessionEvent.from_dict(value)
    except M.SessionStoreError as exc:
        return {"input": value, "error": str(exc)}
    return {"input": value, "expected": event.to_dict()}


def index_case(value):
    try:
        entry = M.SessionIndexEntry.from_dict(value)
    except M.SessionStoreError as exc:
        return {"input": value, "error": str(exc)}
    return {"input": value, "expected": entry.to_dict()}


def create_case(session_id, event_type, payload, parent_id):
    now = datetime(2026, 9, 18, 3, 5, 29, 123456, tzinfo=timezone.utc)
    case_input = {
        "session_id": session_id,
        "event_type": event_type,
        "payload": payload,
        "parent_id": parent_id,
        "now": M.format_datetime(now),
    }
    try:
        event = M.SessionEvent.create(
            session_id=session_id,
            event_type=event_type,
            payload=payload,
            parent_id=parent_id,
            now=now,
        )
    except M.SessionStoreError as exc:
        return {"input": case_input, "error": str(exc)}
    data = event.to_dict()
    data["event_id"] = None
    return {"input": case_input, "expected": data}


def line_case(value):
    event = M.SessionEvent.from_dict(value)
    return {
        "event": event.to_dict(),
        "line": json.dumps(event.to_dict(), ensure_ascii=False, separators=(",", ":")),
    }


def main() -> None:
    fixture = {
        "source": ["omnicrawl/state/session_models.py"],
        "constants": {
            "session_event_version": M.SESSION_EVENT_VERSION,
            "compact_summary_prefix": M.COMPACT_SUMMARY_PREFIX,
            "message_event_types": sorted(M.MESSAGE_EVENT_TYPES),
            "model_context_event_types": sorted(M.MODEL_CONTEXT_EVENT_TYPES),
            "empty_session_event_types": sorted(M.EMPTY_SESSION_EVENT_TYPES),
            "subagent_event_types": sorted(M.SUBAGENT_EVENT_TYPES),
        },
        "session_ids": [attempt(M.normalize_session_id, value) for value in SESSION_IDS],
        "event_types": [attempt(M.normalize_event_type, value) for value in EVENT_TYPES],
        "relative_paths": [
            attempt(M.normalize_relative_file_path, value) for value in RELATIVE_PATHS
        ],
        "titles": [attempt(M.clean_title, value) for value in TITLES],
        "datetimes": [datetime_case(value) for value in DATETIMES],
        "events": [event_case(value) for value in EVENTS],
        "index_entries": [index_case(value) for value in INDEX_ENTRIES],
        "payload_counts": [
            attempt(M.read_payload_non_negative_int, value) for value in PAYLOAD_COUNTS
        ],
        "created_events": [
            create_case("20260918-030529-abcdef", "assistant_message", {"text": "好"}, None),
            create_case(" 20260918-030529-abcdef ", "user_message", {}, ""),
            create_case(" 20260918-030529-abcdef ", "assistant_message", {"a": 1}, " 0123456789abcdef01234567 "),
            create_case("bad", "user_message", {}, None),
            create_case("20260918-030529-abcdef", "Bad", {}, None),
        ],
        "lines": [line_case(value) for value in LINE_EVENTS],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    counts = {
        section: len(value)
        for section, value in fixture.items()
        if isinstance(value, list)
    }
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：{counts}")


if __name__ == "__main__":
    main()
