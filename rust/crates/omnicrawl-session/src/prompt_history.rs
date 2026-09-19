//! 用户提示历史的数据模型与 JSONL 存储：对齐 Python `omnicrawl/state/prompt_history.py`。
//!
//! `.agent_sessions/history.jsonl` 一行一条提示。写入走耐久追加（由 `SessionStore` 统一持锁，
//! 这里只负责追加语义）；读取时坏行不阻断其余有效记录，尾部半行降级为 warning。
//!
//! 与 Python 的一处差异（crate README 有记录）：`json_error` 明细是各自 JSON 库的错误文本；
//! `log_record_diagnostics` 的调用点由调用方决定怎么记（内核没有 logging 设施）。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::error::SessionStoreError;
use crate::locking::append_text_line;
use crate::naming::normalize_session_id;
use crate::project::normalize_project_path;
use crate::records::{
    split_lines_python, SessionRecordDiagnostic, DIAG_INVALID_JSON, DIAG_NOT_OBJECT,
    DIAG_PROMPT_INVALID, DIAG_TRAILING_INCOMPLETE, SEVERITY_ERROR, SEVERITY_WARNING,
};
use crate::redaction::{redact_sensitive_text, redact_sensitive_values};
use crate::time::{datetime_to_millis, utc_now};

pub const MAX_PROMPT_HISTORY_DISPLAY_CHARS: usize = 4000;

/// `.agent_sessions/history.jsonl` 中的一条用户提示历史。
#[derive(Debug, Clone, PartialEq)]
pub struct PromptHistoryEntry {
    pub display: String,
    pub timestamp: i64,
    pub project: String,
    pub session_id: String,
    pub pasted_contents: Map<String, Value>,
}

impl PromptHistoryEntry {
    pub fn create(
        display: &str,
        project: &str,
        session_id: &str,
        pasted_contents: Option<Map<String, Value>>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Self, SessionStoreError> {
        Ok(Self {
            display: redact_sensitive_text(&clean_prompt_display(display)),
            timestamp: datetime_to_millis(now.unwrap_or_else(utc_now)),
            project: normalize_project_path(project)?,
            session_id: normalize_session_id(session_id)?,
            pasted_contents: redact_map(pasted_contents.unwrap_or_default()),
        })
    }

    /// 从 `history.jsonl` 的一行构造：读取时也返回安全的展示数据，但不改写用户已有 JSONL。
    pub fn from_dict(data: &Value) -> Result<Self, SessionStoreError> {
        let Some(object) = data.as_object() else {
            return Err(SessionStoreError::new(
                "提示历史 display 必须是非空字符串。",
            ));
        };

        let display = object
            .get("display")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        if !display
            .as_str()
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
        {
            return Err(SessionStoreError::new(
                "提示历史 display 必须是非空字符串。",
            ));
        }

        let timestamp = object.get("timestamp").cloned().unwrap_or(Value::Null);
        let timestamp = match &timestamp {
            Value::Number(number) => number.as_i64(),
            _ => None,
        };
        let Some(timestamp) = timestamp.filter(|value| *value >= 0) else {
            return Err(SessionStoreError::new(
                "提示历史 timestamp 必须是非负整数。",
            ));
        };

        let project = object
            .get("project")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        if !project
            .as_str()
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false)
        {
            return Err(SessionStoreError::new(
                "提示历史 project 必须是非空字符串。",
            ));
        }

        let pasted = object
            .get("pasted_contents")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let Value::Object(pasted) = pasted else {
            return Err(SessionStoreError::new(
                "提示历史 pasted_contents 必须是 JSON 对象。",
            ));
        };

        let session_id = object
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default();

        Ok(Self {
            display: redact_sensitive_text(&clean_prompt_display(
                display.as_str().expect("已校验"),
            )),
            timestamp,
            project: project.as_str().expect("已校验").trim().to_string(),
            session_id: normalize_session_id(session_id)?,
            pasted_contents: redact_map(pasted),
        })
    }

    pub fn created_at(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp_millis(self.timestamp)
    }

