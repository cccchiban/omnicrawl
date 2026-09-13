"""值类型规则层：正则识别「无键名的敏感值类型」并产出待脱敏区间（设计稿 §5.3）。

结构感知层（键名规则）要求值处在有键名的结构里；熵兜底层要求值具备高随机性。
两者都覆盖不到的，是「形态确定、随机性低」的敏感值类型：PEM 私钥、数据库连接串、
邮箱、银行卡、内外网 IP、网址、MAC 地址、大陆车牌。本模块用固定正则 + 校验器
（银行卡 Luhn、IP `ipaddress` 分类）识别这些类型；`gitleaks.py` 把 gitleaks 规则解析成
同一套 ``PatternRule``。引擎（``engine.py``）按结构层 → 规则层 → 熵兜底的顺序调用。

规则语义（与 gitleaks 对齐）：

- ``keywords``：文本级预过滤（大小写不敏感），命中任一关键字才运行该规则；
- ``secret_group``：指定「秘密」捕获组，缺省时整段匹配即秘密；
- ``min_entropy``：对候选值做香农熵下限；
- ``validator``：形态校验（Luhn / IP 分类），不通过则丢弃；
- ``allowlist`` / ``match_allowlist``：命中候选值 / 整段匹配即判为误报并跳过；
- ``trim_trailing``：把值尾部的标点 / 空白留在原文（避免破坏 JSON、引号结构）。

区间去重：同一段文本按规则优先级「先命中先占位」，重叠区间只保留优先级最高的一条。

性能（长会话热点，实测 42KB 文本、232 条规则）：

- ``gitleaks:generic-api-key`` 单条占全量扫描耗时的一半以上（51.8ms / 89.1ms）；
- ``PatternRule.locality``（局部化扫描）把该条降到 2.2ms（23x）、全部规则降到 41.1ms，
  且与全量扫描逐条等价（见 ``_iter_anchored_matches`` 的等价性说明）；
- ``scan_pattern_rules`` 缓存纯扫描结果，同一文本再度出现时降到微秒级。
"""

from __future__ import annotations

import ipaddress
import re
import sys
import threading
from bisect import bisect_left, insort
from collections import Counter, OrderedDict
from dataclasses import dataclass
from functools import lru_cache
from math import log2
from typing import Any, Callable, Iterator, Sequence

# ── 规则类别（与 [desensitization] 的 detect_* 开关一一对应） ──────────────

CATEGORY_PEM_PRIVATE_KEY = "pem_private_key"
CATEGORY_DB_CONNECTION_STRING = "db_connection_string"
CATEGORY_EMAIL = "email"
CATEGORY_BANK_CARD = "bank_card"
CATEGORY_INTERNAL_IP = "internal_ip"
CATEGORY_EXTERNAL_IP = "external_ip"
CATEGORY_URL = "url"
CATEGORY_MAC_ADDRESS = "mac_address"
CATEGORY_LICENSE_PLATE = "license_plate"
CATEGORY_GITLEAKS = "gitleaks"

#: 类别 → 配置字段名（``build_enabled_rules`` 用来按配置裁剪内置规则）。
CATEGORY_CONFIG_FLAGS = {
    CATEGORY_PEM_PRIVATE_KEY: "detect_pem_private_key",
    CATEGORY_DB_CONNECTION_STRING: "detect_db_connection_string",
    CATEGORY_EMAIL: "detect_email",
    CATEGORY_BANK_CARD: "detect_bank_card",
    CATEGORY_INTERNAL_IP: "detect_internal_ip",
    CATEGORY_EXTERNAL_IP: "detect_external_ip",
    CATEGORY_URL: "detect_url",
    CATEGORY_MAC_ADDRESS: "detect_mac_address",
    CATEGORY_LICENSE_PLATE: "detect_license_plate",
}

#: 值尾部需要留在原文的标点 / 空白（占位符只替换值本体，避免破坏结构）。
TRAILING_TRIM_CHARS = " \t\r\n\u3000\"'`.,;:!?)]}>,、。；：！？"


