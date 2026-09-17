#!/usr/bin/env python3
"""生成凭据脱敏的对照数据集，供 Rust 侧 `omnicrawl-session::redaction` 使用。

期望值来自 Python 真实现 `omnicrawl/common/redaction.py`。六轮替换的顺序、`\b` 边界、
量词下界都属于语义的一部分，因此用例要覆盖命中与**不该命中**两侧。

用法：``python rust/tools/gen_redaction_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/redaction_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/redaction_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.common import redaction as R  # noqa: E402

if not Path(R.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

SK = "sk-" + "a" * 24
SHORT_SK = "sk-" + "a" * 23
GH = "ghp_" + "b" * 20
PAT = "github_pat_" + "c" * 20
AWS = "AKIA" + "D" * 16
AWS_LOWER = "akia" + "d" * 16
PEM = (
    "-----BEGIN RSA PRIVATE KEY-----\n"
    "MIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu\n"
    "-----END RSA PRIVATE KEY-----"
)
PEM_PLAIN = "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----"

TEXTS = [
    "api_key=abcdef123456",
    'API-KEY: "abcdef123456"',
    "apikey = abcdef123456",
    "token: abcdef123456",
    "password='abcdef123456'",
    "secret=abcdef123456",
    "cookie=abcdef123456",
    "access_token=abcdef123456",
    "refresh-token: abcdef123456",
    "id_token=abcdef123456",
    "keyboard=abcdef123456",
    "monkey=abcdef123456",
    "key_count=5",
    "tokenizer=abcdef123456",
    "authorization: Bearer abcdef1234567890",
    'Authorization="abcdef123456"',
    "authorization=Bearer abcdef1234567890",
    "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
    "bearer abcdefgh",
    "Bearer short",
    f"provider {SK} end",
    f"provider {SHORT_SK} end",
    f"github {GH} end",
    f"github {PAT} end",
    f"aws {AWS} end",
    f"aws {AWS_LOWER} end",
    PEM,
    PEM_PLAIN,
    "-----BEGIN PRIVATE KEY-----\n没有结束标记",
    f"{PEM}\n{PEM_PLAIN}",
    f"混合文本：api_key={SK} 且 Bearer {GH} 还有 {AWS}",
    "没有秘密的普通文本，包含 key 这个词",
    "",
]

VALUES = [
    {"api_key": "abc", "nested": {"password": "p", "keep": "v"}},
    {"API-KEY": "abc", "x-auth-token": "abc", "x_api_key": "abc"},
    {"keyboard": "abc", "id": "abc", "tokens": "abc"},
    {"text": "api_key=abcdef123456"},
    [{"token": "abc"}, "plain", 5, None],
    {"list": [{"password": "p"}], "deep": {"a": {"b": {"secret": "s"}}}},
    "Bearer abcdefghijklmnop",
]


def main() -> None:
    fixture = {
        "source": ["omnicrawl/common/redaction.py"],
        "texts": [
            {"input": text, "expected": R.redact_sensitive_text(text)} for text in TEXTS
        ],
        "values": [
            {"input": value, "expected": R.redact_sensitive_values(value)} for value in VALUES
        ],
        "long_list": {
            "length": 150,
            "expected_length": len(R.redact_sensitive_values(list(range(150)))),
        },
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    changed = sum(1 for item in fixture["texts"] if item["input"] != item["expected"])
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：文本 {len(TEXTS)} 条（{changed} 条发生变化），"
        f"结构 {len(VALUES)} 条"
    )


if __name__ == "__main__":
    main()
