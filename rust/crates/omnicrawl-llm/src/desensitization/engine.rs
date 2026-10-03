//! 匹配引擎：结构感知（键名规则）为主、值类型规则层与熵兜底为辅，产出待脱敏值并完成替换。
//!
//! 语义基准是 Python `omnicrawl/llm/desensitization/engine.py`。三层顺序与设计稿一致：
//! 结构层（`.env` 赋值 / `key: value` / JSON 字符串值 / 已解析结构体的敏感键）→ 值类型规则层
//! （`rules.rs` 的 11 条规则）→ 熵兜底（长度 / 字符类混合 / 香农熵，形态白名单优先跳过）。
//! 命中值交给 `MaskContext::placeholder_for` 分配占位符；豁免表优先于命中。
//!
//! 值类型规则层是手写等价实现：Python 侧的每条正则在这里都有一份等价实现（行首锚定的赋值、
//! 转义感知的 JSON 串、以及熵兜底的一整组形态判定）。
//!
//! NER 语义兜底层（`ner.rs`）是**最后一层**：由调用方注入 [`NerLayer`]（未启用时为 `None`），
//! 命中区间与其余层一样走 `placeholder_for`，并计入 `ner_masked` / 计划缓存的 `ner` 阶段。
//!
//! 屏蔽计划缓存（Python `plan_cache.py`）已接线：`mask_text` 先按缓存计划重放，未命中时
//! 用记录器收集各阶段区间、跑完再把计划写回缓存。计划不含原文，只记区间 + 稳定序号。

use std::collections::HashSet;
use std::sync::Arc;

use omnicrawl_protocol::ProviderWarning;
use serde_json::Value;

use super::ner::NerLayer;
use super::plan_cache::{stage_counter_field, MaskPlanBuilder, MaskPlanCache, PlanSpan};
use super::rules::{scan_pattern_rules, shannon_entropy_bits, PatternRule};
use super::{match_placeholder, DesensitizationStats, PlaceholderCycle, PLACEHOLDER_MARKER};

// ── 键名规则（与 omnicrawl/mcp/security.py::_SENSITIVE_FIELD_NAMES 同步） ───

pub const SENSITIVE_KEY_WORDS: [&str; 12] = [
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
];

pub const EXTRA_SENSITIVE_KEY_WORDS: [&str; 7] = [
    "passwd",
    "pwd",
    "credential",
    "credentials",
    "private_key",
    "session",
    "csrf",
];

const CN_SENSITIVE_WORDS: [&str; 7] =
    ["密码", "密钥", "令牌", "身份证", "手机号", "银行卡", "口令"];

pub const DEFAULT_EXEMPT_KEY_WORDS: [&str; 2] = ["public_key", "example"];

/// 熵兜底候选两侧剥离的标点：只替换值本体，标点与空白留在原文。
pub const ENTROPY_TOKEN_STRIP_CHARS: &str = "\"'`()[]{}<>,;!?*.:";

const ENTROPY_SEGMENT_MAX_LENGTH: usize = 10;
const WORD_SEGMENT_MIN_LENGTH: usize = 2;
const WORD_SEGMENT_MAX_LENGTH: usize = 20;
const NAMING_SEGMENT_MAX_LENGTH: usize = 18;
const CODE_PUNCTUATION_CHARS: &str = "()[]{}'\";,<>`";

/// 键名归一：去空白、大小写不敏感、`-` / 空白归一为 `_`、折叠重复下划线。
pub fn normalize_key(key: &str) -> String {
    let mut expanded = String::with_capacity(key.len());
    for character in key.trim().chars() {
        for lowered in character.to_lowercase() {
            if lowered.is_whitespace() || lowered == '-' {
                expanded.push('_');
            } else {
                expanded.push(lowered);
            }
        }
    }
    let mut normalized = String::with_capacity(expanded.len());
    for character in expanded.chars() {
        if character == '_' {
            if !normalized.is_empty() && !normalized.ends_with('_') {
                normalized.push('_');
            }
        } else {
            normalized.push(character);
        }
    }
    normalized.trim_matches('_').to_string()
}

fn normalize_keys<'a, I>(keys: I) -> HashSet<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut normalized = HashSet::new();
    for key in keys {
        let value = normalize_key(key);
        if !value.is_empty() {
            normalized.insert(value);
        }
    }
    normalized
}

/// 匹配单个词（含简单复数：`tokens` → `token`）。
fn matches_word(word: &str, words: &HashSet<String>) -> bool {
    if words.contains(word) {
        return true;
    }
    if let Some(stripped) = word.strip_suffix('s') {
        return words.contains(stripped);
    }
    false
}

/// 键名匹配：归一化全名 / `_` 分段命中 / 中文敏感词子串；豁免表优先。
pub struct SensitiveMatcher {
    sensitive: HashSet<String>,
    exempt: HashSet<String>,
}

impl SensitiveMatcher {
    pub fn new(extra_keys: &[String], exempt_keys: &[String]) -> Self {
        let mut sensitive: HashSet<String> = SENSITIVE_KEY_WORDS
            .iter()
            .chain(EXTRA_SENSITIVE_KEY_WORDS.iter())
            .map(|word| (*word).to_string())
            .collect();
        sensitive.extend(normalize_keys(extra_keys.iter().map(|key| key.as_str())));
        let mut exempt: HashSet<String> = DEFAULT_EXEMPT_KEY_WORDS
            .iter()
            .map(|word| (*word).to_string())
            .collect();
        exempt.extend(normalize_keys(exempt_keys.iter().map(|key| key.as_str())));
        Self { sensitive, exempt }
    }

    /// 判断键名是否命中敏感规则（豁免优先于命中）。
    pub fn is_sensitive(&self, key: &str) -> bool {
        let normalized = normalize_key(key);
        if normalized.is_empty() {
            return false;
        }
        let parts: Vec<&str> = normalized.split('_').collect();
        if self.exempt.contains(&normalized)
            || parts.iter().any(|part| matches_word(part, &self.exempt))
        {
            return false;
        }
        if self.sensitive.contains(&normalized)
            || parts.iter().any(|part| matches_word(part, &self.sensitive))
        {
            return true;
        }
        CN_SENSITIVE_WORDS
            .iter()
            .any(|word| normalized.contains(word))
    }
}

/// 跳过无需脱敏的值：空串、已脱敏串（`***`）、整串恰为一个占位符。
pub fn should_skip_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.trim_matches('*').is_empty() {
        return true;
    }
    is_whole_placeholder(trimmed)
}