def shannon_entropy_bits(text: str) -> float:
    """字符频率的香农熵（bit/char）；长度 ≤1 时为 0。"""

    length = len(text)
    if length <= 1:
        return 0.0
    counts = Counter(text)
    return -sum((count / length) * log2(count / length) for count in counts.values())


@dataclass(frozen=True)
class RuleMatch:
    """一条规则命中的待脱敏区间（半开区间；``value`` 为原文值）。"""

    rule_id: str
    category: str
    start: int
    end: int
    value: str


@dataclass(frozen=True)
class RuleLocality:
    """局部化扫描参数：只在「锚点候选起点」上重匹配，避免整段扫描。

    登记方必须能证明以下两条（否则不要登记 ``PatternRule.locality``，全量扫描
    永远是正确的兜底）：

    1. 每个命中内部都含 ``anchor`` 的一次匹配（``anchor`` 是模式中必选子表达式
       的等价正则）；
    2. 命中起点最多早于该锚点起点 ``prefix_max`` 个字符。

    两条保证「所有可能的命中起点」都落在某个锚点命中点左侧 ``prefix_max`` 个
    字符的窗口内；对候选起点用完整模式重匹配即可复现全量扫描的结果。注意本策略
    不限制命中**结尾**，因此对 ``x{11,}`` 这类无上界后缀同样安全——常规「按窗口
    finditer」会截断这类长值，绝不可用于本场景。
    """

    anchor: "re.Pattern[str]"
    prefix_max: int


@dataclass(frozen=True)
class PatternRule:
    """单条值类型规则：正则 + 关键字 / 熵 / 校验器 / 豁免表。"""

    rule_id: str
    category: str
    pattern: "re.Pattern[str]"
    description: str = ""
    keywords: tuple[str, ...] = ()
    secret_group: int | None = None
    min_entropy: float | None = None
    validator: Callable[[str], bool] | None = None
    #: 可选局部化扫描参数；为 None 时走全量 ``finditer``（默认、永远正确）。
    locality: RuleLocality | None = None
    #: 对「候选值」生效的豁免正则（gitleaks regexTarget 缺省 / secret）。
    allowlist: tuple["re.Pattern[str]", ...] = ()
    #: 对「整段匹配」生效的豁免正则（gitleaks regexTarget = match）。
    match_allowlist: tuple["re.Pattern[str]", ...] = ()
    #: 子串命中的停用词（值含任一即跳过，大小写不敏感）。
    stopwords: frozenset[str] = frozenset()
    trim_trailing: bool = True

    def accepts(self, value: str, match_text: str) -> bool:
        """候选值 / 整段匹配是否通过豁免与校验（不含熵，熵在取原始值前判）。"""

        if not value:
            return False
        lowered = value.lower()
        if self.stopwords and any(word in lowered for word in self.stopwords):
            return False
        for pattern in self.allowlist:
            if pattern.search(value):
                return False
        for pattern in self.match_allowlist:
            if pattern.search(match_text):
                return False
        if self.validator is not None and not self.validator(value):
            return False
        return True

    def _iter_pattern_matches(self, text: str) -> Iterator["re.Match[str]"]:
        """产出本规则的模式命中：默认全量扫描；登记 ``locality`` 时走局部化。"""

        if self.locality is None:
            yield from self.pattern.finditer(text)
            return
        yield from _iter_anchored_matches(self.pattern, self.locality, text)

    def iter_matches(self, text: str) -> Iterator[tuple[int, int, str]]:
        """产出本规则在文本中的待脱敏区间 ``(start, end, value)``。"""

        secret_group = self.secret_group
        for match in self._iter_pattern_matches(text):
            if secret_group is None:
                start, end = match.span(0)
            else:
                start, end = match.span(secret_group)
                if start < 0:
                    start, end = match.span(0)
            if start < 0 or start >= end:
                continue
            raw = text[start:end]
            if (
                self.min_entropy is not None
                and shannon_entropy_bits(raw) < self.min_entropy
            ):
                continue
            if not self.accepts(raw, match.group(0)):
                continue
            if self.trim_trailing:
                start, end = _trim_span(text, start, end)
            if start >= end:
                continue
            yield start, end, text[start:end]


