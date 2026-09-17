#!/usr/bin/env python3
"""生成会话 artifact 转存与脱敏的对照数据集，供 Rust 侧 `omnicrawl-session::artifact` 使用。

期望值来自 Python 真实现 `omnicrawl/state/session_artifacts.py`。所有产物名都基于
sha256 前缀，因此无需归一化：每一步的返回载荷与最终文件快照都可逐字比对。

用法：``python rust/tools/gen_artifact_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/artifact_parity.json``
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/artifact_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state.session_artifacts import SessionArtifactStore  # noqa: E402

if not Path(sys.modules["omnicrawl.state.session_artifacts"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

SESSION_ID = "20260918-030529-abcdef"
OTHER_SESSION = "20260918-030530-abcdef"
SECRET = "sk-" + "a" * 24
LONG_OUTPUT = "行内容" * 3000 + f"\n尾部 api_key={SECRET}\n"
SMALL_RESULT = "子任务结论：一切正常。"
LARGE_RESULT = "子任务结论：" + "很长的正文" * 400

STEPS = [
    {"kind": "event", "event_type": "tool_result", "payload": {"tool": "read_file", "ok": True, "output": f"短输出 token={SECRET}"}},
    {"kind": "event", "event_type": "tool_result", "payload": {"tool": "grep", "ok": False, "output": LONG_OUTPUT}},
    {"kind": "event", "event_type": "assistant_message", "payload": {"content": "普通回复", "api_key": SECRET, "nested": {"password": SECRET}}},
    {
        "kind": "event",
        "event_type": "tool_result",
        "payload": {
            "tool": "render",
            "ok": True,
            "output": "短输出",
            "ui_artifact": {
                "type": "html",
                "title": "  预览标题  ",
                "path": "report.html",
                "html": f'<html data-api_key="{SECRET}">{{"token": "{SECRET}"}}</html>',
            },
        },
    },
    {
        "kind": "event",
        "event_type": "tool_result",
        "payload": {
            "tool": "render",
            "ok": True,
            "output": "短输出",
            "ui_artifact": {"type": "html", "title": "", "path": "empty.html", "html": "   "},
        },
    },
    {"kind": "event", "event_type": "tool_result", "payload": {"tool": "read_file", "ok": True, "output": 123}},
    {"kind": "subagent", "task_id": "task-0123456789ab", "agent_type": "explore", "description": "看代码", "result_text": SMALL_RESULT, "summary_chars": 200},
    {"kind": "subagent", "task_id": "task-0123456789ac", "agent_type": "explore", "description": "看代码", "result_text": LARGE_RESULT, "summary_chars": 50},
    {"kind": "subagent", "task_id": "task-zzz", "agent_type": "explore", "description": "看代码", "result_text": SMALL_RESULT, "summary_chars": 50},
    {"kind": "subagent", "task_id": "task-0123456789ad", "agent_type": "explore", "description": "看代码", "result_text": SMALL_RESULT, "summary_chars": 0},
    {"kind": "read", "session_id": SESSION_ID, "artifact_path": None},
    {"kind": "read", "session_id": SESSION_ID, "artifact_path": "artifacts/不存在.txt"},
    {"kind": "read", "session_id": OTHER_SESSION, "artifact_path": None},
    {"kind": "read", "session_id": SESSION_ID, "artifact_path": "../escape.txt"},
    {"kind": "read", "session_id": SESSION_ID, "artifact_path": "其他目录/x.txt"},
    {"kind": "preview", "text": "短的", "max_chars": 10},
    {"kind": "preview", "text": "一二三四五六七八九十", "max_chars": 5},
    {"kind": "summary", "text": LONG_OUTPUT},
    {"kind": "html", "html": f'<a api-key="{SECRET}" password=abc>{"token"}: "{SECRET}"</a>'},
]


def snapshot(root: Path) -> dict:
    files = {}
    for path in sorted(root.rglob("*")):
        if path.is_file():
            files[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    return files


def run_step(store: SessionArtifactStore, step: dict, first_artifact: list[str]) -> dict:
    kind = step["kind"]
    try:
        if kind == "event":
            payload = store.prepare_event_payload(
                session_id=SESSION_ID,
                event_type=step["event_type"],
                payload=dict(step["payload"]),
            )
            return {"kind": kind, "payload": payload}
        if kind == "subagent":
            return {
                "kind": kind,
                "result": store.prepare_subagent_result(
                    session_id=SESSION_ID,
                    task_id=step["task_id"],
                    agent_type=step["agent_type"],
                    description=step["description"],
                    result_text=step["result_text"],
                    summary_chars=step["summary_chars"],
                ),
            }
        if kind == "read":
            path = step["artifact_path"] or (first_artifact[0] if first_artifact else "artifacts/none.txt")
            return {
                "kind": kind,
                "artifact_path": path,
                "text": store.read_text(step["session_id"], path),
            }
        if kind == "preview":
            from omnicrawl.state.session_artifacts import preview_text

            return {"kind": kind, "text": preview_text(step["text"], step["max_chars"])}
        if kind == "summary":
            from omnicrawl.state.session_artifacts import tool_output_summary

            return {"kind": kind, "text": tool_output_summary(step["text"])}
        if kind == "html":
            from omnicrawl.state.session_artifacts import redact_sensitive_html

            return {"kind": kind, "text": redact_sensitive_html(step["html"])}
    except Exception as exc:  # noqa: BLE001 - 对照要记录错误文案
        return {"kind": kind, "error": str(exc)}
    raise SystemExit(f"未知步骤：{kind}")


def main() -> None:
    root = Path(tempfile.mkdtemp(prefix="artifact-"))
    try:
        artifacts_dir = root / "artifacts"
        store = SessionArtifactStore(root, artifacts_dir)
        outputs = []
        first_artifact: list[str] = []
        for step in STEPS:
            result = run_step(store, step, first_artifact)
            payload = result.get("payload")
            if isinstance(payload, dict) and payload.get("artifact_path") and not first_artifact:
                first_artifact.append(str(payload["artifact_path"]))
            outputs.append(result)
        files = snapshot(root)
    finally:
        shutil.rmtree(root, ignore_errors=True)

    fixture = {
        "source": ["omnicrawl/state/session_artifacts.py", "omnicrawl/common/redaction.py"],
        "session_id": SESSION_ID,
        "steps": STEPS,
        "expected_outputs": outputs,
        "expected_files": files,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    errors = sum(1 for item in outputs if "error" in item)
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：步骤 {len(STEPS)}（{errors} 个报错用例），"
        f"产物文件 {sorted(files)}"
    )


if __name__ == "__main__":
    main()
