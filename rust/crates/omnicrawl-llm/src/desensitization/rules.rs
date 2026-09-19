//! 值类型规则层：按形态识别「无键名的敏感值类型」，产出待脱敏区间。
//!
//! 语义基准是 Python `omnicrawl/llm/desensitization/rules.py`。结构感知层（键名规则）要求值
//! 处在有键名的结构里，熵兜底层要求值具备高随机性；本层覆盖两者之间的「形态确定、随机性低」
//! 的类型。本层每条规则的正则等价物都是手写匹配器，逐条用
//! 对照数据集验证（`tests/desensitization_rules_parity.rs`）。
//!
//! 已搬：全部 11 条内置规则——PEM 私钥（完整块 / 截断正文）、数据库连接串（URI / ADO 键值）、
//! 网址、邮箱、银行卡（Luhn）、MAC 地址、大陆车牌、内外网 IP（对齐 Python `ipaddress` 分类表），
//! 以及整套规则语义——关键字预过滤、熵下限、校验器、豁免表、停用词、尾部标点留在原文、
//! 重叠区间先命中先占位、结果按起点排序。
//!
//! gitleaks 规则表由 `gitleaks.rs` 承接（运行时正则）；`locality` 局部化扫描是 Python 侧重正则
//! 的性能优化（内核的手写匹配器逐条扫描本就没有整段回溯的代价，结果与全量扫描一致），
//! 扫描结果缓存由 [`ScanCache`] 承接。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::char_offsets;

fn find_candidates(matcher: RuleMatcher, characters: &[char]) -> Vec<(usize, usize)> {
    match matcher {
        RuleMatcher::PemBlock => find_pem_block(characters),
        RuleMatcher::PemBody => find_pem_body(characters),
        RuleMatcher::DbUri => find_db_uri(characters),
        RuleMatcher::DbKeyValue => find_db_key_value(characters),
        RuleMatcher::Url => find_url(characters),
        RuleMatcher::Email => find_email(characters),
        RuleMatcher::LicensePlate => find_license_plate(characters),
        RuleMatcher::BankCard => find_bank_card(characters),
        RuleMatcher::MacAddress => find_mac_address(characters),
        RuleMatcher::IpAddress => find_ip_address(characters),
    }
}

// ── 规则类别（与 [desensitization] 的 detect_* 开关一一对应） ──────────────

pub const CATEGORY_PEM_PRIVATE_KEY: &str = "pem_private_key";
pub const CATEGORY_DB_CONNECTION_STRING: &str = "db_connection_string";
pub const CATEGORY_EMAIL: &str = "email";
pub const CATEGORY_BANK_CARD: &str = "bank_card";
pub const CATEGORY_INTERNAL_IP: &str = "internal_ip";
pub const CATEGORY_EXTERNAL_IP: &str = "external_ip";
pub const CATEGORY_URL: &str = "url";
pub const CATEGORY_MAC_ADDRESS: &str = "mac_address";
pub const CATEGORY_LICENSE_PLATE: &str = "license_plate";
pub const CATEGORY_GITLEAKS: &str = "gitleaks";

/// 类别 → 配置字段名（按类别裁剪内置规则时用）。
pub const CATEGORY_CONFIG_FLAGS: [(&str, &str); 9] = [
    (CATEGORY_PEM_PRIVATE_KEY, "detect_pem_private_key"),
    (CATEGORY_DB_CONNECTION_STRING, "detect_db_connection_string"),
    (CATEGORY_EMAIL, "detect_email"),
    (CATEGORY_BANK_CARD, "detect_bank_card"),
    (CATEGORY_INTERNAL_IP, "detect_internal_ip"),
    (CATEGORY_EXTERNAL_IP, "detect_external_ip"),
    (CATEGORY_URL, "detect_url"),
    (CATEGORY_MAC_ADDRESS, "detect_mac_address"),
    (CATEGORY_LICENSE_PLATE, "detect_license_plate"),
];

/// 值尾部需要留在原文的标点 / 空白（占位符只替换值本体，避免破坏结构）。
pub const TRAILING_TRIM_CHARS: &str = " \t\r\n\u{3000}\"'`.,;:!?)]}>,、。；：！？";

/// 字符频率的香农熵（bit/char）；长度 ≤1 时为 0。
///
/// 求和顺序按「字符首次出现」——与 Python `Counter` 的迭代序一致，浮点结果逐位可比。
pub fn shannon_entropy_bits(text: &str) -> f64 {
    let length = text.chars().count();
    if length <= 1 {
        return 0.0;
    }
    let mut order: Vec<char> = Vec::new();
    let mut counts: HashMap<char, usize> = HashMap::new();
    for character in text.chars() {
        match counts.get_mut(&character) {
            Some(count) => *count += 1,
            None => {
                counts.insert(character, 1);
                order.push(character);
            }
        }
    }
    -order
        .iter()
        .map(|character| {
            let probability = counts[character] as f64 / length as f64;
            probability * probability.log2()
        })
        .sum::<f64>()
}

/// 一条规则命中的待脱敏区间（半开区间，字节偏移；`value` 为裁掉尾部标点后的原文值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    pub rule_id: &'static str,
    pub category: &'static str,
    pub start: usize,
    pub end: usize,
    pub value: String,
}

/// 手写匹配器：Python 侧是正则，内核侧是等价的手写实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMatcher {
    PemBlock,
    PemBody,
    DbUri,
    DbKeyValue,
    Url,
    Email,
    LicensePlate,
    BankCard,
    MacAddress,
    IpAddress,
}

/// 值级豁免 / 校验谓词（Python 侧是正则与函数对象）。
pub type ValuePredicate = fn(&str) -> bool;

/// 单条值类型规则：手写匹配器 + 关键字 / 熵 / 校验器 / 豁免表。
#[derive(Clone)]
pub struct PatternRule {
    pub rule_id: &'static str,
    pub category: &'static str,
    pub matcher: RuleMatcher,
    pub description: &'static str,
    /// 文本级预过滤（大小写不敏感）：命中任一关键字才运行本规则。
    pub keywords: &'static [&'static str],
    /// 候选值的香农熵下限。
    pub min_entropy: Option<f64>,
    /// 形态校验（不通过则丢弃）。
    pub validator: Option<ValuePredicate>,
    /// 候选值命中即判为误报。
    pub allowlist: &'static [ValuePredicate],
    /// 值里含任一停用词即跳过（大小写不敏感）。
    pub stopwords: &'static [&'static str],
    /// 把值尾部的标点 / 空白留在原文。
    pub trim_trailing: bool,
}