def _iter_anchored_matches(
    pattern: "re.Pattern[str]",
    locality: RuleLocality,
    text: str,
) -> Iterator["re.Match[str]"]:
    """局部化扫描：只用完整模式在「锚点候选起点」上重匹配。

    语义与 ``pattern.finditer(text)`` 等价（见 ``RuleLocality`` 的两条前提）：

    - 候选起点集合是所有锚点命中点左侧 ``prefix_max`` 窗口的并集，是「全部真实命中
      起点」的超集；
    - 每个候选点用 ``pattern.match(text, position)`` 重匹配——``endpos`` 仍是文本
      末尾，命中不会被截断，因此无上界后缀（``x{11,}``）也能拿到完整值；
    - 候选点升序 + 游标跳过重叠，复现 ``finditer`` 的「最左优先、互不重叠」语义。

    候选点密度过高时（锚点数 × 窗口 ≥ 文本长度）逐点重匹配不再划算，退回全量扫描；
    两条路径结果一致。
    """

    anchor_starts = [match.start() for match in locality.anchor.finditer(text)]
    if not anchor_starts:
        return
    prefix_max = locality.prefix_max
    if len(anchor_starts) * (prefix_max + 1) >= len(text):
        yield from pattern.finditer(text)
        return

    candidates: set[int] = set()
    for start in anchor_starts:
        candidates.update(range(max(0, start - prefix_max), start + 1))

    cursor = 0
    for position in sorted(candidates):
        if position < cursor:
            continue
        match = pattern.match(text, position)
        if match is None or match.end() <= match.start():
            continue
        cursor = match.end()
        yield match


def _trim_span(text: str, start: int, end: int) -> tuple[int, int]:
    """把值尾部的标点 / 空白留在原文（只收缩区间，不改变前缀）。"""

    while end > start and text[end - 1] in TRAILING_TRIM_CHARS:
        end -= 1
    return start, end


# ── 扫描结果缓存 ──────────────────────────────────────────────────────────

# 长会话里每轮都会重新脱敏同一批未变历史（消息历史只追加），扫描是纯函数
# （规则 + 文本 → 区间），因此可跨请求复用。掩码阶段仍按周期分配占位符，缓存
# 不参与占位符语义，也不保存原文之外的任何内容（原文本来就在内存里）。
_SCAN_CACHE_MAX_BYTES = 4 * 1024 * 1024
_SCAN_CACHE_MAX_TEXT_CHARS = 256 * 1024


class _ScanCache:
    """按 (文本, 规则集合) 记忆扫描结果的 LRU，按字节预算淘汰条目。

    键里的规则集合用 ``id()``（O(1)）而非元组本身（每次调用都要哈希 200+ 条规则）：
    条目持有规则集合的强引用，因此 ``id`` 不会被回收复用；命中时再用 ``is`` 校验，
    确保绝不错配到另一组规则。
    """

    def __init__(self, max_bytes: int, max_text_chars: int) -> None:
        self._max_bytes = max_bytes
        self._max_text_chars = max_text_chars
        self._lock = threading.Lock()
        self._entries: "OrderedDict[tuple[str, int], tuple[Any, tuple[RuleMatch, ...]]]" = (
            OrderedDict()
        )
        self._size_bytes = 0
        self.hits = 0
        self.misses = 0
        self.evictions = 0

    def get(self, text: str, rules: tuple[PatternRule, ...]) -> tuple[RuleMatch, ...] | None:
        if len(text) > self._max_text_chars:
            return None
        key = (text, id(rules))
        with self._lock:
            entry = self._entries.get(key)
            if entry is None or entry[0] is not rules:
                self.misses += 1
                return None
            self._entries.move_to_end(key)
            self.hits += 1
            return entry[1]

    def put(
        self,
        text: str,
        rules: tuple[PatternRule, ...],
        matches: tuple[RuleMatch, ...],
    ) -> None:
        if len(text) > self._max_text_chars:
            return
        key = (text, id(rules))
        cost = _entry_bytes(text, matches)
        if cost > self._max_bytes:
            return
        with self._lock:
            previous = self._entries.pop(key, None)
            if previous is not None:
                self._size_bytes -= _entry_bytes(text, previous[1])
            self._entries[key] = (rules, matches)
            self._size_bytes += cost
            while self._size_bytes > self._max_bytes and self._entries:
                _evicted_key, evicted = self._entries.popitem(last=False)
                self._size_bytes -= _entry_bytes(_evicted_key[0], evicted[1])
                self.evictions += 1

    def clear(self) -> None:
        with self._lock:
            self._entries.clear()
            self._size_bytes = 0

    def stats(self) -> dict[str, int]:
        with self._lock:
            return {
                "entries": len(self._entries),
                "size_bytes": self._size_bytes,
                "hits": self.hits,
                "misses": self.misses,
                "evictions": self.evictions,
            }


