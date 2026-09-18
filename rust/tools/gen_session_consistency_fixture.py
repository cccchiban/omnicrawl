#!/usr/bin/env python3
"""生成会话一致性诊断的对照数据集，供 Rust 侧 `omnicrawl-session` 的 parity 测试使用。

期望值来自 Python 真实现 `omnicrawl/state/session_consistency.py`：从转录重建索引条目
（`build_index_entry_from_events`）与现有条目对照（`compare_index_entry`）。

用法：``python rust/tools/gen_session_consistency_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/session_consistency_parity.json``
"""

from __future__ import annotations

import json
import sys
from itertools import count
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-session/tests/fixtures/session_consistency_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.state import session_consistency as C  # noqa: E402
from omnicrawl.state import session_models as M  # noqa: E402

if not Path(C.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{C.__file__}")

SESSION_ID = "20260918-030529-abcdef"
EVENT_IDS = count(1)


def event(event_type: str, created_at: str, payload: dict | None = None) -> dict:
    """造一条合法转录事件；event_id 用计数器保证数据集可复现。"""

    return {
        "version": 1,
        "session_id": SESSION_ID,
        "event_id": f"{next(EVENT_IDS):024x}",
        "parent_id": None,
        "type": event_type,
        "created_at": created_at,
        "payload": payload or {},
    }


STARTED = event(
    "session_started",
    "2026-09-18T03:05:29.123456+00:00",
    {"workspace_root": "/work/demo", "title": "起点标题"},
)
USER = event("user_message", "2026-09-18T03:06:00.000000+00:00", {"text": "你好"})
ASSISTANT = event("assistant_message", "2026-09-18T03:06:05.500000+00:00", {"text": "在"})
TOOL = event("tool_result", "2026-09-18T03:06:09.000000+00:00", {"tool_name": "read_file"})
RENAMED = event("session_renamed", "2026-09-18T03:07:00.000000+00:00", {"title": "改名后的标题"})
ARCHIVED = event("session_archived", "2026-09-18T04:00:00.000000+00:00", {})
UNARCHIVED = event("session_unarchived", "2026-09-18T05:00:00.000000+00:00", {})

CASES = [
    {
        "name": "活跃会话一致",
        "relative_path": f"sessions/{SESSION_ID}.jsonl",
        "events": [STARTED, USER, ASSISTANT, TOOL, RENAMED],
        "override": {},
    },
    {
        "name": "路径与计数与最后事件类型不一致",
        "relative_path": f"sessions/{SESSION_ID}.jsonl",
        "events": [STARTED, USER, ASSISTANT],
        "override": {
            "path": f"sessions/20260918-030529-aaaaaa.jsonl",
            "event_count": 1,
            "message_count": 0,
            "last_event_type": "session_started",
        },
    },
    {
        "name": "标题不一致（警告）",
        "relative_path": f"sessions/{SESSION_ID}.jsonl",
        "events": [STARTED, USER, ASSISTANT],
        "override": {"title": "手工改过的标题"},
    },
    {
        "name": "更新时间与工作区不一致（警告）",
        "relative_path": f"sessions/{SESSION_ID}.jsonl",
        "events": [STARTED, USER, ASSISTANT],
        "override": {
            "updated_at": "2026-09-18T00:00:00.000000+00:00",
            "workspace_root": "/work/other",
        },
    },
    {
        "name": "归档目录决定归档状态",
        "relative_path": f"archive/{SESSION_ID}.jsonl",
        "events": [STARTED, USER, ASSISTANT],
        "override": {},
    },
    {
        "name": "活跃目录里的历史归档事件不算归档",
        "relative_path": f"sessions/{SESSION_ID}.jsonl",
        "events": [STARTED, ARCHIVED, RENAMED, UNARCHIVED, USER],
        "override": {"archived_at": "2026-09-18T04:00:00.000000+00:00"},
    },
    {
        "name": "缺少工作区时用占位值",
        "relative_path": f"sessions/{SESSION_ID}.jsonl",
        "events": [event("session_started", "2026-09-18T03:05:29.123456+00:00"), USER],
        "override": {},
    },
]


def build_case(case: dict) -> dict:
    events = [M.SessionEvent.from_dict(item) for item in case["events"]]
    rebuilt = C.build_index_entry_from_events(
        session_id=SESSION_ID,
        relative_path=case["relative_path"],
        events=events,
    )
    current_payload = rebuilt.to_dict()
    current_payload.update(case["override"])
    current = M.SessionIndexEntry.from_dict(current_payload)
    issues = [issue.to_dict() for issue in C.compare_index_entry(current, rebuilt)]
    return {
        "name": case["name"],
        "session_id": SESSION_ID,
        "relative_path": case["relative_path"],
        "events": case["events"],
        "current_index": current_payload,
        "expected": {
            "rebuilt": rebuilt.to_dict(),
            "issues": issues,
        },
    }


def main() -> None:
    cases = [build_case(case) for case in CASES]

    # 空转录必须报错：文案也是契约的一部分。
    try:
        C.build_index_entry_from_events(
            session_id=SESSION_ID,
            relative_path=f"sessions/{SESSION_ID}.jsonl",
            events=[],
        )
    except M.SessionStoreError as error:
        empty_events_message = str(error)
    else:
        raise SystemExit("空转录没有报错：Python 侧行为已改变。")

    payload = {
        "source": "omnicrawl/state/session_consistency.py",
        "cases": cases,
        "empty_events": {
            "session_id": SESSION_ID,
            "relative_path": f"sessions/{SESSION_ID}.jsonl",
            "message": empty_events_message,
        },
    }
    FIXTURE_PATH.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"已写出 {FIXTURE_PATH.relative_to(ROOT)}：{len(cases)} 个用例")


if __name__ == "__main__":
    main()