impl PatternRule {
    /// 本规则在文本中的待脱敏区间（含熵 / 豁免 / 校验 / 尾部收缩；字节偏移）。
    pub fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let characters: Vec<char> = text.chars().collect();
        let offsets = char_offsets(&characters);
        let mut found = Vec::new();
        for (start, end) in find_candidates(self.matcher, &characters) {
            if start >= end {
                continue;
            }
            let raw: String = characters[start..end].iter().collect();
            if let Some(min_entropy) = self.min_entropy {
                if shannon_entropy_bits(&raw) < min_entropy {
                    continue;
                }
            }
            if !self.accepts(&raw) {
                continue;
            }
            let end = if self.trim_trailing {
                trim_span(&characters, start, end)
            } else {
                end
            };
            if start >= end {
                continue;
            }
            found.push((offsets[start], offsets[end]));
        }
        found
    }

    /// 候选值是否通过豁免与校验（熵在取原始值前判，不在这里）。
    pub fn accepts(&self, value: &str) -> bool {
        if value.is_empty() {
            return false;
        }
        let lowered = value.to_lowercase();
        if !self.stopwords.is_empty() && self.stopwords.iter().any(|word| lowered.contains(word)) {
            return false;
        }
        if self.allowlist.iter().any(|predicate| predicate(value)) {
            return false;
        }
        if let Some(validator) = self.validator {
            if !validator(value) {
                return false;
            }
        }
        true
    }
}

static BUILTIN_RULES: OnceLock<Vec<PatternRule>> = OnceLock::new();

/// 内置值类型规则（顺序即优先级；重叠区间由先者占位）。
///
/// 规则集合是常量：只构造一次并常驻（Python 侧对应 `@lru_cache(maxsize=1)`）。
pub fn builtin_rules() -> &'static [PatternRule] {
    BUILTIN_RULES.get_or_init(build_builtin_rules)
}

fn build_builtin_rules() -> Vec<PatternRule> {
    vec![
        PatternRule {
            rule_id: "pem-private-key",
            category: CATEGORY_PEM_PRIVATE_KEY,
            matcher: RuleMatcher::PemBlock,
            description: "PEM 私钥块（BEGIN/END … PRIVATE KEY）",
            keywords: &["private key"],
            min_entropy: None,
            validator: None,
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "pem-private-key-body",
            category: CATEGORY_PEM_PRIVATE_KEY,
            matcher: RuleMatcher::PemBody,
            description: "PEM 私钥头与 base64 正文（无 END 的截断场景）",
            keywords: &["private key"],
            min_entropy: None,
            validator: None,
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "db-connection-uri",
            category: CATEGORY_DB_CONNECTION_STRING,
            matcher: RuleMatcher::DbUri,
            description: "数据库连接串（URI 形态）",
            keywords: &[],
            min_entropy: None,
            validator: None,
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "db-connection-kv",
            category: CATEGORY_DB_CONNECTION_STRING,
            matcher: RuleMatcher::DbKeyValue,
            description: "数据库连接串（ADO / .NET 键值形态）",
            keywords: &["password", "pwd"],
            min_entropy: None,
            validator: None,
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "url",
            category: CATEGORY_URL,
            matcher: RuleMatcher::Url,
            description: "网址（http / https / ftp）",
            keywords: &["://"],
            min_entropy: None,
            validator: None,
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "email",
            category: CATEGORY_EMAIL,
            matcher: RuleMatcher::Email,
            description: "邮箱地址",
            keywords: &["@"],
            min_entropy: None,
            validator: None,
            allowlist: &[is_example_domain],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "license-plate-cn",
            category: CATEGORY_LICENSE_PLATE,
            matcher: RuleMatcher::LicensePlate,
            description: "中国大陆车牌",
            keywords: &[],
            min_entropy: None,
            validator: None,
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "bank-card",
            category: CATEGORY_BANK_CARD,
            matcher: RuleMatcher::BankCard,
            description: "银行卡号（Luhn 校验）",
            keywords: &[],
            min_entropy: None,
            validator: Some(is_luhn_valid),
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "mac-address",
            category: CATEGORY_MAC_ADDRESS,
            matcher: RuleMatcher::MacAddress,
            description: "MAC 地址",
            keywords: &[],
            min_entropy: None,
            validator: None,
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "ip-internal",
            category: CATEGORY_INTERNAL_IP,
            matcher: RuleMatcher::IpAddress,
            description: "内网 IP（私有 / 链路本地）",
            keywords: &[],
            min_entropy: None,
            validator: Some(is_internal_ip),
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
        PatternRule {
            rule_id: "ip-external",
            category: CATEGORY_EXTERNAL_IP,
            matcher: RuleMatcher::IpAddress,
            description: "外网 IP（公网可路由）",
            keywords: &[],
            min_entropy: None,
            validator: Some(is_external_ip),
            allowlist: &[],
            stopwords: &[],
            trim_trailing: true,
        },
    ]
}

/// 按启用类别裁剪内置规则。Python 侧按配置字段名裁剪并追加 gitleaks 规则；内核侧由调用方
/// 给类别集合，gitleaks 追加留到 gitleaks 片。
pub fn build_enabled_rules(categories: &[&str]) -> Vec<PatternRule> {
    builtin_rules()
        .iter()
        .filter(|rule| categories.contains(&rule.category))
        .cloned()
        .collect()
}

/// 按规则优先级扫描文本；重叠区间只保留先命中的一条，结果按起点排序。
pub fn scan_pattern_rules(text: &str, rules: &[PatternRule]) -> Vec<RuleMatch> {
    if text.is_empty() || rules.is_empty() {
        return Vec::new();
    }
    let lowered = text.to_lowercase();
    let mut accepted: Vec<(usize, usize)> = Vec::new();
    let mut matches: Vec<RuleMatch> = Vec::new();
    for rule in rules {
        if !rule.keywords.is_empty()
            && !rule
                .keywords
                .iter()
                .any(|keyword| lowered.contains(keyword))
        {
            continue;
        }
        for (start, end) in rule.find(text) {
            if overlaps(&accepted, start, end) {
                continue;
            }
            let position = accepted.partition_point(|(known, _)| *known < start);
            accepted.insert(position, (start, end));
            matches.push(RuleMatch {
                rule_id: rule.rule_id,
                category: rule.category,
                start,
                end,
                value: text[start..end].to_string(),
            });
        }
    }
    matches.sort_by_key(|item| item.start);
    matches
}

/// 已接受区间互不重叠且按起点有序，二分判断新区间是否冲突。
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

/// 把值尾部的标点 / 空白留在原文（只收缩区间，不改变前缀）。
fn trim_span(characters: &[char], start: usize, end: usize) -> usize {
    let mut end = end;
    while end > start && TRAILING_TRIM_CHARS.contains(characters[end - 1]) {
        end -= 1;
    }
    end
}

// ── 校验器与豁免谓词 ──────────────────────────────────────────────────────

/// 银行卡 Luhn 校验；同时排除位数不符与全同数字串。
pub fn is_luhn_valid(value: &str) -> bool {
    let digits: Vec<u32> = value
        .chars()
        .filter(|character| character.is_ascii_digit())
        .map(|character| character as u32 - '0' as u32)
        .collect();
    if digits.len() < 12 || digits.len() > 19 {
        return false;
    }
    if digits.iter().all(|digit| *digit == digits[0]) {
        return false;
    }
    let mut total = 0;
    let mut double = false;
    for digit in digits.iter().rev() {
        let mut digit = *digit;
        if double {
            digit *= 2;
            if digit > 9 {
                digit -= 9;
            }
        }
        total += digit;
        double = !double;
    }
    total % 10 == 0
}

/// 邮箱豁免：示例域名与 localhost 判为误报（对应 Python 的 `_EMAIL_ALLOWLIST`）。
///
/// Python 侧是在值里「搜」`@(?:example\.(?:com|org|net)|localhost)\b`，因此任意一个 `@` 命中即豁免。
pub fn is_example_domain(value: &str) -> bool {
    for (position, character) in value.char_indices() {
        if character != '@' {
            continue;
        }
        let domain = value[position + 1..].to_lowercase();
        for candidate in ["example.com", "example.org", "example.net", "localhost"] {
            if let Some(rest) = domain.strip_prefix(candidate) {
                // `\b`：域名后面不能再跟单词字符。
                if rest.is_empty() || !rest.chars().next().is_some_and(is_word) {
                    return true;
                }
            }
        }
    }
    false
}

// ── 手写匹配器 ────────────────────────────────────────────────────────────

/// `\b(?:https?|ftp)://[^\s"'<>`\\]+`（大小写不敏感）的等价实现。
fn find_url(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if is_word_boundary(characters, index) {
            let prefix = if starts_with_ignore_case(characters, index, "https://") {
                Some(8)
            } else if starts_with_ignore_case(characters, index, "http://")
                || starts_with_ignore_case(characters, index, "ftp://")
            {
                Some(7)
            } else {
                None
            };
            if let Some(length) = prefix {
                let mut end = index + length;
                while end < characters.len() && !is_url_stop(characters[end]) {
                    end += 1;
                }
                if end > index + length {
                    found.push((index, end));
                    index = end;
                    continue;
                }
            }
        }
        index += 1;
    }
    found
}

