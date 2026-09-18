#!/usr/bin/env python3
"""生成消息脱敏「匹配引擎」的对照数据集，供 Rust 侧 `omnicrawl-llm` 使用。

期望值来自 Python 真实现 `omnicrawl/llm/desensitization/engine.py`：三层替换（结构层 → 值类型规则层
→ 熵兜底）、已解析结构体的递归屏蔽，以及各层判定函数（键名归一与命中、跳过规则、形态白名单、
候选判定、扫描区间）。两侧都注入计数器建注册表，占位符序号因此确定。

覆盖范围：结构层三种形态、规则层（内置 11 条规则）、熵兜底（两套参数）、全部判定函数；
NER 兜底层未搬（Python 侧可选依赖 torch），不参与对照。

语料里的值一律是显式写出的假值（或由数字 / 片段拼出）；需要「已有占位符」的用例由
`R.format_placeholder()` 现拼，源码里不出现成形字面量。

用法：``python rust/tools/gen_desensitization_engine_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/desensitization_engine_parity.json``
"""

from __future__ import annotations

import itertools
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/desensitization_engine_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm.desensitization import engine as E  # noqa: E402
from omnicrawl.llm.desensitization import registry as R  # noqa: E402
from omnicrawl.llm.desensitization import rules as rules_module  # noqa: E402

for module in (E, R, rules_module):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

EXTRA_KEYS = ["MyCustomSecret", "team-token"]
EXEMPT_KEYS = ["internal_example", "public-example-key"]

BACKSLASH = chr(92)
FAKE_EMAIL = "worker" + "@" + "corp.local"
FAKE_CARD = "4111" + "1111" + "1111" + "1111"  # 标准测试号，Luhn 合法
FAKE_IP = "10." + "20.30.40"
FAKE_URL = "https://" + "db.corp.local" + "/app"
FAKE_MAC = ":".join(["00", "1A", "2B", "3C", "4D", "5E"])
FAKE_PLATE = "沪" + "B" + "D" + "12345"
MIXED_SECRET = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4"
PURE_LETTERS = "GhbZkqXmnpRsTuVwXyZ"
PURE_DIGITS = "98765432109876543210"
HEX_DIGEST = "8f14e45fceea167a5a36dedd4bea2543"
UUID = "550e8400-e29b-41d4-a716-446655440000"

TEXT_CASES = [
    ("API_KEY=" + MIXED_SECRET, "env 赋值"),
    ("export DB_PASSWORD = 'hunter2000!!'", "env 带 export 与引号"),
    ("TOKEN=abc123  # 行内注释保留", "env 无引号带注释"),
    ('SECRET="abc123" # 引号外注释保留', "env 引号外注释"),
    ("TOKEN=abc123 # 注释", "env 无引号单注释"),
    ("password: hunter2000!!", "kv 形态"),
    ("db.password: hunter2000!!", "kv 点分键"),
    ("PWD : hunter2000!!", "kv 冒号前空格"),
    ('{"api_key": "hunter2000!!", "name": "zhang"}', "json 串键敏感"),
    ('{"password": "abc' + BACKSLASH + '"123", "name": "zhang"}', "json 串含转义引号"),
    ('{"note": "plain", "count": 3}', "json 串键不敏感"),
    ("联系 " + FAKE_EMAIL + " 与卡号 " + FAKE_CARD + " 结束", "规则层邮箱与银行卡"),
    ("内网 " + FAKE_IP + " 与网卡 " + FAKE_MAC + " 结束", "规则层 IP 与 MAC"),
    ("网址 " + FAKE_URL + " 与车牌 " + FAKE_PLATE + " 结束", "规则层网址与车牌"),
    ("普通文本 " + MIXED_SECRET + " 结束", "熵兜底裸值"),
    ("已经是 *** 与 " + R.format_placeholder(7) + " 结束", "跳过已脱敏与占位符"),
    ("UUID " + UUID + " 结束", "白名单 UUID"),
    ("哈希 " + HEX_DIGEST + " 结束", "白名单十六进制"),
    ("路径 /usr/local/share/app/config.toml 结束", "白名单路径"),
    ("版本 v1.2.3-beta.4 结束", "白名单版本号"),
    ("时间 2026-09-18T03:05:00.123Z 结束", "白名单时间戳"),
    ("标识符 utf8_encode_value_longer 结束", "白名单词形"),
    ("Authorization=Bearer " + MIXED_SECRET, "凭据键 + 熵兜底"),
    ("", "空串"),
]