fn is_whole_placeholder(text: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    match_placeholder(&characters, 0).is_some_and(|(end, _)| end == characters.len())
}

// ── 熵兜底（形态白名单 + 字符类混合 + 香农熵） ─────────────────────────────

/// 纯字母 / 纯数字 token 是否按「词形标识符」跳过。
pub fn is_word_shaped_letters(token: &str) -> bool {
    let segments = split_word_boundaries(token);
    if segments.is_empty() {
        return false;
    }
    if segments.iter().any(|segment| {
        let length = segment.chars().count();
        !(WORD_SEGMENT_MIN_LENGTH..=WORD_SEGMENT_MAX_LENGTH).contains(&length)
    }) {
        return false;
    }
    let with_vowel = segments
        .iter()
        .filter(|segment| {
            segment
                .to_lowercase()
                .chars()
                .any(|character| "aeiou".contains(character))
        })
        .count();
    with_vowel * 2 >= segments.len()
}

/// 按 camelCase / PascalCase / 下划线切段（对应 Python 的零宽断言 + `_+`）。
fn split_word_boundaries(token: &str) -> Vec<String> {
    let characters: Vec<char> = token.chars().collect();
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    for (index, character) in characters.iter().enumerate() {
        if *character == '_' {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            continue;
        }
        let previous = index.checked_sub(1).and_then(|i| characters.get(i));
        let next = characters.get(index + 1);
        let after_next = characters.get(index + 2);
        let lower_to_upper = previous.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && character.is_ascii_uppercase();
        let acronym_boundary = previous.is_some_and(|c| c.is_ascii_uppercase())
            && character.is_ascii_uppercase()
            && next.is_some_and(|c| c.is_ascii_lowercase());
        let _ = after_next;
        if (lower_to_upper || acronym_boundary) && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
        current.push(*character);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// 熵兜底的形态白名单：哈希 / 地址 / 路径 / 词形标识符等一律跳过。
pub fn is_entropy_exempt(token: &str) -> bool {
    let stripped = token.trim_start_matches(['$', '@', '#', '_']);
    let token = if stripped.is_empty() { token } else { stripped };
    if token
        .to_lowercase()
        .contains(&PLACEHOLDER_MARKER.to_lowercase())
    {
        return true;
    }
    if is_uuid(token)
        || is_hex(token)
        || is_hex_colon(token)
        || is_prefixed_hex(token)
        || is_version(token)
        || is_datetime(token)
    {
        return true;
    }
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    if token
        .chars()
        .any(|character| CODE_PUNCTUATION_CHARS.contains(character))
    {
        return true;
    }
    if is_naming_chain(token) {
        return true;
    }
    if token.trim_end_matches('=').contains('=') {
        return true;
    }
    if let Some(body) = token.strip_suffix('=') {
        if is_naming_chain(body) {
            return true;
        }
    }
    if is_word_like_segmented(token) {
        return true;
    }
    if is_identifier(token) && !token.chars().any(|character| character.is_ascii_digit()) {
        return true;
    }
    false
}

/// 熵兜底候选判定：长度 + 单类开关 + 形态白名单 + 字符类混合 + 香农熵。
pub fn is_entropy_candidate(
    token: &str,
    min_length: usize,
    min_bits: f64,
    pure_letters: bool,
    pure_digits: bool,
) -> bool {
    if token.chars().count() < min_length {
        return false;
    }
    if pure_digits && is_pure_digits(token) {
        return true;
    }
    if pure_letters && is_pure_letters(token) {
        if is_hex(token)
            || token
                .to_lowercase()
                .contains(&PLACEHOLDER_MARKER.to_lowercase())
        {
            return false;
        }
        return !is_word_shaped_letters(token);
    }
    if is_entropy_exempt(token) {
        return false;
    }
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_symbol = token.chars().any(|c| !c.is_alphanumeric());
    if !has_lower && !has_upper {
        return false;
    }
    if !has_digit && !has_symbol {
        return false;
    }
    let classes = [has_lower, has_upper, has_digit, has_symbol]
        .iter()
        .filter(|flag| **flag)
        .count();
    if classes < 2 {
        return false;
    }
    shannon_entropy_bits(token) >= min_bits
}

/// 扫描文本并返回需要熵脱敏的 token 区间（字节偏移，标点保留在区间外）。
pub fn find_entropy_spans(
    text: &str,
    min_length: usize,
    min_bits: f64,
    pure_letters: bool,
    pure_digits: bool,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    if text.is_empty() {
        return spans;
    }
    let minimum = min_length.max(1);
    let mut run_start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        let printable = character.is_ascii_graphic();
        match (printable, run_start) {
            (true, None) => run_start = Some(index),
            (false, Some(start)) => {
                collect_entropy_span(
                    text,
                    start,
                    index,
                    minimum,
                    min_bits,
                    pure_letters,
                    pure_digits,
                    &mut spans,
                );
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        collect_entropy_span(
            text,
            start,
            text.len(),
            minimum,
            min_bits,
            pure_letters,
            pure_digits,
            &mut spans,
        );
    }
    spans
}

#[allow(clippy::too_many_arguments)]
fn collect_entropy_span(
    text: &str,
    start: usize,
    end: usize,
    min_length: usize,
    min_bits: f64,
    pure_letters: bool,
    pure_digits: bool,
    spans: &mut Vec<(usize, usize)>,
) {
    if end - start < min_length {
        return;
    }
    let mut body_start = start;
    let mut body_end = end;
    while body_start < body_end {
        let character = text[body_start..].chars().next().expect("非空");
        if !ENTROPY_TOKEN_STRIP_CHARS.contains(character) {
            break;
        }
        body_start += character.len_utf8();
    }
    while body_end > body_start {
        let character = text[..body_end].chars().next_back().expect("非空");
        if !ENTROPY_TOKEN_STRIP_CHARS.contains(character) {
            break;
        }
        body_end -= character.len_utf8();
    }
    if body_end <= body_start {
        return;
    }
    let token = &text[body_start..body_end];
    if is_entropy_candidate(token, min_length, min_bits, pure_letters, pure_digits) {
        spans.push((body_start, body_end));
    }
}

// ── 屏蔽上下文与三层替换 ───────────────────────────────────────────────────

/// 一次出站屏蔽的上下文：匹配器 + 周期注册表 + 审计计数 + 熵兜底参数。
pub struct MaskContext<'a> {
    pub matcher: &'a SensitiveMatcher,
    pub cycle: &'a mut PlaceholderCycle,
    pub stats: &'a mut DesensitizationStats,
    pub entropy_enabled: bool,
    pub entropy_min_length: usize,
    pub entropy_min_bits: f64,
    pub entropy_pure_letters: bool,
    pub entropy_pure_digits: bool,
    pub pattern_rules: &'a [PatternRule],
    /// gitleaks 规则（运行时正则）：排在值类型规则之后，重叠区间由先命中者占位。
    pub gitleaks_rules: &'a [super::gitleaks::GitleaksRule],
    /// NER 语义兜底层：`None` 表示未启用（Python 侧也是可选依赖，默认关闭）。
    /// 它排在熵兜底之后，只看前几层没动过的剩余文本。
    pub ner: Option<&'a NerLayer>,
    /// 屏蔽计划缓存（运行时实例级）：`None` 等价于未接线，屏蔽路径不走计划重放与记录。
    pub plan_cache: Option<Arc<MaskPlanCache>>,
    /// 本次文本屏蔽的计划记录器；只有 [`MaskContext::recording_copy`] 产出的副本带记录器。
    pub plan_builder: Option<MaskPlanBuilder>,
}

impl MaskContext<'_> {
    /// 值 → 占位符；被跳过（空串 / 已脱敏 / 占位符样式）时返回 `None`。
    pub fn placeholder_for(&mut self, value: &str) -> Option<String> {
        if should_skip_value(value) {
            self.stats.skipped_values += 1;
            return None;
        }
        let (seq, created) = self.cycle.seq_for_value(value);
        if created {
            self.stats.values_masked += 1;
        }
        Some(super::format_placeholder(seq))
    }

    /// 登记值并返回占位符与序号（被跳过的值返回 `(None, None)`）。
    pub fn placeholder_and_seq(&mut self, value: &str) -> (Option<String>, Option<u64>) {
        if should_skip_value(value) {
            self.stats.skipped_values += 1;
            return (None, None);
        }
        let (seq, created) = self.cycle.seq_for_value(value);
        if created {
            self.stats.values_masked += 1;
        }
        if let Some(builder) = self.plan_builder.as_mut() {
            builder.note_registration();
        }
        (Some(super::format_placeholder(seq)), Some(seq))
    }

    /// 把 `text[start..end]` 替换为占位符，并记录该区间（启用计划缓存时）。
    ///
    /// `value` 用于「值与文本切片不一致」的类型（JSON 转义）：不一致时不记录区间，
    /// 该文本的计划因「记录数 != 分配数」判为不可重放，只损失速度、不改语义。
    pub fn placeholder_at(
        &mut self,
        text: &str,
        start: usize,
        end: usize,
        value: Option<&str>,
    ) -> Option<String> {
        let sliced = &text[start..end];
        let target = value.unwrap_or(sliced);
        let (placeholder, seq) = self.placeholder_and_seq(target);
        let placeholder = placeholder?;
        if let Some(builder) = self.plan_builder.as_mut() {
            if let Some(seq) = seq {
                if value.is_none() || value == Some(sliced) {
                    builder.record(start, end, seq);
                }
            }
        }
        Some(placeholder)
    }

    /// 开启一个新的计划阶段（对应一次赋值替换 / 一层规则）；未启用计划缓存时为空操作。
    pub fn begin_stage(&mut self, counter: &str) {
        if let Some(builder) = self.plan_builder.as_mut() {
            builder.begin_stage(counter);
        }
    }

    /// 返回带计划记录器的上下文副本；两侧共享匹配器、周期注册表与计数。
    ///
    /// 未启用计划缓存时副本同样没有记录器（等价于直接沿用原上下文）。
    pub fn recording_copy(&mut self) -> MaskContext<'_> {
        let cache = self.plan_cache.clone();
        let builder = cache.as_ref().map(|_| MaskPlanBuilder::new());
        MaskContext {
            matcher: self.matcher,
            cycle: &mut *self.cycle,
            stats: &mut *self.stats,
            entropy_enabled: self.entropy_enabled,
            entropy_min_length: self.entropy_min_length,
            entropy_min_bits: self.entropy_min_bits,
            entropy_pure_letters: self.entropy_pure_letters,
            entropy_pure_digits: self.entropy_pure_digits,
            pattern_rules: self.pattern_rules,
            gitleaks_rules: self.gitleaks_rules,
            ner: self.ner,
            plan_cache: cache,
            plan_builder: builder,
        }
    }

    /// 把本次文本屏蔽的匹配计划写入缓存；计划不完整或未启用缓存时跳过。
    pub fn store_plan(&mut self, text: &str) {
        let Some(cache) = self.plan_cache.clone() else {
            return;
        };
        let Some(builder) = self.plan_builder.take() else {
            return;
        };
        if let Some(plan) = builder.build() {
            cache.put(text, plan);
        }
    }

    /// 按缓存的匹配计划重建屏蔽结果；未启用缓存 / 未命中 / 计划失效时返回 `None`。
    ///
    /// 计划内的区间按各自阶段的输入文本记录，逐阶段重放即可复现原坐标；每个区间都
    /// 从当前文本重新取值并走 [`MaskContext::placeholder_for`]，因此当前周期照样登记
    /// 原文（可还原）。序号与计划不一致时按未命中收场，让调用方走完整屏蔽。
    pub fn replay_plan(&mut self, text: &str) -> Option<String> {
        let cache = self.plan_cache.clone()?;
        let plan = cache.get(text)?;
        let mut result = text.to_string();
        for (stage_index, stage) in plan.stages.iter().enumerate() {
            // 阶段内按起点降序应用：区间替换会改变长度，靠后区间的坐标只在它右侧文本未被动过时成立。
            // 规则层 / 熵兜底层 / NER 层逆序应用替换、记录顺序也是降序；结构层正序记录，
            // 靠这里排序纠正。不看记录顺序，是因为各阶段的记录方向并不一致。
            let mut ordered: Vec<&PlanSpan> = stage.iter().collect();
            ordered.sort_by(|left, right| right.start.cmp(&left.start));
            for span in ordered {
                // 区间必须先与当前文本对齐再切片；不对齐就按未命中收场。
                //
                // 为什么不只是「防御式编程」：计划与当前文本一旦错位，下标就会落在错误位置。
                // Python 侧下标是码点，偏了只会取到错值、再由下面的序号比对判为失效；
                // Rust 侧下标是字节，偏了会落在多字节字符中间 —— `panic = "abort"` 下直接
                // 干掉整个内核进程。真实现场：一段带中文地名的文本第二次屏蔽时命中计划，
                // 内核静默退出。
                if span.start > span.end
                    || span.end > result.len()
                    || !result.is_char_boundary(span.start)
                    || !result.is_char_boundary(span.end)
                {
                    report_plan_misalignment(text, stage_index, span, &result);
                    cache.note_invalid();
                    return None;
                }
                let value = result[span.start..span.end].to_string();
                let before = self.stats.values_masked;
                let placeholder = self.placeholder_for(&value);
                if placeholder.as_deref() != Some(super::format_placeholder(span.seq).as_str()) {
                    cache.note_invalid();
                    return None;
                }
                let placeholder = placeholder.expect("上面已比对过是否为 None");
                if let Some(field) = stage_counter_field(&span.counter) {
                    if self.stats.values_masked > before {
                        note_stage_counter(self.stats, field);
                    }
                }
                result.replace_range(span.start..span.end, &placeholder);
            }
        }
        Some(result)
    }
}

