#!/usr/bin/env python3
"""生成消息脱敏「流式还原」的对照数据集，供 Rust 侧 `omnicrawl-llm` 使用。

期望值来自 Python 真实现 `omnicrawl/llm/desensitization/stream.py`：每个场景是一串操作，
生成器把它们在真的 `StreamRestorer` 上跑一遍，记录每步的返回值、三路还原计数与告警。
两侧都用注入计数器建注册表，占位符序号因此确定；数据集里只出现假值。

注意：占位符一律用 `placeholder()` 拼接，源码里**不出现完整占位符字面量**。生成器可能运行在
启用了消息脱敏的 OmniCrawl 会话里（本仓库自己就是那个宿主），此时完整字面量会被还原成会话
注册表里的原文，落盘的数据集就成了「测试照常通过、语料却是假的」。

用法：``python rust/tools/gen_desensitization_stream_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/desensitization_stream_parity.json``
"""

from __future__ import annotations

import itertools
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/desensitization_stream_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm.desensitization import registry as R  # noqa: E402
from omnicrawl.llm.desensitization import stream as S  # noqa: E402
from omnicrawl.llm.desensitization.middleware import DesensitizationError  # noqa: E402

for module in (R, S):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

BRACE_OPEN = "\uff5b"
BRACE_CLOSE = "\uff5d"
FULLWIDTH_COLON = "\uff1a"
MARKER = "Desensitized"

# 周期里预先登记的值；两侧都按注入计数器分配，序号固定为 1 / 2 / 3。
VALUES = ["值一", "值二", "值三"]

TRUNCATED_REASONS = ["length", "incomplete", "max_tokens", "content_filter", "failed"]


def placeholder(seq: int) -> str:
    """规范占位符：全角花括号、序号无前导零（见上文的字面量注意事项）。"""

    return BRACE_OPEN + MARKER + ":" + str(seq) + BRACE_CLOSE


def half_width(seq: int, colon: str = ":") -> str:
    """还原端兼容的变体：半角花括号 / 冒号变体 / 序号两侧空白。"""

    return "{" + MARKER + colon + " " + str(seq) + " }"


RESTORE_ARGUMENTS_VALUE = {
    "path": "workspace/notes.txt",
    placeholder(2): "键不动",
    "nested": {"list": [placeholder(3), 7, True, None], "empty": {}},
    "plain": "无占位符",
    "number": 12.5,
}


def apply_op(restorer: S.StreamRestorer, op: dict) -> object:
    kind = op["op"]
    if kind == "feed_text":
        return restorer.feed_text(op["text"])
    if kind == "feed_reasoning":
        return restorer.feed_reasoning(op["text"])
    if kind == "feed_tool_arguments":
        return restorer.feed_tool_arguments(op["call_id"], op["text"])
    if kind == "flush":
        text, reasoning = restorer.flush()
        return {"text": text, "reasoning": reasoning}
    if kind == "flush_tool_arguments":
        return [[call_id, tail] for call_id, tail in restorer.flush_tool_arguments().items()]
    if kind == "restore_string":
        return restorer.restore_string(op["text"])
    if kind == "restore_arguments":
        return restorer.restore_arguments(op["value"])
    if kind == "note_text":
        restorer.note_text(op["text"])
        return None
    if kind == "note_reasoning":
        restorer.note_reasoning(op["text"])
        return None
    if kind == "note_tool_call":
        restorer.note_tool_call()
        return None
    if kind == "note_completed":
        restorer.note_completed(op["finish_reason"])
        return None
    if kind == "reply_usable":
        return restorer.reply_usable
    if kind == "take_warnings":
        return [{"code": item.code, "message": item.message} for item in restorer.take_warnings()]
    raise SystemExit(f"未知操作：{kind}")


def run_scenario(scenario: dict) -> dict:
    registry = R.SequenceRegistry(sequence_source=itertools.count(1).__next__)
    cycle, _ = registry.begin_cycle("流式还原对照")
    values = []
    for value in VALUES:
        seq, _ = cycle.seq_for_value(value)
        values.append([value, seq])

    restorer = S.StreamRestorer(cycle, registry.stats, strict=scenario.get("strict", False))
    recorded = []
    for op in scenario["ops"]:
        entry = dict(op)
        try:
            entry["result"] = apply_op(restorer, op)
        except DesensitizationError as exc:
            entry["error"] = str(exc)
        entry["stats"] = {
            "hits": registry.stats.restore_hits,
            "unresolved": registry.stats.restore_unresolved,
            "malformed": registry.stats.restore_malformed,
        }
        recorded.append(entry)

    return {
        "name": scenario["name"],
        "strict": restorer._strict,
        "values": values,
        "ops": recorded,
        "finish_reason": restorer.finish_reason,
        "saw_text": restorer.saw_text,
        "saw_reasoning": restorer.saw_reasoning,
        "saw_tool_call": restorer.saw_tool_call,
    }


