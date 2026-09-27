//! gitleaks 规则接入（Python `desensitization/gitleaks.py`）。
//!
//! 内置快照 `data/gitleaks.toml`（上游默认配置的离线副本，与 Python 侧
//! `omnicrawl/llm/desensitization/gitleaks.toml` 逐字节一致）默认加载；传入自定义路径时，
//! 按其规则 id 覆盖 / 追加。
//!
//! 解析遵循 gitleaks 语义：`keywords` 文本级预过滤（大小写不敏感）、`entropy` 候选值熵下限、
//! `secretGroup` 指定「秘密」捕获组（缺省整段匹配）、规则级与全局 allowlist（值豁免 / 整段豁免 /
//! 停用词命中即判误报）。无法在「纯文本、无文件路径、无行上下文」下忠实执行的豁免条件一律
//! **保守跳过**（宁可多脱敏，不可漏脱敏）：`paths`、`commits`、`regexTarget = "line"`、
//! `condition = "AND"` 只跳过其豁免判断，规则本身仍生效。
//!
//! 兼容性归一与 Python 一致：Go 的文本尾锚点 `\z` 归一为 `\Z`；出现在模式中部的全局内联标志
//! `(?i)` 上提到模式开头（Python 不允许非起始全局标志，不上提会让两侧规则集不同）。
//! 编译失败的规则整条跳过，不影响其余规则。
//!
//! 未搬：`locality` 局部化扫描（纯性能优化，不影响命中集合）。

use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};

use super::rules::{shannon_entropy_bits, CATEGORY_GITLEAKS, TRAILING_TRIM_CHARS};

/// 随 crate 分发的上游快照：与 Python 侧同名文件逐字节一致，改一处必须改两处。
pub const GITLEAKS_SNAPSHOT: &str = include_str!("../../data/gitleaks.toml");

/// 一条编译好的 gitleaks 规则。
#[derive(Clone)]
pub struct GitleaksRule {
    pub rule_id: String,
    pub description: String,
    /// 归一化后的模式原文（与 Python 侧逐字一致；编译期的 RE2→Rust 适配不回写这里）。
    pub pattern_source: String,
    pub pattern: Regex,
    pub keywords: Vec<String>,
    pub secret_group: Option<usize>,
    pub min_entropy: Option<f64>,
    pub allowlist: Vec<Regex>,
    pub match_allowlist: Vec<Regex>,
    /// 豁免表的模式原文（与 Python 侧逐字一致，便于对照与导出）。
    pub allowlist_sources: Vec<String>,
    pub match_allowlist_sources: Vec<String>,
    pub stopwords: Vec<String>,
}

/// 一条 gitleaks 命中：`rule_id` 是运行时值，因此不复用静态的 [`super::rules::RuleMatch`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitleaksMatch {
    pub rule_id: String,
    pub category: &'static str,
    pub start: usize,
    pub end: usize,
    pub value: String,
}

static DEFAULT_RULES: OnceLock<Vec<GitleaksRule>> = OnceLock::new();

/// 内置快照的规则集（只解析一次并常驻）。
pub fn default_rules() -> &'static [GitleaksRule] {
    DEFAULT_RULES.get_or_init(|| build_rules(&parse_toml(GITLEAKS_SNAPSHOT)))
}

/// 内置快照 + 自定义文件（按规则 id 覆盖 / 追加，同 id 保持原位置）。
///
/// 任何读取 / 解析失败都回退到已成功加载的部分，绝不抛错中断脱敏运行时。
pub fn load_rules(config_path: Option<&str>) -> Vec<GitleaksRule> {
    let path = config_path.map(str::trim).unwrap_or_default();
    if path.is_empty() {
        return default_rules().to_vec();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return default_rules().to_vec();
    };
    let extra = build_rules(&parse_toml(&text));
    if extra.is_empty() {
        return default_rules().to_vec();
    }
    merge_rules(default_rules(), extra)
}

fn merge_rules(base: &[GitleaksRule], extra: Vec<GitleaksRule>) -> Vec<GitleaksRule> {
    let mut merged: Vec<GitleaksRule> = base.to_vec();
    for rule in extra {
        match merged
            .iter()
            .position(|known| known.rule_id == rule.rule_id)
        {
            Some(index) => merged[index] = rule,
            None => merged.push(rule),
        }
    }
    merged
}