/// 计划与文本不对齐时的诊断：诊断输出已随环境变量开关一并移除，保留空实现与调用点。
fn report_plan_misalignment(
    _text: &str,
    _stage_index: usize,
    _span: &super::plan_cache::PlanSpan,
    _result: &str,
) {
}

/// 按阶段标签把「本周期首次登记」计入对应计数口径。
///
/// 标签表来自 [`super::plan_cache::STAGE_COUNTER_FIELDS`]（`rules` / `entropy` / `ner`），
/// 与未命中路径里各层自己的计数同义。
fn note_stage_counter(stats: &mut DesensitizationStats, field: &str) {
    match field {
        "rules_masked" => stats.rules_masked += 1,
        "entropy_masked" => stats.entropy_masked += 1,
        "ner_masked" => stats.ner_masked += 1,
        _ => {}
    }
}

/// 递归处理 dict / list：敏感键（或其子树）下的字符串叶子替换为占位符。
pub fn mask_structured_value(value: &Value, ctx: &mut MaskContext<'_>) -> Value {
    match value {
        Value::Object(entries) => {
            let mut result = serde_json::Map::new();
            for (key, item) in entries {
                let masked = if ctx.matcher.is_sensitive(key) {
                    mask_sensitive_subtree(item, ctx)
                } else {
                    mask_structured_value(item, ctx)
                };
                result.insert(key.clone(), masked);
            }
            Value::Object(result)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| mask_structured_value(item, ctx))
                .collect(),
        ),
        Value::String(text) => Value::String(mask_text(text, ctx)),
        other => other.clone(),
    }
}