/// 邮箱：本地部分（1–64 个 `[A-Za-z0-9._%+-]`，左侧不能续接同类字符）+ 域名（标签链 + 字母 TLD）。
fn find_email(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] != '@' {
            index += 1;
            continue;
        }
        if let Some((start, end)) = email_at(characters, index) {
            if end > start {
                found.push((start, end));
                index = end;
                continue;
            }
        }
        index += 1;
    }
    found
}

fn email_at(characters: &[char], at: usize) -> Option<(usize, usize)> {
    let mut start = at;
    while start > 0 && is_email_local(characters[start - 1]) {
        start -= 1;
    }
    // `{1,64}` 上界叠加左侧否定环视：本地部分超过 64 个字符时任何起点都不成立。
    if start == at || at - start > 64 {
        return None;
    }
    // 域名取 `[A-Za-z0-9.-]` 的极大连续段，再按「贪婪标签链 + 末尾 TLD」裁剪。
    let mut run_end = at + 1;
    while run_end < characters.len() && is_domain_char(characters[run_end]) {
        run_end += 1;
    }
    if run_end == at + 1 {
        return None;
    }
    let domain_end = resolve_domain(characters, at + 1, run_end)?;
    Some((start, domain_end))
}

/// 域名结构：`label(\.label)*\.tld`，其中 tld 为 2–63 个字母。
///
/// 从「尽量多的标签」开始回溯（与正则的贪婪星号一致）；命中结束处右侧不能是 `[A-Za-z0-9-]`。
fn resolve_domain(characters: &[char], start: usize, run_end: usize) -> Option<usize> {
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut cursor = start;
    while cursor < run_end {
        let mut end = cursor;
        while end < run_end && characters[end] != '.' {
            end += 1;
        }
        segments.push((cursor, end));
        if end >= run_end {
            break;
        }
        cursor = end + 1;
        if cursor >= run_end {
            segments.push((cursor, run_end));
            break;
        }
    }
    if segments.len() < 2 {
        return None;
    }
    let last = segments.len() - 1;
    for tld_index in (1..=last).rev() {
        if !is_tld(&characters[segments[tld_index].0..segments[tld_index].1]) {
            continue;
        }
        if !segments[..tld_index]
            .iter()
            .all(|(begin, end)| is_label(&characters[*begin..*end]))
        {
            continue;
        }
        let end = segments[tld_index].1;
        // `(?![A-Za-z0-9-])`：段尾之后要么到文本末尾，要么不是字母数字 / 连字符（段内只会是 `.`）。
        if end >= characters.len() || !is_alnum_dash(characters[end]) {
            return Some(end);
        }
    }
    None
}

