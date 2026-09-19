//! `omnicrawl/agent/subagents/recovery.py` 的移植：从父 Session 的 additive 事件重建
//! 跨进程 SubAgent 任务快照。
//!
//! 恢复语义刻意收窄：
//!
//! 1. 只恢复控制面可查询的安全任务快照（list/get）；
//! 2. 终态 `completed/failed/cancelled` 按事件原文恢复；
//! 3. 进程崩溃时仍停留在 `queued/running/waiting_approval` 的任务标记为 `failed` +
//!    `SUBAGENT_INTERRUPTED`，绝不自动重跑；
//! 4. 不恢复审批请求、模型 Runtime、Fork 上下文、原始 prompt 或隐藏推理。

use serde_json::{json, Map, Value};

use omnicrawl_session::redaction::redact_sensitive_text;

use crate::shared::{python_str, python_truthy};

/// 会话事件的只读投影；`created_at_seconds` 是事件自身的 `created_at`（datetime → epoch 秒）。
#[derive(Debug, Clone)]
pub struct LifecycleEvent {
    pub event_type: String,
    pub payload: Value,
    pub created_at_seconds: Option<f64>,
}

const TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];
const ACTIVE_STATUSES: [&str; 4] = ["queued", "running", "waiting_approval", "cancelling"];

fn event_status(event_type: &str) -> Option<&'static str> {
    match event_type {
        "subagent_task_queued" => Some("queued"),
        "subagent_task_started" => Some("running"),
        "subagent_task_waiting_approval" => Some("waiting_approval"),
        "subagent_task_completed" => Some("completed"),
        "subagent_task_failed" => Some("failed"),
        "subagent_task_cancelled" => Some("cancelled"),
        "subagent_task_partial" => Some("failed"),
        "subagent_task_timed_out" => Some("failed"),
        _ => None,
    }
}