def _entry_bytes(text: str, matches: tuple[RuleMatch, ...]) -> int:
    """条目占用估算：文本本体 + 每个匹配对象与字符串切片的固定开销。"""

    return sys.getsizeof(text) + len(matches) * 96 + 64


_SCAN_CACHE = _ScanCache(_SCAN_CACHE_MAX_BYTES, _SCAN_CACHE_MAX_TEXT_CHARS)


def clear_scan_cache() -> None:
    """清空扫描结果缓存（测试与配置变更后手动失效用）。"""

    _SCAN_CACHE.clear()


def scan_cache_stats() -> dict[str, int]:
    """返回扫描缓存计数（测试 / 观测用，不含任何文本）。"""

    return _SCAN_CACHE.stats()


def scan_pattern_rules(text: str, rules: Sequence[PatternRule]) -> list[RuleMatch]:
    """按规则优先级扫描文本；重叠区间只保留先命中的一条，结果按起点排序。

    结果按 (文本, 规则集合) 缓存：同一段未变历史在后续请求中不再重复扫描。返回
    可变列表副本，调用方改动不影响缓存。

    只对**元组**规则集合启用缓存：元组不可变，可以安全地当作键；列表等可变容器
    可能被调用方改规则而缓存不失效，那会漏脱敏，因此一律不缓存（只损失速度）。
    """

    if not text or not rules:
        return []
    if not isinstance(rules, tuple):
        return _scan_pattern_rules_uncached(text, rules)
    cached = _SCAN_CACHE.get(text, rules)
    if cached is not None:
        return list(cached)
    matches = _scan_pattern_rules_uncached(text, rules)
    _SCAN_CACHE.put(text, rules, tuple(matches))
    return matches


def _scan_pattern_rules_uncached(
    text: str, rules: Sequence[PatternRule]
) -> list[RuleMatch]:
    """实际扫描实现（缓存未命中路径）。"""

    lowered = text.lower()
    intervals: list[tuple[int, int]] = []
    matches: list[RuleMatch] = []
    for rule in rules:
        if rule.keywords and not any(keyword in lowered for keyword in rule.keywords):
            continue
        for start, end, value in rule.iter_matches(text):
            if _overlaps(intervals, start, end):
                continue
            insort(intervals, (start, end))
            matches.append(RuleMatch(rule.rule_id, rule.category, start, end, value))
    matches.sort(key=lambda item: item.start)
    return matches


def _overlaps(intervals: list[tuple[int, int]], start: int, end: int) -> bool:
    """已接受区间互不重叠且按起点有序，二分判断新区间是否冲突。"""

    index = bisect_left(intervals, (start, start))
    if index > 0 and intervals[index - 1][1] > start:
        return True
    if index < len(intervals) and intervals[index][0] < end:
        return True
    return False


