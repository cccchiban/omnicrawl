#!/usr/bin/env python3
"""生成消息脱敏「middleware 编排件 + oneshot」的对照数据集，供 Rust 侧 `omnicrawl-llm` 使用。

期望值来自 Python 真实现：
- `omnicrawl/llm/desensitization/middleware.py` 的运行时无关部分（逐消息屏蔽、消息文本收集、
  工具参数里「已分配但无法还原」的序号扫描）；
- `omnicrawl/llm/desensitization/oneshot.py`（单次屏蔽 → 还原 → 注销）。

两侧都用注入计数器建注册表，占位符序号因此确定。装饰器本体（`DesensitizationRuntime` /
`maybe_wrap_runtime`，需要 Rust 侧运行时抽象）与 `_MessageMaskMemo`（性能缓存）未搬，不参与对照。

用法：``python rust/tools/gen_desensitization_middleware_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/desensitization_middleware_parity.json``
"""

from __future__ import annotations

import itertools
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/desensitization_middleware_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.config.features.desensitization import (  # noqa: E402
    DesensitizationConfig,
)
from omnicrawl.llm.desensitization import engine as E  # noqa: E402
from omnicrawl.llm.desensitization import middleware as M  # noqa: E402
from omnicrawl.llm.desensitization import oneshot as O  # noqa: E402
from omnicrawl.llm import protocol as P  # noqa: E402
from omnicrawl.llm.desensitization import registry as R  # noqa: E402
from omnicrawl.llm.desensitization import rules as rules_module  # noqa: E402

for module in (E, M, O, P, R, rules_module):
    path = getattr(module, "__file__", None)
    if path is None or not Path(path).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

EXTRA_KEYS = ["team-token"]
EXEMPT_KEYS = ["example"]
FAKE_EMAIL = "worker" + "@" + "corp.local"
FAKE_CARD = "4111" + "1111" + "1111" + "1111"
FAKE_IP = "10." + "20.30.40"
FAKE_MAC = ":".join(["00", "1A", "2B", "3C", "4D", "5E"])
MIXED_SECRET = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4"

MASKER_OPTIONS = dict(
    entropy_enabled=True,
    entropy_min_length=20,
    entropy_min_bits=3.5,
    entropy_pure_letters=False,
    entropy_pure_digits=False,
)

ONESHOT_TEXTS = [
    "联系 " + FAKE_EMAIL + " 与卡号 " + FAKE_CARD + " 结束",
    "token " + MIXED_SECRET + " 结束",
    "普通文本没有秘密",
    "",
]

MESSAGE_SPECS = [
    ("user", ["联系 " + FAKE_EMAIL + " 与内网 " + FAKE_IP + " 结束"], "", "user 文本"),
    ("system", ["系统提示词里的 " + FAKE_EMAIL + " 不动"], "", "system 文本不参与"),
    ("assistant", ["回复正文，" + FAKE_MAC + " 会被屏蔽"], "思考里的 " + FAKE_CARD, "助手文本与思考"),
    (
        "user",
        [],
        "",
        "工具调用参数（敏感键 + 自由文本）",
        {
            "password": "hunter2000!!",
            "note": "联系 " + FAKE_EMAIL + " 结束",
            "nested": {"team-token": "abc' + BACKSLASH + '", "list": ["plain", FAKE_IP]},
        },
    ),
    ("tool", [], "", "工具结果文本", "结果 " + MIXED_SECRET + " 与 " + FAKE_MAC),
    ("user", ["已有 " + R.format_placeholder(7) + " 与 " + FAKE_IP], "", "已有占位符"),
]


def build_message(spec) -> P.ConversationMessage:
    role, texts, reasoning, _note = spec[0], spec[1], spec[2], spec[3]
    blocks: list[object] = [P.TextBlock(text=text) for text in texts]
    if len(spec) > 4:
        blocks.append(
            P.ToolCallBlock(call_id="call_1", name="write_file", arguments=spec[4])
        )
    if len(spec) > 5:
        blocks.append(P.ToolResultBlock(call_id="call_1", ok=True, content=spec[5]))
    return P.ConversationMessage(role=role, blocks=tuple(blocks), reasoning=reasoning)


