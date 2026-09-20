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
    """把步骤里的 $session / $artifact 占位换成上一步产出的真实值。"""

    resolved = {}
    for key, value in step.items():
        if isinstance(value, str):
            if "$session" in value:
                value = value.replace("$session", context.get("session_id", "$session"))
            if "$artifact" in value:
                value = value.replace("$artifact", context.get("artifact_path", "$artifact"))
        resolved[key] = value
    return resolved


def report_payload(report) -> dict:
    """报告转 dict；备份文件路径只留文件名（两侧临时根目录不同，绝对路径不可比）。"""

    data = report.to_dict()
    if data.get("backup_path"):
        data["backup_path"] = Path(data["backup_path"]).name
    return data


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
        if kind == "list_sessions_filtered":
            entries = store.list_sessions(
                workspace_root=Path(step["workspace_root"]) if step.get("workspace_root") else None,
                project_path=step.get("project_path"),
                limit=step.get("limit", 10),
                include_archived=step.get("include_archived", False),
                archived_only=step.get("archived_only", False),
            )
            return {
                "kind": kind,
                "sessions": [entry.to_dict() for entry in entries],
            }
        if kind == "list_project_paths":
            return {
                "kind": kind,
                "paths": store.list_project_paths(include_archived=step.get("include_archived", True)),
            }
        if kind == "rename_session":
            state = store.rename_session(
                step["session_id"],
                step["title"],
                now=BASE_TIME + timedelta(seconds=step.get("offset_seconds", 1)),
            )
            return {"kind": kind, "title": state.title}
        if kind == "export_session_markdown":
            path = store.export_session_markdown(
                step["session_id"],
                step["text"],
                now=BASE_TIME + timedelta(seconds=step.get("offset_seconds", 1)),
            )
            return {"kind": kind, "file": path.name}
        if kind == "archive_session":
            state = store.archive_session(
                step["session_id"],
                now=BASE_TIME + timedelta(seconds=step.get("offset_seconds", 1)),
            )
            return {"kind": kind, "archived": state.archived_at is not None}
        if kind == "unarchive_session":
            state = store.unarchive_session(
                step["session_id"],
                now=BASE_TIME + timedelta(seconds=step.get("offset_seconds", 1)),
            )
            return {"kind": kind, "archived": state.archived_at is not None}
        if kind == "delete_session":
            store.delete_session(step["session_id"])
            return {"kind": kind}
        if kind == "discard_empty_session":
            return {"kind": kind, "discarded": store.discard_empty_session(step["session_id"])}
        if kind == "write_tool_result_artifact":
            artifact = store.write_tool_result_artifact(step["session_id"], step["output"])
            context["artifact_path"] = artifact
            return {"kind": kind, "artifact_path": artifact}
        if kind == "read_artifact_text":
            return {
                "kind": kind,
                "text": store.read_artifact_text(step["session_id"], step["artifact_path"]),
            }
        if kind == "prepare_undo_last_turn":
            plan = store.prepare_undo_last_turn(step["session_id"])
            context["undo_plan"] = plan
            return {
                "kind": kind,
                "plan_kind": plan.kind,
                "event_types": [event.type for event in plan.events],
                "has_assistant_id": plan.assistant_event_id is not None,
            }
        if kind in {"commit_undo_plan", "undo_last_turn"}:
            if kind == "commit_undo_plan":
                plan = context["undo_plan"]
                store.commit_undo_plan(
                    plan,
                    side_effects_reverted=step.get("side_effects_reverted", False),
                )
            else:
                store.undo_last_turn(step["session_id"])
            # 只回报回退后的有效事件流（提交时刻的时间戳两侧不同，不能进对照）。
            active = store.read_session_events(step["session_id"])
            return {"kind": kind, "active_event_types": [event.type for event in active]}
        if kind == "check_consistency":
            return {"kind": kind, "report": report_payload(store.check_consistency())}
        if kind == "rebuild_index":
            report = store.rebuild_index(
                apply=step.get("apply", False),
                now=BASE_TIME + timedelta(seconds=step.get("offset_seconds", 1)),
            )
            return {"kind": kind, "report": report_payload(report)}
        if kind == "mkdir":
            path = root / step["path"]
            path.mkdir(parents=True, exist_ok=True)
            return {"kind": kind, "path": step["path"]}
        if kind == "remove_path":
            path = root / step["path"]
            if path.is_dir():
                shutil.rmtree(path, ignore_errors=True)
            elif path.exists():
                path.unlink()
            return {"kind": kind, "path": step["path"]}
        if kind == "copy_file":
            shutil.copyfile(root / step["from"], root / step["to"])
            return {"kind": kind, "from": step["from"], "to": step["to"]}
        if kind == "corrupt_line":
            # 人为往转录里插一行坏 JSON，观察读取行为
            path = root / "sessions" / f"{step['session_id']}.jsonl"
            with path.open("a", encoding="utf-8") as handle:
                handle.write(step["line"] + "\n")
            return {"kind": kind, "line": step["line"]}
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
    (
        "rename_session",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "旧标题"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "第一轮"},
                "offset_seconds": 1,
            },
            {
                "kind": "rename_session",
                "session_id": "$session",
                "title": "  新标题   带空格  ",
                "offset_seconds": 5,
            },
            {
                "kind": "rename_session",
                "session_id": "$session",
                "title": "   ",
                "offset_seconds": 6,
            },
            {"kind": "read_events", "session_id": "$session"},
            {"kind": "list_sessions"},
        ],
    ),
    (
        "export_session_markdown",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "导出"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "导出我"},
                "offset_seconds": 1,
            },
            {
                "kind": "export_session_markdown",
                "session_id": "$session",
                "text": "# 会话导出" + chr(10) + chr(10) + "第一轮对话。" + chr(10),
                "offset_seconds": 3600,
            },
            {
                "kind": "export_session_markdown",
                "session_id": "$session",
                "text": "   ",
                "offset_seconds": 3661,
            },
            {"kind": "read_events", "session_id": "$session"},
        ],
    ),
    (
        "archive_and_unarchive",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "归档"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "归档前"},
                "offset_seconds": 1,
            },
            {"kind": "archive_session", "session_id": "$session", "offset_seconds": 10},
            {"kind": "list_sessions"},
            {"kind": "list_sessions_filtered", "include_archived": True},
            {"kind": "list_sessions_filtered", "archived_only": True},
            {"kind": "archive_session", "session_id": "$session", "offset_seconds": 11},
            {"kind": "read_events", "session_id": "$session"},
            {"kind": "unarchive_session", "session_id": "$session", "offset_seconds": 20},
            {"kind": "unarchive_session", "session_id": "$session", "offset_seconds": 21},
            {"kind": "read_events", "session_id": "$session"},
            {"kind": "list_sessions"},
        ],
    ),
    (
        "discard_empty_and_delete",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "空会话"},
            {"kind": "discard_empty_session", "session_id": "$session"},
            {"kind": "list_sessions"},
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "有内容"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "留下"},
                "offset_seconds": 1,
            },
            {"kind": "discard_empty_session", "session_id": "$session"},
            {
                "kind": "write_tool_result_artifact",
                "session_id": "$session",
                "output": "第一行" + chr(10) + "第二行" + chr(10),
            },
            {
                "kind": "read_artifact_text",
                "session_id": "$session",
                "artifact_path": "$artifact",
            },
            {
                "kind": "read_artifact_text",
                "session_id": "$session",
                "artifact_path": "artifacts/$session/missing.txt",
            },
            {
                "kind": "read_artifact_text",
                "session_id": "$session",
                "artifact_path": "artifacts/other-session/tool_result_0000000000000000.txt",
            },
            {"kind": "delete_session", "session_id": "$session"},
            {"kind": "list_sessions"},
        ],
    ),
    (
        "session_list_filters",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "甲"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "甲的内容"},
                "offset_seconds": 3,
            },
            {"kind": "start_session", "workspace_root": "D:/work/other", "title": "乙"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "乙的内容"},
                "offset_seconds": 4,
            },
            {"kind": "list_sessions_filtered", "workspace_root": WORKSPACE},
            {"kind": "list_sessions_filtered", "workspace_root": "D:/work/other"},
            {"kind": "list_sessions_filtered", "project_path": "D:/work/other", "limit": 1},
            {"kind": "list_sessions_filtered", "limit": 1},
            {"kind": "archive_session", "session_id": "$session", "offset_seconds": 10},
            {"kind": "list_sessions_filtered", "include_archived": True},
            {"kind": "list_sessions_filtered", "archived_only": True},
            {"kind": "list_project_paths"},
            {"kind": "list_project_paths", "include_archived": False},
        ],
    ),
    (
        "consistency_report_and_rebuild",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "一致性"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "检查一致性"},
                "offset_seconds": 1,
            },
            {"kind": "check_consistency"},
            # 索引被清空：转录成为孤立条目，修复应把它补回索引。
            {
                "kind": "write_file",
                "path": "index.json",
                "text": '{"schema_version":1,"sessions":[]}' + chr(10),
            },
            {"kind": "check_consistency"},
            {"kind": "rebuild_index", "apply": False, "offset_seconds": 30},
            {"kind": "rebuild_index", "apply": True, "offset_seconds": 30},
            # 同一秒再修一次：备份文件名要追加序号。
            {"kind": "rebuild_index", "apply": True, "offset_seconds": 30},
            {"kind": "list_sessions"},
        ],
    ),
    (
        "consistency_missing_and_empty_transcript",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "缺失"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "内容"},
                "offset_seconds": 1,
            },
            {"kind": "remove_path", "path": "sessions/$session.jsonl"},
            {"kind": "check_consistency"},
            # 转录文件回来了但没有任何事件：只告警，索引条目保留。
            {"kind": "write_file", "path": "sessions/$session.jsonl", "text": ""},
            {"kind": "check_consistency"},
        ],
    ),
    (
        "consistency_duplicate_transcript",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "重复"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "重复转录"},
                "offset_seconds": 1,
            },
            {
                "kind": "copy_file",
                "from": "sessions/$session.jsonl",
                "to": "archive/$session.jsonl",
            },
            {"kind": "check_consistency"},
        ],
    ),
    (
        # 孤立 artifact 只能用「非本场景生成的固定 id」表达：同一场景里不要再混入真实会话，
        # 否则两侧的 id 编号会不一致（见 main() 里的编号说明）。
        "consistency_orphan_artifact",
        [
            {"kind": "mkdir", "path": "artifacts/20260101-000000-bbbbbb"},
            {"kind": "check_consistency"},
        ],
    ),
    (
        # 索引里只有条目、磁盘上有同名文件但文件名不符合会话 id 命名：扫描不到，报告路径不一致。
        "consistency_index_only_entry",
        [
            {"kind": "write_file", "path": "sessions/custom-name.jsonl", "text": ""},
            {
                "kind": "write_file",
                "path": "index.json",
                "text": (
                    '{"schema_version":1,"sessions":[{"session_id":"20260101-000000-cccccc",'
                    '"title":"路径","workspace_root":"/workspace/demo",'
                    '"path":"sessions/custom-name.jsonl",'
                    '"created_at":"2026-09-18T03:05:29.123456+00:00",'
                    '"updated_at":"2026-09-18T03:05:30.123456+00:00",'
                    '"event_count":2,"message_count":1,"last_event_type":"user_message",'
                    '"archived_at":null}]}'
                    + chr(10)
                ),
            },
            {"kind": "check_consistency"},
        ],
    ),
    (
        # undo 的提交时刻两侧无法共用时钟（Python 用当前时间），因此这组场景只比对 outputs。
        "undo_complete_turn",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "回退"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "第一轮"},
                "offset_seconds": 1,
            },
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "assistant_message",
                "payload": {"content": "第一轮回复"},
                "offset_seconds": 2,
            },
            {"kind": "prepare_undo_last_turn", "session_id": "$session"},
            {
                "kind": "commit_undo_plan",
                "session_id": "$session",
                "side_effects_reverted": True,
            },
            # 计划已过期：提交后同一份计划再提交一次。
            {"kind": "commit_undo_plan", "session_id": "$session"},
        ],
        {"compare": ["outputs"]},
    ),
    (
        "undo_stale_plan",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "过期计划"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "第一轮"},
                "offset_seconds": 1,
            },
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "assistant_message",
                "payload": {"content": "第一轮回复"},
                "offset_seconds": 2,
            },
            {"kind": "prepare_undo_last_turn", "session_id": "$session"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "插进来的新一轮"},
                "offset_seconds": 3,
            },
            {"kind": "commit_undo_plan", "session_id": "$session"},
        ],
        {"compare": ["outputs"]},
    ),
    (
        "undo_incomplete_turn",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "未完成"},
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "user_message",
                "payload": {"content": "只有用户消息"},
                "offset_seconds": 1,
            },
            {
                "kind": "append_event",
                "session_id": "$session",
                "event_type": "turn_cancelled",
                "payload": {"reason": "用户取消"},
                "offset_seconds": 2,
            },
            {"kind": "prepare_undo_last_turn", "session_id": "$session"},
            {"kind": "undo_last_turn", "session_id": "$session"},
        ],
        {"compare": ["outputs"]},
    ),
    (
        "undo_without_turn",
        [
            {"kind": "start_session", "workspace_root": WORKSPACE, "title": "空会话"},
            {"kind": "prepare_undo_last_turn", "session_id": "$session"},
        ],
        {"compare": ["outputs"]},
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


def normalize_pids(value):
    """锁文件记的是持有者 pid，跨进程天然不同，落库前替换成占位。"""

    if isinstance(value, str):
        return re.sub(r"pid=" + r"\d+", "pid=<pid>", value)
    if isinstance(value, list):
        return [normalize_pids(item) for item in value]
    if isinstance(value, dict):
        return {key: normalize_pids(item) for key, item in value.items()}
    return value


def normalize_runtime(value):
    """session_started 载荷里的运行时身份依赖解释器环境，落库前替换成占位。"""

    if isinstance(value, dict):
        return {
            key: (RUNTIME_SLOT if key == "runtime" else normalize_runtime(item))
            for key, item in value.items()
        }
    if isinstance(value, list):
        return [normalize_runtime(item) for item in value]
    return value


def main() -> None:
    scenarios = []
    for entry in SCENARIOS:
        name, steps = entry[0], entry[1]
        options = entry[2] if len(entry) > 2 else {}
        result = run_scenario(name, steps)
        raw = json.dumps(result, ensure_ascii=False)
        # 会话 id 的编号按“创建顺序”定：outputs 里 start_session 的先后是确定的，
        # 而文件名里带随机后缀，按文件名排序会让编号在两次生成之间漂移。
        #
        # 因此数据集里不要出现「自定义的 id 形状字面量 + 真实会话 id」混用的场景：
        # 真实 id 只由 start_session 生成，字面量（如 20260101-000000-bbbbbb）在两侧
        # 归一化时的编号顺序无法保证一致，单独成场景即可。
        session_ids = re.findall(
            r"20260918-030529-[a-f0-9]{6}",
            json.dumps(result["outputs"], ensure_ascii=False),
        )
        event_ids = re.findall(r"\b[0-9a-f]{24}\b", raw)
        mapping = {}
        mapping.update(indexed_mapping(list(dict.fromkeys(session_ids)), "session-id"))
        mapping.update({value: "<event-id>" for value in dict.fromkeys(event_ids)})

        result["outputs"] = normalize_runtime(apply_mapping(result["outputs"], mapping))
        # 键按“映射后的相对路径”排序：文件名里带随机后缀，按原名排序会让
        # 两次生成的键序漂移（值本身一致，但 fixture 应当逐字节可复现）。
        files = {
            key: value
            for key, value in sorted(
                apply_mapping(
                    {
                        path: normalize_transcript(text)
                        for path, text in result["files"].items()
                    },
                    mapping,
                ).items()
            )
        }
        result["files"] = normalize_pids(normalize_runtime(files))
        if options.get("compare"):
            # 只有显式声明时才裁剪比对面：例如 undo 的提交时刻两侧无法共用时钟。
            result["compare"] = list(options["compare"])
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
