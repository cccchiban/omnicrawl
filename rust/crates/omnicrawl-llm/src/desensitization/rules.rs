//! 值类型规则层：按形态识别「无键名的敏感值类型」，产出待脱敏区间。
//!
//! 语义基准是 Python `omnicrawl/llm/desensitization/rules.py`。结构感知层（键名规则）要求值
//! 处在有键名的结构里，熵兜底层要求值具备高随机性；本层覆盖两者之间的「形态确定、随机性低」
//! 的类型。内核**不引入正则依赖**（项目决定）：每条规则的正则等价物都是手写匹配器，逐条用
//! 对照数据集验证（`tests/desensitization_rules_parity.rs`）。
//!
//! 已搬：网址、邮箱、银行卡（Luhn）、MAC 地址、大陆车牌，以及整套规则语义——关键字预过滤、
//! 熵下限、校验器、豁免表、停用词、尾部标点留在原文、重叠区间先命中先占位、结果按起点排序。
//!
//! 未搬：PEM 私钥、数据库连接串、内外网 IP（这三类要做多行懒匹配与 `ipaddress` 分类表，
//! 另行一片）、gitleaks 规则表（`secret_group` / 整段豁免要等它一起补）、`locality` 局部化
//! 扫描与扫描结果缓存（纯性能优化，不影响语义）。

use std::collections::HashMap;

use super::char_offsets;

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

/// 手写匹配器：Python 侧是正则，内核侧是等价实现（不引入 `regex` 依赖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMatcher {
    Url,
    Email,
    BankCard,
    MacAddress,
    LicensePlate,
}

/// 值级豁免 / 校验谓词（Python 侧是正则与函数对象）。
pub type ValuePredicate = fn(&str) -> bool;

/// 单条值类型规则：手写匹配器 + 关键字 / 熵 / 校验器 / 豁免表。
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

/// 内置值类型规则（顺序即优先级；重叠区间由先者占位）。**仅含已搬的类别**。
pub fn builtin_rules() -> Vec<PatternRule> {
    vec![
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
    ]
}

/// 按启用类别裁剪内置规则。Python 侧按配置字段名裁剪并追加 gitleaks 规则；内核侧由调用方
/// 给类别集合，gitleaks 追加留到 gitleaks 片。
pub fn build_enabled_rules(categories: &[&str]) -> Vec<PatternRule> {
    builtin_rules()
        .into_iter()
        .filter(|rule| categories.contains(&rule.category))
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

fn find_candidates(matcher: RuleMatcher, characters: &[char]) -> Vec<(usize, usize)> {
    match matcher {
        RuleMatcher::Url => find_url(characters),
        RuleMatcher::Email => find_email(characters),
        RuleMatcher::BankCard => find_bank_card(characters),
        RuleMatcher::MacAddress => find_mac_address(characters),
        RuleMatcher::LicensePlate => find_license_plate(characters),
    }
}

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