PURE_TEXT_CASES = [
    ("纯字母 " + PURE_LETTERS + " 结束", "纯字母开关"),
    ("纯数字 " + PURE_DIGITS + " 结束", "纯数字开关"),
    ("混合 " + MIXED_SECRET + " 结束", "混合仍走熵判定"),
]

STRUCTURED_CASES = [
    {
        "api_key": "hunter2000!!",
        "user": {"password": "hunter2000!!", "name": "zhang"},
        "list": ["secret_token", {"nested_token": "hunter2000!!"}, 42, True, None],
    },
    {
        "name": "zhang",
        "note": "联系 " + FAKE_EMAIL + " 与 " + FAKE_IP + " 结束",
        "public_key": "not-secret",
        "example": "not-secret",
        "team-token": "hunter2000!!",
    },
    {"items": [["deep_password", "hunter2000!!"], {"ok": "plain"}], "empty": {}},
]

KEY_VALUES = [
    "api_key",
    "API-Key",
    " api key ",
    "DB__PASSWORD",
    "X-Token",
    "apiKey",
    "access_token",
    "tokens",
    "public_key",
    "example",
    "my_public_key",
    "internal_example",
    "team-token",
    "MyCustomSecret",
    "credentials",
    "csrf",
    "session",
    "密码",
    "用户密码字段",
    "cookie",
    "user",
    "note",
    "",
    "   ",
]

SKIP_VALUES = [
    "",
    "   ",
    "***",
    "****",
    "***abc",
    R.format_placeholder(3),
    "x " + R.format_placeholder(3),
    "plain",
]

ENTROPY_EXEMPT_TOKENS = [
    UUID,
    HEX_DIGEST,
    FAKE_MAC,
    "sha256:" + HEX_DIGEST,
    "v1.2.3-beta.4",
    "2026-09-18T03:05:00.123Z",
    "2026-09-18",
    "/usr/local/bin/app",
    "C:" + BACKSLASH + "Users" + BACKSLASH + "app" + BACKSLASH + "config.toml",
    'say("hello")',
    "self.config.value",
    "hook:topic-name",
    "utf8_encode_value_longer",
    "some_identifier_name",
    "$HOME_VAR",
    "_private_name",
    "name=",
    "a=b",
    R.format_placeholder(31),
    "abcdef",
    "12345678",
    "",
]

CANDIDATE_CASES = [
    (MIXED_SECRET, 20, 3.5, False, False),
    (PURE_LETTERS, 20, 3.5, True, False),
    (PURE_LETTERS, 20, 3.5, False, False),
    (PURE_DIGITS, 20, 3.5, False, True),
    (PURE_DIGITS, 20, 3.5, False, False),
    (HEX_DIGEST, 20, 3.5, True, False),
    ("utf8_encode_value_longer", 20, 3.5, False, False),
    ("short", 20, 3.5, False, False),
    ("中文中文中文中文中文中文", 20, 3.5, False, False),
    (MIXED_SECRET, 5, 3.5, False, False),
    ("aB3", 4, 1.0, False, False),
]

WORD_SHAPED_TOKENS = [
    "camelCaseValue",
    "PascalCaseName",
    "snake_case_name",
    "lowercaseword",
    PURE_LETTERS,
    "AAAAAAAAAAAA",
    "aB",
    "",
]

SPAN_CASES = [
    ("前缀 " + MIXED_SECRET + " 后缀", 20, 3.5),
    ("地址 " + UUID + " 结束", 20, 3.5),
    ("两个 " + MIXED_SECRET + " 与 " + PURE_DIGITS + " 结束", 20, 3.5),
    ("无候选 plain text here 结束", 20, 3.5),
    (MIXED_SECRET, 5, 3.5),
    ("", 20, 3.5),
]


def context(
    registry: R.SequenceRegistry,
    cycle: R.PlaceholderCycle,
    *,
    entropy_pure_letters: bool,
    entropy_pure_digits: bool,
) -> E.MaskContext:
    return E.MaskContext(
        matcher=E.SensitiveMatcher(extra_keys=EXTRA_KEYS, exempt_keys=EXEMPT_KEYS),
        cycle=cycle,
        stats=registry.stats,
        entropy_enabled=True,
        entropy_min_length=20,
        entropy_min_bits=3.5,
        entropy_pure_letters=entropy_pure_letters,
        entropy_pure_digits=entropy_pure_digits,
        pattern_rules=tuple(rules_module.builtin_rules()),
    )


