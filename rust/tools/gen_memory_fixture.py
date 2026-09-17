#!/usr/bin/env python3
"""生成记忆层格式对照数据集，供 Rust 侧 `omnicrawl-session::memory` 的 parity 测试使用。

期望值来自 Python 真实现 `omnicrawl/state/memory.py` 的格式层：目录与路径归一化、
关联目录去重、正文归一化、Markdown 记录格式与正文提取、索引条目字段校验。

时间戳统一按 UTC 记录：两边都按各自本地时区渲染（同一台机器上一致），
跨机比对时归一化到 UTC 才不会因时区不同而失败。

用法：``python rust/tools/gen_memory_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/memory_parity.json``
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/memory_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state import memory as M  # noqa: E402

if not Path(M.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

TIMESTAMP = datetime(2026, 9, 18, 3, 5, 29, 123456, tzinfo=timezone.utc)
TIMESTAMP_UTC_TEXT = TIMESTAMP.astimezone(timezone.utc).isoformat(timespec="seconds")
TIMESTAMP_UTC_SLOT = "｛Desensitized:122｝"


def attempt(func, value):
    try:
        return {"input": value, "expected": func(value)}
    except M.MemoryStoreError as exc:
        return {"input": value, "error": str(exc)}


def markdown_case(name: str, related: list[str], content: str) -> dict:
    text = M._format_memory_markdown(TIMESTAMP, related, content)
    # 把本地时区渲染的时间戳换成 UTC 渲染，比对就不再依赖运行机器的时区。
    local_text = M._format_datetime(TIMESTAMP)
    return {
        "name": name,
        "timestamp": TIMESTAMP_UTC_TEXT,
        "related_directories": related,
        "content": content,
        "expected": text.replace(local_text, TIMESTAMP_UTC_SLOT),
    }


def body_case(name: str, text: str) -> dict:
    root = Path(tempfile.mkdtemp(prefix="memory-body-"))
    try:
        path = root / "记录.md"
        path.write_text(text, encoding="utf-8")
        return {"name": name, "text": text, "expected": M._read_markdown_body(path)}
    finally:
        shutil.rmtree(root, ignore_errors=True)


def index_case(value: dict) -> dict:
    try:
        entry = M.MemoryIndexEntry.from_dict(value)
    except M.MemoryStoreError as exc:
        return {"input": value, "error": str(exc)}
    data = entry.to_dict()
    data["timestamp"] = TIMESTAMP_UTC_SLOT
    return {"input": value, "expected": data}


BASE_ENTRY = {
    "id": "20260918-030529",
    "path": "code-knowledge/General/记录.md",
    "storage_directory": "code-knowledge/general",
    "timestamp": TIMESTAMP_UTC_TEXT,
    "touch_count": 3,
    "related_directories": ["Code-Knowledge/General", "src//lib", ".", "../逃逸", 5],
    "summary": "  摘要  ",
}

DIRECTORIES = [
    "code-knowledge/general",
    "  Code-Knowledge//General  ",
    "多 个 空白/目录",
    "中文目录/子目录",
    "a/../../escape",
    "../escape",
    "/absolute/path",
    "C:/drive/path",
    "",
    "   ",
    ".",
    "..",
    "---",
    "a.b-c_d/e",
]

PATHS = [
    "code-knowledge/general/20260918-030529.md",
    "Dir/Sub/My File.md",
    "dir/sub/x.txt",
    "记录.md",
    ".md",
    "dir//x.md",
    "A/B/C.MD",
    "dir/../x.md",
]

CONTENTS = [
    "第一行\n第二行",
    "第一行\r\n第二行",
    "第一行\r第二行",
    "  带空白  ",
    "",
]


def main() -> None:
    fixture = {
        "source": ["omnicrawl/state/memory.py"],
        "timestamp": TIMESTAMP_UTC_TEXT,
        "timestamp_slot": TIMESTAMP_UTC_SLOT,
        "directories": [attempt(M._normalize_directory, value) for value in DIRECTORIES],
        "paths": [attempt(M._normalize_relative_file_path, value) for value in PATHS],
        "dedupe_directories": [
            {
                "input": ["Code-Knowledge/General", "code-knowledge/general", ".", " ", 5, "src"],
                "expected": M._dedupe_directories(
                    ["Code-Knowledge/General", "code-knowledge/general", ".", " ", 5, "src"]
                ),
            },
            {"input": [], "expected": M._dedupe_directories([])},
        ],
        "dedupe_strings": [
            {
                "input": ["  一  ", "一", "", "   ", 5, "二"],
                "expected": M._dedupe_strings(["  一  ", "一", "", "   ", 5, "二"]),
            },
            {"input": [], "expected": M._dedupe_strings([])},
        ],
        "contents": [{"input": value, "expected": M._normalize_content(value)} for value in CONTENTS],
        "markdown": [
            markdown_case("plain", ["code-knowledge/general"], "正文第一行\n正文第二行"),
            markdown_case("with_quotes", ["a/b"], '正文里带 "引号" 与 \\ 反斜杠'),
            markdown_case("no_related", [], "无关联目录"),
            markdown_case("trailing_blank", ["x"], "正文末尾有空行\n\n"),
        ],
        "bodies": [
            body_case("frontmatter", "---\ntimestamp: \"x\"\nrelated_directories:\n---\n\n正文\n"),
            body_case("frontmatter_crlf", "---\r\ntimestamp: \"x\"\r\n---\r\n\r\n正文\r\n"),
            body_case("no_frontmatter", "  正文  "),
            body_case("unterminated", "---\ntimestamp: \"x\"\n正文"),
            body_case("empty", ""),
        ],
        "index_entries": [
            index_case(BASE_ENTRY),
            index_case({key: value for key, value in BASE_ENTRY.items() if key != "timestamp"}),
            index_case({**BASE_ENTRY, "id": "  "}),
            index_case({**BASE_ENTRY, "path": "not-markdown.txt"}),
            index_case({**BASE_ENTRY, "storage_directory": "/abs"}),
            index_case({**BASE_ENTRY, "timestamp": "不是时间"}),
            index_case({**BASE_ENTRY, "related_directories": "不是列表"}),
            index_case({**BASE_ENTRY, "related_directories": ["a/b", 5]}),
            index_case({**BASE_ENTRY, "touch_count": -1}),
            index_case({**BASE_ENTRY, "touch_count": True}),
            index_case({**BASE_ENTRY, "summary": 5}),
            index_case({**BASE_ENTRY, "touch_count": 0, "summary": ""}),
        ],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    counts = {key: len(value) for key, value in fixture.items() if isinstance(value, list)}
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：{counts}")


if __name__ == "__main__":
    main()