/// 把 Go RE2 模式归一为可用模式（见模块说明）。
pub fn normalize_gitleaks_pattern(pattern: &str) -> String {
    let normalized = pattern.replace(r"\z", r"\Z");
    let mut flags: Vec<char> = Vec::new();
    let mut body = normalized.as_str();

    if let Some(rest) = leading_flag_group(body) {
        let (letters, tail) = rest;
        flags.extend(letters.chars());
        body = tail;
    }
    let (stripped, collected) = strip_inline_flag_groups(body);
    flags.extend(collected.chars());
    if flags.is_empty() {
        return stripped;
    }
    flags.sort_unstable();
    flags.dedup();
    format!("(?{}){stripped}", flags.into_iter().collect::<String>())
}

/// 模式开头的 `(?flags)`：返回（标志字母，剩余模式）。
fn leading_flag_group(pattern: &str) -> Option<(&str, &str)> {
    let rest = pattern.strip_prefix("(?")?;
    let end = rest.find(')')?;
    let letters = &rest[..end];
    if letters.is_empty() || !letters.chars().all(is_flag_letter) {
        return None;
    }
    Some((letters, &rest[end + 1..]))
}

/// 摘掉模式中部的 `(?flags)`，返回（新模式，收集到的标志字母）。
///
/// 只认「连续的标志字母 + `)`」这一种形态；`(?:` / `(?=` / `(?-i:` 等一律原样保留
/// （与 Python 的 `\(\?([aiLmsux]+)\)` 同口径）。
fn strip_inline_flag_groups(pattern: &str) -> (String, String) {
    let bytes = pattern.as_bytes();
    let mut result = String::with_capacity(pattern.len());
    let mut flags = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'(' && index + 2 < bytes.len() && bytes[index + 1] == b'?' {
            let mut cursor = index + 2;
            let mut letters = String::new();
            while cursor < bytes.len() && is_flag_byte(bytes[cursor]) {
                letters.push(bytes[cursor] as char);
                cursor += 1;
            }
            if !letters.is_empty() && cursor < bytes.len() && bytes[cursor] == b')' {
                flags.push_str(&letters);
                index = cursor + 1;
                continue;
            }
        }
        let character = pattern[index..].chars().next().unwrap_or(' ');
        result.push(character);
        index += character.len_utf8();
    }
    (result, flags)
}

fn is_flag_byte(byte: u8) -> bool {
    matches!(byte, b'a' | b'i' | b'L' | b'm' | b's' | b'u' | b'x')
}

fn is_flag_letter(letter: char) -> bool {
    matches!(letter, 'a' | 'i' | 'L' | 'm' | 's' | 'u' | 'x')
}

fn parse_toml(text: &str) -> toml::Value {
    text.parse::<toml::Value>()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()))
}

fn build_rules(document: &toml::Value) -> Vec<GitleaksRule> {
    let global_allowlist = document
        .get("allowlist")
        .or_else(|| document.get("allowlists"));
    let (global_secret, global_match, global_stopwords) = allowlist_parts(global_allowlist);

    let sources = as_mappings(document.get("rules"));
    // 规则表整体是常量，只在进程内第一次构造时解析一次；但 221 条模式的正则编译在
    // release 下要 ~5s、debug 下 ~27s，串行编译会让「启用脱敏后的第一个回合」在发请求前
    // 静默卡住（用户报的「很久没反应，然后一口气全出来」里有一半是它）。这里按核数切成
    // 若干块并行编译，块内保持原顺序，最后按块序拼回——结果与串行逐条一致。
    let compiled: Vec<Vec<GitleaksRule>> = parallel_map(&sources, |chunk| {
        let mut rules: Vec<GitleaksRule> = Vec::new();
        for raw in chunk {
            build_rule(
                raw,
                &global_secret,
                &global_match,
                &global_stopwords,
                &mut rules,
            );
        }
        rules
    });
    compiled.into_iter().flatten().collect()
}