# ── 值类型正则（内置规则） ────────────────────────────────────────────────

# PEM 私钥块：BEGIN <TYPE> PRIVATE KEY [BLOCK] ----- … ----- END … （跨行）。
_PEM_BLOCK_PATTERN = re.compile(
    r"-----BEGIN[ A-Z0-9_-]*PRIVATE KEY[ A-Z0-9_-]*-----"
    r"[\s\S]*?"
    r"-----END[ A-Z0-9_-]*PRIVATE KEY[ A-Z0-9_-]*-----",
    re.IGNORECASE,
)
# 私钥头 + 后续 base64 正文行（无 END 的截断场景，避免正文外泄）。
_PEM_BODY_PATTERN = re.compile(
    r"-----BEGIN[ A-Z0-9_-]*PRIVATE KEY[ A-Z0-9_-]*-----"
    r"(?:[\r\n]+[A-Za-z0-9+/=]{16,76})+",
    re.IGNORECASE,
)

# 数据库连接串（URI / SQLAlchemy 驱动后缀 / JDBC 前缀）。
_DB_URI_PATTERN = re.compile(
    r"(?:(?:jdbc|r2dbc):)?"
    r"(?:postgresql|postgres|pgsql|mysql|mariadb|mongodb\+srv|mongodb|redis|rediss|"
    r"amqps|amqp|mssql|sqlserver|oracle|clickhouse|elasticsearch|cassandra|neo4j|"
    r"sqlite|cockroachdb|db2|h2|influxdb|memcached|etcd)"
    r"(?:\+[A-Za-z0-9_]+)?://[^\s\"'`<>]+",
    re.IGNORECASE,
)
# ADO / .NET 风格连接串：Server=…;…;Password=…（关键字预过滤降低误报）。
_DB_KV_PATTERN = re.compile(
    r"(?i)\b(?:server|data\s*source|host|addr|address)\s*=\s*[^;\r\n]{1,200};"
    r"[^\r\n]{0,400}?(?:password|pwd)\s*=\s*[^;\r\n]{1,200}",
)

# 网址（http / https / ftp；尾部标点由 trim 留在原文）。
_URL_PATTERN = re.compile(r"(?i)\b(?:https?|ftp)://[^\s\"'<>`\\]+")

# 邮箱（域名须含字母 TLD；example.* / localhost 判为误报）。
_EMAIL_PATTERN = re.compile(
    r"(?i)(?<![A-Za-z0-9._%+-])"
    r"[A-Za-z0-9._%+-]{1,64}@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?"
    r"(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*\.[A-Za-z]{2,63}"
    r"(?![A-Za-z0-9-])"
)
_EMAIL_ALLOWLIST = re.compile(
    r"(?i)@(?:example\.(?:com|org|net)|localhost)\b",
)

# 银行卡号：12–19 位数字（可含单个空格 / 连字符分隔），再经 Luhn 校验。
_BANK_CARD_PATTERN = re.compile(r"(?<![\d.-])\d(?:[ -]?\d){11,18}(?![\d.-])")

# MAC 地址：冒号 / 连字符 6 组，或 Cisco 点分三组。
_MAC_PATTERN = re.compile(
    r"(?<![0-9A-Fa-f:.-])(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}(?![0-9A-Fa-f:-])"
    r"|(?<![0-9A-Fa-f.])(?:[0-9A-Fa-f]{4}\.){2}[0-9A-Fa-f]{4}(?![0-9A-Fa-f.])"
)

# 中国大陆车牌：普通（省份 + 字母 + 5 位）与新能源（省份 + 字母 + D/F + 5 位数字）。
_CN_PLATE_PROVINCES = "京津沪渝冀豫云辽黑湘皖鲁新苏浙赣鄂桂甘晋蒙陕吉闽贵粤青藏川宁琼使领"
_CN_PLATE_PATTERN = re.compile(
    r"(?<![A-Za-z0-9\u4e00-\u9fff])"
    r"(?:[" + _CN_PLATE_PROVINCES + r"][A-HJ-NP-Z][A-HJ-NP-Z0-9]{4}[A-HJ-NP-Z0-9挂学警港澳]"
    r"|[" + _CN_PLATE_PROVINCES + r"][A-HJ-NP-Z][DABCEFGHJK][0-9]{5})"
    r"(?![A-Za-z0-9])"
)

