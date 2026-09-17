#!/usr/bin/env python3
"""生成消息脱敏「序号注册表」的对照数据集，供 Rust 侧 `omnicrawl-llm::desensitization` 使用。

期望值来自 Python 真实现 `omnicrawl/llm/desensitization/registry.py`。为了让序号确定，
两侧都注入自增计数器；周期号是进程级全局的，比对前统一归一化成 `<cycle-id>`。
指纹使用进程级随机盐，只比对「同值同指纹、异值不同指纹」这类性质，不比字面值。

用法：``python rust/tools/gen_desensitization_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/desensitization_parity.json``
"""

from __future__ import annotations

import itertools
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/desensitization_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm.desensitization import registry as R  # noqa: E402

if not Path(R.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

CYCLE_PATTERN = re.compile(r"cycle-\d+")

TEXTS = [
    "HTML+Paged.js",
    "前缀 半角 &input.system_prompt 后缀",
    "大写 123456789012345678901234567890",
    "全角冒号 self.context_window_tokens,",
    "半角冒号 self.context_window_tokens,",
    "序号带空格 &self.session_id,",
    "畸形 ｛Desensitized: 未闭合",
    "疑似前缀 { desensitized",
    "普通文本 {括号} 与 Desensitized",
    "多个 text+tool_calls+reasoning 与 int = 0",
    "",
]

FINGERPRINT_VALUES = ["同一个值", "同一个值", "另一个值"]


def main() -> None:
    registry_counter = itertools.count(1).__next__
    registry = R.SequenceRegistry(sequence_source=registry_counter)

    index_counter = itertools.count(100).__next__
    index = R.StableSequenceIndex(sequence_source=index_counter)

    # 稳定索引：复用与跳过预留序号
    index_steps = []
    for value, reserved in [
        ("甲", []),
        ("甲", []),
        ("乙", [100, 101, 102]),
        ("丙", []),
        ("乙", []),
    ]:
        seq, reused = index.sequence_for(value, reserved=set(reserved))
        index_steps.append({"value": value, "reserved": reserved, "seq": seq, "reused": reused})
    index_state = {"size": index.size, "assigned_102": index.assigned(102)}

    # 周期：同值同号、adopt、lookup、pairs_from、close
    cycle, reused = registry.begin_cycle("请求 A")
    cycle_steps = []
    for value in ["值一", "值一", "值二"]:
        seq, first = cycle.seq_for_value(value)
        cycle_steps.append({"value": value, "seq": seq, "first": first})
    cycle.adopt("值三", 777)
    cycle.adopt("值三", 777)
    pairs = [list(item) for item in cycle.pairs_from(1)]
    close_snapshot = {
        "reused": reused,
        "stable_reuses": cycle.stable_reuses,
        "lookup_known": cycle.lookup(777),
        "lookup_unknown": cycle.lookup(999),
        "pairs_from_1": pairs,
    }
    cycle.close()
    after_close = {"closed": cycle.closed, "pairs": [list(item) for item in cycle.pairs_from(0)]}

    # 会话级映射：未知会话标识不算变更
    cache = R.SessionSequenceCache("会话一")
    cache.entries[1] = "原文一"
    cache.rebind("")
    after_unknown = dict(cache.entries)
    cache.rebind("会话二")
    after_change = dict(cache.entries)
    cache.entries[2] = "原文二"
    cache.clear()
    after_clear = dict(cache.entries)

    # 注册表：新建 / 重试复用 / 换请求注销 / 关闭 / 清空
    registry2 = R.SequenceRegistry(sequence_source=itertools.count(1).__next__)
    first_cycle, first_reused = registry2.begin_cycle("同请求")
    registry2._last_cycle.masked_request = "屏蔽副本"
    second_cycle, second_reused = registry2.begin_cycle("同请求")
    third_cycle, third_reused = registry2.begin_cycle("新请求")
    open_after_switch = registry2.open_cycle_count
    registry2.close_cycle(third_cycle)
    open_after_close = registry2.open_cycle_count
    registry2.drop_all()
    open_after_drop = registry2.open_cycle_count
    registry_steps = {
        "cycle_ids_equal_on_reuse": first_cycle.cycle_id == second_cycle.cycle_id,
        "first_reused": first_reused,
        "second_reused": second_reused,
        "third_reused": third_reused,
        "third_cycle_distinct": third_cycle.cycle_id != first_cycle.cycle_id,
        "open_after_switch": open_after_switch,
        "open_after_close": open_after_close,
        "open_after_drop": open_after_drop,
        "stats": {
            "cycles_started": registry2.stats.cycles_started,
            "cycles_reused": registry2.stats.cycles_reused,
        },
    }

    fixture = {
        "source": ["omnicrawl/llm/desensitization/registry.py"],
        "formatted": [R.format_placeholder(seq) for seq in [1, 12, 999]],
        "placeholders": [
            {
                "text": text,
                "found": [
                    {"text": match.group(0), "seq": int(match.group(1))}
                    for match in R.PLACEHOLDER_PATTERN.finditer(text)
                ],
            }
            for text in TEXTS
        ],
        "prefixes": [R.PLACEHOLDER_PREFIX_PATTERN.search(text) is not None for text in TEXTS],
        "collected": sorted(R.collect_placeholder_numbers(TEXTS)),
        "fingerprints": {
            "same": R.sequence_fingerprint("同一个值") == R.sequence_fingerprint("同一个值"),
            "different": R.sequence_fingerprint("同一个值") != R.sequence_fingerprint("另一个值"),
            "length": len(R.sequence_fingerprint("同一个值")),
        },
        "index_steps": index_steps,
        "index_state": index_state,
        "cycle_steps": cycle_steps,
        "cycle_close": close_snapshot,
        "cycle_after_close": after_close,
        "cache": {
            "after_unknown": [[key, value] for key, value in after_unknown.items()],
            "after_change": [[key, value] for key, value in after_change.items()],
            "after_clear": [[key, value] for key, value in after_clear.items()],
        },
        "registry": registry_steps,
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：占位符 {len(TEXTS)} 条，"
        f"序号步骤 {len(index_steps)} 步，指纹性质 {fixture['fingerprints']}"
    )


if __name__ == "__main__":
    main()
