//! 长期记忆的存储契约：Markdown 记录格式、目录/路径归一化、索引条目字段校验。
//!
//! 语义基准是 Python `omnicrawl/state/memory.py` 的格式层：记忆正文落成 Markdown
//! （frontmatter 带 timestamp 与 related_directories），目录名做安全归一化，
//! 索引条目 `index.json` 记录 id/path/timestamp/touch_count 等。
//! 本片只搬数据契约与文件格式；记忆的读写、检索与排序在后续切片。
//!
//! 归一化里的字符过滤照搬 Python 的字符类，用逐字符判断实现（不引正则依赖）。

use std::path::Path;

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, SecondsFormat, Utc};
use serde_json::{json, Map, Value};

use crate::error::SessionStoreError;

/// 记忆正文里允许保留的目录字符之外一律折成 `-`（对应 Python 的字符类）。
fn is_directory_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || character == '_'
        || character == '.'
        || character == '-'
        || ('\u{4e00}'..='\u{9fff}').contains(&character)
}

fn is_filename_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '.' || character == '-'
}

/// 目录名归一化：反斜杠折成正斜杠、空白折成 `-`、片段过滤后小写。
pub fn normalize_directory(raw_directory: &str) -> Result<String, SessionStoreError> {
    let mut directory = raw_directory.trim().replace('\\', "/");
    directory = collapse_whitespace(&directory, '-');
    directory = collapse_char(&directory, '/', '/');
    let directory = directory.trim_matches('/').to_string();
    if directory.is_empty() || directory == "." || directory == ".." {
        return Err(SessionStoreError::new("记忆目录不能为空或为相对跳转目录。"));
    }
    if directory.starts_with("../") || directory.contains("/../") || directory.ends_with("/..") {
        return Err(SessionStoreError::new(format!(
            "记忆目录不能包含上级跳转：{raw_directory}"
        )));
    }
    if is_absolute_directory(&directory) {
        return Err(SessionStoreError::new(format!(
            "记忆目录必须是相对路径：{raw_directory}"
        )));
    }

    let mut segments = Vec::new();
    for segment in directory.split('/') {
        let safe = filter_chars(segment, is_directory_char)
            .trim_matches(|character| character == '.' || character == '-')
            .to_string();
        if safe.is_empty() {
            continue;
        }
        segments.push(safe.to_lowercase());
    }
    if segments.is_empty() {
        return Err(SessionStoreError::new(format!(
            "记忆目录无有效片段：{raw_directory}"
        )));
    }
    Ok(segments.join("/"))
}

/// 记忆文件相对路径归一化：必须以 `.md` 结尾，目录按目录规则归一化、文件名保留大小写。
pub fn normalize_relative_file_path(raw_path: &str) -> Result<String, SessionStoreError> {
    let path = raw_path.trim().replace('\\', "/");
    if !path.ends_with(".md") {
        return Err(SessionStoreError::new(format!(
            "记忆文件必须是 Markdown：{raw_path}"
        )));
    }
    let (parent, filename) = split_parent(&path);
    let directory = normalize_directory(&parent)?;
    let safe_name = filter_chars(&filename, is_filename_char)
        .trim_matches(|character| character == '.' || character == '-')
        .to_string();
    if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
        return Err(SessionStoreError::new(format!(
            "记忆文件名无效：{raw_path}"
        )));
    }
    Ok(format!("{directory}/{safe_name}"))
}

/// 关联目录去重：无效条目视为「不限定目录」直接跳过，而不是让整次调用失败。
pub fn dedupe_directories(directories: &[Value]) -> Vec<String> {
    let mut result = Vec::new();
    for directory in directories {
        let Some(text) = directory.as_str() else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let Ok(normalized) = normalize_directory(text) else {
            continue;
        };
        if result.contains(&normalized) {
            continue;
        }
        result.push(normalized);
    }
    result
}

pub fn dedupe_strings(values: &[Value]) -> Vec<String> {
    let mut result = Vec::new();
    for value in values {
        let Some(text) = value.as_str() else {
            continue;
        };
        let cleaned = text.trim();
        if cleaned.is_empty() || result.iter().any(|existing| existing == cleaned) {
            continue;
        }
        result.push(cleaned.to_string());
    }
    result
}