    pub fn to_dict(&self) -> Value {
        json!({
            "display": self.display,
            "timestamp": self.timestamp,
            "project": self.project,
            "session_id": self.session_id,
            "pasted_contents": Value::Object(self.pasted_contents.clone()),
        })
    }
}

/// 用户提示历史 JSONL 存储。
pub struct PromptHistoryStore {
    path: PathBuf,
    root: PathBuf,
    fsync: bool,
}

impl PromptHistoryStore {
    pub fn open(path: impl AsRef<Path>, fsync: bool) -> Self {
        let raw = path.as_ref().to_string_lossy().to_string();
        let resolved = match normalize_project_path(&raw) {
            Ok(resolved) => PathBuf::from(resolved),
            Err(_) => PathBuf::from(raw),
        };
        let root = resolved
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| resolved.clone());
        Self {
            path: resolved,
            root,
            fsync,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure(&self) -> Result<(), SessionStoreError> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            SessionStoreError::new(format!("创建目录失败：{}，{error}", self.root.display()))
        })?;
        if !self.path.exists() {
            std::fs::write(&self.path, "").map_err(|error| {
                SessionStoreError::new(format!(
                    "写入提示历史失败：{}，{error}",
                    self.path.display()
                ))
            })?;
        }
        Ok(())
    }

    /// 追加用户提示；空提示会被忽略并返回 `None`。
    pub fn append(
        &self,
        display: &str,
        project: &str,
        session_id: &str,
        pasted_contents: Option<Map<String, Value>>,
        now: Option<DateTime<Utc>>,
    ) -> Result<Option<PromptHistoryEntry>, SessionStoreError> {
        let cleaned = clean_prompt_display(display);
        if cleaned.is_empty() {
            return Ok(None);
        }
        self.ensure()?;
        let entry =
            PromptHistoryEntry::create(&cleaned, project, session_id, pasted_contents, now)?;
        let line = serde_json::to_string(&entry.to_dict()).expect("提示历史可序列化");
        append_text_line(&self.path, &line, self.fsync).map_err(|error| {
            SessionStoreError::new(format!(
                "写入提示历史失败：{}，{error}",
                self.path.display()
            ))
        })?;
        Ok(Some(entry))
    }

    /// 查询提示历史，返回按时间倒序排列的去重结果。
    pub fn search(
        &self,
        project: Option<&str>,
        session_id: Option<&str>,
        query: &str,
        limit: i64,
    ) -> Result<Vec<PromptHistoryEntry>, SessionStoreError> {
        let (mut entries, _diagnostics) = self.read_entries_with_diagnostics()?;
        if let Some(project) = project {
            let project_root = normalize_project_path(project)?;
            entries.retain(|entry| entry.project == project_root);
        }
        if let Some(raw) = session_id.filter(|value| !value.trim().is_empty()) {
            let normalized = normalize_session_id(raw)?;
            entries.retain(|entry| entry.session_id == normalized);
        }

        let keyword = query.trim().to_lowercase();
        if !keyword.is_empty() {
            entries.retain(|entry| entry.display.to_lowercase().contains(&keyword));
        }

        let mut indexed: Vec<(usize, &PromptHistoryEntry)> = entries.iter().enumerate().collect();
        indexed.sort_by_key(|item| std::cmp::Reverse((item.1.timestamp, item.0)));

        let cap = limit.clamp(1, 100) as usize;
        let mut seen: Vec<String> = Vec::new();
        let mut results: Vec<PromptHistoryEntry> = Vec::new();
        for (_index, entry) in indexed {
            let dedupe_key = entry.display.to_lowercase();
            if seen.contains(&dedupe_key) {
                continue;
            }
            seen.push(dedupe_key);
            results.push(entry.clone());
            if results.len() >= cap {
                break;
            }
        }
        Ok(results)
    }

    /// 读取提示历史并返回结构化诊断；坏行不阻断其余有效记录。
    pub fn read_entries_with_diagnostics(
        &self,
    ) -> Result<(Vec<PromptHistoryEntry>, Vec<SessionRecordDiagnostic>), SessionStoreError> {
        if !self.path.exists() {
            return Ok((Vec::new(), Vec::new()));
        }
        let bytes = std::fs::read(&self.path).map_err(|error| {
            SessionStoreError::new(format!(
                "读取提示历史失败：{}，{error}",
                self.path.display()
            ))
        })?;
        let raw_text = String::from_utf8(bytes).map_err(|_| {
            SessionStoreError::new(format!("提示历史不是 UTF-8 文本：{}", self.path.display()))
        })?;

        let lines = split_lines_python(&raw_text);
        let file_ends_with_newline = raw_text.ends_with('\n') || raw_text.is_empty();
        let last_nonempty = lines.iter().rposition(|line| !line.trim().is_empty());
        let display_path = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut entries: Vec<PromptHistoryEntry> = Vec::new();
        let mut diagnostics: Vec<SessionRecordDiagnostic> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let (entry, line_diagnostics) = decode_line(
                line,
                &display_path,
                index + 1,
                Some(index) == last_nonempty,
                file_ends_with_newline,
            );
            diagnostics.extend(line_diagnostics);
            if let Some(entry) = entry {
                entries.push(entry);
            }
        }
        Ok((entries, diagnostics))
    }
}