/// `(?<![\d.-])\d(?:[ -]?\d){11,18}(?![\d.-])` 的等价实现。
fn find_bank_card(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if !characters[index].is_ascii_digit()
            || (index > 0 && matches!(characters[index - 1], '0'..='9' | '.' | '-'))
        {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        let mut digits = 1;
        while digits < 19 {
            let step = match characters.get(end) {
                Some(character) if character.is_ascii_digit() => 1,
                Some(' ') | Some('-')
                    if characters
                        .get(end + 1)
                        .is_some_and(|character| character.is_ascii_digit()) =>
                {
                    2
                }
                _ => 0,
            };
            if step == 0 {
                break;
            }
            end += step;
            digits += 1;
        }
        if digits >= 12
            && !characters.get(end).is_some_and(|character| {
                character.is_ascii_digit() || matches!(character, '.' | '-')
            })
        {
            found.push((index, end));
            index = end;
            continue;
        }
        index += 1;
    }
    found
}

/// MAC 地址：`(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}` 或 `(?:[0-9A-Fa-f]{4}\.){2}[0-9A-Fa-f]{4}`。
fn find_mac_address(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if let Some(end) = mac_colon_form(characters, index) {
            found.push((index, end));
            index = end;
            continue;
        }
        if let Some(end) = mac_dotted_form(characters, index) {
            found.push((index, end));
            index = end;
            continue;
        }
        index += 1;
    }
    found
}

fn mac_colon_form(characters: &[char], start: usize) -> Option<usize> {
    if start > 0
        && matches!(characters[start - 1], '0'..='9' | 'A'..='F' | 'a'..='f' | ':' | '.' | '-')
    {
        return None;
    }
    let mut end = start;
    for _ in 0..5 {
        if !characters.get(end).is_some_and(|c| c.is_ascii_hexdigit())
            || !characters
                .get(end + 1)
                .is_some_and(|c| c.is_ascii_hexdigit())
        {
            return None;
        }
        if !matches!(characters.get(end + 2), Some(':') | Some('-')) {
            return None;
        }
        end += 3;
    }
    if !characters.get(end).is_some_and(|c| c.is_ascii_hexdigit())
        || !characters
            .get(end + 1)
            .is_some_and(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    end += 2;
    if characters
        .get(end)
        .is_some_and(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '-'))
    {
        return None;
    }
    Some(end)
}

fn mac_dotted_form(characters: &[char], start: usize) -> Option<usize> {
    if start > 0 && matches!(characters[start - 1], '0'..='9' | 'A'..='F' | 'a'..='f' | '.') {
        return None;
    }
    let mut end = start;
    for _ in 0..2 {
        for offset in 0..4 {
            if !characters
                .get(end + offset)
                .is_some_and(|c| c.is_ascii_hexdigit())
            {
                return None;
            }
        }
        if characters.get(end + 4) != Some(&'.') {
            return None;
        }
        end += 5;
    }
    for offset in 0..4 {
        if !characters
            .get(end + offset)
            .is_some_and(|c| c.is_ascii_hexdigit())
        {
            return None;
        }
    }
    end += 4;
    if characters
        .get(end)
        .is_some_and(|c| c.is_ascii_hexdigit() || *c == '.')
    {
        return None;
    }
    Some(end)
}

/// 中国大陆车牌：普通（省份 + 字母 + 5 位）与新能源（省份 + 字母 + D/F 类字母 + 5 位数字）。
const LICENSE_PLATE_PROVINCES: &str =
    "京津沪渝冀豫云辽黑湘皖鲁新苏浙赣鄂桂甘晋蒙陕吉闽贵粤青藏川宁琼使领";

fn find_license_plate(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if let Some(end) = plate_at(characters, index) {
            found.push((index, end));
            index = end;
            continue;
        }
        index += 1;
    }
    found
}

fn plate_at(characters: &[char], start: usize) -> Option<usize> {
    if start > 0 {
        let previous = characters[start - 1];
        if previous.is_ascii_alphanumeric() || is_cjk(previous) {
            return None;
        }
    }
    if !LICENSE_PLATE_PROVINCES.contains(*characters.get(start)?) {
        return None;
    }
    if !characters
        .get(start + 1)
        .is_some_and(|c| is_plate_letter(*c))
    {
        return None;
    }
    // 两个分支各自带尾部环视：普通号牌的 7 字形状后面若还跟着字母数字，先失败再交给新能源分支。
    plate_normal_end(characters, start)
        .filter(|end| plate_tail_boundary(characters, *end))
        .or_else(|| {
            plate_new_energy_end(characters, start)
                .filter(|end| plate_tail_boundary(characters, *end))
        })
}

/// 普通号牌：省份 + 字母 + 4 位 + 末位（含 挂 / 学 / 警 / 港 / 澳）。
fn plate_normal_end(characters: &[char], start: usize) -> Option<usize> {
    for offset in 0..4 {
        if !characters
            .get(start + 2 + offset)
            .is_some_and(|c| is_plate_tail(*c))
        {
            return None;
        }
    }
    let last = characters
        .get(start + 6)
        .is_some_and(|c| is_plate_tail(*c) || "挂学警港澳".contains(*c));
    last.then_some(start + 7)
}

/// 新能源号牌：省份 + 字母 + D/F 类字母 + 5 位数字。
fn plate_new_energy_end(characters: &[char], start: usize) -> Option<usize> {
    let third = characters.get(start + 2).copied()?;
    if !"DABCEFGHJK".contains(third) {
        return None;
    }
    let digits = (0..5).all(|offset| {
        characters
            .get(start + 3 + offset)
            .is_some_and(|c| c.is_ascii_digit())
    });
    digits.then_some(start + 8)
}

/// `(?![A-Za-z0-9])`。
fn plate_tail_boundary(characters: &[char], end: usize) -> bool {
    !characters
        .get(end)
        .is_some_and(|c| c.is_ascii_alphanumeric())
}

// ── PEM 私钥 ──────────────────────────────────────────────────────────────

/// PEM 头/尾里的类字符：`[ A-Z0-9_-]`（大小写不敏感）。
fn is_pem_class(character: char) -> bool {
    character == ' ' || character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
}

fn is_base64_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=')
}

fn is_line_break(character: char) -> bool {
    matches!(character, '\r' | '\n')
}

