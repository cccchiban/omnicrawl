#!/usr/bin/env python3
"""生成 gitleaks 规则接入的对照数据集，供 Rust 侧 `omnicrawl-llm` 的 parity 测试使用。

期望值全部来自 Python 真实现（``omnicrawl/llm/desensitization/gitleaks.py`` 与
``rules.py``）：``load_gitleaks_rules`` 的规则集、``normalize_gitleaks_pattern`` 的归一化结果、
``scan_pattern_rules`` 在一批语料上的命中（rule_id / 区间 / 值）。

区间统一换算成**字节偏移**：Python 的 ``re`` 给的是字符索引，内核按字节切串，
两侧必须换算到同一坐标系才能逐字段比对。

语料里的「秘密」一律**拼接构造**（``"AKIA" + "IOSFODNN7EXAMPLE"``）：本仓库自己就是宿主，
直接写完整密钥字面量会被会话脱敏改写成占位符，数据集就会静默失真。

用法：``python rust/tools/gen_desensitization_gitleaks_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/gitleaks_parity.json``
"""

from __future__ import annotations

import hashlib
import json
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/gitleaks_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm.desensitization.gitleaks import (  # noqa: E402
    GITLEAKS_SNAPSHOT_PATH,
    load_gitleaks_rules,
    normalize_gitleaks_pattern,
)
from omnicrawl.llm.desensitization.rules import scan_pattern_rules  # noqa: E402

if not Path(GITLEAKS_SNAPSHOT_PATH).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库快照：{GITLEAKS_SNAPSHOT_PATH}")

AWS_KEY = "AKIA" + "Q7X3ZL9PMN2R5TV8"
GITHUB_TOKEN = "ghp_" + "16C7e42F292c6912E7710c838347Ae178B4a"
GITHUB_TOKEN_2 = "ghp_" + "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"
PEM_HEADER = "-----BEGIN RSA " + "PRIVATE KEY-----"
GENERIC_SECRET = "x7f9Q2mK8pL4vN6bR1tY3wZ5aC0dE"

SCAN_CASES = [
    ("AWS Access Key", f"aws_access_key_id = {AWS_KEY}"),
    ("GitHub PAT", f"token: {GITHUB_TOKEN}"),
    ("PEM 私钥头", f"证书内容\n{PEM_HEADER}\nMIIEowIBAAKCAQEA"),
    ("generic-api-key", f'api_key = "{GENERIC_SECRET}"'),
    ("无命中的普通文本", "今天天气不错，适合写代码。"),
    ("低熵值被熵下限挡掉", 'api_key = "aaaaaaaaaaaaaaaa"'),
    ("同一行两个命中", f"a={GITHUB_TOKEN} b={GITHUB_TOKEN_2}"),
    ("命中后带尾部标点", f"key={AWS_KEY}."),
    ("中文夹一个命中", f"密钥是 {GITHUB_TOKEN}，请勿外传。"),
    ("空文本", ""),
]

NORMALIZE_CASES = [
    (r"abc\z", r"abc\Z"),
    (r"(?i)secret", r"(?i)secret"),
    (r"a(?i)b(?m)c", r"(?im)abc"),
    (r"(?:x)(?s)y", r"(?s)(?:x)y"),
    (r"plain", r"plain"),
]

# 自定义文件：同 id 覆盖 + 新 id 追加，并带一条 condition=AND 的豁免（保守跳过）
CUSTOM_TOML = """
[[rules]]
id = "custom-rule"
description = "自定义规则"
regex = '''CUSTOM-[A-Z]{4}'''
keywords = ["custom-"]

[[rules]]
id = "aws-access-token"
description = "覆盖后的 AWS 规则"
regex = '''AKIA[0-9A-Z]{16}'''

[[rules]]
id = "and-condition-rule"
description = "带 AND 豁免的规则"
regex = '''ANDCOND-[A-Z]{4}'''

  [[rules.allowlists]]
  condition = "AND"
  regexes = ['''ANDCOND-[A-Z]{4}''']
"""


def byte_offset(text: str, char_index: int) -> int:
    return len(text[:char_index].encode("utf-8"))


def rule_to_json(rule) -> dict:
    return {
        "rule_id": rule.rule_id,
        "category": rule.category,
        "pattern": rule.pattern.pattern,
        "description": rule.description,
        "keywords": list(rule.keywords),
        "secret_group": rule.secret_group,
        "min_entropy": rule.min_entropy,
        "allowlist": [item.pattern for item in rule.allowlist],
        "match_allowlist": [item.pattern for item in rule.match_allowlist],
        "stopwords": sorted(rule.stopwords),
        "trim_trailing": rule.trim_trailing,
    }


def scan_case(label: str, text: str, rules) -> dict:
    hits = [
        {
            "rule_id": hit.rule_id,
            "start": byte_offset(text, hit.start),
            "end": byte_offset(text, hit.end),
            "value": hit.value,
        }
        for hit in scan_pattern_rules(text, rules)
    ]
    return {"label": label, "text": text, "expected": hits}


def main() -> None:
    rules = load_gitleaks_rules()
    if not rules:
        raise SystemExit("内置快照没有加载出任何规则")

    with tempfile.TemporaryDirectory() as directory:
        custom_path = Path(directory) / "custom-gitleaks.toml"
        custom_path.write_text(CUSTOM_TOML, encoding="utf-8")
        merged = load_gitleaks_rules(custom_path)
        custom = {
            "toml": CUSTOM_TOML,
            "rule_ids": [rule.rule_id for rule in merged],
            "overridden": rule_to_json(
                next(rule for rule in merged if rule.rule_id == "gitleaks:aws-access-token")
            ),
            "appended": rule_to_json(
                next(rule for rule in merged if rule.rule_id == "gitleaks:custom-rule")
            ),
            "and_condition_allowlist_skipped": [
                rule_to_json(rule)["allowlist"]
                for rule in merged
                if rule.rule_id == "gitleaks:and-condition-rule"
            ],
        }

    fixture = {
        "source": "omnicrawl/llm/desensitization/gitleaks.py",
        "snapshot_sha256": hashlib.sha256(GITLEAKS_SNAPSHOT_PATH.read_bytes()).hexdigest(),
        "rule_count": len(rules),
        "rules": [rule_to_json(rule) for rule in rules],
        "normalize": [
            {"input": pattern, "expected": normalize_gitleaks_pattern(pattern)}
            for pattern, _ in NORMALIZE_CASES
        ],
        "scan": [scan_case(label, text, rules) for label, text in SCAN_CASES],
        "custom": custom,
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：规则 {len(rules)} 条、"
        f"归一化 {len(fixture['normalize'])}、扫描 {len(fixture['scan'])}、"
        f"自定义合并后 {len(custom['rule_ids'])} 条"
    )


if __name__ == "__main__":
    main()
