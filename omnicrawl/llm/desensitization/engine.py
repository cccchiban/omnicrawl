"""匹配引擎：结构感知（键名规则）为主、熵检测兜底为辅，产出待脱敏值并完成替换。

结构感知层（设计稿 §5.1）：已解析结构（工具调用参数等 dict）递归匹配，以及
文本中的可解析片段（``.env`` 赋值 / ``key: value`` / JSON 字符串值）。
值类型规则层（§5.3）：无键名但形态确定的敏感值（PEM / 连接串 / 邮箱 / 银行卡 /
IP / URL / MAC / 车牌 / gitleaks），由 ``rules.py`` / ``gitleaks.py`` 提供规则。
熵检测兜底（§5.2）：对无键名、无法结构化的「裸值」按长度 / 字符类混合 / 香农熵
判定，形态白名单优先跳过（宁少勿滥）。纯字母 / 纯数字单类令牌在对应开关开启时
按长度直接判定（§5.2 扩展）。命中值替换为占位符；豁免表优先。
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, replace
from functools import lru_cache
from typing import TYPE_CHECKING, Any, Iterable

from .plan_cache import STAGE_COUNTER_FIELDS, MaskPlanBuilder, MaskPlanCache
from .registry import (
    PLACEHOLDER_PATTERN,
    DesensitizationStats,
    PlaceholderCycle,
    format_placeholder,
)
from .rules import PatternRule, scan_pattern_rules, shannon_entropy_bits

if TYPE_CHECKING:  # 避免 engine ↔ ner 顶层循环依赖；NER 为可选层。
    from .ner import NerLayer

# 种子与 omnicrawl/mcp/security.py::_SENSITIVE_FIELD_NAMES 保持同步（设计稿 §5.1）；
# llm 层不反向依赖 mcp 包，故声明为独立集合，由测试守护两者一致。
SENSITIVE_KEY_WORDS = frozenset(
    {
        "api_key",
        "apikey",
        "access_key",
        "secret_key",
        "authorization",
        "cookie",
        "password",
        "secret",
        "token",
        "access_token",
        "refresh_token",
        "id_token",
    }
)

# 中英文常见键扩展（设计稿 §5.1）。
EXTRA_SENSITIVE_KEY_WORDS = frozenset(
    {
        "passwd",
        "pwd",
        "credential",
        "credentials",
        "private_key",
        "session",
        "csrf",
    }
)

_CN_SENSITIVE_WORDS = ("密码", "密钥", "令牌", "身份证", "手机号", "银行卡", "口令")

DEFAULT_EXEMPT_KEY_WORDS = frozenset({"public_key", "example"})

_KEY_SEPARATOR_PATTERN = re.compile(r"[\s\-]+")
_UNDERSCORE_RUN_PATTERN = re.compile(r"_+")

# .env / shell 风格赋值：行首 KEY=VALUE（支持 export 前缀与引号包裹）。
_ENV_ASSIGNMENT_PATTERN = re.compile(
    r"(?m)^(\s*(?:export\s+)?)([A-Za-z_][A-Za-z0-9_]{0,63})(\s*=\s*)(\S.*)$"
)
# key: value（YAML / TOML 行内片段）；冒号后要求空白，避免误伤 URL 等形态。
_KV_ASSIGNMENT_PATTERN = re.compile(
    r"(?m)^(\s*)([A-Za-z_][A-Za-z0-9_.\-]{0,63})(\s*:\s+)(\S.*)$"
)
# JSON 字符串值对（含嵌套与多行文本中的片段）。
_JSON_STRING_PAIR_PATTERN = re.compile(
    r'"(?P<key>(?:[^"\\]|\\.){1,80})"\s*:\s*"(?P<value>(?:[^"\\]|\\.)*)"'
)


def normalize_key(key: str) -> str:
    """键名归一：大小写不敏感、`-` / 空格归一为 `_`、折叠重复下划线（§5.1）。"""

    normalized = _KEY_SEPARATOR_PATTERN.sub("_", key.strip().casefold())
    return _UNDERSCORE_RUN_PATTERN.sub("_", normalized).strip("_")


def _normalize_keys(keys: Iterable[str]) -> frozenset[str]:
    return frozenset(value for value in (normalize_key(str(item)) for item in keys) if value)


def _matches_word(word: str, words: frozenset[str]) -> bool:
    """匹配单个词（含简单复数：tokens → token）。"""

    if word in words:
        return True
    return word.endswith("s") and word[:-1] in words


class SensitiveMatcher:
    """键名匹配：归一化全名 / `_` 分段命中 / 中文敏感词子串；豁免表优先。"""

    def __init__(
        self,
        *,
        extra_keys: Iterable[str] = (),
        exempt_keys: Iterable[str] = (),
    ) -> None:
        self._sensitive = (
            SENSITIVE_KEY_WORDS | EXTRA_SENSITIVE_KEY_WORDS | _normalize_keys(extra_keys)
        )
        self._exempt = DEFAULT_EXEMPT_KEY_WORDS | _normalize_keys(exempt_keys)

    def is_sensitive(self, key: str) -> bool:
        """判断键名是否命中敏感规则（豁免优先于命中）。"""

        normalized = normalize_key(key)
        if not normalized:
            return False
        parts = normalized.split("_")
        if normalized in self._exempt or any(
            _matches_word(part, self._exempt) for part in parts
        ):
            return False
        if normalized in self._sensitive or any(
            _matches_word(part, self._sensitive) for part in parts
        ):
            return True
        return any(word in normalized for word in _CN_SENSITIVE_WORDS)


def should_skip_value(value: str) -> bool:
    """跳过无需脱敏的值：空串、已有脱敏串（***）、已是占位符样式（§5.1）。"""

    if not value or not value.strip():
        return True
    if not value.strip().strip("*"):
        return True
    # 只跳过「整串恰为一个占位符」的值：值里嵌有占位符样式文本时其余部分仍须继续脱敏，
    # 否则同一条值里的真实秘密会随整串一起出网（§5.1）。
    return PLACEHOLDER_PATTERN.fullmatch(value.strip()) is not None


# ── 熵检测兜底（设计稿 §5.2） ────────────────────────────────────────────

# 候选值两侧的标点剥离集：只替换值本体，标点与空白保留在原文中。
ENTROPY_TOKEN_STRIP_CHARS = "\"'`()[]{}<>,;!?*.:"

# 分段词形的段长阈值：各段均短于该长度的 `a_b-c` 串按「词形」跳过（代码标识符 /
# 组合词），保证 `utf8_encode_value_longer` 类高熵词形不误报。
_ENTROPY_SEGMENT_MAX_LENGTH = 10

# 纯字母词的词段边界（camelCase / PascalCase / 下划线）与段长区间：单类开关开启时，
# 普通类型名 / 函数名 / 单词按词形跳过，只有切段后形态不像词的随机串仍进候选（§5.2 扩展）。
_WORD_BOUNDARY_PATTERN = re.compile(
    r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])|_+"
)
_WORD_SEGMENT_MIN_LENGTH = 2
_WORD_SEGMENT_MAX_LENGTH = 20
# 词段须含元音：随机串的段常为无元音辅音簇（`Ghb` / `Zkq`）。
_VOWEL_CHARS = frozenset("aeiou")

_UUID_FULL_PATTERN = re.compile(
    r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
)
# 全十六进制（含纯数字）：摘要 / 提交哈希 / 编号等，按哈希类跳过。
_HEX_FULL_PATTERN = re.compile(r"[0-9a-fA-F]+")
# 十六进制 + 冒号：MAC / IPv6 地址等形态。
_HEX_COLON_FULL_PATTERN = re.compile(r"[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{1,4}){2,}")
# 前缀哈希：sha256:… / md5:… 等摘要标记。
_PREFIXED_HEX_FULL_PATTERN = re.compile(r"[A-Za-z][A-Za-z0-9_]{0,15}:[0-9a-fA-F]{16,}")
_SEGMENTED_FULL_PATTERN = re.compile(r"[A-Za-z0-9]+(?:[_\-][A-Za-z0-9]+)+")
_SEGMENT_SPLIT_PATTERN = re.compile(r"[_\-]")
_VERSION_FULL_PATTERN = re.compile(r"v?\d+(\.\d+){1,3}([-+][0-9A-Za-z.+\-]+)?")
_DATETIME_FULL_PATTERN = re.compile(
    r"\d{4}-\d{2}-\d{2}([T ]\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?)?"
)
_IDENTIFIER_FULL_PATTERN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# 单类令牌（纯字母 / 纯数字）：对应开关开启后长度达标即视为候选（§5.2 扩展）。
_PURE_LETTERS_FULL_PATTERN = re.compile(r"[A-Za-z]+")
_PURE_DIGITS_FULL_PATTERN = re.compile(r"[0-9]+")

# 代码结构标点：token 含任一即按代码 / 序列化片段跳过（普通代码不误伤）。
_CODE_PUNCTUATION_CHARS = frozenset("()[]{}'\";,<>`")
# 命名链（蛇形 / 点分 / 命名空间路径 / 枚举）：各段短于该长度或为无数字词时跳过。
_NAMING_SEGMENT_MAX_LENGTH = 18
_NAMING_CHAIN_FULL_PATTERN = re.compile(r"[A-Za-z0-9]+(?:[_.\-:|]+[A-Za-z0-9]+)+[_.\-:|]*")
_NAMING_SEGMENT_SPLIT_PATTERN = re.compile(r"[_.\-:|]+")


def is_entropy_exempt(token: str) -> bool:
    """熵兜底的形态白名单（§5.2）：哈希 / 地址 / 路径 / 词形标识符等一律跳过。"""

    # 变量 / 装饰器 / 标签 / 下划线前导：按剩余词形判定，避免误伤 shell / CSS / 装饰器。
    token = token.lstrip("$@#_") or token
    if "esensitized" in token.lower():
        return True
    if _UUID_FULL_PATTERN.fullmatch(token):
        return True
    if _HEX_FULL_PATTERN.fullmatch(token):
        return True
    if _HEX_COLON_FULL_PATTERN.fullmatch(token):
        return True
    if _PREFIXED_HEX_FULL_PATTERN.fullmatch(token):
        return True
    if _VERSION_FULL_PATTERN.fullmatch(token):
        return True
    if _DATETIME_FULL_PATTERN.fullmatch(token):
        return True
    if "/" in token or "\\" in token:
        # URL / 文件路径（含 Windows 路径）统一跳过。
        return True
    if not _CODE_PUNCTUATION_CHARS.isdisjoint(token):
        # 代码 / 序列化片段（函数调用、字面量、列表等）：普通代码不误伤。
        return True
    if _is_naming_chain(token):
        # 命名链（self.a.b、命名空间路径等）：按词形命名跳过。
        return True
    if "=" in token.rstrip("="):
        # 非尾部等号（赋值 / 查询片段）；base64 的 padding 等号在尾部，不受影响。
        return True
    if token.endswith("=") and _is_naming_chain(token.rstrip("=")):
        # 形如 `name=` 的命名片段（f-string / 参数名残留）。
        return True
    if _is_word_like_segmented(token):
        return True
    if _IDENTIFIER_FULL_PATTERN.fullmatch(token) and not any(
        character.isdigit() for character in token
    ):
        # 纯词形标识符（无数字的 snake_case / camelCase）。
        return True
    return False


def _is_word_like_segmented(token: str) -> bool:
    """分段词形：`a_b` / `a-b` 且所有分段短于阈值（代码标识符 / 组合词）。"""

    if _SEGMENTED_FULL_PATTERN.fullmatch(token) is None:
        return False
    segments = [segment for segment in _SEGMENT_SPLIT_PATTERN.split(token) if segment]
    return max((len(segment) for segment in segments), default=0) < _ENTROPY_SEGMENT_MAX_LENGTH


def _is_naming_chain(token: str) -> bool:
    """命名链：`a.b` / `a_b_c` / `hook:topic` 等（蛇形 / 点分 / 命名空间 / 枚举）。

    每段为无数字词或短于阈值时按命名跳过（普通代码不误伤）；秘密的随机段
    通常含数字且较长（JWT 的 base64url 段普遍 ≥ 20），仍会被检测。
    """

    if _NAMING_CHAIN_FULL_PATTERN.fullmatch(token) is None:
        return False
    segments = [
        segment for segment in _NAMING_SEGMENT_SPLIT_PATTERN.split(token) if segment
    ]
    return all(
        len(segment) < _NAMING_SEGMENT_MAX_LENGTH
        or not any(character.isdigit() for character in segment)
        for segment in segments
    )


def is_word_shaped_letters(token: str) -> bool:
    """纯字母 token 是否为词形标识符（camelCase / PascalCase / 下划线分词 / 单词）。

    随机串在大小写边界上切出的段普遍只有 1–2 字符、整段远超词长、或段内普遍凑不出元音；
    词形标识符的段长都落在词长区间内，且至少半数段含元音（缩写段如 `Http` / `Rpc` 允许
    少数无元音）。命中词形即按代码标识符跳过，避免类型名、函数名与普通单词被当作秘密。
    """

    segments = [segment for segment in _WORD_BOUNDARY_PATTERN.split(token) if segment]
    if not segments:
        return False
    if any(
        len(segment) < _WORD_SEGMENT_MIN_LENGTH or len(segment) > _WORD_SEGMENT_MAX_LENGTH
        for segment in segments
    ):
        return False
    vowel_segments = sum(
        1
        for segment in segments
        if any(character in _VOWEL_CHARS for character in segment.casefold())
    )
    return vowel_segments * 2 >= len(segments)


def is_entropy_candidate(
    token: str,
    *,
    min_length: int,
    min_bits: float,
    pure_letters: bool = False,
    pure_digits: bool = False,
) -> bool:
    """熵兜底候选判定：长度 + 单类开关 + 形态白名单 + 字符类混合 + 香农熵（§5.2）。

    纯字母 / 纯数字（``pure_letters`` / ``pure_digits`` 开启时）不受字符类混合与
    熵阈值约束：长度达标即视为候选；仍跳过十六进制字母串（哈希 / 编号语义）、
    含 ``Desensitized`` 字样的占位符残留与词形标识符（类型名 / 函数名 / 单词）。
    """

    if len(token) < min_length:
        return False
    if pure_digits and _PURE_DIGITS_FULL_PATTERN.fullmatch(token):
        return True
    if pure_letters and _PURE_LETTERS_FULL_PATTERN.fullmatch(token):
        if _HEX_FULL_PATTERN.fullmatch(token) or "esensitized" in token.lower():
            return False
        if is_word_shaped_letters(token):
            return False
        return True
    if is_entropy_exempt(token):
        return False
    has_lower = any("a" <= character <= "z" for character in token)
    has_upper = any("A" <= character <= "Z" for character in token)
    has_digit = any("0" <= character <= "9" for character in token)
    has_symbol = any(not character.isalnum() for character in token)
    if not (has_lower or has_upper):
        return False
    if not (has_digit or has_symbol):
        return False
    if sum((has_lower, has_upper, has_digit, has_symbol)) < 2:
        return False
    return shannon_entropy_bits(token) >= min_bits


@lru_cache(maxsize=8)
def _entropy_scan_pattern(min_length: int) -> re.Pattern:
    """按长度下限生成候选扫描正则（可打印 ASCII 连续段）。"""

    return re.compile(rf"[!-~]{{{max(1, int(min_length))},}}")


def find_entropy_spans(
    text: str,
    *,
    min_length: int,
    min_bits: float,
    pure_letters: bool = False,
    pure_digits: bool = False,
) -> list[tuple[int, int]]:
    """扫描文本并返回需要熵脱敏的 token 区间（半开区间，标点保留在区间外）。"""

    spans: list[tuple[int, int]] = []
    if not text:
        return spans
    for match in _entropy_scan_pattern(min_length).finditer(text):
        start, end = match.start(), match.end()
        while start < end and text[start] in ENTROPY_TOKEN_STRIP_CHARS:
            start += 1
        while end > start and text[end - 1] in ENTROPY_TOKEN_STRIP_CHARS:
            end -= 1
        token = text[start:end]
        if not token:
            continue
        if is_entropy_candidate(
            token,
            min_length=min_length,
            min_bits=min_bits,
            pure_letters=pure_letters,
            pure_digits=pure_digits,
        ):
            spans.append((start, end))
    return spans


@dataclass
class MaskContext:
    """一次出站屏蔽的上下文：匹配器 + 周期注册表 + 审计计数 + 熵兜底参数。"""

    matcher: SensitiveMatcher
    cycle: PlaceholderCycle
    stats: DesensitizationStats
    # 默认值与 omnicrawl/config/features/desensitization.py 保持一致。
    entropy_enabled: bool = True
    entropy_min_length: int = 20
    entropy_min_bits: float = 3.5
    entropy_pure_letters: bool = False
    entropy_pure_digits: bool = False
    # 值类型规则层（PEM / 连接串 / 邮箱 / 银行卡 / IP / URL / MAC / 车牌 / gitleaks）。
    # 为空时不启用规则层（保持仅结构 + 熵的旧行为）。
    pattern_rules: tuple[PatternRule, ...] = ()
    # NER 兜底层（BiLSTM-CRF，可选依赖 torch）：结构 / 规则 / 熵之外的最后一道语义兜底。
    # 为 None 时本层不参与，保持既有行为。
    ner_layer: "NerLayer | None" = None
    # 屏蔽计划缓存与记录器（运行时实例级）：命中时按计划重放，未启用时为空。
    plan_cache: "MaskPlanCache | None" = None
    plan_builder: "MaskPlanBuilder | None" = None

    def begin_stage(self, counter: str = "") -> None:
        """开始一个新的计划阶段；未启用计划缓存时为空操作。"""

        if self.plan_builder is not None:
            self.plan_builder.begin_stage(counter)

    def placeholder_and_seq(self, value: str) -> tuple[str | None, int | None]:
        """登记值并返回占位符与序号（被跳过的值返回 ``(None, None)``）。"""

        if should_skip_value(value):
            self.stats.skipped_values += 1
            return None, None
        seq, created = self.cycle.seq_for_value(value)
        if created:
            self.stats.values_masked += 1
        if self.plan_builder is not None:
            self.plan_builder.note_registration()
        return format_placeholder(seq), seq

    def placeholder_at(
        self, text: str, start: int, end: int, *, value: str | None = None
    ) -> str | None:
        """把 ``text[start:end]`` 替换为占位符，并记录该区间（启用计划缓存时）。

        ``value`` 用于「值与文本切片不一致」的类型（JSON 转义）：不一致时不记录区间，
        该文本的计划因「记录数 != 分配数」判为不可重放，只损失速度、不改语义。
        """

        sliced = text[start:end]
        target = sliced if value is None else value
        placeholder, seq = self.placeholder_and_seq(target)
        if placeholder is None:
            return None
        if self.plan_builder is None:
            return placeholder
        if seq is not None and (value is None or value == sliced):
            self.plan_builder.record(start, end, seq)
        return placeholder

    def recording_copy(self):
        """返回带计划记录器的上下文副本；未启用计划缓存时返回自身。"""

        if self.plan_cache is None:
            return self
        return replace(self, plan_builder=MaskPlanBuilder())

    def store_plan(self, text: str) -> None:
        """把本次文本屏蔽的匹配计划写入缓存；计划不完整或未启用缓存时跳过。"""

        builder = self.plan_builder
        if builder is None or self.plan_cache is None:
            return
        plan = builder.build()
        if plan is not None:
            self.plan_cache.put(text, plan)

    def replay_plan(self, text: str) -> str | None:
        """按缓存的匹配计划重建屏蔽结果；未启用缓存 / 未命中 / 计划失效时返回 None。

        计划内的区间按各自阶段的输入文本记录，逐阶段重放即可复现原坐标；每个区间都
        从当前文本重新取值并走 ``placeholder_for``，因此当前周期照样登记原文（可还原）。
        """

        cache = self.plan_cache
        if cache is None:
            return None
        plan = cache.get(text)
        if plan is None:
            return None
        result = text
        for stage in plan.stages:
            for span in reversed(stage):
                value = result[span.start : span.end]
                before = self.stats.values_masked
                placeholder = self.placeholder_for(value)
                if placeholder is None or placeholder != format_placeholder(span.seq):
                    cache.note_invalid()
                    return None
                field = STAGE_COUNTER_FIELDS.get(span.counter)
                if field and self.stats.values_masked > before:
                    setattr(self.stats, field, getattr(self.stats, field) + 1)
                result = result[: span.start] + placeholder + result[span.end :]
        return result

    def placeholder_for(self, value: str) -> str | None:
        """值 → 占位符；被跳过（空串 / 已脱敏 / 占位符样式）时返回 None。"""

        if should_skip_value(value):
            self.stats.skipped_values += 1
            return None
        seq, created = self.cycle.seq_for_value(value)
        if created:
            self.stats.values_masked += 1
        return format_placeholder(seq)


def mask_structured_value(value: Any, ctx: MaskContext) -> Any:
    """递归处理 dict / list：敏感键（或其子树）下的字符串叶子替换为占位符。

    键、结构与非字符串标量不动；非敏感键下的字符串同样做文本级匹配
    （结构 + 熵兜底），覆盖参数内自由文本，避免长历史回程时的原文回声。
    """

    if isinstance(value, dict):
        result: dict[Any, Any] = {}
        for key, item in value.items():
            if isinstance(key, str) and ctx.matcher.is_sensitive(key):
                result[key] = _mask_sensitive_subtree(item, ctx)
            else:
                result[key] = mask_structured_value(item, ctx)
        return result
    if isinstance(value, list):
        return [mask_structured_value(item, ctx) for item in value]
    if isinstance(value, str):
        return mask_text(value, ctx)
    return value


def _mask_sensitive_subtree(value: Any, ctx: MaskContext) -> Any:
    """敏感键之下的子树：所有字符串叶子都视为值并脱敏。"""

    if isinstance(value, str):
        placeholder = ctx.placeholder_for(value)
        return placeholder if placeholder is not None else value
    if isinstance(value, dict):
        return {key: _mask_sensitive_subtree(item, ctx) for key, item in value.items()}
    if isinstance(value, list):
        return [_mask_sensitive_subtree(item, ctx) for item in value]
    return value


def mask_text(text: str, ctx: MaskContext) -> str:
    """文本匹配：结构感知 → 值类型规则层 → 熵兜底 → NER 语义兜底。

    优先级遵循设计稿 §5.4：豁免表 → 结构命中 → 值类型规则 → 熵兜底 → NER 兜底；
    同一位置只登记一次（最先命中的层级生效，规则层内部按规则顺序与重叠去重）。

    NER 层接收的是**前几层处理后的文本**（命中值已是占位符），因此不会被邮箱 /
    手机号等其它类型数据干扰，也不会重复登记已脱敏的值。
    """

    if not text:
        return text
    replayed = ctx.replay_plan(text)
    if replayed is not None:
        return replayed
    local = ctx.recording_copy()
    local.begin_stage("")
    masked = _ENV_ASSIGNMENT_PATTERN.sub(lambda match: _replace_assignment(match, local), text)
    local.begin_stage("")
    masked = _KV_ASSIGNMENT_PATTERN.sub(lambda match: _replace_assignment(match, local), masked)
    local.begin_stage("")
    masked = _JSON_STRING_PAIR_PATTERN.sub(lambda match: _replace_json_pair(match, local), masked)
    if local.pattern_rules:
        masked = _mask_pattern_text(masked, local)
    if local.entropy_enabled:
        masked = _mask_entropy_text(masked, local)
    if local.ner_layer is not None:
        masked = _mask_ner_text(masked, local)
    local.store_plan(text)
    return masked


def _mask_pattern_text(text: str, ctx: MaskContext) -> str:
    """值类型规则层：命中区间替换为占位符（重叠区间由先命中的规则占位）。"""

    matches = scan_pattern_rules(text, ctx.pattern_rules)
    if not matches:
        return text
    ctx.begin_stage("rules")
    result = text
    for match in reversed(matches):
        before = ctx.stats.values_masked
        placeholder = ctx.placeholder_at(text, match.start, match.end, value=match.value)
        if placeholder is None:
            continue
        if ctx.stats.values_masked > before:
            ctx.stats.rules_masked += 1
        result = result[: match.start] + placeholder + result[match.end :]
    return result


def _mask_entropy_text(text: str, ctx: MaskContext) -> str:
    """对结构层未覆盖的剩余文本做熵兜底；已生成的占位符不参与候选（跳过）。"""

    spans = find_entropy_spans(
        text,
        min_length=ctx.entropy_min_length,
        min_bits=ctx.entropy_min_bits,
        pure_letters=ctx.entropy_pure_letters,
        pure_digits=ctx.entropy_pure_digits,
    )
    if not spans:
        return text
    ctx.begin_stage("entropy")
    result = text
    for start, end in reversed(spans):
        before = ctx.stats.values_masked
        placeholder = ctx.placeholder_at(text, start, end)
        if placeholder is None:
            continue
        if ctx.stats.values_masked > before:
            ctx.stats.entropy_masked += 1
        result = result[:start] + placeholder + result[end:]
    return result


def _mask_ner_text(text: str, ctx: MaskContext) -> str:
    """NER 兜底层：把语义实体（人名 / 地名 / 机构名）替换为占位符。

    与前几层一样只替换值本体；区间由 ``NerLayer`` 过滤（实体类型 / 含汉字 / 最小
    长度 / 不与既有占位符重叠）。登记走 ``placeholder_for``，还原沿用标准机制。
    """

    layer = ctx.ner_layer
    spans = layer.find_spans(text)
    if not spans:
        return text
    ctx.begin_stage("ner")
    result = text
    for start, end in reversed(spans):
        before = ctx.stats.values_masked
        placeholder = ctx.placeholder_at(text, start, end)
        if placeholder is None:
            continue
        if ctx.stats.values_masked > before:
            ctx.stats.ner_masked += 1
        result = result[:start] + placeholder + result[end:]
    return result


def _replace_assignment(match: re.Match, ctx: MaskContext) -> str:
    if not ctx.matcher.is_sensitive(match.group(2)):
        return match.group(0)
    raw_value = match.group(4)
<<<<<<< ours
<<<<<<< ours
    body = _assignment_value_body(raw_value)
    if body is None:
        return match.group(0)
    body_start, body_end = body
    placeholder = ctx.placeholder_for(raw_value[body_start:body_end])
    if placeholder is None:
=======
=======
>>>>>>> theirs
    masked = _mask_assignment_value(raw_value, ctx, text=match.string, start=match.start(4))
    if masked == raw_value:
>>>>>>> theirs
        return match.group(0)
    masked_value = raw_value[:body_start] + placeholder + raw_value[body_end:]
    start, end = match.span(4)
    return match.string[match.start() : start] + masked_value + match.string[end : match.end()]


<<<<<<< ours
<<<<<<< ours
def _assignment_value_body(raw_value: str) -> tuple[int, int] | None:
    """定位赋值右侧的值本体区间（相对 ``raw_value``）；形态不明确时返回 None。

    带引号时只取**配对引号内部**：`KEY = "v"  # 注释` 只替换 `v`，注释保留；
    `KEY = "` 这类被换行截断的片段（引号未配对）不当作值，避免连引号和后续文本一起吞掉。
    """

    stripped = raw_value.rstrip()
    if not stripped:
        return None
    quote = stripped[0]
    if quote in "\"'":
        closing = stripped.find(quote, 1)
        if closing < 0:
            return None
        return 1, closing
    body_end = _inline_comment_start(stripped)
    if body_end == 0:
        return None
    return 0, body_end


def _inline_comment_start(text: str) -> int:
    """无引号值的行内注释起点：``#`` 位于串首或前一个
    字符为空白时才算注释，所以 `v#frag` 与值中的普通 `#` 仍算值本体。

    返回注释前的值尾（已去掉尾随空白）；无注释时返回 ``len(text)``，值本体
    为空（如 `KEY = # 说明`）时返回 0，由调用方按「形态不明确」处理。
    """

    for index, character in enumerate(text):
        if character == "#" and (index == 0 or text[index - 1].isspace()):
            return len(text[:index].rstrip())
    return len(text)
=======
=======
>>>>>>> theirs
def _mask_assignment_value(
    raw_value: str,
    ctx: MaskContext,
    *,
    text: str,
    start: int,
) -> str:
    """处理赋值右侧值区间：保留引号与行尾空白，只替换值本体。"""

    stripped = raw_value.rstrip()
    trailing = raw_value[len(stripped) :]
    if len(stripped) >= 2 and stripped[0] == stripped[-1] and stripped[0] in "\"'":
        inner = stripped[1:-1]
        placeholder = ctx.placeholder_at(text, start + 1, start + len(stripped) - 1, value=inner)
        if placeholder is None:
            return raw_value
        return f"{stripped[0]}{placeholder}{stripped[-1]}{trailing}"
    placeholder = ctx.placeholder_at(text, start, start + len(stripped), value=stripped)
    if placeholder is None:
        return raw_value
    return f"{placeholder}{trailing}"
>>>>>>> theirs


def _replace_json_pair(match: re.Match, ctx: MaskContext) -> str:
    if not ctx.matcher.is_sensitive(_try_unescape_json_string(match.group("key"))):
        return match.group(0)
    raw_value = match.group("value")
    start, end = match.span("value")
    value = _try_unescape_json_string(raw_value)
    placeholder = ctx.placeholder_at(match.string, start, end, value=value)
    if placeholder is None:
        return match.group(0)
    return match.string[match.start() : start] + placeholder + match.string[end : match.end()]


def _try_unescape_json_string(raw: str) -> str:
    try:
        value = json.loads(f'"{raw}"')
    except Exception:
        return raw
    return value if isinstance(value, str) else raw


__all__ = [
    "DEFAULT_EXEMPT_KEY_WORDS",
    "ENTROPY_TOKEN_STRIP_CHARS",
    "EXTRA_SENSITIVE_KEY_WORDS",
    "MaskContext",
    "SENSITIVE_KEY_WORDS",
    "SensitiveMatcher",
    "find_entropy_spans",
    "is_entropy_candidate",
    "is_entropy_exempt",
    "mask_structured_value",
    "mask_text",
    "normalize_key",
    "shannon_entropy_bits",
    "should_skip_value",
]