fn is_db_kv_value_char(character: char) -> bool {
    character != ';' && !is_line_break(character)
}

fn is_ipv6_char(character: char) -> bool {
    character.is_ascii_hexdigit() || character == ':'
}

fn is_db_uri_stop(character: char) -> bool {
    character.is_whitespace() || matches!(character, '"' | '\'' | '`' | '<' | '>')
}

fn is_ascii_word_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// 从 `from` 起连续满足 `accepts` 的字符段末位（下标语义同 Python 的切片）。
fn class_run_end(characters: &[char], from: usize, accepts: fn(char) -> bool) -> usize {
    let mut index = from;
    while characters.get(index).is_some_and(|value| accepts(*value)) {
        index += 1;
    }
    index
}

/// `[ A-Z0-9_-]*PRIVATE KEY[ A-Z0-9_-]*-----`：两段贪婪前缀各自从最长往短回退。
fn pem_key_tail_end(characters: &[char], from: usize) -> Option<usize> {
    let run_end = class_run_end(characters, from, is_pem_class);
    let marker = "PRIVATE KEY";
    let mut candidate = run_end as isize - marker.len() as isize;
    while candidate >= from as isize {
        let start = candidate as usize;
        if starts_with_ignore_case(characters, start, marker) {
            let tail = start + marker.len();
            let mut dashes = run_end as isize;
            while dashes >= tail as isize + 5 {
                let position = dashes as usize - 5;
                if (0..5).all(|offset| characters.get(position + offset) == Some(&'-')) {
                    return Some(position + 5);
                }
                dashes -= 1;
            }
        }
        candidate -= 1;
    }
    None
}

/// `-----BEGIN[ A-Z0-9_-]*PRIVATE KEY[ A-Z0-9_-]*-----` 的结束位置。
fn pem_header_end(characters: &[char], start: usize) -> Option<usize> {
    if !starts_with_ignore_case(characters, start, "-----BEGIN") {
        return None;
    }
    pem_key_tail_end(characters, start + 10)
}

/// PEM 私钥块：BEGIN 头 + 惰性任意内容 + 最早的 END 尾。
fn find_pem_block(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if let Some(header_end) = pem_header_end(characters, index) {
            let mut cursor = header_end;
            while cursor < characters.len() {
                if starts_with_ignore_case(characters, cursor, "-----END") {
                    if let Some(end) = pem_key_tail_end(characters, cursor + 8) {
                        found.push((index, end));
                        index = end;
                        break;
                    }
                }
                cursor += 1;
            }
            if cursor < characters.len() {
                continue;
            }
        }
        index += 1;
    }
    found
}

/// PEM 私钥头 + 后续 base64 正文行（无 END 的截断场景）。
fn find_pem_body(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if let Some(header_end) = pem_header_end(characters, index) {
            let mut cursor = header_end;
            let mut matched = false;
            loop {
                let line_start = class_run_end(characters, cursor, is_line_break);
                if line_start == cursor {
                    break;
                }
                let line_end = class_run_end(characters, line_start, is_base64_char);
                let length = line_end - line_start;
                if length < 16 {
                    break;
                }
                cursor = line_start + length.min(76);
                matched = true;
            }
            if matched {
                found.push((index, cursor));
                index = cursor;
                continue;
            }
        }
        index += 1;
    }
    found
}

// ── 数据库连接串 ──────────────────────────────────────────────────────────

/// 顺序即正则里的分支顺序（先命中先算）。
const DB_URI_SCHEMES: [&str; 25] = [
    "postgresql",
    "postgres",
    "pgsql",
    "mysql",
    "mariadb",
    "mongodb+srv",
    "mongodb",
    "redis",
    "rediss",
    "amqps",
    "amqp",
    "mssql",
    "sqlserver",
    "oracle",
    "clickhouse",
    "elasticsearch",
    "cassandra",
    "neo4j",
    "sqlite",
    "cockroachdb",
    "db2",
    "h2",
    "influxdb",
    "memcached",
    "etcd",
];

/// `(?:jdbc|r2dbc):` 可选前缀 + 协议名 + 可选 `+驱动` + `://` + 非空白体。
fn find_db_uri(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let mut matched = None;
        // 可选的协议前缀是贪婪组：先试 `jdbc:`，再 `r2dbc:`，最后不带前缀。
        for prefix in [Some("jdbc:"), Some("r2dbc:"), None] {
            let start = match prefix {
                Some(text) => {
                    if !starts_with_ignore_case(characters, index, text) {
                        continue;
                    }
                    index + text.len()
                }
                None => index,
            };
            if let Some(end) = db_uri_after_protocol(characters, start) {
                matched = Some(end);
                break;
            }
        }
        if let Some(end) = matched {
            found.push((index, end));
            index = end;
            continue;
        }
        index += 1;
    }
    found
}

fn db_uri_after_protocol(characters: &[char], start: usize) -> Option<usize> {
    for scheme in DB_URI_SCHEMES {
        if !starts_with_ignore_case(characters, start, scheme) {
            continue;
        }
        let after_scheme = start + scheme.len();
        let mut ends = Vec::new();
        if characters.get(after_scheme) == Some(&'+') {
            let driver_end = class_run_end(characters, after_scheme + 1, is_ascii_word_char);
            if driver_end > after_scheme + 1 {
                ends.push(driver_end);
            }
        }
        ends.push(after_scheme);
        for end in ends {
            if !starts_with_ignore_case(characters, end, "://") {
                continue;
            }
            let body_start = end + 3;
            let body_end = class_run_end(characters, body_start, |character| {
                !is_db_uri_stop(character)
            });
            if body_end > body_start {
                return Some(body_end);
            }
        }
    }
    None
}

/// ADO / .NET 键值连接串：`key = value; … password = value`。
fn find_db_key_value(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if let Some(end) = db_key_value_at(characters, index) {
            found.push((index, end));
            index = end;
            continue;
        }
        index += 1;
    }
    found
}