/// 正文归一化：换行统一成 `\n` 并去掉首尾空白。
pub fn normalize_content(content: &str) -> String {
    content
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

/// 记忆 Markdown：frontmatter（timestamp + 关联目录）+ 空行 + 正文 + 结尾换行。
pub fn format_memory_markdown(
    timestamp: DateTime<FixedOffset>,
    related_directories: &[String],
    content: &str,
) -> String {
    let mut lines = vec![
        "---".to_string(),
        format!("timestamp: \"{}\"", format_memory_datetime(timestamp)),
        "related_directories:".to_string(),
    ];
    for directory in related_directories {
        lines.push(format!("  - \"{}\"", escape_frontmatter_string(directory)));
    }
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push(content.trim_end().to_string());
    lines.push(String::new());
    lines.join("\n")
}

/// 从记忆 Markdown 里取出正文：没有 frontmatter 时返回整份文本（同样去空白）。
pub fn read_markdown_body(path: &Path) -> Result<String, SessionStoreError> {
    let text = std::fs::read(path).map_err(|error| {
        SessionStoreError::new(format!("读取记忆文件失败：{}，{error}", path.display()))
    })?;
    let text = String::from_utf8(text).map_err(|_| {
        SessionStoreError::new(format!("记忆文件不是 UTF-8 文本：{}", path.display()))
    })?;
    Ok(body_of(&text))
}

/// `read_markdown_body` 的纯文本版本，便于测试与内存调用。
pub fn body_of(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else {
        return normalized.trim().to_string();
    };
    match rest.find("\n---") {
        Some(index) => rest[index + 4..].trim().to_string(),
        None => normalized.trim().to_string(),
    }
}

/// 记忆时间戳渲染：转成本地时区、精确到秒（Python `isoformat(timespec="seconds")`）。
pub fn format_memory_datetime(value: DateTime<FixedOffset>) -> String {
    value
        .with_timezone(&Local)
        .to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// 解析记忆时间戳：无时区按 UTC，随后转本地时区（与 Python `astimezone()` 一致）。
pub fn parse_memory_datetime(raw: &str) -> Result<DateTime<FixedOffset>, SessionStoreError> {
    let normalized = raw.replace('Z', "+00:00");
    let parsed = DateTime::parse_from_rfc3339(&normalized)
        .map(|value| value.with_timezone(&Local).fixed_offset())
        .or_else(|_| {
            NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|naive| naive.and_utc().with_timezone(&Local).fixed_offset())
        });
    parsed.map_err(|_| SessionStoreError::new(format!("记忆时间戳格式无效：{raw}")))
}

/// 索引条目：Markdown 只存三部分（时间戳、关联目录、正文），其余元数据落在索引里。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryIndexEntry {
    pub id: String,
    pub path: String,
    pub storage_directory: String,
    pub timestamp: DateTime<FixedOffset>,
    pub touch_count: u64,
    pub related_directories: Vec<String>,
    pub summary: String,
}

