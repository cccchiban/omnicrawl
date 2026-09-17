#!/usr/bin/env python3
"""生成会话存储对照数据集，供 Rust 侧 `omnicrawl-session::SessionStore` 的 parity 测试使用。

做法：每个场景在临时目录里跑真的 `omnicrawl.state.session.SessionStore`，记录
「磁盘上出现了哪些文件、每个文件的确切字节、索引与事件的解析结果、以及失败时的错误文案」。
Rust 侧在同样的临时目录里重放同一串操作，比对文件与行为。

用法：``python rust/tools/gen_session_store_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/session_store_parity.json``
"""

from __future__ import annotations

import json
import re
import shutil
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-session/tests/fixtures/session_store_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.state.session import SessionStore  # noqa: E402
from omnicrawl.state.session_models import SessionStoreError  # noqa: E402

if not Path(sys.modules["omnicrawl.state.session"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

# 固定时间：fixture 必须可重复
BASE_TIME = datetime(2026, 9, 18, 3, 5, 29, 123456, tzinfo=timezone.utc)
WORKSPACE = "D:/work/demo"

# 会话目录骨架（ensure 之后应当存在的东西），其余目录用于归档/导出，本片不覆盖
LAYOUT_DIRS = [
    "sessions",
    "artifacts",
    "summaries",
    "exports",
    "archive",
    "archive/compacted",
]


def resolve_step(step: dict, context: dict):
    """把步骤里的 $session 占位换成刚创建的会话 id。"""

    resolved = {}
    for key, value in step.items():
        if isinstance(value, str) and "$session" in value:
            value = value.replace("$session", context.get("session_id", "$session"))
        resolved[key] = value
    return resolved


def snapshot(root: Path) -> dict:
    """记录磁盘状态：文件相对路径 → 字节内容（文本按 UTF-8 解码）。"""

    files = {}
    for path in sorted(root.rglob("*")):
        if path.is_dir():
            continue
        relative = path.relative_to(root).as_posix()
        try:
            files[relative] = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            files[relative] = f"<binary:{path.stat().st_size}>"
    return files


def layout_of(root: Path) -> list[str]:
    return sorted(
        path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_dir()
    )


def run_scenario(name: str, steps: list[dict]) -> dict:
    """按脚本跑一串操作；每步记录其产出或错误。"""

    root = Path(tempfile.mkdtemp(prefix="session-store-"))
    store = SessionStore(root)
    outputs = []
    context: dict = {}
    try:
        for step in steps:
            outputs.append(run_step(store, root, step, context))
        return {
            "name": name,
            "steps": steps,
            "outputs": outputs,
            "files": snapshot(root),
            "dirs": layout_of(root),
        }
    finally:
        shutil.rmtree(root, ignore_errors=True)


def run_step(store: SessionStore, root: Path, step: dict, context: dict) -> dict:
    step = resolve_step(step, context)
    kind = step["kind"]
    try:
        if kind == "ensure":
            store.ensure()
            return {"kind": kind}
        if kind == "start_session":
            state = store.start_session(
                Path(step["workspace_root"]),
                title=step.get("title", ""),
                now=BASE_TIME,
            )
            # runtime 身份依赖解释器环境（源码哈希与已加载模块），无法跨实现比对，比对前剔除。
            context["session_id"] = state.session_id
            return {
                "kind": kind,
                "session_id": state.session_id,
                "title": state.title,
                "event_count": state.event_count,
                "last_event_type": state.last_event_type,
            }
        if kind == "append_event":
            event = store.append_event(
                step["session_id"],
                step["event_type"],
                step.get("payload"),
                parent_id=step.get("parent_id"),
                now=BASE_TIME + timedelta(seconds=step.get("offset_seconds", 1)),
            )
            return {"kind": kind, "event": event.to_dict()}
        if kind == "read_events":
            events = store.read_session_events(step["session_id"])
            return {"kind": kind, "events": [event.to_dict() for event in events]}
        if kind == "list_sessions":
            entries = store.list_sessions()
            return {
                "kind": kind,
                "sessions": [entry.to_dict() for entry in entries],
            }
        if kind == "corrupt_line":
            # 人为往转录里插一行坏 JSON，观察读取行为
            path = root / "sessions" / f"{step['session_id']}.jsonl"
            with path.open("a", encoding="utf-8") as handle:
                handle.write(step["line"] + "\n")
            return {"kind": kind, "line": step["line"]}
        if kind == "read_file":
            path = root / step["path"]
            return {"kind": kind, "path": step["path"], "text": path.read_text("utf-8")}
        if kind == "write_file":
            path = root / step["path"]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(step["text"], encoding="utf-8")
            return {"kind": kind, "path": step["path"]}
        raise SystemExit(f"未知步骤：{kind}")
    except SessionStoreError as exc:
        return {"kind": kind, "error": str(exc)}
    except Exception as exc:  # noqa: BLE001 - 非会话错误也如实记录，便于发现语义差异
        return {"kind": kind, "error_type": type(exc).__name__, "error": str(exc)}


SESSION_ID_SLOT = "<session-id>"
EVENT_ID_SLOT = "<event-id>"
RUNTIME_SLOT = {"<normalized>": True}


def with_session_id(steps: list[dict]) -> list[dict]:
    """把占位会话 id 换成上一步真实生成的 id（fixture 里时间固定因此 id 也固定）。"""

    return steps


SCENARIOS = [
    ("ensure_only", [{"kind": "ensure"}]),
    (
        "start_session",
        [{"kind": "start_session", "workspace_root": WORKSPACE, "title": "多轮   任务  "}],
    ),
    (
        "start_session_without_title",
        [{"kind": "start_session", "workspace_root": WORKSPACE}],
    ),
    (
        "append_events",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "任务"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "帮我看看 README 的排版"},
                "offset_seconds": 1,
            },
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "assistant_message",
                "payload": {"content": "好的，先读文件。", "reasoning": "先确认路径"},
                "offset_seconds": 2,
            },
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "tool_result",
                "payload": {"call_id": "call_1", "output": "第一行" + chr(10) + "第二行"},
                "offset_seconds": 3,
            },
            {"kind": "read_events", "session_id": "$session"},
        ],
    ),
    (
        "append_to_unknown_session",
        [
            {"kind": "ensure"},
            {
                "kind": "append_event",
                "session_id": "20260101-000000-aaaaaa",
                "event_type": "user_message",
                "payload": {"content": "落空的会话"},
            },
        ],
    ),
    (
        "corrupt_transcript_line",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "坏行"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "正常一轮"},
            },
            {
                "kind": "corrupt_line",
                "session_id": "$session",
                "line": "{\"version\":1,\"session_id\":\"bad\"",
            },
            {"kind": "corrupt_line", "session_id": "$session", "line": "not json at all"},
            {"kind": "read_events", "session_id": "$session"},
        ],
    ),
    (
        "start_two_sessions_then_list",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "甲"},
            {"kind": "start_session", "workspace_root": "D:/work/other", "title": "乙"},
            {"kind": "list_sessions"},
        ],
    ),
]