/// 敏感键之下的子树：所有字符串叶子都视为值并脱敏。
fn mask_sensitive_subtree(value: &Value, ctx: &mut MaskContext<'_>) -> Value {
    match value {
        Value::String(text) => match ctx.placeholder_for(text) {
            Some(placeholder) => Value::String(placeholder),
            None => Value::String(text.clone()),
        },
        Value::Object(entries) => {
            let mut result = serde_json::Map::new();
            for (key, item) in entries {
                result.insert(key.clone(), mask_sensitive_subtree(item, ctx));
            }
            Value::Object(result)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| mask_sensitive_subtree(item, ctx))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// 文本匹配：结构感知 → 值类型规则层 → 熵兜底。
///
/// 启用计划缓存时先按缓存计划重放；未命中则用带记录器的副本跑完各层，再把本次计划写回缓存。
/// 阶段划分与 Python 侧一致（三步结构层各占一个无名阶段，规则层与熵兜底各带标签），
/// 阶段内的坐标同源，逐一重放即可复现原结果。
pub fn mask_text(text: &str, ctx: &mut MaskContext<'_>) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    if let Some(replayed) = ctx.replay_plan(text) {
        return replayed;
    }
    let mut local = ctx.recording_copy();
    local.begin_stage("");
    let mut masked = substitute_assignments(text, &mut local, AssignmentKind::Env);
    local.begin_stage("");
    masked = substitute_assignments(&masked, &mut local, AssignmentKind::KeyValue);
    local.begin_stage("");
    masked = substitute_json_pairs(&masked, &mut local);
    if !local.pattern_rules.is_empty() {
        masked = mask_pattern_text(&masked, &mut local);
    }
    if local.entropy_enabled {
        masked = mask_entropy_text(&masked, &mut local);
    }
    // 兜底层排在最后：前面的层已经替换过的区间不会再进模型（NER 只认原文片段，
    // 占位符区间由 `NerLayer::find_spans` 自己剔除）。
    if let Some(layer) = local.ner {
        masked = mask_ner_text(&masked, &mut local, layer);
    }
    local.store_plan(text);
    masked
}

/// NER 兜底层：实体区间替换为占位符（区间来自模型，已是字符偏移，这里换算成字节）。
///
/// 与其余层同一套簿记：占位符分配走 `placeholder_at`（因此也进计划缓存），
/// 新登记的值计入 `ner_masked`。
fn mask_ner_text(text: &str, ctx: &mut MaskContext<'_>, layer: &NerLayer) -> String {
    let spans = layer.find_spans(text);
    if spans.is_empty() {
        return text.to_string();
    }
    // 字符偏移 → 字节偏移：一个字符表建一次，比每个区间重扫便宜。
    let mut byte_offsets: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    byte_offsets.push(text.len());
    let byte_spans: Vec<(usize, usize)> = spans
        .iter()
        .filter_map(|(start, end)| {
            let start_byte = *byte_offsets.get(*start)?;
            let end_byte = *byte_offsets.get(*end)?;
            (end_byte > start_byte).then_some((start_byte, end_byte))
        })
        .collect();
    if byte_spans.is_empty() {
        return text.to_string();
    }
    // 重叠实体只保留先命中的一条（与规则层「重叠区间由先命中者占位」同一口径）。
    // NER 解码本身通常不产重叠；这里兼作兜底，保证「记下的计划一定能逆序原地重放」——
    // 重叠会让后面那条的坐标在替换过程中失效，Python 侧下标是码点只取错值，
    // Rust 侧是字节则会 panic。
    let mut accepted: Vec<(usize, usize)> = Vec::with_capacity(byte_spans.len());
    for (start, end) in byte_spans {
        if accepted
            .iter()
            .any(|(known_start, known_end)| start < *known_end && *known_start < end)
        {
            continue;
        }
        accepted.push((start, end));
    }
    let byte_spans = accepted;
    ctx.begin_stage("ner");
    let mut result = text.to_string();
    for (start, end) in byte_spans.iter().rev() {
        let before = ctx.stats.values_masked;
        let placeholder = match ctx.placeholder_at(text, *start, *end, None) {
            Some(placeholder) => placeholder,
            None => continue,
        };
        if ctx.stats.values_masked > before {
            ctx.stats.ner_masked += 1;
        }
        result.replace_range(*start..*end, &placeholder);
    }
    result
}

/// 值类型规则层：命中区间替换为占位符（重叠区间由先命中的规则占位）。
///
/// 值类型规则（手写匹配器）排在 gitleaks 规则之前：与 Python 的 `build_enabled_rules` 顺序一致，
/// gitleaks 命中只在不与前者重叠时才登记。
fn mask_pattern_text(text: &str, ctx: &mut MaskContext<'_>) -> String {
    let pattern_hits = scan_pattern_rules(text, ctx.pattern_rules);
    let mut hits: Vec<(usize, usize, String)> = pattern_hits
        .iter()
        .map(|hit| (hit.start, hit.end, hit.value.clone()))
        .collect();
    if !ctx.gitleaks_rules.is_empty() {
        let mut accepted: Vec<(usize, usize)> = pattern_hits
            .iter()
            .map(|hit| (hit.start, hit.end))
            .collect();
        accepted.sort_by_key(|(start, _)| *start);
        hits.extend(
            super::gitleaks::scan_gitleaks_rules(text, ctx.gitleaks_rules, &mut accepted)
                .into_iter()
                .map(|hit| (hit.start, hit.end, hit.value)),
        );
        hits.sort_by_key(|(start, _, _)| *start);
    }
    if hits.is_empty() {
        return text.to_string();
    }
    ctx.begin_stage("rules");
    let mut result = text.to_string();
    for (start, end, value) in hits.iter().rev() {
        let before = ctx.stats.values_masked;
        let placeholder = match ctx.placeholder_at(text, *start, *end, Some(value)) {
            Some(placeholder) => placeholder,
            None => continue,
        };
        if ctx.stats.values_masked > before {
            ctx.stats.rules_masked += 1;
        }
        result.replace_range(*start..*end, &placeholder);
    }
    result
}

/// 对结构层未覆盖的剩余文本做熵兜底；已生成的占位符不参与候选。
fn mask_entropy_text(text: &str, ctx: &mut MaskContext<'_>) -> String {
    let spans = find_entropy_spans(
        text,
        ctx.entropy_min_length,
        ctx.entropy_min_bits,
        ctx.entropy_pure_letters,
        ctx.entropy_pure_digits,
    );
    if spans.is_empty() {
        return text.to_string();
    }
    ctx.begin_stage("entropy");
    let mut result = text.to_string();
    for (start, end) in spans.iter().rev() {
        let before = ctx.stats.values_masked;
        let placeholder = match ctx.placeholder_at(text, *start, *end, None) {
            Some(placeholder) => placeholder,
            None => continue,
        };
        if ctx.stats.values_masked > before {
            ctx.stats.entropy_masked += 1;
        }
        result.replace_range(*start..*end, &placeholder);
    }
    result
}

// ── 结构层：行首赋值与 JSON 串 ────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssignmentKind {
    Env,
    KeyValue,
}

