//! 跨子系统共用的凭据脱敏：对齐 Python `omnicrawl/common/redaction.py`。
//!
//! 文本侧按固定顺序做六轮替换（私钥块 → 赋值式 → authorization 赋值 → Bearer →
//! 厂商密钥 → GitHub 令牌 → 云 access key）；结构侧递归清理常见凭据字段。
//!
//! Python 用正则表达，这里用等价的手写匹配器实现（字符类与量词都是固定形态），不引正则依赖。

use serde_json::{Map, Value};

/// 凭据容器的稳定语义名；保持精确可避免误删 keyboard、monkey 这类正常字段。
const SENSITIVE_FIELD_NAMES: [&str; 12] = [
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

/// 递归脱敏常见密钥字段，供审计与事件载荷使用。
pub fn redact_sensitive_values(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut redacted = Map::new();
            for (key, item) in map {
                let normalized = key.trim().to_lowercase().replace('-', "_");
                let is_header_secret = normalized
                    .strip_prefix("x_")
                    .is_some_and(|rest| SENSITIVE_FIELD_NAMES.contains(&rest));
                let replacement =
                    if SENSITIVE_FIELD_NAMES.contains(&normalized.as_str()) || is_header_secret {
                        Value::String("***".to_string())
                    } else {
                        redact_sensitive_values(item)
                    };
                redacted.insert(key.clone(), replacement);
            }
            Value::Object(redacted)
        }
        // 只保留前 100 项，与 Python 一致（避免超长列表拖慢审计路径）。
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(100)
                .map(redact_sensitive_values)
                .collect(),
        ),
        Value::String(text) => Value::String(redact_sensitive_text(text)),
        other => other.clone(),
    }
}

/// 清理文本中的常见明文凭据（赋值、Bearer、厂商密钥、私钥、云凭据）。
pub fn redact_sensitive_text(text: &str) -> String {
    let redacted = replace_private_keys(text);
    let redacted = replace_assignments(&redacted);
    let redacted = replace_authorization(&redacted);
    let redacted = replace_bearer(&redacted);
    let redacted = replace_provider_secrets(&redacted);
    let redacted = replace_github_secrets(&redacted);
    replace_aws_access_keys(&redacted)
}

/// `-----BEGIN [TYPE ]PRIVATE KEY-----` 到最近的对应 `END` 之间整段替换。
fn replace_private_keys(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN") {
        let Some(body) = rest.get(start..) else {
            break;
        };
        let Some(header_len) = match_key_header(body) else {
            // 不是私钥块的头，原样保留这三个字符再继续找。
            result.push_str(&rest[..start + 9]);
            rest = &rest[start + 9..];
            continue;
        };
        let end = match_key_footer(&body[header_len..]);
        let Some(footer_len) = end else {
            result.push_str(&rest[..start + header_len]);
            rest = &rest[start + header_len..];
            continue;
        };
        result.push_str(&rest[..start]);
        result.push_str("*** PRIVATE KEY REDACTED ***");
        rest = &body[header_len + footer_len..];
    }
    result.push_str(rest);
    result
}

/// 匹配 `-----BEGIN( [A-Z0-9]+)? PRIVATE KEY-----`，返回头部长度。
///
/// 可选组是「贪婪 + 回溯」：先试带类型（` RSA PRIVATE KEY`），失败再试不带类型
/// （` PRIVATE KEY`）——只做贪婪会让不带类型的私钥头匹配不上。
fn match_key_header(body: &str) -> Option<usize> {
    let rest = body.strip_prefix("-----BEGIN")?;
    for offset in optional_type_offsets(rest) {
        if rest[offset..].starts_with(" PRIVATE KEY-----") {
            return Some("-----BEGIN".len() + offset + " PRIVATE KEY-----".len());
        }
    }
    None
}