def indexed_mapping(values, prefix: str) -> dict:
    """随机值按出现顺序映射成 <prefix-1>、<prefix-2>…，两个实现才可比。"""

    mapping = {}
    for value in values:
        if value not in mapping:
            mapping[value] = f"<{prefix}-{len(mapping) + 1}>"
    return mapping


def apply_mapping(value, mapping: dict):
    if isinstance(value, str):
        for original, replacement in mapping.items():
            value = value.replace(original, replacement)
        return value
    if isinstance(value, list):
        return [apply_mapping(item, mapping) for item in value]
    if isinstance(value, dict):
        return {
            apply_mapping(key, mapping): apply_mapping(item, mapping)
            for key, item in value.items()
        }
    return value


def normalize_transcript(text: str) -> str:
    """转录行里的 runtime 身份依赖解释器环境，比对前替换成占位。"""

    lines = []
    for line in text.splitlines():
        try:
            data = json.loads(line)
        except json.JSONDecodeError:
            lines.append(line)
            continue
        payload = data.get("payload") if isinstance(data, dict) else None
        if isinstance(payload, dict) and "runtime" in payload:
            payload["runtime"] = RUNTIME_SLOT
        lines.append(json.dumps(data, ensure_ascii=False, separators=(",", ":")))
    return "".join(line + chr(10) for line in lines)


def main() -> None:
    scenarios = []
    for name, steps in SCENARIOS:
        result = run_scenario(name, steps)
        raw = json.dumps(result, ensure_ascii=False)
        session_ids = re.findall(r"20260918-030529-[a-f0-9]{6}", raw)
        event_ids = re.findall(r"｛Desensitized:104｝", raw)
        mapping = {}
        mapping.update(indexed_mapping(list(dict.fromkeys(session_ids)), "session-id"))
        mapping.update(indexed_mapping(list(dict.fromkeys(event_ids)), "event-id"))
        result["outputs"] = apply_mapping(result["outputs"], mapping)
        result["files"] = apply_mapping(
            {path: normalize_transcript(text) for path, text in result["files"].items()},
            mapping,
        )
        scenarios.append(result)

    fixture = {
        "source": ["omnicrawl/state/session.py", "omnicrawl/state/session_models.py"],
        "base_time": "2026-09-18T03:05:29.123456+00:00",
        "workspace_root": WORKSPACE,
        "layout_dirs": LAYOUT_DIRS,
        "runtime_slot": RUNTIME_SLOT,
        "scenarios": scenarios,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：场景 {len(scenarios)}")
    for scenario in scenarios:
        print(f"  {scenario['name']}: 文件 {sorted(scenario['files'])}")


if __name__ == "__main__":
    main()