/// 把会话事件折叠为可导入 TaskManager 的终态快照列表。
///
/// 同一 `task_id` 以最后一条合法生命周期事件为准；非 SubAgent 事件、坏 `task_id` 与缺字段
/// payload 被静默忽略，避免损坏转录阻断会话恢复。
pub fn rebuild_task_snapshots_from_session_events(
    events: &[LifecycleEvent],
    owner_id: &str,
    session_id: &str,
) -> Vec<Value> {
    let mut records: Vec<(String, Map<String, Value>)> = Vec::new();
    for event in events {
        let event_type = event.event_type.trim().to_string();
        let Some(fallback) = event_status(&event_type) else {
            continue;
        };
        let Some(payload) = event.payload.as_object() else {
            continue;
        };
        let task_id = or_empty(payload.get("task_id")).trim().to_string();
        if !matches_id_pattern(&task_id, "task-") {
            continue;
        }
        let created_at = event_timestamp(event, payload);
        let status = normalize_status(payload.get("status"), fallback);

        let index = records.iter().position(|(key, _)| *key == task_id);
        let Some(index) = index else {
            records.push((
                task_id.clone(),
                new_record(&task_id, payload, &status, created_at, owner_id, session_id),
            ));
            continue;
        };

        let record = &mut records[index].1;
        record.insert("updated_at".to_string(), number_value(created_at));
        record.insert("status".to_string(), Value::String(status.clone()));
        let description = safe_text(payload.get("description"), 120);
        if !description.is_empty() {
            record.insert("description".to_string(), Value::String(description));
        }
        let agent_type = safe_text(payload.get("agent_type"), 80);
        if !agent_type.is_empty() {
            record.insert("agent_type".to_string(), Value::String(agent_type));
        }
        let batch_id = or_empty(payload.get("batch_id")).trim().to_string();
        if matches_id_pattern(&batch_id, "batch-") {
            record.insert("batch_id".to_string(), Value::String(batch_id));
        }
        if is_terminal(&status) {
            record.insert("result".to_string(), terminal_result(payload, &status));
            record.insert(
                "error".to_string(),
                bound_error(payload.get("error"), &status),
            );
        } else {
            record.insert("result".to_string(), Value::Null);
            record.insert("error".to_string(), Value::Null);
        }
    }

    let mut snapshots: Vec<Value> = Vec::new();
    for (_, mut record) in records {
        let status = record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !is_terminal(&status) {
            record.insert("status".to_string(), Value::String("failed".to_string()));
            record.insert(
                "result".to_string(),
                json!({
                    "status": "failed",
                    "summary": "任务在进程重启前未完成。",
                    "artifacts": [],
                    "usage": {},
                    "recovered": true,
                }),
            );
            record.insert(
                "error".to_string(),
                json!({
                    "code": "SUBAGENT_INTERRUPTED",
                    "message": "任务在进程重启时中断，未自动重跑。",
                }),
            );
        } else if record.get("result").map(Value::is_null).unwrap_or(true) && status == "completed"
        {
            record.insert(
                "result".to_string(),
                terminal_result(&Map::new(), "completed"),
            );
        }
        snapshots.push(Value::Object(record));
    }

    snapshots.sort_by(|left, right| {
        let left_key = (
            left.get("created_at")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            left.get("task_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        let right_key = (
            right
                .get("created_at")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            right
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        left_key
            .partial_cmp(&right_key)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    snapshots
}

fn new_record(
    task_id: &str,
    payload: &Map<String, Value>,
    status: &str,
    created_at: f64,
    owner_id: &str,
    session_id: &str,
) -> Map<String, Value> {
    let mut batch_id = or_empty(payload.get("batch_id")).trim().to_string();
    if !matches_id_pattern(&batch_id, "batch-") {
        batch_id = format!("batch-{}", task_id.get(5..).unwrap_or_default());
    }
    let description = safe_text(payload.get("description"), 120);
    let agent_type = safe_text(payload.get("agent_type"), 80);
    let mut record = Map::new();
    record.insert("task_id".to_string(), Value::String(task_id.to_string()));
    record.insert("batch_id".to_string(), Value::String(batch_id));
    record.insert("owner_id".to_string(), Value::String(owner_id.to_string()));
    record.insert(
        "session_id".to_string(),
        Value::String(session_id.to_string()),
    );
    record.insert(
        "description".to_string(),
        Value::String(if description.is_empty() {
            "SubAgent 任务".to_string()
        } else {
            description
        }),
    );
    record.insert(
        "agent_type".to_string(),
        Value::String(if agent_type.is_empty() {
            "unknown".to_string()
        } else {
            agent_type
        }),
    );
    record.insert("status".to_string(), Value::String(status.to_string()));
    record.insert("result".to_string(), Value::Null);
    record.insert("error".to_string(), Value::Null);
    record.insert("created_at".to_string(), number_value(created_at));
    record.insert("updated_at".to_string(), number_value(created_at));
    if is_terminal(status) {
        record.insert("result".to_string(), terminal_result(payload, status));
        record.insert(
            "error".to_string(),
            bound_error(payload.get("error"), status),
        );
    }
    record
}

fn terminal_result(payload: &Map<String, Value>, status: &str) -> Value {
    let mut artifacts: Vec<Value> = payload
        .get("artifacts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut usage: Map<String, Value> = payload
        .get("usage")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut summary = String::new();
    if let Some(result) = payload.get("result").and_then(Value::as_object) {
        summary = safe_text(result.get("summary"), 6000);
        if artifacts.is_empty() {
            if let Some(items) = result.get("artifacts").and_then(Value::as_array) {
                artifacts = items.clone();
            }
        }
        if usage.is_empty() {
            if let Some(values) = result.get("usage").and_then(Value::as_object) {
                usage = values.clone();
            }
        }
    }
    if summary.is_empty() {
        summary = safe_text(payload.get("summary"), 6000);
    }
    artifacts.truncate(16);
    json!({
        "status": status,
        "summary": summary,
        "artifacts": artifacts,
        "usage": Value::Object(usage),
        "recovered": true,
    })
}

fn bound_error(error: Option<&Value>, status: &str) -> Value {
    if status == "completed" {
        return Value::Null;
    }
    if let Some(map) = error.and_then(Value::as_object) {
        let code = safe_text(map.get("code"), 80);
        let message = safe_text(map.get("message"), 500);
        return json!({
            "code": if code.is_empty() { "SUBAGENT_ERROR".to_string() } else { code },
            "message": if message.is_empty() { "子任务失败。".to_string() } else { message },
        });
    }
    if status == "cancelled" {
        return json!({"code": "SUBAGENT_CANCELLED", "message": "任务已取消。"});
    }
    if status == "failed" {
        return json!({"code": "SUBAGENT_ERROR", "message": "子任务失败。"});
    }
    Value::Null
}

fn normalize_status(raw: Option<&Value>, fallback: &str) -> String {
    let status = or_empty(raw).trim().to_lowercase();
    if is_terminal(&status) || ACTIVE_STATUSES.contains(&status.as_str()) {
        return status;
    }
    if fallback == "running" && status == "started" {
        return "running".to_string();
    }
    fallback.to_string()
}

fn is_terminal(status: &str) -> bool {
    TERMINAL_STATUSES.contains(&status)
}

/// Python `str(value or "")`：falsy 一律成空串。
fn or_empty(value: Option<&Value>) -> String {
    match value {
        Some(item) if python_truthy(item) => python_str(item),
        _ => String::new(),
    }
}

fn safe_text(value: Option<&Value>, limit: usize) -> String {
    let text = redact_sensitive_text(&or_empty(value));
    let trimmed = text.trim();
    if trimmed.chars().count() > limit {
        return trimmed.chars().take(limit).collect();
    }
    trimmed.to_string()
}

fn matches_id_pattern(value: &str, prefix: &str) -> bool {
    let Some(rest) = value.strip_prefix(prefix) else {
        return false;
    };
    rest.chars().count() == 12
        && rest
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character))
}

fn event_timestamp(event: &LifecycleEvent, payload: &Map<String, Value>) -> f64 {
    if let Some(seconds) = event.created_at_seconds {
        return seconds;
    }
    match payload.get("timestamp") {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn number_value(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}