# IP 地址：IPv4 点分十进制 + 常见 IPv6 形态（完整 / :: 压缩 / 前导 ::）。
_IPV4_OCTET = r"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)"
_IPV6_PATTERN = (
    r"(?:"
    r"(?:[0-9A-Fa-f]{1,4}:){7}[0-9A-Fa-f]{1,4}"
    r"|(?:[0-9A-Fa-f]{1,4}:){1,7}:"
    r"|(?:[0-9A-Fa-f]{1,4}:){1,6}:[0-9A-Fa-f]{1,4}"
    r"|(?:[0-9A-Fa-f]{1,4}:){1,5}(?::[0-9A-Fa-f]{1,4}){1,2}"
    r"|(?:[0-9A-Fa-f]{1,4}:){1,4}(?::[0-9A-Fa-f]{1,4}){1,3}"
    r"|(?:[0-9A-Fa-f]{1,4}:){1,3}(?::[0-9A-Fa-f]{1,4}){1,4}"
    r"|(?:[0-9A-Fa-f]{1,4}:){1,2}(?::[0-9A-Fa-f]{1,4}){1,5}"
    r"|[0-9A-Fa-f]{1,4}:(?::[0-9A-Fa-f]{1,4}){1,6}"
    r"|:(?:(?::[0-9A-Fa-f]{1,4}){1,7}|:)"
    r")"
)
_IP_PATTERN = re.compile(
    r"(?<![\w.:])(?:"
    r"(?:" + _IPV4_OCTET + r"\.){3}" + _IPV4_OCTET + r"|" + _IPV6_PATTERN + r")"
    r"(?![\w:])(?!\.\d)"
)


def _luhn_valid(value: str) -> bool:
    """银行卡 Luhn 校验；同时排除位数不符与全同数字串。"""

    digits = [ord(character) - 48 for character in value if character.isdigit()]
    if not 12 <= len(digits) <= 19 or len(set(digits)) == 1:
        return False
    total = 0
    double = False
    for digit in reversed(digits):
        if double:
            digit *= 2
            if digit > 9:
                digit -= 9
        total += digit
        double = not double
    return total % 10 == 0


def _parse_ip(value: str) -> "ipaddress._BaseAddress | None":
    try:
        return ipaddress.ip_address(value)
    except ValueError:
        return None


def is_internal_ip(value: str) -> bool:
    """内网 / 回环以外的私有地址（10/8、172.16/12、192.168/16、链路本地、IPv6 ULA）。"""

    address = _parse_ip(value)
    if address is None:
        return False
    if address.is_loopback or address.is_unspecified or address.is_multicast:
        return False
    return bool(address.is_private or address.is_link_local)


def is_external_ip(value: str) -> bool:
    """公网可路由地址（排除私有 / 环回 / 链路本地 / 组播 / 保留段）。"""

    address = _parse_ip(value)
    if address is None:
        return False
    if address.is_loopback or address.is_unspecified or address.is_multicast:
        return False
    return bool(address.is_global)