def run_texts(cases, *, pure_letters: bool = False, pure_digits: bool = False):
    registry = R.SequenceRegistry(sequence_source=itertools.count(1).__next__)
    cycle, _ = registry.begin_cycle("引擎对照")
    ctx = context(
        registry,
        cycle,
        entropy_pure_letters=pure_letters,
        entropy_pure_digits=pure_digits,
    )
    results = []
    for text, note in cases:
        masked = E.mask_text(text, ctx)
        results.append(
            {
                "text": text,
                "note": note,
                "masked": masked,
                "stats": {
                    "values_masked": registry.stats.values_masked,
                    "skipped_values": registry.stats.skipped_values,
                    "rules_masked": registry.stats.rules_masked,
                    "entropy_masked": registry.stats.entropy_masked,
                },
            }
        )
    return results


def run_structured():
    registry = R.SequenceRegistry(sequence_source=itertools.count(1).__next__)
    cycle, _ = registry.begin_cycle("引擎对照")
    ctx = context(
        registry,
        cycle,
        entropy_pure_letters=False,
        entropy_pure_digits=False,
    )
    return [
        {"value": case, "masked": E.mask_structured_value(case, ctx)}
        for case in STRUCTURED_CASES
    ]


def main() -> None:
    matcher = E.SensitiveMatcher(extra_keys=EXTRA_KEYS, exempt_keys=EXEMPT_KEYS)

    fixture = {
        "source": ["omnicrawl/llm/desensitization/engine.py"],
        "extra_keys": EXTRA_KEYS,
        "exempt_keys": EXEMPT_KEYS,
        "texts": run_texts(TEXT_CASES),
        "pure_texts": run_texts(PURE_TEXT_CASES, pure_letters=True, pure_digits=True),
        "structured": run_structured(),
        "normalize_key": [
            {"key": key, "normalized": E.normalize_key(key)} for key in KEY_VALUES
        ],
        "is_sensitive": [
            {"key": key, "sensitive": matcher.is_sensitive(key)} for key in KEY_VALUES
        ],
        "should_skip": [
            {"value": value, "skip": E.should_skip_value(value)} for value in SKIP_VALUES
        ],
        "entropy_exempt": [
            {"token": token, "exempt": E.is_entropy_exempt(token)}
            for token in ENTROPY_EXEMPT_TOKENS
        ],
        "entropy_candidate": [
            {
                "token": token,
                "min_length": min_length,
                "min_bits": min_bits,
                "pure_letters": pure_letters,
                "pure_digits": pure_digits,
                "candidate": E.is_entropy_candidate(
                    token,
                    min_length=min_length,
                    min_bits=min_bits,
                    pure_letters=pure_letters,
                    pure_digits=pure_digits,
                ),
            }
            for token, min_length, min_bits, pure_letters, pure_digits in CANDIDATE_CASES
        ],
        "word_shaped": [
            {"token": token, "word_shaped": E.is_word_shaped_letters(token)}
            for token in WORD_SHAPED_TOKENS
        ],
        "spans": [
            {
                "text": text,
                "min_length": min_length,
                "min_bits": min_bits,
                "spans": [
                    [start, end]
                    for start, end in E.find_entropy_spans(
                        text, min_length=min_length, min_bits=min_bits
                    )
                ],
            }
            for text, min_length, min_bits in SPAN_CASES
        ],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        "已写入 %s：文本 %d + %d 条、结构用例 %d、键名 %d、跳过 %d、形态 %d、候选 %d、"
        "词形 %d、扫描 %d"
        % (
            FIXTURE_PATH.relative_to(ROOT),
            len(TEXT_CASES),
            len(PURE_TEXT_CASES),
            len(STRUCTURED_CASES),
            len(KEY_VALUES),
            len(SKIP_VALUES),
            len(ENTROPY_EXEMPT_TOKENS),
            len(CANDIDATE_CASES),
            len(WORD_SHAPED_TOKENS),
            len(SPAN_CASES),
        )
    )


if __name__ == "__main__":
    main()
