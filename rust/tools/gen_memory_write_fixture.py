#!/usr/bin/env python3
"""生成记忆写入链路的对照数据集，供 Rust 侧 `memory_store` 的写入路径使用。

期望值来自 Python 真实现 `omnicrawl/state/memory.py`：批量写入（校验先行、创建/合并、
失败回滚、id 生成、写入后过期清理）与索引落盘。

记忆 id 由时间戳生成、时间戳又是「现在」，因此两侧都用同一套规则归一化：
先按出现顺序把 id 换成 `<memory-id-N>`（先扫输出、再扫文件名与正文），再把时间戳换成占位。

用法：``python rust/tools/gen_memory_write_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/memory_write_parity.json``
"""

from __future__ import annotations

import json
import re
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/memory_write_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state.memory import MemoryStore, MemoryWriteRequest  # noqa: E402

if not Path(sys.modules["omnicrawl.state.memory"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

MEMORY_ID_PATTERN = re.compile(r"\d{8}-\d{6}(?:-\d{3})?")
TIMESTAMP_PATTERN = re.compile(
    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})"
)

STEPS = [
    {
        "kind": "write",
        "requests": [
            {
                "content": "用户偏好：回答要简短。",
                "related_directories": ["User-Preferences/Communication-Style"],
            },
            {
                "content": "项目架构分三层：入口、控制器、运行时。",
                "related_directories": [],
                "storage_directory": "Project-Context/General",
            },
            {"content": "错误处理踩坑记录。", "related_directories": []},
        ],
    },
    {
        "kind": "write",
        "requests": [
            {
                "content": "用户偏好：回答要简短。",
                "related_directories": ["user-preferences/communication-style"],
            }
        ],
    },
    {"kind": "write", "requests": [{"content": "   ", "related_directories": []}]},
    {"kind": "write", "requests": []},
]


def snapshot(root: Path) -> dict:
    files = {}
    for path in sorted(root.rglob("*")):
        if path.is_file():
            files[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    return files


def discover_ids(outputs_text: str, files: dict) -> list[str]:
    found: list[str] = []

    def collect(text: str) -> None:
        for match in MEMORY_ID_PATTERN.findall(text):
            if match not in found:
                found.append(match)

    collect(outputs_text)
    for relative in sorted(files):
        collect(relative)
        collect(files[relative])
    return found


def normalize(outputs_text: str, files: dict) -> tuple[object, dict]:
    ids = discover_ids(outputs_text, files)
    mapping = {memory_id: f"<memory-id-{index + 1}>" for index, memory_id in enumerate(ids)}

    # 先替换更长的 id：同秒生成的记忆 id 互为前缀，短的先替换会串到长的里面去。
    ordered = sorted(mapping.items(), key=lambda item: len(item[0]), reverse=True)

    def apply(text: str) -> str:
        for memory_id, placeholder in ordered:
            text = text.replace(memory_id, placeholder)
        return TIMESTAMP_PATTERN.sub("<timestamp>", text)

    normalized_files = {apply(key): apply(value) for key, value in files.items()}
    return json.loads(apply(outputs_text)), normalized_files


def run_step(store: MemoryStore, step: dict) -> dict:
    requests = [
        MemoryWriteRequest(
            content=item["content"],
            related_directories=list(item.get("related_directories", [])),
            storage_directory=item.get("storage_directory"),
        )
        for item in step["requests"]
    ]
    try:
        records = store.write(requests)
    except Exception as exc:  # noqa: BLE001 - 对照要记录错误文案
        return {"kind": step["kind"], "error": str(exc)}
    return {
        "kind": step["kind"],
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


def main() -> None:
    root = Path(tempfile.mkdtemp(prefix="memory-write-"))
    try:
        store = MemoryStore(root)
        outputs = [run_step(store, step) for step in STEPS]
        files = snapshot(root)
    finally:
        shutil.rmtree(root, ignore_errors=True)

    outputs_text = json.dumps(outputs, ensure_ascii=False)
    normalized_outputs, normalized_files = normalize(outputs_text, files)

    fixture = {
        "source": ["omnicrawl/state/memory.py"],
        "steps": STEPS,
        "expected_outputs": normalized_outputs,
        "expected_files": normalized_files,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：步骤 {len(STEPS)}，"
        f"文件 {sorted(normalized_files)}，"
        f"id 数 {len(discover_ids(outputs_text, files))}"
    )


if __name__ == "__main__":
    main()
