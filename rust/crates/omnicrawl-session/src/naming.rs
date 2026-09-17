//! 会话命名与路径校验：会话 id、事件类型、转录相对路径、标题。
//!
//! 语义基准是 Python `session_models.py` 的对应函数，包括错误文案。
//! `*_from_json` 是对外（读文件、读协议载荷）的边界入口，负责 Python 那边的类型检查；
//! 类型化版本供内部调用，不重复做 JSON 类型判断。

use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::SessionStoreError;

pub const SESSION_EVENT_VERSION: u32 = 1;
/// 压缩摘要写回会话时使用的正文前缀。
pub const COMPACT_SUMMARY_PREFIX: &str = "会话压缩摘要：\n";
pub const MESSAGE_EVENT_TYPES: &[&str] = &["user_message", "assistant_message"];
pub const MODEL_CONTEXT_EVENT_TYPES: &[&str] = &[
    "user_message",
    "assistant_message",
    "compact_summary",
    "tool_call_requested",
    "tool_call_denied",
    "tool_result",
    "run_guard_paused",
];
pub const EMPTY_SESSION_EVENT_TYPES: &[&str] = &["session_started", "session_closed"];
pub const SUBAGENT_EVENT_TYPES: &[&str] = &[
    "subagent_batch_created",
    "subagent_task_queued",
    "subagent_task_started",
    "subagent_task_waiting_approval",
    "subagent_task_completed",
    "subagent_task_failed",
    "subagent_task_cancelled",
];

const SESSION_ID_LENGTH: usize = 8 + 1 + 6 + 1 + 6;
const TITLE_LIMIT: usize = 60;
const TITLE_PREFIX: usize = 57;
const EVENT_TYPE_LIMIT: usize = 64;

/// 生成会话 id：`YYYYMMDD-HHMMSS-xxxxxx`（时间取 UTC，与 Python 侧一致）。
pub fn new_session_id(now: DateTime<Utc>) -> String {
    let stamp = now.with_timezone(&Utc).format("%Y%m%d-%H%M%S");
    format!("{stamp}-{}", random_suffix())
}

pub fn session_id_from_json(value: &Value) -> Result<String, SessionStoreError> {
    let Some(text) = value.as_str() else {
        return Err(SessionStoreError::new("session_id 必须是非空字符串。"));
    };
    if text.trim().is_empty() {
        return Err(SessionStoreError::new("session_id 必须是非空字符串。"));
    }
    normalize_session_id(text)
}

pub fn normalize_session_id(raw: &str) -> Result<String, SessionStoreError> {
    let session_id = raw.trim();
    if session_id.is_empty() {
        return Err(SessionStoreError::new("session_id 必须是非空字符串。"));
    }
    if !is_session_id(session_id) {
        return Err(SessionStoreError::new(format!(
            "session_id 格式无效：{raw}"
        )));
    }
    Ok(session_id.to_string())
}

pub fn event_type_from_json(value: &Value) -> Result<String, SessionStoreError> {
    let Some(text) = value.as_str() else {
        return Err(SessionStoreError::new("会话事件 type 必须是非空字符串。"));
    };
    if text.trim().is_empty() {
        return Err(SessionStoreError::new("会话事件 type 必须是非空字符串。"));
    }
    normalize_event_type(text)
}

pub fn normalize_event_type(raw: &str) -> Result<String, SessionStoreError> {
    let event_type = raw.trim();
    if event_type.is_empty() {
        return Err(SessionStoreError::new("会话事件 type 必须是非空字符串。"));
    }
    if !is_event_type(event_type) {
        return Err(SessionStoreError::new(format!(
            "会话事件 type 格式无效：{raw}"
        )));
    }
    Ok(event_type.to_string())
}

pub fn path_from_json(value: &Value) -> Result<String, SessionStoreError> {
    let Some(text) = value.as_str() else {
        return Err(SessionStoreError::new("会话路径必须是非空字符串。"));
    };
    if text.trim().is_empty() {
        return Err(SessionStoreError::new("会话路径必须是非空字符串。"));
    }
    normalize_relative_file_path(text)
}

/// 校验并规范化转录文件路径：必须是不带 `..`、以 `.jsonl` 结尾的相对路径。
pub fn normalize_relative_file_path(raw: &str) -> Result<String, SessionStoreError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SessionStoreError::new("会话路径必须是非空字符串。"));
    }
    let normalized = trimmed.replace('\\', "/");
    let path = Path::new(&normalized);
    let escapes_root = path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir));
    if escapes_root {
        return Err(SessionStoreError::new(format!(
            "会话路径必须是安全相对路径：{raw}"
        )));
    }
    if !is_jsonl_name(path) {
        return Err(SessionStoreError::new(format!(
            "会话转录文件必须是 JSONL：{raw}"
        )));
    }
    Ok(normalized)
}

/// 标题折叠：空白压缩成单空格，超长按字符（不是字节）截断。
pub fn clean_title(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > TITLE_LIMIT {
        let prefix: String = collapsed.chars().take(TITLE_PREFIX).collect();
        return format!("{prefix}...");
    }
    collapsed
}

/// 载荷里的计数：不是「非负整数」时按 0 处理（对应 Python `read_payload_non_negative_int`）。
pub fn read_payload_non_negative_int(value: &Value) -> u64 {
    if value.is_boolean() {
        return 0;
    }
    value.as_u64().unwrap_or(0)
}

fn is_session_id(session_id: &str) -> bool {
    if session_id.len() != SESSION_ID_LENGTH {
        return false;
    }
    let bytes = session_id.as_bytes();
    let digits =
        |range: std::ops::Range<usize>| bytes[range].iter().all(|byte| byte.is_ascii_digit());
    let hex = |range: std::ops::Range<usize>| {
        bytes[range]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    digits(0..8) && bytes[8] == b'-' && digits(9..15) && bytes[15] == b'-' && hex(16..22)
}

fn is_event_type(event_type: &str) -> bool {
    let mut chars = event_type.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    if rest.len() > EVENT_TYPE_LIMIT - 1 {
        return false;
    }
    rest.iter().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || *character == '_'
    })
}

/// 对应 Python `Path.suffix.lower() == ".jsonl"`：取文件名里最后一个点之后的部分，
/// 且点前必须有名字（`.jsonl` 这种隐藏文件在 Python 里没有后缀）。
fn is_jsonl_name(path: &Path) -> bool {
    let Some(Some(name)) = path.file_name().map(|name| name.to_str()) else {
        return false;
    };
    match name.rsplit_once('.') {
        Some((stem, suffix)) => !stem.is_empty() && suffix.eq_ignore_ascii_case("jsonl"),
        None => false,
    }
}

/// 12 字节随机后缀的十六进制写法（与 Python `secrets.token_hex(3)`/`token_hex(12)` 同形）。
///
/// 这里不是密码学随机源：事件 id 与会话 id 只需要在一台机器上不撞车，
/// 所以用时间戳 + 进程 id + 计数器做散列。凭据绝不走这条路径。
fn random_suffix() -> String {
    let mut digest = random_digest();
    digest.truncate(3);
    hex(&digest)
}

pub(crate) fn random_event_id() -> String {
    let mut digest = random_digest();
    digest.truncate(12);
    hex(&digest)
}

fn random_digest() -> Vec<u8> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = Sha256::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(std::thread::current().name().unwrap_or_default().as_bytes());
    hasher.finalize().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