/// 把切片按可用并行度切块，交给 [`std::thread::scope`] 并行处理，再按块序拼回。
///
/// 不引入 rayon 之类的依赖（本 crate 的传输/解析链路都刻意保持同步、无运行时）；
/// 并行度取 `available_parallelism`，块数不超过元素数，元素少时退化为串行。
fn parallel_map<T, R, F>(items: &[T], work: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&[T]) -> R + Sync,
{
    let workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .min(items.len().max(1));
    if workers <= 1 {
        return vec![work(items)];
    }
    // 向上取整分块：前 `remainder` 块各多 1 个元素，保证「块序拼回 == 原顺序」。
    let chunk_size = items.len().div_ceil(workers);
    let chunks: Vec<&[T]> = items.chunks(chunk_size).collect();
    let results: Vec<R> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                let work = &work;
                scope.spawn(move || work(chunk))
            })
            .collect();
        // 线程 panic 时对应结果丢弃（编译期规则构造不该 panic，真发生也不该让整进程崩）。
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    });
    results
}

fn build_rule(
    raw: &toml::Value,
    global_secret: &[AllowlistPattern],
    global_match: &[AllowlistPattern],
    global_stopwords: &[String],
    rules: &mut Vec<GitleaksRule>,
) {
    let rule_id = raw
        .get("id")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if rule_id.is_empty() {
        return;
    }

    let mut sources: Vec<String> = Vec::new();
    if let Some(pattern) = raw.get("regex").and_then(toml::Value::as_str) {
        sources.push(pattern.to_string());
    }
    for item in as_array(raw.get("regexes")) {
        if let Some(pattern) = item.as_str() {
            sources.push(pattern.to_string());
        }
    }
    let compiled: Vec<(String, Regex)> = sources
        .iter()
        .map(|pattern| normalize_gitleaks_pattern(pattern))
        .filter_map(|normalized| compile(&normalized).map(|regex| (normalized, regex)))
        .collect();
    if compiled.is_empty() {
        return;
    }

    let secret_group = raw
        .get("secretGroup")
        .and_then(toml::Value::as_integer)
        .filter(|value| *value >= 0)
        .map(|value| value as usize);
    let min_entropy = raw.get("entropy").and_then(|value| match value {
        // TOML 里 `entropy = 4` 是整数、`4.0` 是浮点，Python 侧两者都收。
        toml::Value::Float(number) => Some(*number),
        toml::Value::Integer(number) => Some(*number as f64),
        _ => None,
    });
    let keywords: Vec<String> = as_array(raw.get("keywords"))
        .into_iter()
        .filter_map(toml::Value::as_str)
        .filter(|keyword| !keyword.is_empty())
        .map(str::to_lowercase)
        .collect();

    let local_allowlist = raw.get("allowlist").or_else(|| raw.get("allowlists"));
    let (local_secret, local_match, local_stopwords) = allowlist_parts(local_allowlist);
    let description = raw
        .get("description")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();

    let allowlist: Vec<Regex> = local_secret
        .iter()
        .map(|(_, regex)| regex.clone())
        .chain(global_secret.iter().map(|(_, regex)| regex.clone()))
        .collect();
    let allowlist_sources: Vec<String> = local_secret
        .iter()
        .map(|(source, _)| source.clone())
        .chain(global_secret.iter().map(|(source, _)| source.clone()))
        .collect();
    let match_allowlist: Vec<Regex> = local_match
        .iter()
        .map(|(_, regex)| regex.clone())
        .chain(global_match.iter().map(|(_, regex)| regex.clone()))
        .collect();
    let match_allowlist_sources: Vec<String> = local_match
        .iter()
        .map(|(source, _)| source.clone())
        .chain(global_match.iter().map(|(source, _)| source.clone()))
        .collect();
    let mut stopwords: Vec<String> = local_stopwords;
    stopwords.extend(global_stopwords.iter().cloned());

    let total = compiled.len();
    for (index, (pattern_source, pattern)) in compiled.into_iter().enumerate() {
        let suffix = if total == 1 {
            String::new()
        } else {
            format!("#{}", index + 1)
        };
        rules.push(GitleaksRule {
            rule_id: format!("gitleaks:{rule_id}{suffix}"),
            description: description.clone(),
            pattern_source,
            pattern,
            keywords: keywords.clone(),
            secret_group,
            min_entropy,
            allowlist: allowlist.clone(),
            match_allowlist: match_allowlist.clone(),
            allowlist_sources: allowlist_sources.clone(),
            match_allowlist_sources: match_allowlist_sources.clone(),
            stopwords: stopwords.clone(),
        });
    }
}

/// 编译后的豁免模式：归一化原文 + 编译结果。
type AllowlistPattern = (String, Regex);

