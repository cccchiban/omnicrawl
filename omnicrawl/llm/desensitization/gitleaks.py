"""gitleaks 规则接入：把 gitleaks.toml 解析为脱敏规则（设计稿 §5.3.1）。

内置快照 ``gitleaks.toml``（上游默认配置的离线副本，随包分发）默认加载；
``[desensitization].gitleaks_config_path`` 指向自定义文件时，按其规则 id 覆盖 / 追加。

解析遵循 gitleaks 语义：

- ``keywords``：文本级预过滤（大小写不敏感，命中任一关键字才运行该规则）；
- ``entropy``：候选值香农熵下限；
- ``secretGroup``：指定「秘密」捕获组，缺省时整段匹配即秘密；
- 规则级 / 全局 ``allowlists``：``regexes`` 与 ``stopwords`` 命中即判为误报。

无法在「纯文本、无文件路径 / 无行上下文」下忠实执行的豁免条件一律**保守跳过**
（宁可多脱敏，不可漏脱敏）：``paths``、``commits``、``regexTarget = "line"`` 与
``condition = "AND"`` 仅跳过其豁免判断，规则本身仍生效。

兼容性归一（Python ``re`` 与 Go RE2 的差异）：

- Go 的文本尾锚点 ``\\z`` 归一为 ``\\Z``；
- 出现在模式中部的全局内联标志 ``(?i)`` 上提到模式开头（Python 不允许非起始全局标志）。

编译失败的规则整条跳过，不影响其余规则（上游 222 条规则在 Python 3.9 下全部可编译）。
"""

from __future__ import annotations

import re
import warnings
from functools import lru_cache
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

from .rules import CATEGORY_GITLEAKS, PatternRule, RuleLocality

#: 随包分发、与上游逐字一致的离线快照。
GITLEAKS_SNAPSHOT_PATH = Path(__file__).with_name("gitleaks.toml")

_INLINE_FLAG_GROUP = re.compile(r"\(\?([aiLmsux]+)\)")
_LEADING_FLAG_GROUP = re.compile(r"^\(\?([aiLmsux]+)\)")
_AND_CONDITION = "and"

# ── 局部化扫描登记 ────────────────────────────────────────────────────────
#
# 只登记「前缀上界可证明」的规则（收益实测见 rules.py 模块说明）。登记值三元组为
# ``(前提原文, 前缀上界, 锚点分支原文)``：左边原文代表模式中锚点分支之前的前缀，
# 上界是它能匹配的最大字符数，右边原文是模式里必选的关键字分支。
#
# 两项检查都按**原文逐字比对**：上游把前缀从 ``{0,50}?`` 改成更宽的上界、或重写
# 关键字分支时，比对立即失败，规则自动退回全量扫描（宁可慢，不可漏脱敏）。
_LOCALITY_SPECS: dict[str, tuple[str, int, str]] = {
    # 上游模式： (?i)[\w.-]{0,50}?(?:access|auth|…|token)(?:…)(value)(?:…)
    # `[\w.-]{0,50}?` 的 getwidth 上界为 50 → 命中起点 ≥ 锚点起点 - 50。
    # 注意不能按「窗口内 finditer」限制结尾：值分支含无上界的 `{11,}`，截断会漏脱敏。
    "generic-api-key": (
        r"[\w.-]{0,50}?",
        50,
        r"(?:access|auth|(?-i:[Aa]pi|API)|credential|creds|key|passw(?:or)?d|secret|token)",
    ),
}

__all__ = [
    "GITLEAKS_SNAPSHOT_PATH",
    "load_gitleaks_rules",
    "normalize_gitleaks_pattern",
]


#: 模式开头全局内联标志 → ``re`` 编译标志。
_FLAG_LETTERS = {
    "a": re.ASCII,
    "i": re.IGNORECASE,
    "m": re.MULTILINE,
    "s": re.DOTALL,
    "u": re.UNICODE,
    "x": re.VERBOSE,
}


