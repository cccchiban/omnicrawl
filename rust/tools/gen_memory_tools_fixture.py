#!/usr/bin/env python3
"""生成 `omnicrawl-tui` 记忆工具的对照数据集。

期望值来自 Python 真实现：`omnicrawl/agent/toolkit/memory_tools.py` 的四个适配函数，
底层存储是 `omnicrawl/state/memory.py` 的 `MemoryStore`。

记忆 id 与时间戳由存储层按其自身规则生成（两侧不会逐字相同），因此数据集把它们
规范化成 `{ID}` / `{TS}` 占位符——存储层的 id/时间戳生成已由 `omnicrawl-session`
的 `memory_store` 对照覆盖，这里对照的是工具层的参数适配与输出形状。

用法（仓库根目录）：

    python rust/tools/gen_memory_tools_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test memory_tools_parity
"""

from __future__ import annotations

import json
import re
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/memory_tools_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.agent.toolkit.memory_tools import (  # noqa: E402
    memory_expand_related_result,
    memory_read_result,
    memory_search_result,
    memory_write_result,
)
from omnicrawl.state.memory import MemoryStore  # noqa: E402

if not Path(sys.modules["omnicrawl.state.memory"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")

ID_PATTERN = re.compile(r'"id": "[^"]*"')
TIMESTAMP_PATTERN = re.compile(r'"timestamp": "[^"]*"')

SEED = {
    "memories": [
        {
            "content": "Rust 客户端需要独立执行工具，不再借用 Python 宿主。",
            "related_directories": ["rust/crates", "rust/crates/omnicrawl-tui"],
            "storage_directory": "code-knowledge/general",
        },
        {
            "content": "知识库工具已经落在 Rust 侧客户端里。",
            "related_directories": ["rust/crates/omnicrawl-tui/src/tools"],
            "storage_directory": "code-knowledge/general",
        },
        {
            "content": "记忆工具的作用域路由：project、session、user 三处。",
            "related_directories": ["omnicrawl/agent/controllers"],
            "storage_directory": "project-context/memory",
        },
    ]
}

CASES: list[tuple[str, dict]] = [
    ("memory_write", {"memories": []}),
    ("memory_write", {"memories": "nope"}),
    ("memory_write", {"memories": ["nope"]}),
    ("memory_write", {"memories": [{"related_directories": ["a"]}]}),
    ("memory_write", {"memories": [{"content": "x", "related_directories": None}]}),
    ("memory_write", {"memories": [{"content": "x", "related_directories": [1]}]}),
    ("memory_write", {"memories": [{"content": "x", "source_event": 7}]}),
    ("memory_write", {"memories": [{"content": "x", "storage_directory": 7}]}),
    (
        "memory_write",
        {
            "memories": [
                {
                    "content": "新的记忆条目：工具层参数适配已对照。",
                    "related_directories": ["rust/crates/omnicrawl-tui"],
                    "storage_directory": "code-knowledge/general",
                    "source_event": "turn-1",
                }
            ]
        },
    ),
    ("memory_search", {"query": "Rust", "reason": "找相关记忆"}),
    ("memory_search", {"query": "", "reason": "x"}),
    ("memory_search", {"query": "x", "reason": ""}),
    (
        "memory_search",
        {
            "query": "Rust",
            "reason": "按目录收窄",
            "candidate_directories": ["rust/crates"],
            "max_results": 2,
        },
    ),
    ("memory_search", {"query": "Rust", "reason": "越界上限", "max_results": 99}),
    ("memory_search", {"query": "记忆 作用域", "reason": "多词查询"}),
    ("memory_search", {"query": "不存在的关键词", "reason": "空结果"}),
    ("memory_read", {"memory_ids": []}),
    ("memory_read", {"memory_ids": ["missing-memory"]}),
    ("memory_read", {"memory_ids": ["{ID1}"]}),
    ("memory_read", {"memory_ids": ["{ID1}", "{ID3}"]}),
    ("memory_expand_related", {"memory_ids": []}),
    ("memory_expand_related", {"memory_ids": ["{ID1}"]}),
    (
        "memory_expand_related",
        {"memory_ids": ["{ID1}", "{ID3}"], "max_depth": 2, "max_results": 3},
    ),
    ("memory_expand_related", {"memory_ids": ["{ID2}"], "max_depth": 9, "max_results": 0}),
]


def normalize(text: str) -> str:
    text = ID_PATTERN.sub('"id": "{ID}"', text)
    return TIMESTAMP_PATTERN.sub('"timestamp": "{TS}"', text)


def substitute(value, ids: list[str]):
    if isinstance(value, str):
        for index, memory_id in enumerate(ids, start=1):
            value = value.replace(f"{{ID{index}}}", memory_id)
        return value
    if isinstance(value, list):
        return [substitute(item, ids) for item in value]
    if isinstance(value, dict):
        return {key: substitute(item, ids) for key, item in value.items()}
    return value


def main() -> int:
    handlers = {
        "memory_search": memory_search_result,
        "memory_read": memory_read_result,
        "memory_expand_related": memory_expand_related_result,
        "memory_write": memory_write_result,
    }

    workdir = Path(tempfile.mkdtemp(prefix="omnicrawl-memory-fixture-"))
    try:
        store = MemoryStore(workdir / "project-memory")
        seeded = memory_write_result(store, SEED)
        if not seeded.ok:
            raise SystemExit(f"预置记忆失败：{seeded.output}")
        records = json.loads(seeded.output)
        ids = [record["id"] for record in records]

        cases = []
        for tool, raw_arguments in CASES:
            arguments = substitute(raw_arguments, ids)
            result = handlers[tool](store, arguments)
            cases.append(
                {
                    "tool": tool,
                    "arguments": raw_arguments,
                    "ok": bool(result.ok),
                    "output": normalize(result.output),
                }
            )
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    data = {
        "source": "omnicrawl/agent/toolkit/memory_tools.py + omnicrawl/state/memory.py",
        "seed": SEED,
        "id_placeholder": "{ID}",
        "timestamp_placeholder": "{TS}",
        "cases": cases,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"已写入 {FIXTURE_PATH}（{len(cases)} 例）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