/// 豁免表拆分结果：值豁免 / 整段豁免 / 停用词。
type AllowlistParts = (Vec<AllowlistPattern>, Vec<AllowlistPattern>, Vec<String>);

/// 拆分豁免表为「值豁免 / 整段豁免 / 停用词」，跳过无法忠实执行的条目。
///
/// 每项豁免同时留下模式原文（与 Python 侧一致）与编译结果。
fn allowlist_parts(value: Option<&toml::Value>) -> AllowlistParts {
    let mut secret: Vec<AllowlistPattern> = Vec::new();
    let mut match_any: Vec<AllowlistPattern> = Vec::new();
    let mut stopwords: Vec<String> = Vec::new();

    for entry in as_mappings(value) {
        for word in as_array(entry.get("stopwords")) {
            if let Some(word) = word.as_str() {
                if !word.is_empty() {
                    stopwords.push(word.to_lowercase());
                }
            }
        }
        let condition = entry
            .get("condition")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if condition == "and" {
            // AND 需要同时满足 paths / regexes 等全部条件；缺文件路径上下文，保守跳过。
            continue;
        }
        let target = entry
            .get("regexTarget")
            .and_then(toml::Value::as_str)
            .unwrap_or("secret")
            .trim()
            .to_lowercase();
        if target == "line" {
            // 行级豁免需要行上下文，纯文本链路不适用。
            continue;
        }
        let bucket = if target == "match" {
            &mut match_any
        } else {
            &mut secret
        };
        let patterns: Vec<String> = as_array(entry.get("regexes"))
            .into_iter()
            .filter_map(toml::Value::as_str)
            .map(normalize_gitleaks_pattern)
            .collect();
        // 豁免模式同样要编译正则，且全局豁免表会被每条规则复用：并行编译缩短首次构造时间。
        for (normalized, regex) in parallel_map(&patterns, |chunk| {
            chunk
                .iter()
                .filter_map(|pattern| compile(pattern).map(|regex| (pattern.clone(), regex)))
                .collect::<Vec<AllowlistPattern>>()
        })
        .into_iter()
        .flatten()
        {
            bucket.push((normalized, regex));
        }
    }
    (secret, match_any, stopwords)
}

fn compile(pattern: &str) -> Option<Regex> {
    // 先按原样编译：能编译的模式绝不动它（适配只用来救编译不过的那些）。
    if let Some(regex) = build(pattern) {
        return Some(regex);
    }
    build(&adapt_for_rust(pattern))
}

fn build(pattern: &str) -> Option<Regex> {
    RegexBuilder::new(pattern)
        // 上游个别规则（generic-api-key / pypi-upload-token / vault-batch-token）在默认
        // 10MB 程序上限下编译不过，这里放宽；编译期长度换运行时确定性。
        .size_limit(64 * 1024 * 1024)
        .build()
        .ok()
}

/// RE2 / Python 与 Rust 的写法差异，只在编译前适配（不改对外可见的归一化结果）：
///
/// - `\Z`（Python 的文本尾锚点）在 Rust 里是 `\z`；
/// - RE2 允许 `\/` 这类「转义非元字符」的写法，Rust 的解析器不认，只放松确定安全的几个字符；
/// - 不构成量词的 `{`（如 `{\d+}`）在 Python 里是字面量，Rust 会要求它开始一个量词，这里转义。
fn adapt_for_rust(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut result = String::with_capacity(pattern.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            let next = bytes[index + 1];
            if matches!(next, b'/' | b':' | b' ' | b'\'') {
                result.push(next as char);
                index += 2;
                continue;
            }
            if next == b'Z' {
                result.push_str("\\z");
                index += 2;
                continue;
            }
        }
        if bytes[index] == b'{' {
            match quantifier_end(&pattern[index..]) {
                Some(length) => {
                    // `{n}` / `{n,}` / `{n,m}`：整体照抄。
                    result.push_str(&pattern[index..index + length]);
                    index += length;
                }
                None => {
                    // Python / RE2 把非量词的 `{` 当字面量，Rust 会报错。
                    result.push_str("\\{");
                    index += 1;
                }
            }
            continue;
        }
        if bytes[index] == b'}' {
            // 量词里的 `}` 已被上面整段照抄；走到这里的都是字面量。
            result.push_str("\\}");
            index += 1;
            continue;
        }
        let character = pattern[index..].chars().next().unwrap_or(' ');
        result.push(character);
        index += character.len_utf8();
    }
    result
}