/// 匹配 `-----END( [A-Z0-9]+)? PRIVATE KEY-----`，返回从起点到结尾的长度。
fn match_key_footer(text: &str) -> Option<usize> {
    let start = text.find("-----END")?;
    let rest = &text[start + "-----END".len()..];
    for offset in optional_type_offsets(rest) {
        if rest[offset..].starts_with(" PRIVATE KEY-----") {
            return Some(start + "-----END".len() + offset + " PRIVATE KEY-----".len());
        }
    }
    None
}

/// 可选类型组的候选偏移：先给「带类型」的贪心结果，再给「不带类型」的 0。
fn optional_type_offsets(rest: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    if rest.starts_with(' ') {
        let mut cursor = 1;
        while rest
            .chars()
            .nth(cursor)
            .is_some_and(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        {
            cursor += 1;
        }
        if cursor > 1 {
            offsets.push(cursor);
        }
    }
    offsets.push(0);
    offsets
}

/// `(api_key|token|password|…)\s*[:=]\s*["']?<值>` → 保留键、值换成 `***`。
fn replace_assignments(text: &str) -> String {
    replace_matches(text, |haystack, index| {
        let (key_end, _key) = match_assignment_key(haystack, index)?;
        let after_key = skip_whitespace(haystack, key_end);
        if !matches!(
            haystack.get(after_key..after_key + 1),
            Some(":") | Some("=")
        ) {
            return None;
        }
        let after_sign = skip_whitespace(haystack, after_key + 1);
        let value_start = match_optional_quote(haystack, after_sign);
        let value_end = take_while(haystack, value_start, |character| {
            !matches!(character, '"' | '\'' | ',' | ';') && !character.is_whitespace()
        });
        if value_end == value_start {
            return None;
        }
        Some((value_end, format!("{}***", &haystack[index..value_start])))
    })
}

/// `authorization\s*[:=]\s*["']?Bearer?\s*<值>` → 保留前缀、值换成 `Bearer ***`。
fn replace_authorization(text: &str) -> String {
    replace_matches(text, |haystack, index| {
        let (key_end, _key) = match_authorization_key(haystack, index)?;
        let after_key = skip_whitespace(haystack, key_end);
        if !matches!(
            haystack.get(after_key..after_key + 1),
            Some(":") | Some("=")
        ) {
            return None;
        }
        let after_sign = skip_whitespace(haystack, after_key + 1);
        let quoted = match_optional_quote(haystack, after_sign);
        let scheme = {
            let candidate = &haystack[quoted..];
            if starts_with_ignore_ascii_case(candidate, "bearer") {
                let after_scheme =
                    take_while(haystack, quoted + 6, |character| character.is_whitespace());
                if after_scheme > quoted + 6 {
                    after_scheme
                } else {
                    quoted
                }
            } else {
                quoted
            }
        };
        let value_end = take_while(haystack, scheme, |character| {
            !matches!(character, '"' | '\'' | ',' | ';') && !character.is_whitespace()
        });
        if value_end == scheme {
            return None;
        }
        Some((value_end, format!("{}Bearer ***", &haystack[index..quoted])))
    })
}

/// `\bBearer\s+[A-Za-z0-9._~+/=-]{8,}` → `Bearer ***`。
fn replace_bearer(text: &str) -> String {
    replace_matches(text, |haystack, index| {
        let candidate = &haystack[index..];
        if !starts_with_ignore_ascii_case(candidate, "bearer") {
            return None;
        }
        if previous_is_word_char(haystack, index) {
            return None;
        }
        let after_scheme = take_while(haystack, index + 6, |character| character.is_whitespace());
        if after_scheme == index + 6 {
            return None;
        }
        let end = take_while(haystack, after_scheme, is_bearer_char);
        if end - after_scheme < 8 {
            return None;
        }
        Some((end, "Bearer ***".to_string()))
    })
}

/// `\b(?:sk|ak|ah)-[A-Za-z0-9_-]{24,}\b` → `***`。
fn replace_provider_secrets(text: &str) -> String {
    replace_matches(text, |haystack, index| {
        if previous_is_word_char(haystack, index) {
            return None;
        }
        let candidate = &haystack[index..];
        let prefix_matches = ["sk-", "ak-", "ah-"]
            .iter()
            .any(|prefix| candidate.starts_with(prefix));
        if !prefix_matches {
            return None;
        }
        let end = take_while(haystack, index + 3, is_provider_char);
        if end - (index + 3) < 24 {
            return None;
        }
        if next_is_word_char(haystack, end) {
            return None;
        }
        Some((end, "***".to_string()))
    })
}

/// `\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b` → `***`。
fn replace_github_secrets(text: &str) -> String {
    replace_matches(text, |haystack, index| {
        if previous_is_word_char(haystack, index) {
            return None;
        }
        let candidate = &haystack[index..];
        let mut matched: Option<usize> = None;
        if candidate.len() >= 4
            && candidate.starts_with("gh")
            && matches!(
                candidate.as_bytes().get(2).copied(),
                Some(b'p' | b'o' | b'u' | b's' | b'r')
            )
            && candidate.as_bytes().get(3) == Some(&b'_')
        {
            let end = take_while(haystack, index + 4, |character| {
                character.is_ascii_alphanumeric()
            });
            if end - (index + 4) >= 20 {
                matched = Some(end);
            }
        }
        if matched.is_none() && candidate.starts_with("github_pat_") {
            let end = take_while(haystack, index + 11, |character| {
                character.is_ascii_alphanumeric() || character == '_'
            });
            if end - (index + 11) >= 20 {
                matched = Some(end);
            }
        }
        let end = matched?;
        if next_is_word_char(haystack, end) {
            return None;
        }
        Some((end, "***".to_string()))
    })
}

/// `\b(?:AKIA|ASIA|…)[A-Z0-9]{16}\b` → `***`。
fn replace_aws_access_keys(text: &str) -> String {
    const PREFIXES: [&str; 8] = [
        "AKIA", "ASIA", "AIDA", "AROA", "AIPA", "ANPA", "ANVA", "ASCA",
    ];
    replace_matches(text, |haystack, index| {
        if previous_is_word_char(haystack, index) {
            return None;
        }
        let candidate = &haystack[index..];
        if !PREFIXES.iter().any(|prefix| candidate.starts_with(prefix)) {
            return None;
        }
        for offset in 4..20 {
            let byte = haystack.as_bytes().get(index + offset).copied();
            let ok = byte.is_some_and(|value| value.is_ascii_uppercase() || value.is_ascii_digit());
            if !ok {
                return None;
            }
        }
        let end = index + 20;
        if next_is_word_char(haystack, end) {
            return None;
        }
        Some((end, "***".to_string()))
    })
}

/// 通用替换驱动：在每个位置尝试匹配，命中则写入替换文本并跳过匹配区间。
fn replace_matches<F>(text: &str, matcher: F) -> String
where
    F: Fn(&str, usize) -> Option<(usize, String)>,
{
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if !text.is_char_boundary(index) {
            index += 1;
            continue;
        }
        match matcher(text, index) {
            Some((end, replacement)) if end > index => {
                result.push_str(&replacement);
                index = end;
            }
            _ => {
                let character = text[index..].chars().next().unwrap_or_default();
                result.push(character);
                index += character.len_utf8();
            }
        }
    }
    result
}