/// 行首赋值替换：`.env` / shell（`KEY=VALUE`）与 `key: value` 两种形态共用一套处理。
fn substitute_assignments(text: &str, ctx: &mut MaskContext<'_>, kind: AssignmentKind) -> String {
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut line_start = 0;
    loop {
        if let Some((match_start, match_end, key, raw_value)) =
            assignment_at(text, line_start, kind)
        {
            result.push_str(&text[cursor..match_start]);
            let replaced = replace_assignment(text, match_start, match_end, &key, &raw_value, ctx);
            result.push_str(&replaced);
            cursor = match_end;
            line_start = match_end;
            continue;
        }
        match text[line_start..].find('\n') {
            Some(offset) => line_start += offset + 1,
            None => break,
        }
        if line_start >= text.len() {
            break;
        }
    }
    result.push_str(&text[cursor..]);
    result
}

/// 在 `line_start` 起尝试匹配一条赋值；返回 `(整体区间, 键名, 右侧原文)`。
fn assignment_at(
    text: &str,
    line_start: usize,
    kind: AssignmentKind,
) -> Option<(usize, usize, String, String)> {
    if line_start >= text.len() {
        return None;
    }
    let characters: Vec<char> = text[line_start..].chars().collect();
    let mut index = skip_while(&characters, 0, |character| character.is_whitespace());
    if kind == AssignmentKind::Env && starts_with(&characters, index, "export") {
        let after = skip_while(&characters, index + 6, |character| {
            character.is_whitespace()
        });
        if after > index + 6 {
            index = after;
        }
    }
    let key_end = key_end(&characters, index, kind)?;
    let key: String = characters[index..key_end].iter().collect();
    let equals = skip_while(&characters, key_end, |character| character.is_whitespace());
    let separator = match kind {
        AssignmentKind::Env => characters.get(equals) == Some(&'='),
        AssignmentKind::KeyValue => characters.get(equals) == Some(&':'),
    };
    if !separator {
        return None;
    }
    let mut value_start = equals + 1;
    if kind == AssignmentKind::Env {
        value_start = skip_while(&characters, value_start, |character| {
            character.is_whitespace()
        });
    } else {
        let after = skip_while(&characters, value_start, |character| {
            character.is_whitespace()
        });
        if after == value_start {
            return None;
        }
        value_start = after;
    }
    if characters
        .get(value_start)
        .is_none_or(|character| character.is_whitespace())
    {
        return None;
    }
    let mut value_end = value_start;
    while characters
        .get(value_end)
        .is_some_and(|character| *character != '\n')
    {
        value_end += 1;
    }
    let raw_value: String = characters[value_start..value_end].iter().collect();
    let base = line_start;
    let offsets = char_offsets_of(&characters);
    Some((base + offsets[0], base + offsets[value_end], key, raw_value))
}