impl MemoryIndexEntry {
    pub fn from_dict(data: &Value) -> Result<Self, SessionStoreError> {
        let Some(object) = data.as_object() else {
            return Err(SessionStoreError::new("记忆索引缺少字段：id。"));
        };
        for field in ["id", "path", "storage_directory", "timestamp"] {
            if !object.contains_key(field) {
                return Err(SessionStoreError::new(format!(
                    "记忆索引缺少字段：{field}。"
                )));
            }
        }
        // 字段校验顺序与 Python 一致：先逐项检查类型，最后才做路径/目录/时间戳归一化。
        let id = match object.get("id").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => return Err(SessionStoreError::new("记忆索引字段 id 必须是非空字符串。")),
        };
        let raw_path = match object.get("path").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => text,
            _ => {
                return Err(SessionStoreError::new(format!(
                    "记忆 {id} 的 path 必须是非空字符串。"
                )))
            }
        };
        let raw_storage_directory = match object.get("storage_directory").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => text,
            _ => {
                return Err(SessionStoreError::new(format!(
                    "记忆 {id} 的 storage_directory 必须是非空字符串。"
                )))
            }
        };
        let raw_timestamp = match object.get("timestamp").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => text,
            _ => {
                return Err(SessionStoreError::new(format!(
                    "记忆 {id} 的 timestamp 必须是非空字符串。"
                )))
            }
        };
        let related = match object.get("related_directories") {
            None => Vec::new(),
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => items.clone(),
            Some(_) => {
                return Err(SessionStoreError::new(format!(
                    "记忆 {id} 的 related_directories 必须是字符串列表。"
                )))
            }
        };
        let touch_count = match object.get("touch_count") {
            None => 0,
            Some(value) if !value.is_boolean() => value.as_u64().ok_or_else(|| {
                SessionStoreError::new(format!("记忆 {id} 的 touch_count 必须是非负整数。"))
            })?,
            Some(_) => {
                return Err(SessionStoreError::new(format!(
                    "记忆 {id} 的 touch_count 必须是非负整数。"
                )))
            }
        };
        let summary = match object.get("summary") {
            None => String::new(),
            Some(Value::String(text)) => text.trim().to_string(),
            Some(_) => {
                return Err(SessionStoreError::new(format!(
                    "记忆 {id} 的 summary 必须是字符串。"
                )))
            }
        };

        Ok(Self {
            id,
            path: normalize_relative_file_path(raw_path)?,
            storage_directory: normalize_directory(raw_storage_directory)?,
            timestamp: parse_memory_datetime(raw_timestamp)?,
            related_directories: dedupe_directories(&related),
            touch_count,
            summary,
        })
    }

    pub fn to_dict(&self) -> Value {
        let mut object = Map::new();
        object.insert("id".to_string(), Value::String(self.id.clone()));
        object.insert("path".to_string(), Value::String(self.path.clone()));
        object.insert(
            "storage_directory".to_string(),
            Value::String(self.storage_directory.clone()),
        );
        object.insert(
            "timestamp".to_string(),
            Value::String(format_memory_datetime(self.timestamp)),
        );
        object.insert("touch_count".to_string(), json!(self.touch_count));
        object.insert(
            "related_directories".to_string(),
            json!(self.related_directories),
        );
        object.insert("summary".to_string(), Value::String(self.summary.clone()));
        Value::Object(object)
    }
}

/// frontmatter 字符串转义：反斜杠与双引号（对应 Python `_escape_frontmatter_string`）。
fn escape_frontmatter_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\"', "\\\"")
}

/// 记忆目录名的安全字符集之外折成 `-`（连续折叠由调用方按需处理）。
fn filter_chars(text: &str, allowed: fn(char) -> bool) -> String {
    text.chars()
        .map(|character| if allowed(character) { character } else { '-' })
        .collect()
}

/// 连续空白折成 `replacement`（对应 Python `re.sub(r"\s+", "-", ...)`）。
fn collapse_whitespace(text: &str, replacement: char) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_run = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !in_run {
                result.push(replacement);
                in_run = true;
            }
        } else {
            result.push(character);
            in_run = false;
        }
    }
    result
}

/// 连续重复字符折成一个（对应 Python `re.sub(r"/{2,}", "/", ...)`）。
fn collapse_char(text: &str, target: char, replacement: char) -> String {
    let mut result = String::with_capacity(text.len());
    let mut previous_was_target = false;
    for character in text.chars() {
        if character == target {
            if previous_was_target {
                continue;
            }
            previous_was_target = true;
            result.push(replacement);
        } else {
            previous_was_target = false;
            result.push(character);
        }
    }
    result
}

/// 盘符或根斜杠开头（对应 Python 的 `^[a-zA-Z]:/` 与 `startswith("/")`）。
fn is_absolute_directory(directory: &str) -> bool {
    if directory.starts_with('/') {
        return true;
    }
    let mut characters = directory.chars();
    matches!(characters.next(), Some(letter) if letter.is_ascii_alphabetic())
        && characters.next() == Some(':')
        && characters.next() == Some('/')
}

/// 拆出父目录与文件名（对应 `Path(path).parent` / `Path(path).name`）。
fn split_parent(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(index) => (path[..index].to_string(), path[index + 1..].to_string()),
        None => (".".to_string(), path.to_string()),
    }
}

/// 便于测试与调用方拿到 UTC 视角（跨机比对时把时间戳归一化到 UTC）。
pub fn to_utc(value: DateTime<FixedOffset>) -> DateTime<Utc> {
    value.with_timezone(&Utc)
}
