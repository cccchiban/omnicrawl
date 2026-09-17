#!/usr/bin/env python3
"""生成记忆存储「索引 + 读取」链路的对照数据集，供 Rust 侧 `memory_store` 使用。

期望值来自 Python 真实现 `omnicrawl/state/memory.py`：索引解析（信封 + 条目校验）、
路径越界防护、读取加深（刷新时间戳、touch_count + 1、重写 Markdown）、索引落盘
（按存储目录与路径排序、原子替换）。

时间戳一律归一化成占位：读取加深会把时间戳刷新成"现在"，只有归一化后才能比对。

用法：``python rust/tools/gen_memory_store_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/memory_store_parity.json``
"""

from __future__ import annotations

import json
import re
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/memory_store_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state.memory import MemoryStore  # noqa: E402

if not Path(sys.modules["omnicrawl.state.memory"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

TIMESTAMP = "2026-09-18T03:05:29+00:00"
TIMESTAMP_SLOT = "<timestamp>"

ENTRIES = [
    {
        "id": "20260918-030529",
        "path": "task-history/general/20260918-030529.md",
        "storage_directory": "task-history/general",
        "timestamp": TIMESTAMP,
        "touch_count": 2,
        "related_directories": ["task-history/general", "project-context/general"],
        "summary": "第一件事的摘要",
    },
    {
        "id": "20260918-030530",
        "path": "code-knowledge/general/20260918-030530.md",
        "storage_directory": "code-knowledge/general",
        "timestamp": TIMESTAMP,
        "touch_count": 0,
        "related_directories": ["code-knowledge/general"],
        "summary": "第二件事的摘要",
    },
    {
        "id": "20260918-030531",
        "path": "error-lessons/general/20260918-030531.md",
        "storage_directory": "error-lessons/general",
        "timestamp": TIMESTAMP,
        "touch_count": 5,
        "related_directories": ["error-lessons/general"],
        "summary": "索引里有但文件不在",
    },
]

MARKDOWN = {
    "task-history/general/20260918-030529.md": (
        f'---\ntimestamp: "{TIMESTAMP}"\nrelated_directories:\n'
        '  - "task-history/general"\n  - "project-context/general"\n---\n\n第一件事的正文\n'
    ),
    "code-knowledge/general/20260918-030530.md": (
        f'---\ntimestamp: "{TIMESTAMP}"\nrelated_directories:\n'
        '  - "code-knowledge/general"\n---\n\n第二件事的正文\n'
    ),
}

STEPS = [
    {"kind": "read", "ids": ["20260918-030529", "20260918-030530"]},
    {"kind": "read", "ids": ["20260918-030529", "20260918-030529", "   ", "不存在"]},
    {"kind": "read", "ids": []},
    {"kind": "read", "ids": ["20260918-030531"]},
    {"kind": "load_ids"},
]

TIMESTAMP_PATTERN = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})")


def normalize(value):
    if isinstance(value, str):
        return TIMESTAMP_PATTERN.sub(TIMESTAMP_SLOT, value)
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, dict):
        return {key: normalize(item) for key, item in value.items()}
    return value


def snapshot(root: Path, *, normalized: bool = True) -> dict:
    files = {}
    for path in sorted(root.rglob("*")):
        if path.is_file():
            text = path.read_text(encoding="utf-8")
            files[path.relative_to(root).as_posix()] = normalize(text) if normalized else text
    return files


def run_step(store: MemoryStore, step: dict) -> dict:
    kind = step["kind"]
    if kind == "read":
        records = store.read(list(step["ids"]))
        return {
            "kind": kind,
            "ids": step["ids"],
            "records": [
                {
                    "id": record.id,
                    "timestamp": record.timestamp.isoformat(timespec="microseconds"),
                    "related_directories": list(record.related_directories),
                    "content": record.content,
                }
                for record in records
            ],
        }
    if kind == "load_ids":
        return {"kind": kind, "ids": [entry.id for entry in store._load_entries()]}
    raise SystemExit(f"未知步骤：{kind}")


def main() -> None:
    root = Path(tempfile.mkdtemp(prefix="memory-store-"))
    try:
        (root / "index.json").write_text(
            json.dumps({"memories": ENTRIES}, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        for relative, text in MARKDOWN.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")

        # 注入必须用原始内容：时间戳占位符会让索引解析失败。
        initial_files = snapshot(root, normalized=False)
        store = MemoryStore(root)
        outputs = [normalize(run_step(store, step)) for step in STEPS]
        files = snapshot(root)
    finally:
        shutil.rmtree(root, ignore_errors=True)

    fixture = {
        "source": ["omnicrawl/state/memory.py"],
        "timestamp_slot": TIMESTAMP_SLOT,
        "initial_files": initial_files,
        "steps": STEPS,
        "expected_outputs": outputs,
        "expected_files": files,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：步骤 {len(STEPS)}，文件 {sorted(files)}")


if __name__ == "__main__":
    main()