/// 键名：`[A-Za-z_][A-Za-z0-9_]{0,63}`（KV 形态额外允许 `.` 与 `-`）。
fn key_end(characters: &[char], start: usize, kind: AssignmentKind) -> Option<usize> {
    let first = characters.get(start)?;
    if !(first.is_ascii_alphabetic() || *first == '_') {
        return None;
    }
    let mut end = start + 1;
    while let Some(character) = characters.get(end) {
        let allowed = character.is_ascii_alphanumeric()
            || *character == '_'
            || (kind == AssignmentKind::KeyValue && matches!(character, '.' | '-'));
        if !allowed {
            break;
        }
        end += 1;
    }
    (end - start <= 64).then_some(end)
}

fn replace_assignment(
    text: &str,
    match_start: usize,
    match_end: usize,
    key: &str,
    raw_value: &str,
    ctx: &mut MaskContext<'_>,
) -> String {
    if !ctx.matcher.is_sensitive(key) {
        return text[match_start..match_end].to_string();
    }
    let Some((body_start, body_end)) = assignment_value_body(raw_value) else {
        return text[match_start..match_end].to_string();
    };
    let value_offset = match_start + (text[match_start..match_end].len() - raw_value.len());
    // 区间按阶段输入文本（`text`）记录；被跳过的值由 `placeholder_at` 返回 `None`。
    let placeholder = match ctx.placeholder_at(
        text,
        value_offset + body_start,
        value_offset + body_end,
        None,
    ) {
        Some(placeholder) => placeholder,
        None => return text[match_start..match_end].to_string(),
    };
    let mut replaced = String::new();
    replaced.push_str(&text[match_start..value_offset + body_start]);
    replaced.push_str(&placeholder);
    replaced.push_str(&text[value_offset + body_end..match_end]);
    replaced
}

/// 定位赋值右侧的值本体区间（相对 `raw_value`）；形态不明确时返回 `None`。
fn assignment_value_body(raw_value: &str) -> Option<(usize, usize)> {
    let stripped = raw_value.trim_end();
    if stripped.is_empty() {
        return None;
    }
    if let Some(quote) = stripped.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let rest = &stripped[quote.len_utf8()..];
        return rest
            .find(quote)
            .map(|closing| (quote.len_utf8(), quote.len_utf8() + closing));
    }
    let body_end = inline_comment_start(stripped);
    if body_end == 0 {
        return None;
    }
    Some((0, body_end))
}

/// 无引号值的行内注释起点（`#` 在串首或前一个字符为空白时才算注释）。
fn inline_comment_start(text: &str) -> usize {
    for (index, character) in text.char_indices() {
        if character == '#'
            && (index == 0
                || text[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|previous| previous.is_whitespace()))
        {
            return text[..index].trim_end().len();
        }
    }
    text.len()
}

/// JSON 字符串值对：`"key": "value"`（转义感知，键与值各自解转义后再判定）。
fn substitute_json_pairs(text: &str, ctx: &mut MaskContext<'_>) -> String {
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < text.len() {
        if !text.is_char_boundary(index) {
            index += 1;
            continue;
        }
        let Some(pair) = json_pair_at(text, index) else {
            index += 1;
            continue;
        };
        let (key, raw_value, value_start, value_end, match_end) = pair;
        result.push_str(&text[cursor..value_start]);
        if !ctx.matcher.is_sensitive(&unescape_json_string(&key)) {
            result.push_str(&text[value_start..value_end]);
        } else {
            let value = unescape_json_string(&raw_value);
            match ctx.placeholder_at(text, value_start, value_end, Some(&value)) {
                Some(placeholder) => result.push_str(&placeholder),
                None => result.push_str(&text[value_start..value_end]),
            }
        }
        cursor = value_end;
        index = match_end;
    }
    result.push_str(&text[cursor..]);
    result
}

/// 在 `index` 起匹配 `"key"\s*:\s*"value"`，返回各片段。
fn json_pair_at(text: &str, index: usize) -> Option<(String, String, usize, usize, usize)> {
    if !text[index..].starts_with('"') {
        return None;
    }
    let (key, after_key) = json_string_body(text, index + 1, 1, 80)?;
    let characters: Vec<char> = text[after_key..].chars().collect();
    let offsets = char_offsets_of(&characters);
    let mut cursor = skip_while(&characters, 0, |character| character.is_whitespace());
    if characters.get(cursor) != Some(&':') {
        return None;
    }
    cursor = skip_while(&characters, cursor + 1, |character| {
        character.is_whitespace()
    });
    if characters.get(cursor) != Some(&'"') {
        return None;
    }
    let value_start = after_key + offsets[cursor + 1];
    let (raw_value, after_value) = json_string_body(text, value_start, 0, usize::MAX)?;
    let value_end = after_value - 1;
    Some((key, raw_value, value_start, value_end, after_value))
}

/// 读取一个 JSON 串的正文（起始位置在开引号之后），返回 `(正文, 闭引号之后的位置)`。
fn json_string_body(
    text: &str,
    start: usize,
    min_atoms: usize,
    max_atoms: usize,
) -> Option<(String, usize)> {
    let mut atoms = 0;
    let mut body_end = start;
    let mut escaped = false;
    let characters: Vec<char> = text[start..].chars().collect();
    let offsets = char_offsets_of(&characters);
    let mut index = 0;
    while let Some(character) = characters.get(index) {
        if escaped {
            escaped = false;
            body_end = start + offsets[index + 1];
            index += 1;
            continue;
        }
        if *character == '\\' {
            escaped = true;
            atoms += 1;
            if atoms > max_atoms {
                return None;
            }
            index += 1;
            continue;
        }
        if *character == '"' {
            if atoms < min_atoms {
                return None;
            }
            return Some((
                text[start..body_end].to_string(),
                start + offsets[index + 1],
            ));
        }
        atoms += 1;
        if atoms > max_atoms {
            return None;
        }
        body_end = start + offsets[index + 1];
        index += 1;
    }
    None
}

/// JSON 串正文解转义；解析失败时原样返回。
fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    match serde_json::from_str::<Value>(&format!("\"{raw}\"")) {
        Ok(Value::String(value)) => value,
        _ => raw.to_string(),
    }
}

// ── 形态判定（Python 侧各条正则的等价实现） ────────────────────────────────

fn char_offsets_of(characters: &[char]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(characters.len() + 1);
    let mut offset = 0;
    for character in characters {
        offsets.push(offset);
        offset += character.len_utf8();
    }
    offsets.push(offset);
    offsets
}

