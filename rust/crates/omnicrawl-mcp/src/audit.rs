//! MCP 调用审计：把工具调用写入工作区内的 JSONL 日志（对应 `omnicrawl/mcp/audit.py`）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Local, SecondsFormat};
use omnicrawl_controllers::json::python_dumps;
use serde_json::{json, Map, Value};

use crate::security::{redact_sensitive_text, redact_sensitive_values};

pub const DEFAULT_MCP_AUDIT_LOG_PATH: &str = ".omnicrawl/logs/mcp-audit.jsonl";

/// 审计只保留输出预览的前 1000 字符；脱敏窗口略大于预览长度，让跨截断边界的
/// 密钥在截断前就被正则吞掉，同时对超长输出保持常数级开销。
const PREVIEW_MAX_CHARS: usize = 1000;
const PREVIEW_REDACTION_WINDOW: usize = 2000;

/// 一条 MCP 调用审计记录。
#[derive(Debug, Clone, PartialEq)]
pub struct McpAuditEvent {
    pub timestamp: String,
    pub session_id: String,
    pub audit_id: String,
    pub server_name: String,
    pub tool_name: String,
    pub arguments_redacted: Value,
    pub approval_mode: String,
    pub approval_result: String,
    pub duration_ms: i64,
    pub ok: bool,
    pub error_code: Option<String>,
    pub output_preview: String,
}

impl McpAuditEvent {
    /// 审计行：字段顺序与 Python dataclass 的 `__dict__` 一致。
    pub fn to_json_line(&self) -> String {
        let mut payload = Map::new();
        payload.insert("timestamp".to_string(), json!(self.timestamp));
        payload.insert("session_id".to_string(), json!(self.session_id));
        payload.insert("audit_id".to_string(), json!(self.audit_id));
        payload.insert("server_name".to_string(), json!(self.server_name));
        payload.insert("tool_name".to_string(), json!(self.tool_name));
        payload.insert(
            "arguments_redacted".to_string(),
            self.arguments_redacted.clone(),
        );
        payload.insert("approval_mode".to_string(), json!(self.approval_mode));
        payload.insert("approval_result".to_string(), json!(self.approval_result));
        payload.insert("duration_ms".to_string(), json!(self.duration_ms));
        payload.insert("ok".to_string(), json!(self.ok));
        payload.insert("error_code".to_string(), json!(self.error_code));
        payload.insert("output_preview".to_string(), json!(self.output_preview));
        python_dumps(&Value::Object(payload), 0)
    }
}

/// 一次调用的审计入参（对应 Python `record_tool_call` 的关键字参数）。
pub struct AuditRecord<'a> {
    pub session_id: &'a str,
    pub audit_id: &'a str,
    pub server_name: &'a str,
    pub tool_name: &'a str,
    pub arguments: &'a Map<String, Value>,
    pub approval_mode: &'a str,
    pub approval_result: &'a str,
    pub duration_ms: i64,
    pub ok: bool,
    pub error_code: Option<&'a str>,
    pub output: &'a str,
}

/// 把 MCP 工具调用写入本地 JSONL 审计日志。
#[derive(Clone)]
pub struct McpAuditLogger {
    enabled: bool,
    workspace_root: Arc<PathBuf>,
    path: Arc<PathBuf>,
    now: Arc<dyn Fn() -> String + Send + Sync>,
    // 目录只需创建一次；每次调用都 mkdir 会给工具调用多塞一次系统调用。
    // 目录被外部删除时由下次写入重新创建。
    directory_ready: Arc<Mutex<bool>>,
}

impl McpAuditLogger {
    pub fn new(workspace_root: impl Into<PathBuf>, enabled: bool) -> Self {
        Self::with_relative_path(workspace_root, enabled, DEFAULT_MCP_AUDIT_LOG_PATH)
    }

    pub fn with_relative_path(
        workspace_root: impl Into<PathBuf>,
        enabled: bool,
        relative_path: &str,
    ) -> Self {
        let root = absolute(&workspace_root.into());
        let path = resolve_log_path(&root, relative_path);
        Self {
            enabled,
            workspace_root: Arc::new(root),
            path: Arc::new(path),
            now: Arc::new(default_timestamp),
            directory_ready: Arc::new(Mutex::new(false)),
        }
    }

    /// 注入时间源（对照测试用固定时刻，运行期用本地时间）。
    pub fn with_clock(mut self, now: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        self.now = now;
        self
    }

    pub fn workspace_root(&self) -> &Path {
        self.workspace_root.as_path()
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// 记录一次 MCP Tool 调用。
    ///
    /// 审计只保存参数脱敏版和输出预览，避免把密钥或大文件内容写入日志。
    pub fn record_tool_call(&self, record: AuditRecord<'_>) {
        if !self.enabled {
            return;
        }

        let event = McpAuditEvent {
            timestamp: (self.now)(),
            session_id: record.session_id.to_string(),
            audit_id: record.audit_id.to_string(),
            server_name: record.server_name.to_string(),
            tool_name: record.tool_name.to_string(),
            arguments_redacted: redact_sensitive_values(&Value::Object(record.arguments.clone())),
            approval_mode: record.approval_mode.to_string(),
            approval_result: record.approval_result.to_string(),
            duration_ms: record.duration_ms,
            ok: record.ok,
            error_code: record.error_code.map(|code| code.to_string()),
            // 先清理再截断，避免位于预览末尾的秘密部分残留在审计日志。
            // 只脱敏有界前缀：输出可能上千倍于预览长度，全文跑 4 轮正则会让
            // 审计成为工具调用路径上的开销大头。
            output_preview: preview(&redact_sensitive_text(&head(
                record.output,
                PREVIEW_REDACTION_WINDOW,
            ))),
        };

        if self.write_line(&event).is_err() {
            // 审计日志不能影响主业务。调用方仍会在工具结果中看到真实执行状态。
            if let Ok(mut ready) = self.directory_ready.lock() {
                *ready = false;
            }
        }
    }

    fn write_line(&self, event: &McpAuditEvent) -> std::io::Result<()> {
        self.ensure_directory()?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.as_path())?;
        file.write_all(event.to_json_line().as_bytes())?;
        file.write_all(b"\n")
    }

