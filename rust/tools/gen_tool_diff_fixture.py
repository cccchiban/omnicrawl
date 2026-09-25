#!/usr/bin/env python3
"""生成 `omnicrawl-tui` 工具卡渲染层（tool_diff）的对照数据集。

期望值来自 Python 真实现 `omnicrawl/ui/fullscreen/rendering/tool_diff.py`：
`tool_disclosure_title`（工具卡标题：色点 + 原名 + 上下文 + 状态/耗时尾部）、
`tool_disclosure_body`（正文：文件变更预览 / fetcher 精选 / read 与记忆类隐藏 /
其余原样输出）与 `plain_tool_title`（纯文本标题）。

正文与标题都带样式（状态点颜色、diff 的 +/- 着色、隐藏类工具的正文为空），
因此数据集同时记录纯文本与「(样式, 文本) 运行段」，Rust 侧两者都要对齐。

用法（仓库根目录）：

    python rust/tools/gen_tool_diff_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test tool_diff_parity
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from omnicrawl.ui.fullscreen.rendering import tool_diff  # noqa: E402

OUTPUT = REPO_ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/tool_diff_parity.json"

LONG_PATH = "D:/projects/omnicrawl/rust/crates/omnicrawl-tui/src/ui/fullscreen/rendering/tool_diff.rs"
LONG_LINE = "x" * 200
EDIT_OLD = "\n".join(f"旧行{index}" for index in range(1, 7))
EDIT_NEW = "\n".join(["新行1", "新行2", "旧行3", "旧行4", "旧行5", "新行6"])
EDIT_RESULT = "\n".join(
    [
        "第 12-17 行",
        "  12 | 旧行1",
        "  13 | 旧行2",
        "  14 | 旧行3",
        "  15 | 旧行4",
        "  16 | 旧行5",
        "  17 | 旧行6",
        "已替换 1 处。",
    ]
)


def title_cases() -> list[dict]:
    """标题：覆盖状态色点、文件变更分支、工作区摘要分支、MCP 命名空间与压缩。"""

    return [
        {
            "tool": "bash",
            "arguments": {"command": "pytest -q"},
            "status": "成功",
            "duration_seconds": 1.5,
            "expanded": True,
            "result_text": "12 passed",
        },
        {
            "tool": "bash",
            "arguments": {"command": "pytest -q"},
            "status": "失败",
            "duration_seconds": 0.4,
            "expanded": True,
            "result_text": "1 failed",
        },
        {
            "tool": "bash",
            "arguments": {"command": "sleep 30"},
            "status": "调用中",
            "duration_seconds": 12.25,
            "expanded": False,
            "result_text": "",
        },
        {
            "tool": "bash",
            "arguments": {"command": "rm -rf /"},
            "status": "等待确认",
            "duration_seconds": 0.0,
            "expanded": True,
            "result_text": "",
        },
        {
            "tool": "bash",
            "arguments": {"command": "curl"},
            "status": "已取消",
            "duration_seconds": 3.0,
            "expanded": True,
            "result_text": "已取消",
        },
        {
            "tool": "read",
            "arguments": {"path": "a.py"},
            "status": "成功",
            "duration_seconds": 0.1,
            "expanded": True,
            "result_text": "第 3-8 行\n内容",
        },
        {
            "tool": "list",
            "arguments": {"path": "src"},
            "status": "成功",
            "duration_seconds": 0.2,
            "expanded": True,
            "result_text": "共 12 个文件",
        },
        {
            "tool": "grep",
            "arguments": {"path": "src", "pattern": "tool_disclosure_title"},
            "status": "调用中",
            "duration_seconds": 0.0,
            "expanded": True,
            "result_text": "",
        },
        {
            "tool": "write_file",
            "arguments": {"path": "a.py", "content": "行1\n行2\n行3"},
            "status": "成功",
            "duration_seconds": 0.6,
            "expanded": True,
            "result_text": "已写入 a.py",
        },
        {
            "tool": "Edit_file",
            "arguments": {"path": "a.py", "old_text": EDIT_OLD, "new_text": EDIT_NEW},
            "status": "成功",
            "duration_seconds": 0.9,
            "expanded": True,
            "result_text": EDIT_RESULT,
        },
        {
            "tool": "ask_user",
            "arguments": {"question": "继续吗？"},
            "status": "等待回复",
            "duration_seconds": 4.0,
            "expanded": True,
            "result_text": "",
        },
        {
            "tool": "fathom.search",
            "arguments": {"query": "omnicrawl"},
            "status": "成功",
            "duration_seconds": 2.0,
            "expanded": True,
            "result_text": "3 条结果",
        },
        {
            "tool": "read",
            "arguments": {"path": LONG_PATH},
            "status": "成功",
            "duration_seconds": 0.3,
            "expanded": True,
            "result_text": "第 1-4200 行",
        },
        {
            "tool": "read_image",
            "arguments": {"path": "shot.png"},
            "status": "成功",
            "duration_seconds": 0.5,
            "expanded": True,
            "result_text": "已读取图片",
        },
    ]


def body_cases() -> list[dict]:
    """正文：原样输出 / 文件变更预览 / fetcher 精选 / 隐藏类（read、记忆、知识库）。"""

    return [
        {
            "tool": "bash",
            "arguments": {"command": "pytest -q"},
            "result_text": "12 passed\n3 warnings",
        },
        {
            "tool": "bash",
            "arguments": {"command": "echo"},
            "result_text": LONG_LINE,
        },
        {
            "tool": "read",
            "arguments": {"path": "a.py"},
            "result_text": "文件全文都不该出现在对话里",
        },
        {
            "tool": "memory_write",
            "arguments": {"key": "k"},
            "result_text": "已记住",
        },
        {
            "tool": "kb_search",
            "arguments": {"query": "q"},
            "result_text": "命中 3 条",
        },
        {
            "tool": "fetcher",
            "arguments": {"url": "https://example.com"},
            "result_text": "URL: https://example.com\n状态: 200\n标题: Example\n正文一大段不该显示",
        },
        {
            "tool": "write_file",
            "arguments": {"path": "a.py", "content": "行1\n行2\n行3"},
            "result_text": "已写入 a.py",
        },
        {
            "tool": "write_file",
            "arguments": {"path": "a.py", "content": "附加行", "mode": "append"},
            "result_text": "已追加 a.py",
        },
        {
            "tool": "Edit_file",
            "arguments": {"path": "a.py", "old_text": EDIT_OLD, "new_text": EDIT_NEW},
            "result_text": EDIT_RESULT,
        },
        {
            "tool": "Edit_file",
            "arguments": {"path": "a.py", "old_text": EDIT_OLD, "new_text": EDIT_NEW},
            "result_text": "",
        },
        {
            "tool": "monitor",
            "arguments": {"command": "tail -f app.log"},
            "result_text": "批次 1：3 行新输出",
        },
    ]


def plain_title_cases() -> list[dict]:
    """纯文本标题（`plain_tool_title`）：终端之外的调用方也用同一条渲染规则。"""

    return [
        {
            "tool": "bash",
            "arguments": {"command": "pytest -q"},
            "status": "成功",
            "duration_seconds": 1.5,
            "expanded": False,
            "result_text": "12 passed",
        },
        {
            "tool": "read",
            "arguments": {"path": "a.py"},
            "status": "成功",
            "duration_seconds": 0.1,
            "expanded": False,
            "result_text": "第 3-8 行\n内容",
        },
        {
            "tool": "Edit_file",
            "arguments": {"path": "a.py", "old_text": EDIT_OLD, "new_text": EDIT_NEW},
            "status": "失败",
            "duration_seconds": 2.5,
            "expanded": False,
            "result_text": EDIT_RESULT,
        },
    ]


def style_runs(text) -> list[list[str]]:
    """把 rich Text 拆成 (样式, 文本) 运行段，相邻同样式合并。"""

    plain = text.plain
    styles: list[str] = [""] * len(plain)
    for start, end, style in text.spans:
        value = "" if style is None else str(style)
        for index in range(start, min(end, len(plain))):
            styles[index] = value
    runs: list[list[str]] = []
    for index, char in enumerate(plain):
        if runs and runs[-1][0] == styles[index]:
            runs[-1][1] += char
        else:
            runs.append([styles[index], char])
    return runs


def title_case(case: dict) -> dict:
    text = tool_diff.tool_disclosure_title(
        tool_name=case["tool"],
        arguments=case["arguments"],
        status=case["status"],
        duration_seconds=case["duration_seconds"],
        expanded=case["expanded"],
        result_text=case["result_text"],
    )
    return {**case, "expected_plain": text.plain, "expected_spans": style_runs(text)}


def body_case(case: dict) -> dict:
    text = tool_diff.tool_disclosure_body(
        tool_name=case["tool"],
        arguments=case["arguments"],
        result_text=case["result_text"],
    )
    return {**case, "expected_plain": text.plain, "expected_spans": style_runs(text)}


def plain_title_case(case: dict) -> dict:
    value = tool_diff.plain_tool_title(
        tool_name=case["tool"],
        arguments=case["arguments"],
        status=case["status"],
        duration_seconds=case["duration_seconds"],
        expanded=case["expanded"],
        result_text=case["result_text"],
    )
    return {**case, "expected": value}


def main() -> int:
    data = {
        "source": "omnicrawl/ui/fullscreen/rendering/tool_diff.py",
        "titles": [title_case(case) for case in title_cases()],
        "bodies": [body_case(case) for case in body_cases()],
        "plain_titles": [plain_title_case(case) for case in plain_title_cases()],
    }
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {OUTPUT}（{len(data['titles'])} 标题 + {len(data['bodies'])} 正文 + {len(data['plain_titles'])} 纯文本标题）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