fn skip_while(characters: &[char], from: usize, accepts: fn(char) -> bool) -> usize {
    let mut index = from;
    while characters.get(index).is_some_and(|value| accepts(*value)) {
        index += 1;
    }
    index
}

fn starts_with(characters: &[char], index: usize, expected: &str) -> bool {
    for (cursor, character) in (index..).zip(expected.chars()) {
        if characters.get(cursor) != Some(&character) {
            return false;
        }
    }
    true
}

fn is_pure_letters(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|character| character.is_ascii_alphabetic())
}

fn is_pure_digits(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|character| character.is_ascii_digit())
}

fn is_hex(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|character| character.is_ascii_hexdigit())
}

fn is_identifier(token: &str) -> bool {
    let mut characters = token.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_uuid(token: &str) -> bool {
    let groups = [8_usize, 4, 4, 4, 12];
    let parts: Vec<&str> = token.split('-').collect();
    parts.len() == groups.len()
        && parts
            .iter()
            .zip(groups.iter())
            .all(|(part, size)| part.len() == *size && is_hex(part))
}

fn is_hex_colon(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    if parts.len() < 3 {
        return false;
    }
    parts
        .iter()
        .all(|part| !part.is_empty() && part.len() <= 4 && is_hex(part))
}

fn is_prefixed_hex(token: &str) -> bool {
    let Some((prefix, value)) = token.split_once(':') else {
        return false;
    };
    if prefix.is_empty() || prefix.len() > 16 {
        return false;
    }
    let mut characters = prefix.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    if !characters.all(|character| character.is_ascii_alphanumeric() || character == '_') {
        return false;
    }
    value.len() >= 16 && is_hex(value)
}

fn is_version(token: &str) -> bool {
    let body = token.strip_prefix('v').unwrap_or(token);
    let (core, suffix) = match body.find(['-', '+']) {
        Some(index) => (&body[..index], Some(&body[index..])),
        None => (body, None),
    };
    let mut parts = core.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if !is_pure_digits(first) {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() || rest.len() > 3 || !rest.iter().all(|part| is_pure_digits(part)) {
        return false;
    }
    match suffix {
        None => true,
        Some(value) => {
            value.len() >= 2
                && value
                    .chars()
                    .skip(1)
                    .all(|character| character.is_ascii_alphanumeric() || ".+-".contains(character))
        }
    }
}

fn is_datetime(token: &str) -> bool {
    // 日期部分是 ASCII：先按字符边界切出前 10 个字符，避免在非边界处切片。
    let boundaries: Vec<usize> = token.char_indices().map(|(index, _)| index).collect();
    let Some(&start) = boundaries.first() else {
        return false;
    };
    let date_end = match boundaries.get(10) {
        Some(end) => *end,
        None => return false,
    };
    let date = &token[start..date_end];
    let rest = &token[date_end..];
    if !(date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-') {
        return false;
    }
    if !is_pure_digits(&date[..4]) || !is_pure_digits(&date[5..7]) || !is_pure_digits(&date[8..]) {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    let mut characters = rest.chars();
    if !matches!(characters.next(), Some('T') | Some(' ')) {
        return false;
    }
    let time = characters.as_str();
    time.len() >= 5
        && is_pure_digits(&time[..2])
        && time.as_bytes()[2] == b':'
        && is_pure_digits(&time[3..5])
        && time_suffix_ok(&time[5..])
}

fn time_suffix_ok(rest: &str) -> bool {
    if rest.is_empty() {
        return true;
    }
    if let Some(body) = rest.strip_prefix('Z') {
        return body.is_empty();
    }
    if let Some(body) = rest.strip_prefix(':') {
        if body.len() < 2 || !is_pure_digits(&body[..2]) {
            return false;
        }
        let tail = &body[2..];
        if tail.is_empty() {
            return true;
        }
        if let Some(fraction) = tail.strip_prefix('.') {
            return is_pure_digits(fraction);
        }
        return timezone_ok(tail);
    }
    timezone_ok(rest)
}

fn timezone_ok(rest: &str) -> bool {
    let mut characters = rest.chars();
    if !matches!(characters.next(), Some('+') | Some('-')) {
        return false;
    }
    let body = characters.as_str();
    if body.len() == 4 {
        return is_pure_digits(body);
    }
    if body.len() == 5 && body.as_bytes()[2] == b':' {
        return is_pure_digits(&body[..2]) && is_pure_digits(&body[3..]);
    }
    false
}

fn is_word_like_segmented(token: &str) -> bool {
    if !token
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return false;
    }
    let segments: Vec<&str> = token
        .split(['_', '-'])
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 2 {
        return false;
    }
    segments
        .iter()
        .map(|segment| segment.chars().count())
        .max()
        .unwrap_or(0)
        < ENTROPY_SEGMENT_MAX_LENGTH
}

fn is_naming_chain(token: &str) -> bool {
    let separators = |character: char| matches!(character, '_' | '.' | '-' | ':' | '|');
    let trimmed = token.trim_end_matches(separators);
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return false;
    }
    let segments: Vec<&str> = trimmed
        .split(separators)
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 2 {
        return false;
    }
    if segments.iter().any(|segment| {
        segment
            .chars()
            .any(|character| !character.is_ascii_alphanumeric())
    }) {
        return false;
    }
    segments.iter().all(|segment| {
        segment.chars().count() < NAMING_SEGMENT_MAX_LENGTH
            || !segment.chars().any(|character| character.is_ascii_digit())
    })
}

/// 供上层复用：把告警补齐（本层当前不产出告警，保留签名以便 middleware 接线）。
pub fn engine_warnings() -> Vec<ProviderWarning> {
    Vec::new()
}

#[cfg(test)]
mod plan_cache_tests {
    use super::*;
    use crate::desensitization::plan_cache::{MaskPlan, MaskPlanCache, PlanSpan};
    use crate::desensitization::rules::builtin_rules;
    use crate::desensitization::SequenceRegistry;

    /// 用同一份上下文按顺序屏蔽多段文本（可选挂计划缓存），返回结果。
    fn run(texts: &[&str], cache: Option<Arc<MaskPlanCache>>) -> Vec<String> {
        let empty: Vec<String> = Vec::new();
        let matcher = SensitiveMatcher::new(&empty, &empty);
        let rules = builtin_rules();
        let mut registry = SequenceRegistry::new();
        let (mut cycle, _) = registry.begin_cycle("计划缓存");
        let mut stats = DesensitizationStats::default();
        let mut ctx = MaskContext {
            matcher: &matcher,
            cycle: &mut cycle,
            stats: &mut stats,
            entropy_enabled: false,
            entropy_min_length: 0,
            entropy_min_bits: 0.0,
            entropy_pure_letters: false,
            entropy_pure_digits: false,
            pattern_rules: rules,
            gitleaks_rules: &[],
            // 这一组用例只管结构层与计划缓存；NER 兜底层的接线见
            // `tests/desensitization_ner_stage.rs`。
            ner: None,
            plan_cache: cache,
            plan_builder: None,
        };
        texts.iter().map(|text| mask_text(text, &mut ctx)).collect()
    }

    /// 同一段文本重复屏蔽：第二次走缓存重放，结果与第一次逐字一致。
    #[test]
    fn repeated_text_hits_the_cached_plan() {
        let cache = Arc::new(MaskPlanCache::default());
        let secret = ["A1b2C3d4E5f6G7h8", "I9j0K1l2M3n4"].concat();
        let text = format!("API_KEY={secret}");
        let outputs = run(&[&text, &text], Some(Arc::clone(&cache)));
        assert_eq!(outputs[0], outputs[1], "重放结果必须与首次屏蔽一致");
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.invalid, 0);
    }

    /// 未挂缓存时行为不变（重放路径不参与）。
    #[test]
    fn without_cache_output_is_unchanged() {
        let secret = ["A1b2C3d4E5f6G7h8", "I9j0K1l2M3n4"].concat();
        let text = format!("API_KEY={secret}");
        let plain = run(&[&text, &text], None);
        let cached = run(&[&text, &text], Some(Arc::new(MaskPlanCache::default())));
        assert_eq!(plain, cached);
    }

    /// 文本变了就是另一个缓存键：不会把旧计划套到新文本上。
    #[test]
    fn changed_text_misses_the_cache() {
        let cache = Arc::new(MaskPlanCache::default());
        let secret = ["A1b2C3d4E5f6G7h8", "I9j0K1l2M3n4"].concat();
        let first = format!("API_KEY={secret}");
        let second = format!("API_KEY={secret}9");
        let outputs = run(&[&first, &second], Some(Arc::clone(&cache)));
        assert_ne!(outputs[0], outputs[1]);
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.stats().hits, 0);
    }

    /// 计划里的序号与当前周期不符（规则或稳定索引漂移）：按未命中重算，不改语义。
    #[test]
    fn drifted_plan_falls_back_to_full_mask() {
        let cache = Arc::new(MaskPlanCache::default());
        let secret = ["A1b2C3d4E5f6G7h8", "I9j0K1l2M3n4"].concat();
        let text = format!("API_KEY={secret}");
        // 让计划指向一个永远不对的序号：重放必然失效。
        cache.put(
            &text,
            MaskPlan {
                stages: vec![vec![PlanSpan {
                    start: 8,
                    end: 8 + secret.len(),
                    seq: 9999,
                    counter: String::new(),
                }]],
            },
        );
        let expected = run(&[text.as_str()], None);
        let replayed = run(&[&text], Some(Arc::clone(&cache)));
        assert_eq!(replayed, expected, "失效后回落到完整屏蔽");
        let stats = cache.stats();
        assert_eq!(stats.invalid, 1);
        assert_eq!(stats.hits, 0, "命中的那次被改记为未命中");
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.lookups, 1);
    }

    /// 计划坐标落进多字节字符中间时必须按未命中收场，而不是直接切片。
    ///
    /// Rust 的 `String` 下标是字节，错位就会落在多字节字符中间；release 档是
    /// `panic = "abort"`，一次 panic 会把整个内核进程带走（现场表现为「内核进程已退出」）。
    /// 这类失配在 CJK 文本里第一次真实发生过，这里把它钉住。
    #[test]
    fn misaligned_plan_falls_back_instead_of_panicking() {
        let cache = Arc::new(MaskPlanCache::default());
        let text = "API_KEY=A1b2C3d4E5f6G7h8 广东省".to_string();
        let province = text.find('省').expect("用例包含多字节字符");
        cache.put(
            &text,
            MaskPlan {
                stages: vec![vec![PlanSpan {
                    start: province + 1,
                    end: province + 2,
                    seq: 1,
                    counter: String::new(),
                }]],
            },
        );
        let expected = run(&[text.as_str()], None);
        let replayed = run(&[&text], Some(Arc::clone(&cache)));
        assert_eq!(replayed, expected, "失配后回落到完整屏蔽");
        assert_eq!(cache.stats().invalid, 1);
        assert_eq!(cache.stats().hits, 0, "失配的那次记作未命中");
    }

    /// 值类型规则层与熵兜底同样按阶段记录：命中后各层计数口径不变。
    #[test]
    fn stage_counters_survive_replay() {
        let cache = Arc::new(MaskPlanCache::default());
        let text = format!("联系 {}", ["worker", "@", "corp.local"].concat());
        let outputs = run(&[&text, &text], Some(Arc::clone(&cache)));
        assert_eq!(outputs[0], outputs[1]);
        let stats = cache.stats();
        assert_eq!(stats.hits, 1, "邮箱命中落在规则阶段，计划可重放");
        assert_eq!(stats.invalid, 0);
    }

    /// 同层多区间（规则层多命中）必须能命中缓存：阶段内的区间逆序记录，重放需按同一方向应用。
    ///
    /// 修复前的重放会正序消费这些区间：前一个占位符与原文长度不同，后一个区间的坐标随之偏掉，
    /// 计划被判失效——这类文本永远命不中缓存，白白损失速度。
    #[test]
    fn multi_span_rules_stage_replays_without_misalignment() {
        let cache = Arc::new(MaskPlanCache::default());
        let first = ["alice", "@", "corp.local"].concat();
        let second = ["bob", "@", "corp.local"].concat();
        let text = format!("联系 {first} 与 {second}");
        let outputs = run(&[&text, &text], Some(Arc::clone(&cache)));
        assert_eq!(outputs[0], outputs[1], "重放结果必须与首次屏蔽一致");
        assert!(!outputs[0].contains(&first) && !outputs[0].contains(&second));
        let stats = cache.stats();
        assert_eq!(stats.hits, 1, "同层多区间的计划也要能重放");
        assert_eq!(stats.invalid, 0, "重放不应错位");
        assert_eq!(stats.misses, 1);
    }
}