/// 赋值式键名（`api_key`、`token`、`password` 等，允许 `-`/`_` 变体）。
fn match_assignment_key(haystack: &str, index: usize) -> Option<(usize, String)> {
    const KEYS: [&str; 11] = [
        "apikey",
        "accesskey",
        "secretkey",
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "cookie",
        "password",
        "secret",
        "token",
        "authorization",
    ];
    // 键名很短，只取前缀做小写比较：对整段剩余文本小写会让长文本退化成 O(n²)。
    let candidate: String = haystack[index..]
        .chars()
        .take(32)
        .collect::<String>()
        .to_lowercase();
    for key in KEYS {
        if key == "authorization" {
            continue;
        }
        if let Some(consumed) = match_flexible_key(&candidate, key) {
            return Some((index + consumed, key.to_string()));
        }
        if key == "apikey" {
            // `api[_-]?key` 允许中间带分隔符。
            if let Some(consumed) = match_separated_key(&candidate, "api", "key", 3) {
                return Some((index + consumed, key.to_string()));
            }
        }
        for (prefix, suffix) in [
            ("access", "key"),
            ("secret", "key"),
            ("access", "token"),
            ("refresh", "token"),
            ("id", "token"),
        ] {
            if key == format!("{prefix}{suffix}") {
                if let Some(consumed) = match_separated_key(&candidate, prefix, suffix, 1) {
                    return Some((index + consumed, key.to_string()));
                }
            }
        }
    }
    None
}

