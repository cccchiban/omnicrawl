#!/usr/bin/env python3
"""生成 Token usage 归一化对照数据集，供 Rust 侧 `omnicrawl-llm` 的 parity 测试使用。

期望值全部由 Python 真实现（``omnicrawl/llm/usage.py`` 的 ``usage_from_openai_payload``）
产出，Rust 侧只做同构映射后逐字段比对。只覆盖 JSON 负载这一路：SDK 对象形态
（``getattr`` + ``model_dump``）不跨进程存在。

用法：``python rust/tools/gen_llm_usage_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_usage_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/openai_chat_usage_parity.json"

# 必须加载仓库源码：已安装的 omnicrawl 在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import omnicrawl.llm.usage as U  # noqa: E402

if not Path(U.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{U.__file__}")

USAGE_CASES = [
    {},
    {"usage": None},
    {"usage": {}},
    {"usage": {"prompt_tokens": 10, "completion_tokens": 5}},
    {"usage": {"input_tokens": 3, "output_tokens": 4}},
    {"usage": {"prompt_tokens": 10}},
    {"usage": {"completion_tokens": 5}},
    {"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}},
    {"response": {"usage": {"prompt_tokens": 1, "completion_tokens": 2}}},
    {"response": {"usage": None}},
    {"response": "not-an-object"},
    # DeepSeek 风格：命中与未命中缓存两段相加才是输入总量。
    {
        "usage": {
            "prompt_cache_hit_tokens": 4,
            "prompt_cache_miss_tokens": 6,
            "completion_tokens": 2,
        }
    },
    {"usage": {"prompt_cache_hit_tokens": 4, "completion_tokens": 2}},
    # 缓存命中的四种写法。
    {"usage": {"prompt_tokens": 7, "completion_tokens": 1, "cached_tokens": 5}},
    {"usage": {"prompt_tokens": 7, "completion_tokens": 1, "cached_input_tokens": 5}},
    {"usage": {"prompt_tokens": 7, "completion_tokens": 1, "input_cached_tokens": 5}},
    {"usage": {"prompt_tokens": 7, "completion_tokens": 1, "prompt_cache_hit_tokens": 6}},
    {
        "usage": {
            "prompt_tokens": 7,
            "completion_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 3},
        }
    },
    {
        "usage": {
            "prompt_tokens": 7,
            "completion_tokens": 1,
            "input_tokens_details": {"cached_tokens": 2},
        }
    },
    # 推理 token：直给、Responses 细节、Chat Completions 细节。
    {"usage": {"prompt_tokens": 1, "completion_tokens": 2, "reasoning_tokens": 9}},
    {"usage": {"prompt_tokens": 1, "completion_tokens": 2, "output_reasoning_tokens": 4}},
    {
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "output_tokens_details": {"reasoning_tokens": 8},
        }
    },
    {
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "completion_tokens_details": {"reasoning_tokens": 6},
        }
    },
    # Python 用 `or` 取第一个真值：null 与空对象都要继续看下一个细节字段。
    {
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "output_tokens_details": None,
            "completion_tokens_details": {"reasoning_tokens": 7},
        }
    },
    {
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "output_tokens_details": {},
            "completion_tokens_details": {"reasoning_tokens": 3},
        }
    },
    {
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "output_tokens_details": {"reasoning_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 3},
        }
    },
    # 非整数取值一律视为缺失：布尔、浮点、字符串。
    {"usage": {"prompt_tokens": True, "completion_tokens": 2}},
    {"usage": {"prompt_tokens": 1.0, "completion_tokens": 2}},
    {"usage": {"prompt_tokens": "10", "completion_tokens": 2}},
    {"usage": {"prompt_tokens": None, "completion_tokens": 2}},
]


def usage_to_json(usage) -> dict | None:
    if usage is None:
        return None
    return {
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
    }


def main() -> None:
    fixture = {
        "source": "omnicrawl/llm/usage.py",
        "usage": [
            {"payload": payload, "expected": usage_to_json(U.usage_from_openai_payload(payload))}
            for payload in USAGE_CASES
        ],
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：用例 {len(fixture['usage'])}"
    )


if __name__ == "__main__":
    main()