@lru_cache(maxsize=1)
def builtin_rules() -> tuple[PatternRule, ...]:
    """内置值类型规则（顺序即优先级；重叠区间由先者占位）。"""

    return (
        PatternRule(
            rule_id="pem-private-key",
            category=CATEGORY_PEM_PRIVATE_KEY,
            pattern=_PEM_BLOCK_PATTERN,
            description="PEM 私钥块（BEGIN/END … PRIVATE KEY）",
            keywords=("private key",),
        ),
        PatternRule(
            rule_id="pem-private-key-body",
            category=CATEGORY_PEM_PRIVATE_KEY,
            pattern=_PEM_BODY_PATTERN,
            description="PEM 私钥头与 base64 正文（无 END 的截断场景）",
            keywords=("private key",),
        ),
        PatternRule(
            rule_id="db-connection-uri",
            category=CATEGORY_DB_CONNECTION_STRING,
            pattern=_DB_URI_PATTERN,
            description="数据库连接串（URI 形态）",
        ),
        PatternRule(
            rule_id="db-connection-kv",
            category=CATEGORY_DB_CONNECTION_STRING,
            pattern=_DB_KV_PATTERN,
            description="数据库连接串（ADO / .NET 键值形态）",
            keywords=("password", "pwd"),
        ),
        PatternRule(
            rule_id="url",
            category=CATEGORY_URL,
            pattern=_URL_PATTERN,
            description="网址（http / https / ftp）",
            keywords=("://",),
        ),
        PatternRule(
            rule_id="email",
            category=CATEGORY_EMAIL,
            pattern=_EMAIL_PATTERN,
            description="邮箱地址",
            keywords=("@",),
            allowlist=(_EMAIL_ALLOWLIST,),
        ),
        PatternRule(
            rule_id="license-plate-cn",
            category=CATEGORY_LICENSE_PLATE,
            pattern=_CN_PLATE_PATTERN,
            description="中国大陆车牌",
        ),
        PatternRule(
            rule_id="bank-card",
            category=CATEGORY_BANK_CARD,
            pattern=_BANK_CARD_PATTERN,
            description="银行卡号（Luhn 校验）",
            validator=_luhn_valid,
        ),
        PatternRule(
            rule_id="mac-address",
            category=CATEGORY_MAC_ADDRESS,
            pattern=_MAC_PATTERN,
            description="MAC 地址",
        ),
        PatternRule(
            rule_id="ip-internal",
            category=CATEGORY_INTERNAL_IP,
            pattern=_IP_PATTERN,
            description="内网 IP（私有 / 链路本地）",
            validator=is_internal_ip,
        ),
        PatternRule(
            rule_id="ip-external",
            category=CATEGORY_EXTERNAL_IP,
            pattern=_IP_PATTERN,
            description="外网 IP（公网可路由）",
            validator=is_external_ip,
        ),
    )


def build_enabled_rules(config: object) -> tuple[PatternRule, ...]:
    """按 ``[desensitization]`` 配置裁剪内置规则，并按需追加 gitleaks 规则。"""

    categories = {
        category
        for category, flag in CATEGORY_CONFIG_FLAGS.items()
        if getattr(config, flag, False)
    }
    rules = [rule for rule in builtin_rules() if rule.category in categories]
    if getattr(config, "gitleaks_enabled", False):
        # 延迟导入：gitleaks 模块依赖本模块的 PatternRule，避免顶层循环依赖。
        from .gitleaks import load_gitleaks_rules

        path = getattr(config, "gitleaks_config_path", "") or None
        rules.extend(load_gitleaks_rules(path))
    return tuple(rules)


__all__ = [
    "CATEGORY_BANK_CARD",
    "CATEGORY_CONFIG_FLAGS",
    "CATEGORY_DB_CONNECTION_STRING",
    "CATEGORY_EMAIL",
    "CATEGORY_EXTERNAL_IP",
    "CATEGORY_GITLEAKS",
    "CATEGORY_INTERNAL_IP",
    "CATEGORY_LICENSE_PLATE",
    "CATEGORY_MAC_ADDRESS",
    "CATEGORY_PEM_PRIVATE_KEY",
    "CATEGORY_URL",
    "PatternRule",
    "RuleLocality",
    "RuleMatch",
    "TRAILING_TRIM_CHARS",
    "build_enabled_rules",
    "builtin_rules",
    "clear_scan_cache",
    "is_external_ip",
    "is_internal_ip",
    "scan_cache_stats",
    "scan_pattern_rules",
    "shannon_entropy_bits",
]