    fn ensure_directory(&self) -> std::io::Result<()> {
        if let Ok(ready) = self.directory_ready.lock() {
            if *ready {
                return Ok(());
            }
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Ok(mut ready) = self.directory_ready.lock() {
            *ready = true;
        }
        Ok(())
    }
}

fn default_timestamp() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// 取输出头部有界片段，用于在脱敏前把正则开销限制为常数。
fn head(output: &str, max_chars: usize) -> String {
    let text = output.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

fn preview(output: &str) -> String {
    let text = output.trim();
    if text.chars().count() <= PREVIEW_MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
    format!("{head}\n... 审计输出预览已截断。")
}

/// 日志路径必须落在工作区内：越界一律回落到默认路径。
fn resolve_log_path(workspace_root: &Path, relative_path: &str) -> PathBuf {
    let cleaned = {
        let trimmed = relative_path.replace('\\', "/");
        let trimmed = trimmed.trim().to_string();
        if trimmed.is_empty() {
            DEFAULT_MCP_AUDIT_LOG_PATH.to_string()
        } else {
            trimmed
        }
    };
    let candidate = PathBuf::from(&cleaned);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        workspace_root.join(candidate)
    };
    let resolved = absolute(&candidate);
    if resolved.starts_with(workspace_root) {
        resolved
    } else {
        workspace_root.join(DEFAULT_MCP_AUDIT_LOG_PATH)
    }
}

/// 规范化路径（Python 侧用 `Path.resolve()`；这里不要求目标存在）。
fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    normalize(&joined)
}

fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logger(root: &Path) -> McpAuditLogger {
        McpAuditLogger::new(root, true).with_clock(Arc::new(|| "2026-09-20T01:54:10+08:00".into()))
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-mcp-audit-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时目录");
        root
    }

    #[test]
    fn record_writes_one_json_line_with_default_separators() {
        let root = temp_root("record");
        let logger = logger(&root);
        let mut arguments = Map::new();
        arguments.insert("api_key".to_string(), json!("sk-0123456789abcdefghijklmn"));
        logger.record_tool_call(AuditRecord {
            session_id: "session-abcdef",
            audit_id: "mcp-0123456789ab",
            server_name: "files",
            tool_name: "read",
            arguments: &arguments,
            approval_mode: "manual",
            approval_result: "approved",
            duration_ms: 12,
            ok: true,
            error_code: None,
            output: "  done  ",
        });
        let text = std::fs::read_to_string(logger.path()).expect("审计日志应当写入");
        assert_eq!(text.lines().count(), 1);
        assert!(
            text.contains("\"timestamp\": \"2026-09-20T01:54:10+08:00\""),
            "{text}"
        );
        assert!(
            text.contains("\"arguments_redacted\": {\"api_key\": \"***\"}"),
            "{text}"
        );
        assert!(text.contains("\"output_preview\": \"done\""), "{text}");
        assert!(text.ends_with("\n"));
    }

    #[test]
    fn long_output_is_redacted_before_truncation() {
        let root = temp_root("truncate");
        let logger = logger(&root);
        let arguments = Map::new();
        let output = format!("token=sk-0123456789abcdefghijklmn {}", "x".repeat(3000));
        logger.record_tool_call(AuditRecord {
            session_id: "s",
            audit_id: "a",
            server_name: "files",
            tool_name: "read",
            arguments: &arguments,
            approval_mode: "auto",
            approval_result: "approved",
            duration_ms: 0,
            ok: true,
            error_code: None,
            output: &output,
        });
        let text = std::fs::read_to_string(logger.path()).expect("审计日志应当写入");
        assert!(!text.contains("sk-0123456789"), "{text}");
        assert!(text.contains("审计输出预览已截断。"), "{text}");
    }

    #[test]
    fn disabled_logger_writes_nothing() {
        let root = temp_root("disabled");
        let logger = McpAuditLogger::new(&root, false);
        let arguments = Map::new();
        logger.record_tool_call(AuditRecord {
            session_id: "s",
            audit_id: "a",
            server_name: "",
            tool_name: "read",
            arguments: &arguments,
            approval_mode: "",
            approval_result: "not_found",
            duration_ms: 0,
            ok: false,
            error_code: Some("TOOL_NOT_FOUND"),
            output: "",
        });
        assert!(!logger.path().exists());
    }

    #[test]
    fn path_outside_workspace_falls_back_to_default() {
        let root = temp_root("escape");
        let logger = McpAuditLogger::with_relative_path(&root, true, "../outside.jsonl");
        assert!(logger.path().starts_with(&root));
        assert!(logger.path().ends_with("mcp-audit.jsonl"));
    }
}