fn match_authorization_key(haystack: &str, index: usize) -> Option<(usize, String)> {
    // 键名很短，只取前缀做小写比较：对整段剩余文本小写会让长文本退化成 O(n²)。
    let candidate: String = haystack[index..]
        .chars()
        .take(32)
        .collect::<String>()
        .to_lowercase();
    if index > 0 && is_word_char(haystack.as_bytes()[index - 1] as char) {
        return None;
    }
    let consumed = match_flexible_key(&candidate, "authorization")?;
    Some((index + consumed, "authorization".to_string()))
}

/// 匹配小写化后的固定键名（允许 `-`/`_` 与键名中的分隔符位置一致）。
fn match_flexible_key(candidate: &str, key: &str) -> Option<usize> {
    let compact: String = candidate
        .chars()
        .take_while(|character| character.is_alphanumeric() || matches!(character, '_' | '-'))
        .collect();
    let normalized: String = compact.replace(&['_', '-'][..], "");
    if normalized.starts_with(key) {
        // 回推原始长度：键名长度加上被去掉的分隔符个数。
        let mut consumed = 0;
        let mut letters = 0;
        for character in compact.chars() {
            consumed += character.len_utf8();
            if !matches!(character, '_' | '-') {
                letters += 1;
            }
            if letters == key.len() {
                return Some(consumed);
            }
        }
    }
    None
}

/// 匹配 `prefix[_-]?suffix`（中间最多一个分隔符）。
fn match_separated_key(
    candidate: &str,
    prefix: &str,
    suffix: &str,
    _padding: usize,
) -> Option<usize> {
    if !candidate.starts_with(prefix) {
        return None;
    }
    let mut consumed = prefix.len();
    if matches!(
        candidate.as_bytes().get(consumed).copied(),
        Some(b'_' | b'-')
    ) {
        consumed += 1;
    }
    if candidate[consumed..].starts_with(suffix) {
        return Some(consumed + suffix.len());
    }
    None
}

fn match_optional_quote(haystack: &str, index: usize) -> usize {
    match haystack.as_bytes().get(index).copied() {
        Some(b'"' | b'\'') => index + 1,
        _ => index,
    }
}

fn skip_whitespace(haystack: &str, index: usize) -> usize {
    take_while(haystack, index, |character| character.is_whitespace())
}

fn take_while<F>(haystack: &str, index: usize, predicate: F) -> usize
where
    F: Fn(char) -> bool,
{
    let mut cursor = index;
    while let Some(character) = haystack[cursor..].chars().next() {
        if !predicate(character) {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

/// 前缀比较（忽略 ASCII 大小写）；长度不足或不在字符边界时返回 false。
fn starts_with_ignore_ascii_case(text: &str, needle: &str) -> bool {
    text.get(..needle.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle))
}

/// 前一个字符是否是单词字符（对应 `` 的左侧判断）。
fn previous_is_word_char(text: &str, index: usize) -> bool {
    text.get(..index)
        .and_then(|prefix| prefix.chars().next_back())
        .is_some_and(is_word_char)
}

/// 后一个字符是否是单词字符（对应 `` 的右侧判断）。
fn next_is_word_char(text: &str, index: usize) -> bool {
    text.get(index..)
        .and_then(|rest| rest.chars().next())
        .is_some_and(is_word_char)
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_bearer_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(character, '.' | '_' | '~' | '+' | '/' | '=' | '-')
}

fn is_provider_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
}