def _anchor_flags(pattern: str) -> int:
    """锚点正则的编译标志：取模式全局内联标志，再叠加 IGNORECASE 做保守放宽。

    锚点只需是「所有真实命中起点」的超集，多找候选点只会变慢、不会漏脱敏：

    - 取全局标志：``normalize_gitleaks_pattern`` 保证全局标志都上提到模式开头，
      不带上它会把 ``SECRET = …`` / ``Key = …`` 这类大小写变体漏掉（真会漏脱敏）；
    - 叠加 IGNORECASE：防范锚点分支处在作用域标志组内的情况；IGNORECASE 对正则
      匹配是单调放宽（字面量 / 正向字符类匹配更多），但会**收窄**否定字符类
      ``[^…]``，因此 ``locality_for`` 对含 ``[^`` 的锚点一律拒绝。
    """

    flags = re.IGNORECASE
    leading = _LEADING_FLAG_GROUP.match(pattern)
    if leading:
        for letter in leading.group(1):
            flags |= _FLAG_LETTERS.get(letter, 0)
    return flags


def locality_for(rule_id: str, pattern: str) -> RuleLocality | None:
    """为规则推导局部化参数；形状与登记值不符时返回 None（走全量扫描）。"""

    spec = _LOCALITY_SPECS.get(rule_id)
    if spec is None:
        return None
    prefix_source, prefix_max, anchor_source = spec
    if "[^" in anchor_source:
        # 否定字符类 + IGNORECASE 会收窄匹配，破坏「候选点是超集」的前提。
        return None
    anchor_index = pattern.find(anchor_source)
    if anchor_index < 0:
        return None
    if not pattern[:anchor_index].endswith(prefix_source):
        return None
    return RuleLocality(
        anchor=re.compile(anchor_source, _anchor_flags(pattern)),
        prefix_max=prefix_max,
    )


def normalize_gitleaks_pattern(pattern: str) -> str:
    """把 Go RE2 模式归一为 Python ``re`` 可编译的等价模式（见模块说明）。"""

    normalized = pattern.replace(r"\z", r"\Z")
    flags: set[str] = set()
    leading = _LEADING_FLAG_GROUP.match(normalized)
    if leading:
        flags.update(leading.group(1))
        normalized = normalized[leading.end():]

    def _collect(match: "re.Match[str]") -> str:
        flags.update(match.group(1))
        return ""

    body = _INLINE_FLAG_GROUP.sub(_collect, normalized)
    if not flags:
        return body
    return f"(?{''.join(sorted(flags))}){body}"


def load_gitleaks_rules(config_path: str | Path | None = None) -> tuple[PatternRule, ...]:
    """加载内置快照 + 自定义 gitleaks.toml（自定义按 id 覆盖 / 追加）。

    任何读取 / 解析失败都回退到「已成功加载的部分」，绝不抛错中断脱敏运行时；
    自定义文件不可用时仍保留内置快照规则。
    """

    key = _path_key(config_path)
    return _load_cached(key)


def _path_key(config_path: str | Path | None) -> str | None:
    if not config_path:
        return None
    text = str(config_path).strip()
    if not text:
        return None
    try:
        return str(Path(text).expanduser().resolve())
    except OSError:
        return text


@lru_cache(maxsize=8)
def _load_cached(path_key: str | None) -> tuple[PatternRule, ...]:
    rules = list(_load_file(GITLEAKS_SNAPSHOT_PATH))
    if path_key:
        try:
            rules = _merge(rules, _load_file(Path(path_key)))
        except Exception:
            # 自定义文件不可读 / 解析失败：保留内置快照，不中断运行时。
            pass
    return tuple(rules)


def _merge(
    base: Sequence[PatternRule],
    extra: Sequence[PatternRule],
) -> list[PatternRule]:
    """按规则 id 合并：同 id 用自定义覆盖（保持原位置），新 id 追加到末尾。"""

    merged: dict[str, PatternRule] = {rule.rule_id: rule for rule in base}
    for rule in extra:
        merged[rule.rule_id] = rule
    return list(merged.values())


def _load_file(path: Path) -> list[PatternRule]:
    text = path.read_text(encoding="utf-8-sig")
    data = _parse_toml(text)
    return _build_rules(data)


def _parse_toml(text: str) -> dict[str, Any]:
    try:
        import tomllib
    except ImportError:  # Python < 3.11
        try:
            import tomli as tomllib
        except ImportError as exc:  # pragma: no cover - 依赖缺失
            raise RuntimeError("缺少 TOML 解析依赖（tomli），无法加载 gitleaks 规则。") from exc
    data = tomllib.loads(text)
    return data if isinstance(data, dict) else {}