fn decode_line(
    line: &str,
    path: &str,
    line_no: usize,
    is_last_nonempty_line: bool,
    file_ends_with_newline: bool,
) -> (Option<PromptHistoryEntry>, Vec<SessionRecordDiagnostic>) {
    let stripped = line.trim();
    if stripped.is_empty() {
        return (None, Vec::new());
    }

    let data: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            let trailing = is_last_nonempty_line && !file_ends_with_newline;
            let (code, severity) = if trailing {
                (DIAG_TRAILING_INCOMPLETE, SEVERITY_WARNING)
            } else {
                (DIAG_INVALID_JSON, SEVERITY_ERROR)
            };
            let message = if trailing {
                "提示历史末尾存在未完成的 JSON 行，可能由写入中断导致。".to_string()
            } else {
                format!("提示历史 JSON 损坏：第 {line_no} 行。")
            };
            let snippet = redact_sensitive_text(&stripped.chars().take(120).collect::<String>());
            return (
                None,
                vec![SessionRecordDiagnostic {
                    code: code.to_string(),
                    severity: severity.to_string(),
                    message,
                    path: Some(path.to_string()),
                    line_no: Some(line_no),
                    session_id: None,
                    recoverable: true,
                    details: match json!({
                        "json_error": error.to_string(),
                        "snippet": snippet,
                        "trailing": trailing,
                    }) {
                        Value::Object(map) => map,
                        _ => Map::new(),
                    },
                }],
            );
        }
    };

    if !data.is_object() {
        return (
            None,
            vec![SessionRecordDiagnostic {
                code: DIAG_NOT_OBJECT.to_string(),
                severity: SEVERITY_ERROR.to_string(),
                message: "提示历史 JSON 顶层必须是对象。".to_string(),
                path: Some(path.to_string()),
                line_no: Some(line_no),
                session_id: None,
                recoverable: true,
                details: Map::new(),
            }],
        );
    }

    match PromptHistoryEntry::from_dict(&data) {
        Ok(entry) => (Some(entry), Vec::new()),
        Err(error) => (
            None,
            vec![SessionRecordDiagnostic {
                code: DIAG_PROMPT_INVALID.to_string(),
                severity: SEVERITY_ERROR.to_string(),
                message: error.to_string(),
                path: Some(path.to_string()),
                line_no: Some(line_no),
                session_id: None,
                recoverable: true,
                details: Map::new(),
            }],
        ),
    }
}

fn redact_map(map: Map<String, Value>) -> Map<String, Value> {
    match redact_sensitive_values(&Value::Object(map)) {
        Value::Object(redacted) => redacted,
        _ => Map::new(),
    }
}

/// 提示展示清洗：统一换行、去首尾空白，超过上限后截断并附说明。
pub fn clean_prompt_display(value: &str) -> String {
    let prompt = value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string();
    if prompt.chars().count() > MAX_PROMPT_HISTORY_DISPLAY_CHARS {
        let kept: String = prompt
            .chars()
            .take(MAX_PROMPT_HISTORY_DISPLAY_CHARS)
            .collect();
        return format!("{kept}\n... 提示历史已截断。");
    }
    prompt
}