def finish_reason_ops() -> list[dict]:
    ops = [{"op": "note_text", "text": "x"}]
    for reason in TRUNCATED_REASONS + ["stop", "tool_calls", ""]:
        ops.append({"op": "note_completed", "finish_reason": reason})
        ops.append({"op": "reply_usable"})
    return ops


SCENARIOS = [
    {
        "name": "跨分片占位符",
        "ops": [
            {"op": "feed_text", "text": "前缀 " + BRACE_OPEN + "Desens"},
            {"op": "feed_text", "text": "itized"},
            {"op": "feed_text", "text": ":1" + BRACE_CLOSE + " 后缀"},
            {"op": "flush"},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "还原变体与多个占位符",
        "ops": [
            {"op": "feed_text", "text": "ａ" + placeholder(1) + "ｂ"},
            {
                "op": "feed_text",
                "text": placeholder(2)
                + "| "
                + half_width(3)
                + " "
                + half_width(19, FULLWIDTH_COLON)
                + " "
                + placeholder(20).upper(),
            },
            {"op": "feed_text", "text": "普通 {括号} 与 " + MARKER + " 文本"},
            {"op": "flush"},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "未注册序号与告警去重",
        "ops": [
            {"op": "feed_text", "text": "未知 " + placeholder(99)},
            {"op": "feed_text", "text": " 又一个 " + placeholder(88)},
            {"op": "take_warnings"},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "畸形与半截前缀",
        "ops": [
            {"op": "feed_text", "text": "畸形 " + BRACE_OPEN + MARKER + ": 未闭合"},
            {"op": "feed_text", "text": " 半截 " + BRACE_OPEN + "Dese"},
            {"op": "take_warnings"},
            {"op": "flush"},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "挂起上限",
        "ops": [
            {"op": "feed_text", "text": "{" + " " * 63},
            {"op": "feed_text", "text": "x"},
            {"op": "feed_text", "text": "{" + " " * 64},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "文本与推理通道隔离",
        "ops": [
            {"op": "feed_text", "text": "A" + BRACE_OPEN + "Dese"},
            {"op": "feed_reasoning", "text": "R" + placeholder(2)},
            {"op": "feed_text", "text": "nsitized:1" + BRACE_CLOSE + "B"},
            {"op": "feed_reasoning", "text": "R2"},
            {"op": "flush"},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "工具参数通道",
        "ops": [
            {"op": "feed_tool_arguments", "call_id": "call_1", "text": '{"key":"' + BRACE_OPEN + "Dese"},
            {"op": "feed_tool_arguments", "call_id": "call_1", "text": "nsitized:1" + BRACE_CLOSE + '"}'},
            {"op": "feed_tool_arguments", "call_id": "call_2", "text": placeholder(2)},
            {"op": "feed_tool_arguments", "call_id": "", "text": "尾部" + BRACE_OPEN + "Dese"},
            {"op": "flush_tool_arguments"},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "结构化还原",
        "ops": [
            {"op": "restore_string", "text": placeholder(18) + placeholder(99) + "-尾"},
            {"op": "restore_arguments", "value": RESTORE_ARGUMENTS_VALUE},
            {"op": "take_warnings"},
        ],
    },
    {
        "name": "生命周期与可用性",
        "ops": [
            {"op": "reply_usable"},
            {"op": "note_text", "text": ""},
            {"op": "reply_usable"},
            {"op": "note_tool_call"},
            {"op": "reply_usable"},
            {"op": "note_completed", "finish_reason": "content_filter"},
            {"op": "reply_usable"},
            {"op": "note_completed", "finish_reason": ""},
            {"op": "reply_usable"},
            {"op": "note_reasoning", "text": "r"},
            {"op": "reply_usable"},
        ],
    },
    {"name": "截断 finish_reason 全表", "ops": finish_reason_ops()},
    {
        "name": "严格模式未注册序号",
        "strict": True,
        "ops": [{"op": "feed_text", "text": placeholder(99)}],
    },
    {
        "name": "严格模式畸形前缀",
        "strict": True,
        "ops": [{"op": "feed_text", "text": "畸形 " + BRACE_OPEN + MARKER + ": 未闭合"}],
    },
    {
        "name": "严格模式正常路径",
        "strict": True,
        "ops": [
            {"op": "feed_text", "text": placeholder(1)},
            {"op": "flush"},
            {"op": "take_warnings"},
        ],
    },
]


def main() -> None:
    scenarios = [run_scenario(scenario) for scenario in SCENARIOS]
    fixture = {
        "source": [
            "omnicrawl/llm/desensitization/stream.py",
            "omnicrawl/llm/desensitization/registry.py",
        ],
        "values": VALUES,
        "truncated_finish_reasons": sorted(S.TRUNCATED_FINISH_REASONS),
        "scenarios": scenarios,
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：场景 {len(scenarios)} 个，"
        f"操作 {sum(len(item['ops']) for item in scenarios)} 步"
    )


if __name__ == "__main__":
    main()
