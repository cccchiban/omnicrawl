//! 时间戳：对齐 Python `datetime` 的 ISO-8601 语义（UTC、微秒精度）。
//!
//! 两个实现读写同一批会话文件，所以格式必须逐字兼容：Python 的 `isoformat()` 只在
//! 微秒非零时输出小数位，且（3.9 的 `fromisoformat`）只认 3 位或 6 位小数。
//! 因此这里一律按微秒截断、输出 6 位小数，绝不写出纳秒。

use chrono::{DateTime, NaiveDate, NaiveDateTime, Timelike, Utc};
use serde_json::Value;

use crate::error::SessionStoreError;

/// 解析时间戳字符串；不带时区的写法按 UTC 处理（对应 Python 的 `ensure_timezone`）。
pub fn parse_datetime(raw: &str) -> Result<DateTime<Utc>, SessionStoreError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SessionStoreError::new("时间戳必须是非空字符串。"));
    }

    // Python 侧先把 `Z` 换成 `+00:00`，分隔符允许任意字符；这里兜住常见的空格与小写 t。
    let mut normalized = trimmed.replace('Z', "+00:00");
    if matches!(normalized.as_bytes().get(10), Some(b' ' | b't')) {
        normalized.replace_range(10..11, "T");
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(&normalized) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return Ok(naive.and_utc());
    }
    // Python 的 fromisoformat 也接受省略秒与省略时间的写法（补 0 到午夜/整分）。
    if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M") {
        return Ok(naive.and_utc());
    }
    if let Ok(date) = NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("午夜时间恒定可用")
            .and_utc());
    }
    Err(invalid(raw))
}

/// JSON 边界上的时间戳：非字符串或空白串给出与 Python 相同的文案。
pub fn datetime_from_json(value: &Value) -> Result<DateTime<Utc>, SessionStoreError> {
    match value.as_str() {
        Some(text) if !text.trim().is_empty() => parse_datetime(text),
        _ => Err(SessionStoreError::new("时间戳必须是非空字符串。")),
    }
}

/// 输出 Python `isoformat()` 的等价写法（UTC、微秒非零才带 6 位小数）。
pub fn format_datetime(value: DateTime<Utc>) -> String {
    let value = value.with_timezone(&Utc);
    if value.timestamp_subsec_micros() == 0 {
        value.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        value.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// 毫秒时间戳。Python 是 `int(value.timestamp() * 1000)`（向零截断），
/// 因此用微秒整除而不是 `timestamp_millis()`（后者对 1970 前的时间向负无穷截断）。
pub fn datetime_to_millis(value: DateTime<Utc>) -> i64 {
    value.timestamp_micros() / 1000
}

pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// 截断到微秒：会话文件里只有微秒精度，内存中的事件不应带着写不出去也不可比对的纳秒。
pub fn truncate_to_micros(value: DateTime<Utc>) -> DateTime<Utc> {
    let micros = value.timestamp_subsec_micros();
    value
        .with_nanosecond(micros * 1_000)
        .unwrap_or_else(|| value.with_nanosecond(0).expect("纳秒归零恒定可用"))
}

fn invalid(raw: &str) -> SessionStoreError {
    SessionStoreError::new(format!("时间戳格式无效：{raw}"))
}