def _build_rules(data: Mapping[str, Any]) -> list[PatternRule]:
    global_secret, global_match, global_stopwords = _allowlist_parts(
        _as_mappings(data.get("allowlist") or data.get("allowlists"))
    )
    rules: list[PatternRule] = []
    for raw in _as_mappings(data.get("rules")):
        rules.extend(
            _build_rule(raw, global_secret, global_match, global_stopwords)
        )
    return rules


def _build_rule(
    raw: Mapping[str, Any],
    global_secret: tuple[re.Pattern[str], ...],
    global_match: tuple[re.Pattern[str], ...],
    global_stopwords: frozenset[str],
) -> list[PatternRule]:
    rule_id = str(raw.get("id") or "").strip()
    if not rule_id:
        return []
    patterns: list[str] = []
    if isinstance(raw.get("regex"), str):
        patterns.append(raw["regex"])
    patterns.extend(
        item for item in raw.get("regexes", ()) if isinstance(item, str)
    )
    compiled = [
        compiled
        for compiled in (
            _compile(normalize_gitleaks_pattern(pattern)) for pattern in patterns
        )
        if compiled is not None
    ]
    if not compiled:
        return []

    secret_group = raw.get("secretGroup")
    secret_group = secret_group if isinstance(secret_group, int) else None
    entropy = raw.get("entropy")
    min_entropy = float(entropy) if isinstance(entropy, (int, float)) else None
    keywords = tuple(
        str(keyword).lower()
        for keyword in raw.get("keywords", ())
        if isinstance(keyword, str) and keyword
    )
    secret_allow, match_allow, stopwords = _allowlist_parts(
        _as_mappings(raw.get("allowlist") or raw.get("allowlists"))
    )
    description = str(raw.get("description") or "").strip()
    total = len(compiled)
    rules: list[PatternRule] = []
    for index, pattern in enumerate(compiled):
        suffix = "" if total == 1 else f"#{index + 1}"
        rules.append(
            PatternRule(
                rule_id=f"gitleaks:{rule_id}{suffix}",
                category=CATEGORY_GITLEAKS,
                pattern=pattern,
                description=description,
                keywords=keywords,
                secret_group=secret_group,
                min_entropy=min_entropy,
                allowlist=secret_allow + global_secret,
                match_allowlist=match_allow + global_match,
                stopwords=stopwords | global_stopwords,
                locality=locality_for(rule_id, pattern.pattern),
            )
        )
    return rules


def _compile(pattern: str) -> re.Pattern[str] | None:
    try:
        # 上游 Go 正则里的 `[[` 等写法在 Python 下会触发 FutureWarning；
        # 这些是外部规则噪声，编译结果仍然正确，这里静默。
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            return re.compile(pattern)
    except re.error:
        return None


def _allowlist_parts(
    entries: Iterable[Mapping[str, Any]],
) -> tuple[tuple[re.Pattern[str], ...], tuple[re.Pattern[str], ...], frozenset[str]]:
    """拆分豁免表为「值豁免 / 整段豁免 / 停用词」，跳过无法忠实执行的条目。"""

    secret: list[re.Pattern[str]] = []
    match: list[re.Pattern[str]] = []
    stopwords: set[str] = set()
    for entry in entries:
        stopwords.update(
            str(word).lower()
            for word in entry.get("stopwords", ())
            if isinstance(word, str) and word
        )
        if str(entry.get("condition", "")).strip().lower() == _AND_CONDITION:
            # AND 需要同时满足 paths / regexes 等全部条件；缺文件路径上下文，保守跳过。
            continue
        target = str(entry.get("regexTarget") or "secret").strip().lower()
        if target == "line":
            # 行级豁免需要行上下文，纯文本链路不适用。
            continue
        bucket = match if target == "match" else secret
        for raw_pattern in entry.get("regexes", ()):
            if not isinstance(raw_pattern, str):
                continue
            compiled = _compile(normalize_gitleaks_pattern(raw_pattern))
            if compiled is not None:
                bucket.append(compiled)
    return tuple(secret), tuple(match), frozenset(stopwords)


def _as_mappings(value: Any) -> list[Mapping[str, Any]]:
    if isinstance(value, Mapping):
        return [value]
    if isinstance(value, (list, tuple)):
        return [item for item in value if isinstance(item, Mapping)]
    return []
