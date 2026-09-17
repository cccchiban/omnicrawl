#!/usr/bin/env python3
"""生成记忆检索入口的对照数据集，供 Rust 侧 `memory_store` 的 search / expand_related 使用。

期望值来自 Python 真实现 `omnicrawl/state/memory.py`。

时间戳统一用一年前的固定时刻：这样搜索打分的「新鲜度」项恒为 0，分数与排序都可确定性比对；
返回的时间戳统一按 UTC 渲染，避免比对依赖运行机器的时区。

用法：``python rust/tools/gen_memory_search_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/memory_search_parity.json``
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/memory_search_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state.memory import MemoryStore  # noqa: E402

if not Path(sys.modules["omnicrawl.state.memory"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

OLD_TIMESTAMP = "2025-01-02T03:04:05+00:00"

ENTRIES = [
    {
        "id": "20250102-030405",
        "path": "task-history/general/20250102-030405.md",
        "storage_directory": "task-history/general",
        "timestamp": OLD_TIMESTAMP,
        "touch_count": 3,
        "related_directories": ["task-history/general", "project-context/general"],
        "summary": "任务完成情况与待办事项",
        "body": "任务正文：完成了三件事，待办两件。",
    },
    {
        "id": "20250102-030406",
        "path": "code-knowledge/general/20250102-030406.md",
        "storage_directory": "code-knowledge/general",
        "timestamp": OLD_TIMESTAMP,
        "touch_count": 0,
        "related_directories": ["code-knowledge/general"],
        "summary": "解析配置的实现细节",
        "body": "配置解析：先读环境变量，再做校验。",
    },
    {
        "id": "20250102-030407",
        "path": "user-preferences/general/20250102-030407.md",
        "storage_directory": "user-preferences/general",
        "timestamp": OLD_TIMESTAMP,
        "touch_count": 7,
        "related_directories": ["user-preferences/communication-style"],
        "summary": "沟通偏好：回答简短",
        "body": "偏好正文：回答要简短。",
    },
    {
        "id": "20250102-030408",
        "path": "error-lessons/general/20250102-030408.md",
        "storage_directory": "error-lessons/general",
        "timestamp": OLD_TIMESTAMP,
        "touch_count": 1,
        "related_directories": ["error-lessons/general"],
        "summary": "踩坑记录：编码问题",
        "body": "这条的文件不在磁盘上，检索应当跳过。",
        "missing_file": True,
    },
    {
        "id": "20250102-030409",
        "path": "project-context/general/20250102-030409.md",
        "storage_directory": "project-context/general",
        "timestamp": OLD_TIMESTAMP,
        "touch_count": 2,
        "related_directories": ["project-context/general", "code-knowledge/general"],
        "summary": "项目架构分层说明",
        "body": "架构正文：入口、控制器、运行时三层。",
    },
]

STEPS = [
    {"kind": "search", "query": "配置", "candidate_directories": []},
    {"kind": "search", "query": "任务", "candidate_directories": ["task-history/general"]},
    {"kind": "search", "query": "架构", "candidate_directories": ["project-context"]},
    {"kind": "search", "query": "", "candidate_directories": []},
    {"kind": "search", "query": "", "candidate_directories": ["code-knowledge"]},
    {"kind": "search", "query": "不存在的词", "candidate_directories": []},
    {"kind": "search", "query": "偏好", "candidate_directories": [], "max_results": 1},
    {"kind": "search", "query": "配置", "candidate_directories": [], "max_results": 0},
    {"kind": "search", "query": "配置", "candidate_directories": [], "max_results": 99},
    {"kind": "expand_related", "ids": ["20250102-030406"], "max_depth": 1},
    {"kind": "expand_related", "ids": ["20250102-030405"], "max_depth": 2, "max_results": 3},
    {"kind": "expand_related", "ids": ["不存在"], "max_depth": 1},
    {"kind": "expand_related", "ids": [], "max_depth": 1},
    {"kind": "expand_related", "ids": ["20250102-030407"], "max_depth": 99},
    {"kind": "expand_related", "ids": ["20250102-030406"], "max_depth": 1, "max_results": 0},
]


def render(result) -> dict:
    return {
        "id": result.id,
        "summary": result.summary,
        "storage_directory": result.storage_directory,
        "related_directories": list(result.related_directories),
        "timestamp": result.timestamp.astimezone(timezone.utc).isoformat(timespec="seconds"),
    }


def run_step(store: MemoryStore, step: dict) -> dict:
    if step["kind"] == "search":
        results = store.search(
            step["query"],
            list(step.get("candidate_directories", [])),
            step.get("max_results", 5),
        )
    else:
        results = store.expand_related(
            list(step["ids"]),
            step.get("max_depth", 1),
            step.get("max_results", 5),
        )
    return {"kind": step["kind"], "results": [render(item) for item in results]}


def main() -> None:
    root = Path(tempfile.mkdtemp(prefix="memory-search-"))
    try:
        index_entries = [
            {key: value for key, value in entry.items() if key not in {"body", "missing_file"}}
            for entry in ENTRIES
        ]
        (root / "index.json").write_text(
            json.dumps({"memories": index_entries}, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        for entry in ENTRIES:
            if entry.get("missing_file"):
                continue
            path = root / entry["path"]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(
                f'---\ntimestamp: "{OLD_TIMESTAMP}"\nrelated_directories:\n'
                + "".join(f'  - "{item}"\n' for item in entry["related_directories"])
                + f'---\n\n{entry["body"]}\n',
                encoding="utf-8",
            )

        initial_files = {}
        for path in sorted(root.rglob("*")):
            if path.is_file():
                initial_files[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")

        store = MemoryStore(root)
        outputs = [run_step(store, step) for step in STEPS]
    finally:
        shutil.rmtree(root, ignore_errors=True)

    fixture = {
        "source": ["omnicrawl/state/memory.py"],
        "timestamp": OLD_TIMESTAMP,
        "entries": index_entries,
        "initial_files": initial_files,
        "steps": STEPS,
        "expected_outputs": outputs,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    counts = [len(item["results"]) for item in outputs]
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：步骤 {len(STEPS)}，各步返回条数 {counts}")


if __name__ == "__main__":
    main()