fn db_key_value_at(characters: &[char], start: usize) -> Option<usize> {
    // `\b(?:server|data\s*source|host|addr|address)`。
    if start > 0 && is_word(characters[start - 1]) {
        return None;
    }
    if !characters
        .get(start)
        .is_some_and(|character| is_word(*character))
    {
        return None;
    }
    for key in ["server", "host", "addr", "address", "datasource"] {
        let after_key = if key == "datasource" {
            if !starts_with_ignore_case(characters, start, "data") {
                continue;
            }
            let source_start =
                class_run_end(characters, start + 4, |character| character.is_whitespace());
            if !starts_with_ignore_case(characters, source_start, "source") {
                continue;
            }
            source_start + 6
        } else {
            if !starts_with_ignore_case(characters, start, key) {
                continue;
            }
            start + key.len()
        };
        if let Some(end) = db_kv_after_key(characters, after_key) {
            return Some(end);
        }
    }
    None
}

/// `\s*=\s*[^;\r\n]{1,200};[^\r\n]{0,400}?(?:password|pwd)\s*=\s*[^;\r\n]{1,200}`。
fn db_kv_after_key(characters: &[char], after_key: usize) -> Option<usize> {
    let equals = class_run_end(characters, after_key, |character| character.is_whitespace());
    if characters.get(equals) != Some(&'=') {
        return None;
    }
    let value_start = class_run_end(characters, equals + 1, |character| {
        character.is_whitespace()
    });
    let value_end = class_run_end(characters, value_start, is_db_kv_value_char);
    let length = value_end - value_start;
    if length == 0 || length > 200 || characters.get(value_end) != Some(&';') {
        return None;
    }
    db_kv_password_end(characters, value_end + 1)
}

fn db_kv_password_end(characters: &[char], from: usize) -> Option<usize> {
    let mut cursor = from;
    while cursor <= from + 400 && cursor < characters.len() {
        if is_line_break(characters[cursor]) {
            return None;
        }
        for key in ["password", "pwd"] {
            if !starts_with_ignore_case(characters, cursor, key) {
                continue;
            }
            let equals = class_run_end(characters, cursor + key.len(), |character| {
                character.is_whitespace()
            });
            if characters.get(equals) != Some(&'=') {
                continue;
            }
            let value_start = class_run_end(characters, equals + 1, |character| {
                character.is_whitespace()
            });
            let value_end = class_run_end(characters, value_start, is_db_kv_value_char);
            if value_end > value_start {
                return Some(value_start + (value_end - value_start).min(200));
            }
        }
        cursor += 1;
    }
    None
}

// ── IP 地址 ───────────────────────────────────────────────────────────────

// 下列表逐条对齐 Python 3.9 `ipaddress` 的常量（`_private_networks` 等），
// 判定语义与 Python 一致：内网 = 私有或链路本地（排除环回 / 未指定 / 组播）；
// 外网 = 全球可达（IPv4 还要排除 100.64/10 这一非全球段）。

/// `IPv4Address._private_networks`：(网络地址, 前缀长度)。
const V4_PRIVATE: [(u32, u8); 14] = [
    (0, 8),
    (167772160, 8),
    (2130706432, 8),
    (2851995648, 16),
    (2886729728, 12),
    (3221225472, 29),
    (3221225642, 31),
    (3221225984, 24),
    (3232235520, 16),
    (3323068416, 15),
    (3325256704, 24),
    (3405803776, 24),
    (4026531840, 4),
    (4294967295, 32),
];
const V4_SHARED: (u32, u8) = (1681915904, 10);
const V4_LINK_LOCAL: (u32, u8) = (2851995648, 16);
const V4_LOOPBACK: (u32, u8) = (2130706432, 8);
const V4_MULTICAST: (u32, u8) = (3758096384, 4);

/// `IPv6Address._private_networks`：(网络地址, 前缀长度)。
const V6_PRIVATE: [(u128, u8); 10] = [
    (0x00000000000000000000000000000001, 128),
    (0x00000000000000000000000000000000, 128),
    (0x00000000000000000000ffff00000000, 96),
    (0x01000000000000000000000000000000, 64),
    (0x20010000000000000000000000000000, 23),
    (0x20010002000000000000000000000000, 48),
    (0x20010db8000000000000000000000000, 32),
    (0x20010010000000000000000000000000, 28),
    (0xfc000000000000000000000000000000, 7),
    (0xfe800000000000000000000000000000, 10),
];
const V6_LINK_LOCAL: (u128, u8) = (0xfe800000000000000000000000000000, 10);
const V6_MULTICAST: (u128, u8) = (0xff000000000000000000000000000000, 8);

fn v4_in(network: (u32, u8), value: u32) -> bool {
    let mask = if network.1 == 0 {
        0
    } else {
        u32::MAX << (32 - network.1)
    };
    (value & mask) == network.0
}

fn v6_in(network: (u128, u8), value: u128) -> bool {
    let mask = if network.1 == 0 {
        0
    } else {
        u128::MAX << (128 - network.1)
    };
    (value & mask) == network.0
}

struct IpFlags {
    loopback: bool,
    unspecified: bool,
    multicast: bool,
    private: bool,
    link_local: bool,
    global: bool,
}

fn classify_ip(value: &str) -> Option<IpFlags> {
    if let Ok(address) = value.parse::<Ipv4Addr>() {
        // `to_bits()` 要到 Rust 1.80；工程声明的 MSRV 是 1.75，用 From 转换（1.26 起稳定）。
        let bits = u32::from(address);
        let private = V4_PRIVATE.iter().any(|network| v4_in(*network, bits));
        return Some(IpFlags {
            loopback: v4_in(V4_LOOPBACK, bits),
            unspecified: bits == 0,
            multicast: v4_in(V4_MULTICAST, bits),
            private,
            link_local: v4_in(V4_LINK_LOCAL, bits),
            global: !v4_in(V4_SHARED, bits) && !private,
        });
    }
    let address: Ipv6Addr = value.parse().ok()?;
    let bits = u128::from(address);
    let private = V6_PRIVATE.iter().any(|network| v6_in(*network, bits));
    Some(IpFlags {
        loopback: bits == 1,
        unspecified: bits == 0,
        multicast: v6_in(V6_MULTICAST, bits),
        private,
        link_local: v6_in(V6_LINK_LOCAL, bits),
        global: !private,
    })
}

