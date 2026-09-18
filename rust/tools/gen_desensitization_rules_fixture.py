#!/usr/bin/env python3
"""生成消息脱敏「值类型规则层」的对照数据集，供 Rust 侧 `omnicrawl-llm` 使用。

期望值来自 Python 真实现 `omnicrawl/llm/desensitization/rules.py`：逐条跑规则的候选区间
（`iter_matches`）、整段扫描（`scan_pattern_rules`，含优先级与重叠去重）、香农熵、Luhn 校验与
邮箱豁免。数据集里只出现假值（无真实凭据），Luhn 合法号由生成器现算，保证可复现。

每条文本都带**期望命中**（按扫描顺序的规则 id），生成器当场断言：语料被写错、被豁免表静默吃掉
或优先级与预期不符时立刻失败——否则数据集照样生成、测试照样通过，实际上什么都没覆盖。

本片只覆盖内核已搬的五类规则（网址 / 邮箱 / 银行卡 / MAC / 车牌）；PEM、连接串、IP 与 gitleaks
留到后续片，同一份数据集按 `rule_ids` 标明范围。

用法：``python rust/tools/gen_desensitization_rules_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/desensitization_rules_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/desensitization_rules_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm.desensitization import rules as R  # noqa: E402

if not Path(R.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

#: 内置规则清单（顺序即优先级）；本片已覆盖全部 11 条。
PORTED_RULE_IDS = [rule.rule_id for rule in R.builtin_rules()]


def luhn_check_digit(prefix: str) -> str:
    """按 Luhn 规则算出校验位，保证语料里的银行卡号确实合法。"""

    total = 0
    double = True
    for character in reversed(prefix):
        digit = int(character)
        if double:
            digit *= 2
            if digit > 9:
                digit -= 9
        total += digit
        double = not double
    return str((10 - total % 10) % 10)


def card(prefix: str) -> str:
    return prefix + luhn_check_digit(prefix)


def group(value: str, separator: str) -> str:
    return separator.join(value[index : index + 4] for index in range(0, len(value), 4))


CARD_16 = card("411111111111111")
CARD_16_SPACED = group(CARD_16, " ")
CARD_16_HYPHEN = group(CARD_16, "-")
CARD_15 = card("37828224631000")
CARD_19 = card("601111111111111111")
CARD_12 = card("12345678901")
CARD_INVALID = "4111111111111112"

LOCAL_64 = "a" * 64
LOCAL_65 = "a" * 65

# 拼接构造：避免源码里出现「占位符形状」的字面量（会被宿主还原成会话原文）。
PLATE_NEW_ENERGY = "沪" + "B" + "D" + "12345"
PLATE_TOO_LONG = "沪" + "A" + "12345678"
PRIVATE_KEY_WORD = "PRIVATE" + " KEY"

# ── PEM 语料（正文用假 base64） ────────────────────────────────────────────
B64_A = "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789+/aBcDeFgHiJkLmNoPq"
B64_B = "sTuVwXyZ0123456789+/aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789+"
PEM_HEADER = "-----BEGIN RSA " + PRIVATE_KEY_WORD + "-----"
PEM_FOOTER = "-----END RSA " + PRIVATE_KEY_WORD + "-----"
PEM_PLAIN_HEADER = "-----BEGIN " + PRIVATE_KEY_WORD + "-----"
PEM_PLAIN_FOOTER = "-----END " + PRIVATE_KEY_WORD + "-----"
PEM_LOWER_HEADER = "-----begin rsa " + PRIVATE_KEY_WORD.lower() + "-----"
PEM_LOWER_FOOTER = "-----end rsa " + PRIVATE_KEY_WORD.lower() + "-----"
PEM_BLOCK = "\n".join([PEM_HEADER, B64_A, B64_B, PEM_FOOTER])
PEM_PLAIN_BLOCK = "\n".join([PEM_PLAIN_HEADER, B64_A, B64_B, PEM_PLAIN_FOOTER])
PEM_LOWER_BLOCK = "\n".join([PEM_LOWER_HEADER, B64_A, PEM_LOWER_FOOTER])
PEM_TRUNCATED = "\n".join([PEM_HEADER, B64_A, B64_B])
PEM_TRUNCATED_LONG = "\n".join([PEM_HEADER, "a" * 100])
PEM_ONE_LINE = "\n".join([PEM_HEADER, B64_A])
PEM_SHORT_BODY = "\n".join([PEM_HEADER, "a" * 8, PEM_FOOTER])
PEM_HEADER_ONLY = "\n".join([PEM_HEADER, "MIIE"])
PEM_TWO_BLOCKS = "\n-----\n".join([PEM_BLOCK, PEM_ONE_LINE + "\n" + PEM_FOOTER])
PEM_CERTIFICATE = "\n".join(
    ["-----BEGIN CERTIFICATE-----", "QUJDRA==", "-----END CERTIFICATE-----"]
)

# ── 数据库连接串语料（账号口令都是假值） ──────────────────────────────────
PW_WORD = "pass" + "word"
PWD_WORD = "p" + "w" + "d"
NL = chr(10)


def kv(pairs) -> str:
    """按 `k=v;…` 拼连接串，避免源码里出现成形的敏感值。"""

    return ";".join(key + "=" + value for key, value in pairs)


DB_URI_PG = "postgresql://" + "app" + ":" + "pgpass" + "@" + "db1.corp.local:5432/appdb"
DB_URI_MYSQL = "mysql://" + "root" + ":" + "mysqlpw" + "@" + "db2.corp.local/appdb"
DB_URI_MONGO = "mongodb" + "+srv://" + "app" + ":" + "mongopw" + "@" + "cluster0.corp.local/admin"
DB_URI_JDBC = "jdbc:" + "postgresql://" + "db3.corp.local:5432/appdb"
DB_URI_R2DBC = "r2dbc:" + "mysql://" + "db4.corp.local:3306/appdb"
DB_URI_REDIS = "redis://" + "cache.corp.local:6379/0"
DB_URI_UPPER = "POSTGRESQL:" + "//DB6.CORP.LOCAL/appdb"
DB_URI_UNKNOWN = "notadb" + "://x/y"
DB_KV = kv([("Server", "db1.corp.local"), ("Database", "app"), ("User", "sa"), (PW_WORD, "pwA")])
DB_KV_PWD = kv([("Server", "db2.corp.local"), ("User", "sa"), (PWD_WORD, "pwB")])
DB_KV_SPACES = "data source = db3.corp.local ;uid=sa; " + PW_WORD + " = pwC"
DB_KV_TRAILING = kv([("Server", "db4.corp.local"), ("uid", "sa"), (PWD_WORD, "pwD")]) + ")"
DB_KV_NO_PASSWORD = kv([("Server", "db5.corp.local"), ("Database", "app"), ("User", "sa")])
DB_KV_LINE_BREAK = (
    kv([("Server", "db6.corp.local"), ("Database", "app")]) + NL + PW_WORD + "=pwE"
)
DB_KV_DISTANT = (
    kv([("Server", "db7.corp.local"), ("filler", "x" * 420), (PW_WORD, "pwF")])
)
DB_KV_PASSWORD_FIRST = PW_WORD + "=pwG;" + kv([("Server", "db8.corp.local")])
DB_KV_NO_BOUNDARY = "x" + kv([("Server", "db9.corp.local"), ("Pwd", "pwH")])


def ipv4(a: int, b: int, c: int, d: int) -> str:
    """按点分十进制拼地址。"""

    return "%d.%d.%d.%d" % (a, b, c, d)


# ── IP 语料（私有段 / 文档段 / 专用段，全部为假地址） ──────────────────────
NAT64 = "64" + ":" + "ff9b" + "::1"
DOC6 = "2001" + ":" + "db8" + "::1"
DOC6_SUB = "2001" + ":" + "db8" + ":1234::1"
DOC6_ALT = "2001" + ":" + "db8" + "::abcd"
ULA = "fd00" + ":" + "1234" + "::1"
LINK6 = "fe80" + "::1"
PUBLIC6 = "2001" + ":" + "4860" + ":4860::8888"
PUBLIC6_ALT = "2a00" + ":" + "1450" + ":4001:800::200e"
SIX_TO_FOUR = "2002" + "::1"
IP_INTERNAL_SAMPLES = [
    ipv4(10, 1, 2, 3),
    ipv4(10, 255, 255, 254),
    ipv4(172, 16, 0, 1),
    ipv4(172, 31, 255, 254),
    ipv4(192, 168, 0, 1),
    ipv4(192, 168, 255, 254),
    ipv4(169, 254, 1, 1),
    ipv4(240, 0, 0, 1),
    ipv4(255, 255, 255, 255),
    ipv4(192, 0, 0, 9),
    ipv4(192, 0, 2, 5),
    ipv4(198, 18, 0, 1),
    ipv4(198, 51, 100, 7),
    ipv4(203, 0, 113, 7),
    ipv4(0, 1, 2, 3),
    DOC6,
    DOC6_SUB,
    DOC6_ALT,
    ULA,
    LINK6,
]
IP_EXTERNAL_SAMPLES = [
    NAT64,
    ipv4(8, 8, 8, 8),
    ipv4(1, 1, 1, 1),
    ipv4(93, 184, 216, 34),
    ipv4(223, 5, 5, 5),
    ipv4(100, 128, 0, 1),
    ipv4(172, 15, 0, 1),
    ipv4(192, 169, 0, 1),
    ipv4(169, 253, 0, 1),
    PUBLIC6,
    PUBLIC6_ALT,
    SIX_TO_FOUR,
    "::2",
]
IP_NEUTRAL_SAMPLES = [
    ipv4(0, 0, 0, 0),
    ipv4(127, 0, 0, 1),
    ipv4(127, 255, 255, 255),
    ipv4(224, 0, 0, 1),
    ipv4(239, 255, 255, 255),
    ipv4(100, 64, 0, 1),
    ipv4(100, 127, 255, 255),
    "::",
    "::1",
    "ff02" + "::1",
    "0:0:0:0:0:0:0:1",
    "256.1.1.1",
    "01.2.3.4",
    "1.2.3",
    "1.2.3.4.5",
    "1:2:3:4:5:6:7:",
    "1:2:3:4:5:6:7:8:",
    "1::2::3",
    "1:::2",
    "12345::1",
    "::ffff:1.2.3.4",
    "not-an-ip",
    "",
]
IP_VALUES = sorted(set(IP_INTERNAL_SAMPLES + IP_EXTERNAL_SAMPLES + IP_NEUTRAL_SAMPLES))

#: (文本, 按扫描顺序期望命中的规则 id)。空列表表示「不应命中」。
TEXTS: list[tuple[str, list[str]]] = [
    # ── 邮箱 ──────────────────────────────────────────────────────────────
    ("联系 alice@corp.local 结束", ["email"]),
    ("用户 <bob@team.corp.local>, 抄送 carol@dept.corp.local。", ["email", "email"]),
    ("模式 demo@example.com 与 ops@localhost 判为误报", []),
    (
        "超长本地部分 " + LOCAL_64 + "@corp.local 与 " + LOCAL_65 + "@corp.local",
        ["email"],
    ),
    ("短 TLD user@b.c.d 与 user@corp.c 结束", []),
    ("非法标签 user@-corp.local 与 user@corp-.local 结束", []),
    ("尾点 user@mail.corp. 与退让 user@mail.corp.c 结束", ["email", "email"]),
    ("点段 user@sub.corp.local 与清单 demo@example.org 结束", ["email"]),
    ("邮箱前后是中文字符邮箱dave@corp.local好", ["email"]),
    # ── 网址 ──────────────────────────────────────────────────────────────
    ("访问 https://example.com/path?q=1#frag。", ["url"]),
    ("用 http://10.0.0.5:8080/a_b-c 和 ftp://files.corp.local/dir/ 结束", ["url", "url"]),
    ('带引号 "https://a.corp.local/x" 与尖括号 <https://b.corp.local/y>', ["url", "url"]),
    ("反斜杠 https://c.corp.local/a\\b 结束", ["url"]),
    ("伪前缀 httpx://not-a-url 与 xhttps://nope.corp.local 结束", []),
    ("中文路径 https://例子.corp.local/路径 结束", ["url"]),
    ("大小写 HTTPS://ExAmPlE.com/A 结束", ["url"]),
    # ── 银行卡 ────────────────────────────────────────────────────────────
    ("卡号 " + CARD_16 + " 结束", ["bank-card"]),
    ("卡号 " + CARD_16_SPACED + " 结束", ["bank-card"]),
    ("卡号 " + CARD_16_HYPHEN + " 结束", ["bank-card"]),
    ("短卡 " + CARD_12 + " 与长卡 " + CARD_19 + " 结束", ["bank-card", "bank-card"]),
    ("十五位 " + CARD_15 + " 结束", ["bank-card"]),
    ("校验失败 " + CARD_INVALID + " 结束", []),
    ("全同数字 4444444444444444 结束", []),
    ("位数不足 12345678901 与位数过多 41111111111111111111 结束", []),
    ("紧贴字母 a" + CARD_16 + "b 与紧贴小数 " + CARD_16 + ".5 结束", ["bank-card"]),
    # ── MAC ──────────────────────────────────────────────────────────────
    ("网卡 00:1A:2B:3C:4D:5E 结束", ["mac-address"]),
    ("网卡 AA-BB-CC-DD-EE-FF 与混合 00:1a-2b:3c-4d:5e 结束", ["mac-address", "mac-address"]),
    ("思科 0011.2233.4455 结束", ["mac-address"]),
    ("残缺 00:1A:2B:3C:4D:5 与超长 00:1A:2B:3C:4D:5E:6F 结束", []),
    ("点分残缺 0011.2233.445 与 0011.2233.44556 结束", []),
    # ── 车牌 ──────────────────────────────────────────────────────────────
    ("车牌 沪A12345 结束", ["license-plate-cn"]),
    ("车辆 京B12345 与 粤BD12345 上路", ["license-plate-cn", "license-plate-cn"]),
    ("车辆 沪A1234学 上路", ["license-plate-cn"]),
    ("紧贴字母 AB沪A12345 与紧贴中文 车沪A12345牌 结束", []),
    ("非法字母 沪I12345 与位数不足 沪A1234 结束", []),
    ("新能源 " + PLATE_NEW_ENERGY + " 上路", ["license-plate-cn"]),
    ("超长号牌 " + PLATE_TOO_LONG + " 结束", []),
        # ── PEM 私钥 ──────────────────────────────────────────────────────────
    (PEM_BLOCK, ["pem-private-key"]),
    (PEM_PLAIN_BLOCK, ["pem-private-key"]),
    (PEM_LOWER_BLOCK, ["pem-private-key"]),
    (PEM_TRUNCATED, ["pem-private-key-body"]),
    (PEM_TRUNCATED_LONG, ["pem-private-key-body"]),
    (PEM_ONE_LINE, ["pem-private-key-body"]),
    ("前缀 " + PEM_BLOCK + " 后缀", ["pem-private-key"]),
    (PEM_TWO_BLOCKS, ["pem-private-key", "pem-private-key"]),
    (PEM_SHORT_BODY, ["pem-private-key"]),
    (PEM_HEADER_ONLY, []),
    (PEM_CERTIFICATE, []),
    # ── 数据库连接串 ──────────────────────────────────────────────────────
    (DB_URI_PG, ["db-connection-uri"]),
    (DB_URI_MYSQL, ["db-connection-uri"]),
    (DB_URI_MONGO, ["db-connection-uri"]),
    (DB_URI_JDBC, ["db-connection-uri"]),
    (DB_URI_R2DBC, ["db-connection-uri"]),
    (DB_URI_REDIS, ["db-connection-uri"]),
    (DB_URI_UPPER, ["db-connection-uri"]),
    ('配置 "' + DB_URI_PG + '" 结束', ["db-connection-uri"]),
    (DB_URI_UNKNOWN, []),
    (DB_KV, ["db-connection-kv"]),
    (DB_KV_PWD, ["db-connection-kv"]),
    (DB_KV_SPACES, ["db-connection-kv"]),
    (DB_KV_TRAILING, ["db-connection-kv"]),
    (DB_KV_NO_PASSWORD, []),
    (DB_KV_LINE_BREAK, []),
    (DB_KV_DISTANT, []),
    (DB_KV_PASSWORD_FIRST, []),
    (DB_KV_NO_BOUNDARY, []),
    # ── IP ────────────────────────────────────────────────────────────────
    ("内网 " + ipv4(10, 1, 2, 3) + " 与公网 " + ipv4(8, 8, 8, 8) + " 结束", ["ip-internal", "ip-external"]),
    ("私网 " + ipv4(172, 16, 0, 1) + " 与 " + ipv4(192, 168, 0, 1) + " 结束", ["ip-internal", "ip-internal"]),
    ("链路本地 " + ipv4(169, 254, 1, 1) + " 结束", ["ip-internal"]),
    ("环回 " + ipv4(127, 0, 0, 1) + " 与未指定 " + ipv4(0, 0, 0, 0) + " 结束", []),
    ("组播 " + ipv4(224, 0, 0, 1) + " 与保留 " + ipv4(240, 0, 0, 1) + " 结束", ["ip-internal"]),
    ("共享地址 " + ipv4(100, 64, 0, 1) + " 结束", []),
    ("测试网 " + ipv4(192, 0, 2, 5) + " 与基准 " + ipv4(198, 18, 0, 1) + " 结束", ["ip-internal", "ip-internal"]),
    ("文档 " + ipv4(198, 51, 100, 7) + " 与 " + ipv4(203, 0, 113, 7) + " 结束", ["ip-internal", "ip-internal"]),
    ("零网段 " + ipv4(0, 1, 2, 3) + " 结束", ["ip-internal"]),
    ("广播 " + ipv4(255, 255, 255, 255) + " 结束", ["ip-internal"]),
    ("IPv6 链路本地 " + LINK6 + " 结束", ["ip-internal"]),
    ("IPv6 ULA " + ULA + " 结束", ["ip-internal"]),
    ("IPv6 文档 " + DOC6 + " 结束", ["ip-internal"]),
    ("IPv6 NAT64 " + NAT64 + " 结束", ["ip-external"]),
    ("IPv6 公网 " + PUBLIC6 + " 结束", ["ip-external"]),
    ("IPv6 六 " + SIX_TO_FOUR + " 结束", ["ip-external"]),
    ("IPv6 环回 ::1 与未指定 :: 结束", []),
    ("IPv6 组播 ff02::1 结束", []),
    ("IPv6 完整 0:0:0:0:0:0:0:1 结束", []),
    ("IPv6 压缩段 ::2 结束", ["ip-external"]),
    ("带端口 " + ipv4(10, 0, 0, 6) + ":8080 结束", []),
    ("方括号 [" + DOC6_SUB + "]:443 结束", ["ip-internal"]),
    ("非法 256.1.1.1 与 01.2.3.4 结束", []),
    ("短 1.2.3 与长 1.2.3.4.5 结束", []),
    ("映射 ::ffff:1.2.3.4 结束", []),
    ("网址里的 http://" + ipv4(10, 0, 0, 5) + ":8080/a 结束", ["url"]),
# ── 跨规则重叠（优先级：网址 → 邮箱 → 车牌 → 银行卡 → MAC） ────────────────
    ("https://user@example.net/x 结束", ["url"]),
    ("https://host.corp.local/00:1A:2B:3C:4D:5E 结束", ["url"]),
    (CARD_16 + "@bank.corp.local 结束", ["email"]),
    (
        "混合段 00:1A:2B:3C:4D:5E 与卡号 " + CARD_16 + " 与 https://z.corp.local/ 结束",
        ["mac-address", "bank-card", "url"],
    ),
    ("连接 " + DB_URI_PG + " 与卡号 " + CARD_16 + " 结束", ["db-connection-uri", "bank-card"]),
    ("", []),
]

ENTROPY_TEXTS = ["", "a", "aa", "ab", "aaa", "abcd", "aA1!", "aaaaaaaaaaaaaaaa", "aB3$xY9-", "中文中文中文"]

LUHN_VALUES = [
    CARD_12,
    CARD_15,
    CARD_16,
    CARD_19,
    CARD_INVALID,
    CARD_16_SPACED,
    CARD_16_HYPHEN,
    "4444444444444444",
    "1111111111111111",
    "1234567890123456",
    "123456789012",
    "",
    "not-a-number",
]

ALLOWLIST_VALUES = [
    "demo@example.com",
    "ops@example.org",
    "dev@example.net",
    "ops@localhost",
    "x@example.com.cn",
    "x@exampl.com",
    "admin@localhost.localdomain",
    "x@sub.example.com",
    "demo@corp.local",
    "first" + "@" + "second" + "@" + "example.com",
    "-user@example.com",
    "",
]


def scan(text: str, rules: tuple[R.PatternRule, ...]) -> list[R.RuleMatch]:
    return R.scan_pattern_rules(text, rules)


def main() -> None:
    builtin = tuple(R.builtin_rules())
    rule_ids = [rule.rule_id for rule in builtin]

    texts = []
    for text, expected in TEXTS:
        rule_matches = [
            {
                "rule": rule.rule_id,
                "category": rule.category,
                "start": start,
                "end": end,
                "value": value,
            }
            for rule in builtin
            for start, end, value in rule.iter_matches(text)
        ]
        hits = scan(text, builtin)
        got = [item.rule_id for item in hits]
        if got != expected:
            raise SystemExit(
                f"语料期望与真实现不符：{text!r} 期望 {expected}，实际 {got}"
            )
        texts.append(
            {
                "text": text,
                "expected": expected,
                "rule_matches": rule_matches,
                "scan": [
                    {
                        "rule": item.rule_id,
                        "category": item.category,
                        "start": item.start,
                        "end": item.end,
                        "value": item.value,
                    }
                    for item in hits
                ],
            }
        )

    fixture = {
        "source": ["omnicrawl/llm/desensitization/rules.py"],
        "rule_ids": rule_ids,
        "categories": [[category, flag] for category, flag in R.CATEGORY_CONFIG_FLAGS.items()],
        "trailing_trim_chars": R.TRAILING_TRIM_CHARS,
        "entropy": [
            {"text": text, "bits": R.shannon_entropy_bits(text)} for text in ENTROPY_TEXTS
        ],
        "luhn": [{"value": value, "valid": R._luhn_valid(value)} for value in LUHN_VALUES],
        "email_allowlist": [
            {"value": value, "allowlisted": R._EMAIL_ALLOWLIST.search(value) is not None}
            for value in ALLOWLIST_VALUES
        ],
        "ip_classification": [
            {
                "value": value,
                "internal": R.is_internal_ip(value),
                "external": R.is_external_ip(value),
            }
            for value in IP_VALUES
        ],
        "texts": texts,
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：文本 {len(texts)} 条（期望全部对上）、"
        f"逐条候选 {sum(len(item['rule_matches']) for item in texts)} 个、"
        f"扫描命中 {sum(len(item['scan']) for item in texts)} 个"
    )


if __name__ == "__main__":
    main()
