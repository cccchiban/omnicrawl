#!/usr/bin/env python3
"""生成记忆排序纯逻辑的对照数据集，供 Rust 侧 `omnicrawl-session::memory_ranking` 使用。

期望值来自 Python 真实现 `omnicrawl/state/memory_ranking.py`：目录分类、摘要生成、
正文合并、目录匹配打分、目录重叠、检索 token 抽取、搜索分与关联展开分、比较用归一化，
以及 `text_similarity`（difflib 的 Ratcliff-Obershelp 匹配，含 autojunk）。

用法：``python rust/tools/gen_memory_ranking_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/memory_ranking_parity.json``
"""

from __future__ import annotations

import json
import random
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/memory_ranking_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state import memory_ranking as R  # noqa: E402
from omnicrawl.state.memory import MemoryIndexEntry  # noqa: E402

if not Path(R.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

NOW = datetime(2026, 9, 18, 3, 5, 29, 123456, tzinfo=timezone.utc)
NOW_TEXT = NOW.astimezone(timezone.utc).isoformat(timespec="microseconds")

CLASSIFY_INPUTS = [
    "用户偏好是回答要简短",
    "这个仓库的架构约束是分层",
    "实现了一个函数用来解析配置",
    "踩坑：这个 bug 是编码问题",
    "外部服务的 token 放在环境变量里",
    "本次任务完成了三件事",
    "plan 里写了跟进事项",
    "毫不相关的普通句子",
    "PREFER 大写的偏好关键词",
    "",
]

SUMMARY_INPUTS = [
    "第一句。第二句。",
    "```python\nprint('x')\n```\n正文在这里。",
    "- 列表项一\n- 列表项二\n# 标题\n> 引用\n正文",
    "很长的正文" + "很长" * 100,
    "   \n\n   ",
    "没有句末标记也没有换行的一整句" * 20,
    "短句！后面还有内容。",
    "只有一个分号；这是后面的内容" * 10,
    "```未闭合的围栏\n正文",
    "前" * 118 + "。尾巴",
]

MERGE_CASES = [
    ("旧内容。", "新内容。"),
    ("", "新内容。"),
    ("旧内容包含关键词。", "内容包含关键词"),
    ("旧内容。", "完全不同的新内容。"),
    ("旧内容。", "  旧内容。  "),
    ("旧内容。", ""),
]

ENTRY_SPECS = [
    {
        "id": "20260918-030529",
        "storage_directory": "task-history/general",
        "related_directories": ["task-history/general", "project-context/general"],
        "summary": "任务完成情况与待办",
        "touch_count": 3,
    },
    {
        "id": "20260918-030530",
        "storage_directory": "code-knowledge/general",
        "related_directories": ["code-knowledge/general"],
        "summary": "解析配置的实现",
        "touch_count": 0,
    },
    {
        "id": "20260918-030531",
        "storage_directory": "user-preferences/general",
        "related_directories": ["user-preferences/communication-style"],
        "summary": "沟通偏好",
        "touch_count": 7,
    },
]

DIRECTORY_INPUTS = [
    "task-history/general",
    "task-history",
    "task-history/general/deep",
    "project-context/general",
    "user-preferences/communication-style",
    "user-preferences",
    "code-knowledge/general",
    "unrelated/dir",
]

OVERLAP_CASES = [
    (["a/b", "c"], ["c/d"]),
    (["a/b"], ["a/b/c"]),
    (["a/b"], ["x/y"]),
    ([], ["x"]),
    (["a/b"], []),
]


SIMILARITY_PAIRS = [
    ("完全相同的内容。", "完全相同的内容。"),
    ("内容 A", "内容 B"),
    ("", "有内容"),
    ("有内容", ""),
    ("", ""),
    ("abc" * 100, "abc" * 100),
    ("a" * 300, "a" * 300),
    ("a" * 199, "a" * 199),
    ("a" * 200, "a" * 200),
    ("abcabcabcabc", "abcabcabc"),
    ("旧内容包含关键词。", "内容包含关键词"),
    ("这次任务完成了三件事：一、二、三。", "这次任务完成了三件事：一、二、三"),
    ("代码实现细节说明", "代码实现细节说明补充"),
    ("很长的中文描述" * 30, "很长的中文描述" * 30),
    ("qwertyuiopasdfghjkl", "poiuytrewqlkjhgfdsa"),
]

random.seed(20260918)
_ALPHABET = "abcdef"
for _ in range(20):
    left = "".join(random.choice(_ALPHABET) for _ in range(random.randint(0, 260)))
    right = "".join(random.choice(_ALPHABET) for _ in range(random.randint(0, 260)))
    SIMILARITY_PAIRS.append((left, right))
for _ in range(5):
    base = "".join(random.choice("abcdefgh") for _ in range(random.randint(30, 240)))
    mutated = list(base)
    for _ in range(random.randint(1, 6)):
        if mutated:
            mutated[random.randrange(len(mutated))] = random.choice("abcdefgh")
    SIMILARITY_PAIRS.append((base, "".join(mutated)))


def entry_of(spec: dict) -> MemoryIndexEntry:
    return MemoryIndexEntry.from_dict(
        {
            "id": spec["id"],
            "path": f"{spec['storage_directory']}/{spec['id']}.md",
            "storage_directory": spec["storage_directory"],
            "timestamp": NOW_TEXT,
            "touch_count": spec["touch_count"],
            "related_directories": spec["related_directories"],
            "summary": spec["summary"],
        }
    )


def main() -> None:
    entries = [entry_of(spec) for spec in ENTRY_SPECS]
    search_cases = []
    for query in ["任务", "配置解析", "", "TASK", "沟通偏好"]:
        for directories in [[], ["task-history/general"], ["code-knowledge"]]:
            search_cases.append(
                {
                    "query": query,
                    "candidate_directories": directories,
                    "entries": [
                        R.score_search_entry(entry, query, directories, now=NOW)
                        for entry in entries
                    ],
                }
            )

    fixture = {
        "source": ["omnicrawl/state/memory_ranking.py"],
        "now": NOW_TEXT,
        "classify": [
            {"input": text, "expected": R.classify_storage_directory(text)}
            for text in CLASSIFY_INPUTS
        ],
        "summaries": [
            {"input": text, "expected": R.make_summary(text)} for text in SUMMARY_INPUTS
        ],
        "merges": [
            {"old": old, "new": new, "expected": R.merge_memory_content(old, new)}
            for old, new in MERGE_CASES
        ],
        "directory_scores": [
            {
                "entry": ENTRY_SPECS[index % len(ENTRY_SPECS)],
                "directory": directory,
                "expected": R.directory_match_score(entries[index % len(entries)], directory),
            }
            for index, directory in enumerate(DIRECTORY_INPUTS)
        ],
        "overlaps": [
            {
                "left": left,
                "right": right,
                "expected": R.directories_overlap(left, right),
            }
            for left, right in OVERLAP_CASES
        ],
        "tokens": [
            {"input": text, "expected": sorted(R.extract_search_tokens(text))}
            for text in [
                "Hello_World 版本2",
                "中文分词测试一下",
                "混合 mixed 内容 abc",
                "a b c",
                "",
                "超长的中文片段需要切成若干元组",
            ]
        ],
        "normalize": [
            {"input": text, "expected": R.normalize_for_compare(text)}
            for text in ["Hello, World!", "中文，标点。", "a_b-c", "   ", "TASK-history"]
        ],
        "similarity": [
            {"left": left, "right": right, "expected": R.text_similarity(left, right)}
            for left, right in SIMILARITY_PAIRS
        ],
        "search_scores": search_cases,
        "related_scores": [
            {
                "entry": spec,
                "directories": directories,
                "depth": depth,
                "expected": R.score_related_entry(
                    entry_of(spec), set(directories), depth
                ),
            }
            for spec in ENTRY_SPECS
            for directories in [
                ["task-history/general"],
                ["user-preferences/communication-style"],
                [],
            ]
            for depth in [0, 1, 2]
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