def to_json(message: P.ConversationMessage) -> dict:
    blocks = []
    for block in message.blocks:
        if isinstance(block, P.TextBlock):
            blocks.append({"Text": {"text": block.text}})
        elif isinstance(block, P.ToolCallBlock):
            blocks.append(
                {
                    "ToolCall": {
                        "call_id": block.call_id,
                        "name": block.name,
                        "arguments": block.arguments,
                        "provider_call_id": block.provider_call_id,
                    }
                }
            )
        elif isinstance(block, P.ToolResultBlock):
            blocks.append(
                {
                    "ToolResult": {
                        "call_id": block.call_id,
                        "ok": block.ok,
                        "content": block.content,
                    }
                }
            )
        else:
            raise SystemExit("未覆盖的块类型：%r" % type(block))
    return {
        "role": message.role,
        "blocks": blocks,
        "reasoning": message.reasoning,
        "tools": [],
    }


def make_context(registry: R.SequenceRegistry, cycle: R.PlaceholderCycle) -> E.MaskContext:
    return E.MaskContext(
        matcher=E.SensitiveMatcher(extra_keys=EXTRA_KEYS, exempt_keys=EXEMPT_KEYS),
        cycle=cycle,
        stats=registry.stats,
        pattern_rules=tuple(rules_module.builtin_rules()),
        **MASKER_OPTIONS,
    )


def run_messages():
    registry = R.SequenceRegistry(sequence_source=itertools.count(1).__next__)
    cycle, _ = registry.begin_cycle("middleware 对照")
    ctx = make_context(registry, cycle)
    results = []
    for spec in MESSAGE_SPECS:
        message = build_message(spec)
        masked = M._mask_message(message, ctx)
        results.append(
            {
                "note": spec[3],
                "message": to_json(message),
                "masked": to_json(masked),
                "texts": list(M._iter_message_texts(message)),
                "referenced": list(M._referenced_sequences(message)),
            }
        )
    # 参数泄露检查：先屏蔽一条含秘密的文本（登记序号），再用同一注册表扫参数。
    secret_message = P.ConversationMessage(
        role="user",
        blocks=(P.TextBlock(text="token " + MIXED_SECRET),),
    )
    secret_masked = M._mask_message(secret_message, ctx)
    placeholder_text = R.format_placeholder(1)
    leak_value = {"note": "值 " + placeholder_text, "list": [3, True, None]}
    return results, {
        "secret_message": to_json(secret_message),
        "secret_masked": to_json(secret_masked),
        "value": leak_value,
        "leaked": list(M._assigned_but_unresolved(registry, leak_value)),
        "assigned_placeholder": placeholder_text,
    }


def run_oneshot(strict: bool):
    masker = O.OneShotMasker(
        DesensitizationConfig(
            enabled=True,
            strict_restore=strict,
            extra_sensitive_keys=tuple(EXTRA_KEYS),
            exempt_keys=tuple(EXEMPT_KEYS),
        ),
        matcher=E.SensitiveMatcher(extra_keys=EXTRA_KEYS, exempt_keys=EXEMPT_KEYS),
        sequence_source=itertools.count(1).__next__,
    )
    results = []
    for text in ONESHOT_TEXTS:
        masked = masker.mask(text)
        entry = {"text": text, "masked": masked}
        try:
            entry["restored"] = masker.restore(masked)
        except Exception as exc:  # noqa: BLE001 - 对照里只关心文案
            entry["error"] = str(exc)
        results.append(entry)
    unknown = R.format_placeholder(999)
    entry = {"text": unknown}
    try:
        entry["restored"] = masker.restore(unknown)
    except Exception as exc:  # noqa: BLE001
        entry["error"] = str(exc)
    results.append(entry)
    stats = {
        "values_masked": masker._stats.values_masked,
        "skipped_values": masker._stats.skipped_values,
        "rules_masked": masker._stats.rules_masked,
        "entropy_masked": masker._stats.entropy_masked,
        "sequence_reuses": masker._stats.sequence_reuses,
    }
    masker.close()
    entry = {"text": R.format_placeholder(1)}
    try:
        entry["restored"] = masker.restore(entry["text"])
    except Exception as exc:  # noqa: BLE001
        entry["error"] = str(exc)
    return {"strict": strict, "steps": results, "stats": stats, "after_close": entry}


def main() -> None:
    messages, leak = run_messages()
    fixture = {
        "source": [
            "omnicrawl/llm/desensitization/middleware.py",
            "omnicrawl/llm/desensitization/oneshot.py",
        ],
        "extra_keys": EXTRA_KEYS,
        "exempt_keys": EXEMPT_KEYS,
        "messages": messages,
        "leak": leak,
        "oneshot": [run_oneshot(False), run_oneshot(True)],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        "已写入 %s：消息 %d 条、泄露检查 1 组、oneshot 场景 %d 组"
        % (FIXTURE_PATH.relative_to(ROOT), len(messages), len(fixture["oneshot"]))
    )


if __name__ == "__main__":
    main()
