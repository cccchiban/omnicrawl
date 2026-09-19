#!/usr/bin/env python3
"""生成 `omnicrawl-tui` 知识库工具的对照数据集。

期望值来自 Python 真实现：`omnicrawl/knowledge/__init__.py` 的 `KnowledgeBase` 与
`omnicrawl/agent/toolkit/knowledge_tools.py` 的五个 `kb_*` 适配函数。

用法（仓库根目录）：

    python rust/tools/gen_knowledge_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test knowledge_tools_parity
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/knowledge_tools_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.agent.toolkit.knowledge_tools import (  # noqa: E402
    kb_append_result,
    kb_list_result,
    kb_read_result,
    kb_search_result,
    kb_write_result,
)
from omnicrawl.knowledge import KnowledgeBase  # noqa: E402

if not Path(sys.modules["omnicrawl.knowledge"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")

TODAY_PLACEHOLDER = "{TODAY}"

# 初始知识库状态：两侧用同一份文本铺底，写入类用例会改变其后续状态。
INITIAL_FILES: dict[str, str] = {
    "projects/示例项目/2001-01-02-Rust改写盘点.md": (
        "---\n"
        "title: Rust 改写盘点\n"
        "created: 2026-09-18\n"
        "updated: 2026-09-19\n"
        "project: AI课堂任务/OmniCrawl\n"
        "tags: [rust, 盘点]\n"
        "type: research\n"
        "status: done\n"
        "---\n"
        "\n"
        "## 目的\n"
        "\n"
        "用户问「继续用 Rust 改写，还有哪些没改写」。检索：Rust 内核。\n"
    ),
    "topics/rust/迁移顺序.md": (
        "---\n"
        "title: 迁移顺序\n"
        "created: 2026-09-01\n"
        "updated: 2026-09-05\n"
        "project: \n"
        "tags: [rust]\n"
        "type: note\n"
        "status: draft\n"
        "---\n"
        "\n"
        "引擎先搬 protocol，再搬内核循环。Rust 检索命中关键词。\n"
    ),
    "daily/2001/01/2001-01-05.md": (
        "---\n"
        "title: 2026-09-19 工作日志\n"
        "created: 2026-09-19\n"
        "updated: 2026-09-19\n"
        "project: AI课堂任务/OmniCrawl\n"
        "tags: [日志]\n"
        "type: log\n"
        "status: draft\n"
        "---\n"
        "\n"
        "今天把 controllers 判定层搬进内核。\n"
    ),
    "notes/plain.md": "无 frontmatter 的纯正文，包含 Rust 字样。\n",
    "notes/extra.md": (
        "---\n"
        "title: 未映射字段\n"
        "created: 2026-09-02\n"
        "updated: 2026-09-03\n"
        "project: \n"
        "tags: [杂项]\n"
        "type: note\n"
        "status: archived\n"
        "owner: 示例\n"
        "---\n"
        "\n"
        "自定义 frontmatter 字段在覆盖时应当保留。\n"
    ),
    "INDEX.md": "# Knowledge Base Index\n\n手工写入，扫描时应当跳过。\n",
    "README.md": "# 工作知识库\n\n扫描时应当跳过。\n",
    ".hidden/skip.md": "隐藏目录里的笔记不会被扫描。\n",
    "attachments/note.txt": "非 Markdown 文件不会被扫描。\n",
}

CASES: list[tuple[str, dict]] = [
    ("kb_list", {}),
    ("kb_list", {"path": "projects"}),
    ("kb_list", {"path": "notes/extra.md"}),
    ("kb_list", {"path": "missing-dir"}),
    ("kb_list", {"project": "示例项目"}),
    ("kb_list", {"tags": ["rust"]}),
    ("kb_list", {"tags": ["not-there"]}),
    ("kb_list", {"type": "note"}),
    ("kb_list", {"status": "done"}),
    ("kb_list", {"max_results": 2}),
    ("kb_list", {"max_results": "3"}),
    ("kb_search", {"query": "Rust"}),
    ("kb_search", {"query": "检索 命中"}),
    ("kb_search", {"query": "内核", "max_results": 1}),
    ("kb_search", {"query": "rust", "project": "示例项目"}),
    ("kb_search", {"query": "Rust", "tags": ["盘点"]}),
    ("kb_search", {"query": "Rust", "type": "research"}),
    ("kb_search", {"query": "Rust", "status": "draft"}),
    ("kb_search", {"query": "zzz-不存在"}),
    ("kb_search", {"query": ""}),
    ("kb_search", {"query": "Rust", "max_results": 99}),
    ("kb_read", {"path": "notes/plain.md"}),
    ("kb_read", {"path": "notes/plain"}),
    ("kb_read", {"path": "notes/plain.md", "max_chars": 10}),
    ("kb_read", {"path": "notes/missing"}),
    ("kb_read", {"path": ""}),
    ("kb_read", {"path": "../escape"}),
    ("kb_read", {"path": "/notes/plain.md"}),
    ("kb_write", {"path": "", "content": "x"}),
    ("kb_write", {"path": "notes/null.md", "content": 5}),
    ("kb_write", {"path": "notes/mode.md", "content": "x", "mode": "replace"}),
    ("kb_write", {"path": "notes/type.md", "content": "x", "type": "todo"}),
    ("kb_write", {"path": "notes/status.md", "content": "x", "status": "wip"}),
    ("kb_write", {"path": "../escape.md", "content": "x"}),
    (
        "kb_write",
        {
            "path": "projects/demo/内部试用",
            "content": "第一段正文。\n\n第二段正文。",
            "title": " 示范笔记 ",
            "project": "演示/项目",
            "tags": [" Rust ", "", "示例"],
            "type": " LOG ",
            "status": "DONE",
            "mode": "create",
        },
    ),
    (
        "kb_write",
        {
            "path": "projects/demo/内部试用",
            "content": "重复创建",
            "mode": "create",
        },
    ),
    (
        "kb_write",
        {
            "path": "notes/extra",
            "content": "覆盖后的正文。",
            "title": "覆盖标题",
            "tags": ["覆盖"],
        },
    ),
    ("kb_write", {"path": "notes/append-target.md", "content": "第一次写入。"}),
    ("kb_append", {"path": "notes/append-target", "content": "追加内容。"}),
    ("kb_append", {"path": "", "content": "x"}),
    ("kb_append", {"path": "notes/append-target.md", "content": 7}),
    ("kb_read", {"path": "notes/append-target.md"}),
    ("kb_read", {"path": "notes/extra.md"}),
    ("kb_list", {"path": "projects/demo"}),
]


def build_base(root: Path) -> KnowledgeBase:
    for rel_path, text in INITIAL_FILES.items():
        target = root / rel_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
    return KnowledgeBase(root)


def normalize(text: str, today: str) -> str:
    return text.replace(today, TODAY_PLACEHOLDER)


def main() -> int:
    today = datetime.now().astimezone().date().isoformat()
    handlers = {
        "kb_search": kb_search_result,
        "kb_read": kb_read_result,
        "kb_write": kb_write_result,
        "kb_append": kb_append_result,
        "kb_list": kb_list_result,
    }

    workdir = Path(tempfile.mkdtemp(prefix="omnicrawl-kb-fixture-"))
    try:
        cases = []
        base = build_base(workdir)
        for tool, arguments in CASES:
            result = handlers[tool](base, arguments)
            cases.append(
                {
                    "tool": tool,
                    "arguments": arguments,
                    "ok": bool(result.ok),
                    "output": normalize(result.output, today),
                }
            )
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    data = {
        "source": (
            "omnicrawl/knowledge/__init__.py + "
            "omnicrawl/agent/toolkit/knowledge_tools.py"
        ),
        "today_placeholder": TODAY_PLACEHOLDER,
        "initial": [
            {"path": path, "text": text} for path, text in INITIAL_FILES.items()
        ],
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