/// 内网 / 链路本地（排除环回、未指定、组播）。
pub fn is_internal_ip(value: &str) -> bool {
    let Some(flags) = classify_ip(value) else {
        return false;
    };
    if flags.loopback || flags.unspecified || flags.multicast {
        return false;
    }
    flags.private || flags.link_local
}

/// 公网可路由（排除私有 / 环回 / 链路本地 / 组播）。
pub fn is_external_ip(value: &str) -> bool {
    let Some(flags) = classify_ip(value) else {
        return false;
    };
    if flags.loopback || flags.unspecified || flags.multicast {
        return false;
    }
    flags.global
}

/// `(?<![\w.:])`：左侧不能是单词字符、`.`、`:`。
fn ip_lookbehind_ok(characters: &[char], start: usize) -> bool {
    match start.checked_sub(1).and_then(|index| characters.get(index)) {
        Some(character) => !is_word(*character) && !matches!(character, '.' | ':'),
        None => true,
    }
}

/// `(?![\w:])(?!\.\d)`：右侧不能是单词字符 / `:`，也不能是「点 + 数字」。
fn ip_lookahead_ok(characters: &[char], end: usize) -> bool {
    match characters.get(end) {
        Some(character) => {
            if is_word(*character) || *character == ':' {
                return false;
            }
            !(*character == '.'
                && characters
                    .get(end + 1)
                    .is_some_and(|next| next.is_ascii_digit()))
        }
        None => true,
    }
}

/// 点分十进制 IPv4（先于 IPv6 分支尝试）。
fn ipv4_end(characters: &[char], start: usize) -> Option<usize> {
    let mut cursor = start;
    for octet in 0..4 {
        let octet_end = ipv4_octet_end(characters, cursor)?;
        cursor = octet_end;
        if octet < 3 {
            if characters.get(cursor) != Some(&'.') {
                return None;
            }
            cursor += 1;
        }
    }
    ip_lookahead_ok(characters, cursor).then_some(cursor)
}

/// `(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)`：1–3 位、无前导零、数值 ≤ 255。
fn ipv4_octet_end(characters: &[char], start: usize) -> Option<usize> {
    let run_end = class_run_end(characters, start, |character| character.is_ascii_digit());
    let length = run_end - start;
    if length == 0 || length > 3 {
        return None;
    }
    let digits: String = characters[start..run_end].iter().collect();
    if length > 1 && digits.starts_with('0') {
        return None;
    }
    (digits.parse::<u32>().ok()? <= 255).then_some(run_end)
}

/// IPv6：`[0-9A-Fa-f:]+` 段整体套用正则的九个分支形态。
///
/// 段内字符只可能是 hex 或 `:`，而两个尾部环视都禁止「后面紧跟 hex 或 `:`」，
/// 所以命中终点只能是该段的末尾——不必逐个终点评分支。
fn ipv6_end(characters: &[char], start: usize) -> Option<usize> {
    let run_end = class_run_end(characters, start, is_ipv6_char);
    if run_end == start {
        return None;
    }
    let token: String = characters[start..run_end].iter().collect();
    if !is_ipv6_shape(&token) {
        return None;
    }
    ip_lookahead_ok(characters, run_end).then_some(run_end)
}

fn is_ipv6_shape(token: &str) -> bool {
    if let Some((left, right)) = token.split_once("::") {
        if right.contains("::") {
            return false;
        }
        let left_groups = match group_count(left) {
            Some(count) => count,
            None => return false,
        };
        let right_groups = match group_count(right) {
            Some(count) => count,
            None => return false,
        };
        return left_groups + right_groups <= 7;
    }
    if let Some(body) = token.strip_suffix(':') {
        return matches!(group_count(body), Some(count) if (1..=7).contains(&count));
    }
    group_count(token) == Some(8)
}

/// `hex(:hex)*` 的组数；任一组成空或不是 1–4 位十六进制时返回 `None`。
fn group_count(body: &str) -> Option<usize> {
    if body.is_empty() {
        return Some(0);
    }
    let mut count = 0;
    for group in body.split(':') {
        if group.is_empty() || group.len() > 4 || !group.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        count += 1;
    }
    Some(count)
}

fn find_ip_address(characters: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if ip_lookbehind_ok(characters, index) {
            let matched = ipv4_end(characters, index).or_else(|| ipv6_end(characters, index));
            if let Some(end) = matched {
                found.push((index, end));
                index = end;
                continue;
            }
        }
        index += 1;
    }
    found
}

// ── 字符判定工具 ──────────────────────────────────────────────────────────

fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_cjk(character: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&character)
}

fn is_plate_letter(character: char) -> bool {
    character.is_ascii_uppercase() && !matches!(character, 'I' | 'O')
}

fn is_plate_tail(character: char) -> bool {
    is_plate_letter(character) || character.is_ascii_digit()
}

fn is_email_local(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '%' | '+' | '-')
}

fn is_domain_char(character: char) -> bool {
    is_alnum_dash(character) || character == '.'
}

/// `[A-Za-z0-9-]`（域名标签的字符集；`.` 只作分隔符）。
fn is_alnum_dash(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '-'
}

fn is_label(characters: &[char]) -> bool {
    if characters.is_empty() || characters.len() > 63 {
        return false;
    }
    let first = characters[0];
    let last = characters[characters.len() - 1];
    first.is_ascii_alphanumeric()
        && last.is_ascii_alphanumeric()
        && characters
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == '-')
}

fn is_tld(characters: &[char]) -> bool {
    (2..=63).contains(&characters.len()) && characters.iter().all(|c| c.is_ascii_alphabetic())
}