/// `{n}` / `{n,}` / `{n,m}` 的字节长度（含花括号）；其余返回 `None`。
fn quantifier_end(rest: &str) -> Option<usize> {
    let body = rest.strip_prefix('{')?;
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let after = &body[digits..];
    if after.starts_with('}') {
        return Some(1 + digits + 1);
    }
    let tail = after.strip_prefix(',')?;
    let more = tail.chars().take_while(char::is_ascii_digit).count();
    if tail[more..].starts_with('}') {
        return Some(1 + digits + 1 + more + 1);
    }
    None
}

/// 单表或表数组都当映射列表读（Python `_as_mappings`）。
fn as_mappings(value: Option<&toml::Value>) -> Vec<&toml::Value> {
    match value {
        Some(toml::Value::Table(_)) => value.into_iter().collect(),
        Some(toml::Value::Array(items)) => items
            .iter()
            .filter(|item| matches!(item, toml::Value::Table(_)))
            .collect(),
        _ => Vec::new(),
    }
}

fn as_array(value: Option<&toml::Value>) -> Vec<&toml::Value> {
    match value {
        Some(toml::Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
        None => Vec::new(),
    }
}

/// 按规则优先级扫描文本；`accepted` 是「已占位区间」（升序、互不重叠），命中即登记。
pub fn scan_gitleaks_rules(
    text: &str,
    rules: &[GitleaksRule],
    accepted: &mut Vec<(usize, usize)>,
) -> Vec<GitleaksMatch> {
    if text.is_empty() || rules.is_empty() {
        return Vec::new();
    }
    let lowered = text.to_lowercase();
    let mut matches: Vec<GitleaksMatch> = Vec::new();

    for rule in rules {
        if !rule.keywords.is_empty()
            && !rule
                .keywords
                .iter()
                .any(|keyword| lowered.contains(keyword.as_str()))
        {
            continue;
        }
        for captures in rule.pattern.captures_iter(text) {
            let Some(whole) = captures.get(0) else {
                continue;
            };
            let (start, end) = match rule.secret_group {
                Some(index) => captures
                    .get(index)
                    .map(|group| (group.start(), group.end()))
                    .unwrap_or((whole.start(), whole.end())),
                None => (whole.start(), whole.end()),
            };
            if start >= end {
                continue;
            }
            let raw = &text[start..end];
            if let Some(min_entropy) = rule.min_entropy {
                if shannon_entropy_bits(raw) < min_entropy {
                    continue;
                }
            }
            if !accepts(rule, raw, whole.as_str()) {
                continue;
            }
            let end = trim_trailing(text, start, end);
            if start >= end {
                continue;
            }
            if overlaps(accepted, start, end) {
                continue;
            }
            let position = accepted.partition_point(|(known, _)| *known < start);
            accepted.insert(position, (start, end));
            matches.push(GitleaksMatch {
                rule_id: rule.rule_id.clone(),
                category: CATEGORY_GITLEAKS,
                start,
                end,
                value: text[start..end].to_string(),
            });
        }
    }
    matches.sort_by_key(|hit| hit.start);
    matches
}

fn accepts(rule: &GitleaksRule, value: &str, match_text: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let lowered = value.to_lowercase();
    if !rule.stopwords.is_empty() && rule.stopwords.iter().any(|word| lowered.contains(word)) {
        return false;
    }
    if rule.allowlist.iter().any(|pattern| pattern.is_match(value)) {
        return false;
    }
    if rule
        .match_allowlist
        .iter()
        .any(|pattern| pattern.is_match(match_text))
    {
        return false;
    }
    true
}

fn trim_trailing(text: &str, start: usize, end: usize) -> usize {
    let mut end = end;
    while end > start {
        let Some(character) = text[start..end].chars().next_back() else {
            break;
        };
        if !TRAILING_TRIM_CHARS.contains(character) {
            break;
        }
        end -= character.len_utf8();
    }
    end
}

fn overlaps(intervals: &[(usize, usize)], start: usize, end: usize) -> bool {
    let index = intervals.partition_point(|(known, _)| *known < start);
    if index > 0 && intervals[index - 1].1 > start {
        return true;
    }
    if index < intervals.len() && intervals[index].0 < end {
        return true;
    }
    false
}
