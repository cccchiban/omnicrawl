#!/usr/bin/env python3
"""生成消息脱敏「屏蔽计划缓存」的对照数据集，供 Rust 侧 `omnicrawl-llm` 使用。

期望值来自 Python 真实现 `omnicrawl/llm/desensitization/plan_cache.py`：文本指纹（SHA-256）、
计划构建器的完整性与空阶段裁剪、有界 LRU 缓存的命中 / 淘汰 / 失效 / 清空与计数口径，
以及阶段标签到计数口径的映射表。

注意加载方式：本脚本**按文件路径直接加载** `plan_cache.py`，不走包导入。包
`omnicrawl/llm/desensitization/__init__.py` 会连带导入 `engine.py` / `middleware.py`，而这两
份文件当前带着未合并的冲突标记（`git ls-files -u` 可见），导入会直接 `SyntaxError`。
`plan_cache.py` 本身只依赖标准库，按路径加载不会碰到它们；等 Python 侧合并收口后，可以把
加载方式换回 `from omnicrawl.llm.desensitization import plan_cache`（模块级行为不变）。

用法：``python rust/tools/gen_desensitization_plan_cache_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/desensitization_plan_cache_parity.json``
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "omnicrawl/llm/desensitization/plan_cache.py"
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/desensitization_plan_cache_parity.json"
)


def load_plan_cache():
    """按文件路径加载 plan_cache 模块（绕开包 `__init__` 的 engine/middleware 导入）。"""

    if not MODULE_PATH.is_file() or not MODULE_PATH.resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")
    spec = importlib.util.spec_from_file_location("oc_plan_cache", MODULE_PATH)
    module = importlib.util.module_from_spec(spec)
    # dataclasses 需要能在 sys.modules 里按 `cls.__module__` 找到本模块。
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


PC = load_plan_cache()

# ── 文本指纹 ──────────────────────────────────────────────────────────────

KEY_TEXTS = [
    "",
    "a",
    "abc",
    "API_KEY=A1b2C3d4E5f6G7h8I9j0K1l2M3n4",
    "密码：hunter2000!!",
    "多行\n文本\r\n第二行",
    "emoji 与代理对：🧪🔐",
    "  " + "x" * 512,
]

# ── 计划构建器：操作序列 → 产出的计划 ─────────────────────────────────────

BUILDER_CASES = [
    {
        "note": "空计划（零区间零分配）照样产出",
        "ops": [],
    },
    {
        "note": "单阶段单区间",
        "ops": [
            ["begin_stage", "rules"],
            ["record", 0, 3, 1],
            ["note_registration"],
        ],
    },
    {
        "note": "未显式开阶段：落进无标签阶段",
        "ops": [
            ["record", 2, 5, 9],
            ["note_registration"],
        ],
    },
    {
        "note": "开了阶段但没记区间：该阶段在产出时丢掉",
        "ops": [
            ["begin_stage", ""],
            ["begin_stage", "rules"],
            ["record", 0, 3, 1],
            ["note_registration"],
        ],
    },
    {
        "note": "记录数与分配数不一致：不可重放",
        "ops": [
            ["begin_stage", "rules"],
            ["record", 0, 3, 1],
            ["note_registration"],
            ["begin_stage", "entropy"],
            ["note_registration"],
        ],
    },
    {
        "note": "多阶段多区间，记录顺序即应用顺序",
        "ops": [
            ["begin_stage", ""],
            ["record", 8, 32, 1],
            ["note_registration"],
            ["begin_stage", "rules"],
            ["record", 40, 52, 2],
            ["record", 10, 20, 3],
            ["note_registration"],
            ["note_registration"],
        ],
    },
]

# ── 缓存：操作序列 → 返回值 / 计数 ────────────────────────────────────────

# 计划规格：阶段列表，阶段内是 [start, end, seq, counter]。
PLAN_ONE = [[[0, 3, 1, "rules"]]]
PLAN_TWO = [[[0, 3, 1, "rules"]], [[10, 12, 2, "entropy"]]]
PLAN_FIVE_SPANS = [[[index * 4, index * 4 + 2, index + 1, ""] for index in range(5)]]

CACHE_CASES = [
    {
        "note": "命中与未命中计数",
        "max_entries": 8,
        "max_bytes": 1_000_000,
        "ops": [
            ["get", "abc"],
            ["put", "abc", PLAN_ONE],
            ["get", "abc"],
            ["get", "abd"],
        ],
        "results": [False, True, False],
    },
    {
        "note": "条目数上限：淘汰最久未用的那条",
        "max_entries": 2,
        "max_bytes": 1_000_000,
        "ops": [
            ["put", "a", PLAN_ONE],
            ["put", "b", PLAN_ONE],
            ["put", "c", PLAN_ONE],
            ["get", "a"],
            ["get", "b"],
            ["get", "c"],
        ],
        "results": [False, True, True],
    },
    {
        "note": "重复写同一个键：条目数不变、旧占用先退回",
        "max_entries": 8,
        "max_bytes": 1_000_000,
        "ops": [
            ["put", "abc", PLAN_FIVE_SPANS],
            ["put", "abc", PLAN_ONE],
            ["get", "abc"],
        ],
        "results": [True],
    },
    {
        "note": "超字节预算的条目直接放弃，不淘汰既有条目",
        "max_entries": 8,
        "max_bytes": 300,
        "ops": [
            ["put", "keep", PLAN_ONE],
            ["put", "huge", PLAN_TWO],
            ["get", "keep"],
            ["get", "huge"],
        ],
        "results": [True, False],
    },
    {
        "note": "字节预算触发的淘汰（单条 448 字节 > 800 的一半）",
        "max_entries": 8,
        "max_bytes": 800,
        "ops": [
            ["put", "a", PLAN_ONE],
            ["put", "b", PLAN_TWO],
            ["put", "c", PLAN_ONE],
            ["get", "a"],
            ["get", "b"],
            ["get", "c"],
        ],
        "results": [False, True, True],
    },
    {
        "note": "命中后重放失败：命中改记未命中，条目留在缓存里",
        "max_entries": 8,
        "max_bytes": 1_000_000,
        "ops": [
            ["put", "abc", PLAN_ONE],
            ["get", "abc"],
            ["note_invalid"],
            ["get", "abc"],
        ],
        "results": [True, True],
    },
    {
        "note": "清空只丢条目，计数保留",
        "max_entries": 8,
        "max_bytes": 1_000_000,
        "ops": [
            ["put", "abc", PLAN_ONE],
            ["get", "abc"],
            ["clear"],
            ["get", "abc"],
        ],
        "results": [True, False],
    },
    {
        "note": "条目数上限为 0：整体停用，连 lookup 都不计",
        "max_entries": 0,
        "max_bytes": 1024,
        "ops": [
            ["put", "abc", PLAN_ONE],
            ["get", "abc"],
        ],
        "results": [False],
    },
    {
        "note": "字节预算为 0：整体停用",
        "max_entries": 1024,
        "max_bytes": 0,
        "ops": [
            ["put", "abc", PLAN_ONE],
            ["get", "abc"],
        ],
        "results": [False],
    },
    {
        "note": "空文本不进缓存",
        "max_entries": 8,
        "max_bytes": 1_000_000,
        "ops": [
            ["put", "", PLAN_ONE],
            ["get", ""],
        ],
        "results": [False],
    },
]


def plan_from_spec(spec: list) -> "PC.MaskPlan":
    """把 [阶段][区间] 的紧凑写法还原成真实现的对象。"""

    stages = tuple(
        tuple(
            PC.PlanSpan(start=span[0], end=span[1], seq=span[2], counter=span[3])
            for span in stage
        )
        for stage in spec
    )
    return PC.MaskPlan(stages=stages)


def plan_json(plan: "PC.MaskPlan | None"):
    if plan is None:
        return None
    return {
        "stages": [
            [
                {"start": span.start, "end": span.end, "seq": span.seq, "counter": span.counter}
                for span in stage
            ]
            for stage in plan.stages
        ],
        "span_count": plan.span_count,
    }


def build_case(case: dict) -> dict:
    builder = PC.MaskPlanBuilder()
    for op in case["ops"]:
        name = op[0]
        if name == "begin_stage":
            builder.begin_stage(op[1])
        elif name == "record":
            builder.record(op[1], op[2], op[3])
        elif name == "note_registration":
            builder.note_registration()
        else:  # pragma: no cover - 数据写错时立即暴露
            raise SystemExit(f"未知构建器操作：{name}")
    return {"note": case["note"], "ops": case["ops"], "plan": plan_json(builder.build())}


def cache_case(case: dict) -> dict:
    cache = PC.MaskPlanCache(max_entries=case["max_entries"], max_bytes=case["max_bytes"])
    results = []
    for op in case["ops"]:
        name = op[0]
        if name == "put":
            cache.put(op[1], plan_from_spec(op[2]))
        elif name == "get":
            results.append(cache.get(op[1]) is not None)
        elif name == "note_invalid":
            cache.note_invalid()
        elif name == "clear":
            cache.clear()
        else:  # pragma: no cover
            raise SystemExit(f"未知缓存操作：{name}")
    assert results == case["results"], f"{case['note']}：返回值与预期不符"
    return {
        "note": case["note"],
        "max_entries": case["max_entries"],
        "max_bytes": case["max_bytes"],
        "ops": case["ops"],
        "results": results,
        "stats": cache.stats(),
    }


def main() -> None:
    fixture = {
        "source": ["omnicrawl/llm/desensitization/plan_cache.py"],
        "defaults": {
            "max_entries": PC.DEFAULT_MAX_ENTRIES,
            "max_bytes": PC.DEFAULT_MAX_BYTES,
            "stage_counter_fields": dict(PC.STAGE_COUNTER_FIELDS),
        },
        "text_keys": [
            {"text": text, "digest": PC.text_key(text).hex()} for text in KEY_TEXTS
        ],
        "builder_cases": [build_case(case) for case in BUILDER_CASES],
        "cache_cases": [cache_case(case) for case in CACHE_CASES],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        "已写入 %s：指纹 %d、构建器 %d、缓存 %d"
        % (
            FIXTURE_PATH.relative_to(ROOT),
            len(fixture["text_keys"]),
            len(fixture["builder_cases"]),
            len(fixture["cache_cases"]),
        )
    )


if __name__ == "__main__":
    main()