fn is_url_stop(character: char) -> bool {
    character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>' | '`' | '\\')
}

/// Python `\b`：位置两侧的「单词性」不同。
fn is_word_boundary(characters: &[char], index: usize) -> bool {
    let current = characters.get(index).is_some_and(|c| is_word(*c));
    let previous = index > 0 && is_word(characters[index - 1]);
    current != previous
}

fn starts_with_ignore_case(characters: &[char], index: usize, expected: &str) -> bool {
    let mut cursor = index;
    for character in expected.chars() {
        match characters.get(cursor) {
            Some(value) if value.eq_ignore_ascii_case(&character) => cursor += 1,
            _ => return false,
        }
    }
    true
}

// ── 扫描结果缓存（长会话里每轮重扫同一批未变历史） ────────────────────────
//
// 扫描是纯函数（规则 + 文本 → 区间），可跨请求复用；掩码阶段仍按周期分配占位符，
// 缓存不参与占位符语义，也不保存原文之外的任何内容。

pub const SCAN_CACHE_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const SCAN_CACHE_MAX_TEXT_CHARS: usize = 256 * 1024;

/// 扫描缓存计数（不含任何文本）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCacheStats {
    pub entries: usize,
    pub size_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

struct ScanCacheEntry {
    matches: Vec<RuleMatch>,
    cost: usize,
}

#[derive(Default)]
struct ScanCacheState {
    order: Vec<(String, u64)>,
    entries: HashMap<(String, u64), ScanCacheEntry>,
    size_bytes: usize,
}

/// 按 (文本, 规则集合) 记忆扫描结果的 LRU，按字节预算淘汰条目。
pub struct ScanCache {
    max_bytes: usize,
    max_text_chars: usize,
    state: Mutex<ScanCacheState>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl Default for ScanCache {
    fn default() -> Self {
        Self::new(SCAN_CACHE_MAX_BYTES, SCAN_CACHE_MAX_TEXT_CHARS)
    }
}

impl ScanCache {
    pub fn new(max_bytes: usize, max_text_chars: usize) -> Self {
        Self {
            max_bytes,
            max_text_chars,
            state: Mutex::new(ScanCacheState::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    pub fn clear(&self) {
        let mut state = scan_lock(&self.state);
        state.order.clear();
        state.entries.clear();
        state.size_bytes = 0;
    }

    pub fn stats(&self) -> ScanCacheStats {
        let state = scan_lock(&self.state);
        ScanCacheStats {
            entries: state.entries.len(),
            size_bytes: state.size_bytes,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    fn get(&self, text: &str, rules: &[PatternRule]) -> Option<Vec<RuleMatch>> {
        if text.chars().count() > self.max_text_chars {
            return None;
        }
        let key = (text.to_string(), rules_identity(rules));
        let mut state = scan_lock(&self.state);
        if let Some(entry) = state.entries.remove(&key) {
            let matches = entry.matches.clone();
            state.order.retain(|item| item != &key);
            state.order.push(key.clone());
            state.entries.insert(key, entry);
            drop(state);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(matches);
        }
        drop(state);
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn put(&self, text: &str, rules: &[PatternRule], matches: &[RuleMatch]) {
        if text.chars().count() > self.max_text_chars {
            return;
        }
        let cost = entry_bytes(text, matches.len());
        if cost > self.max_bytes {
            return;
        }
        let key = (text.to_string(), rules_identity(rules));
        let mut state = scan_lock(&self.state);
        if let Some(previous) = state.entries.remove(&key) {
            state.size_bytes = state.size_bytes.saturating_sub(previous.cost);
            state.order.retain(|item| item != &key);
        }
        state.size_bytes += cost;
        state.order.push(key.clone());
        state.entries.insert(
            key,
            ScanCacheEntry {
                matches: matches.to_vec(),
                cost,
            },
        );
        while state.size_bytes > self.max_bytes && !state.order.is_empty() {
            let Some(evicted) = state.order.first().cloned() else {
                break;
            };
            state.order.remove(0);
            if let Some(entry) = state.entries.remove(&evicted) {
                state.size_bytes = state.size_bytes.saturating_sub(entry.cost);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn scan_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 规则集合的身份：指针 + 长度 + 首尾规则 id（避免地址复用导致错配）。
fn rules_identity(rules: &[PatternRule]) -> u64 {
    let mut hasher = DefaultHasher::new();
    (rules.as_ptr() as usize).hash(&mut hasher);
    rules.len().hash(&mut hasher);
    if let Some(first) = rules.first() {
        first.rule_id.hash(&mut hasher);
    }
    if let Some(last) = rules.last() {
        last.rule_id.hash(&mut hasher);
    }
    hasher.finish()
}

/// 条目占用估算：文本本体 + 每个匹配对象与字符串切片的固定开销。
fn entry_bytes(text: &str, matches: usize) -> usize {
    49 + text.chars().count() * 4 + matches * 96 + 64
}

/// 按规则优先级扫描文本；命中缓存时直接复用区间。
pub fn scan_pattern_rules_cached(
    text: &str,
    rules: &[PatternRule],
    cache: &ScanCache,
) -> Vec<RuleMatch> {
    if text.is_empty() || rules.is_empty() {
        return Vec::new();
    }
    if let Some(cached) = cache.get(text, rules) {
        return cached;
    }
    let matches = scan_pattern_rules(text, rules);
    cache.put(text, rules, &matches);
    matches
}

// ── 扫描缓存的行为测试 ────────────────────────────────────────────────────

#[cfg(test)]
mod scan_cache_tests {
    use super::*;

    #[test]
    fn scan_cache_reuses_and_counts() {
        let cache = ScanCache::new(1024 * 1024, 4096);
        let rules = builtin_rules();
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        let first = scan_pattern_rules_cached(text, rules, &cache);
        assert!(!first.is_empty(), "私钥应命中");
        let second = scan_pattern_rules_cached(text, rules, &cache);
        assert_eq!(first, second, "命中缓存后结果一致");
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn scan_cache_skips_oversized_text() {
        let cache = ScanCache::new(1024 * 1024, 4);
        let rules = builtin_rules();
        let _ = scan_pattern_rules_cached("-----BEGIN RSA PRIVATE KEY-----", rules, &cache);
        assert_eq!(cache.stats().entries, 0, "超过文本上限不入缓存");
    }

    #[test]
    fn scan_cache_evicts_by_budget() {
        let cache = ScanCache::new(600, 4096);
        let rules = builtin_rules();
        for index in 0..4 {
            let text = format!("user{index}@corp{index}.cn");
            let _ = scan_pattern_rules_cached(&text, rules, &cache);
        }
        let stats = cache.stats();
        assert!(stats.evictions > 0, "超出预算后应发生淘汰");
        assert!(stats.size_bytes <= 600, "淘汰后占用回到预算内");
    }
}
