//! `omnicrawl/agent/controllers/subagents/orchestration.py` 的判定与投影面。
//!
//! 失败描述、Fork 上下文冻结、子任务结果公开投影、后台通知注入与「是否需要完整评审报告」
//! 判定收进内核（任务生命周期本体在同目录 `tasks.rs`）；模型运行时引导、线程池、
//! worktree 的 git 操作与 Session 落盘留在宿主（`omnicrawl-cli`）。

use omnicrawl_session::redaction::{redact_sensitive_text, redact_sensitive_values};
use serde_json::{json, Map, Value};

use crate::error::AgentError;

/// 后台终态通知一次注入的最大条数。
pub const NOTIFICATION_PREVIEW_ITEMS: usize = 16;
/// 后台终态通知注入文本的最大字符数。
pub const NOTIFICATION_PREVIEW_CHARS: usize = 12000;
/// 子任务结果里的 worktree 产物标记。
pub const WORKTREE_MARKER: &str = "[worktree]";
/// 单条 worktree 产物的字符上限。
pub const WORKTREE_ARTIFACT_CHARS: usize = 8000;
/// 摘要超限时的截断后缀。
pub const SUMMARY_TRUNCATED_SUFFIX: &str = "\n... 子任务结果已截断。";
/// 候选错误里找不到可展示细节时的通用失败文案。
pub const SUBAGENT_FAILURE_FALLBACK: &str = "子任务执行失败。";
/// `subagent` 工具在协调器缺席时的结果文案。
pub const SUBAGENT_DISABLED: &str = "SubAgent 功能未启用。";
/// `run_subagent_task` 在协调器缺席时的错误文案。
pub const SUBAGENT_DISABLED_TASK: &str = "SubAgent 功能未启用，无法执行该任务。";
/// 子任务结果无法解析为 JSON 时的错误文案。
pub const SUBAGENT_UNPARSABLE: &str = "子任务返回结果无法解析。";
/// 批次返回但没有任何子任务结果时的错误文案。
pub const SUBAGENT_EMPTY_RESULT: &str = "子任务未返回结果。";

/// 子任务结果的公开投影（`SubAgentPublicResult` 的数据面）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublicResult {
    pub summary: String,
    pub artifacts: Vec<Value>,
}

/// `_describe_subagent_run_failure`：把失败投影为有信息量且已脱敏的文案。
pub fn describe_run_failure(payload: &Value, task: Option<&Value>) -> String {
    let mut candidates: Vec<Value> = Vec::new();
    match task {
        Some(task) => candidates.push(error_field(task)),
        None => {
            if let Some(items) = payload.get("results").and_then(Value::as_array) {
                for item in items {
                    let completed = item.get("status").and_then(Value::as_str) == Some("completed");
                    if item.is_object() && !completed {
                        candidates.push(error_field(item));
                    }
                }
            }
            candidates.push(error_field(payload));
        }
    }
    for error in &candidates {
        let detail = error_detail(error);
        if !detail.is_empty() {
            return detail;
        }
    }
    SUBAGENT_FAILURE_FALLBACK.to_string()
}

/// `_fork_task_message`：追加到冻结父上下文后的独立任务指令。
pub fn fork_task_message(description: &str, prompt: &str) -> Value {
    json!({
        "role": "user",
        "content": format!(
            "<subagent_task context=\"fork\">\n描述：{description}\n任务：\n{prompt}\n</subagent_task>"
        ),
    })
}

/// `_freeze_fork_context_messages` 的结构面：脱敏后只保留协议消息对象。
pub fn fork_context_snapshot(messages: &Value) -> Result<Vec<Value>, AgentError> {
    let redacted = redact_sensitive_values(messages);
    let Some(items) = redacted.as_array() else {
        return Err(AgentError::new("Fork 上下文必须是消息数组。"));
    };
    Ok(items
        .iter()
        .filter(|item| item.is_object())
        .cloned()
        .collect())
}

/// `_prepare_subagent_public_result` 在无 Session 时的本地投影。
pub fn local_public_result(result_text: &str, summary_chars: usize) -> PublicResult {
    let mut text = result_text.to_string();
    let mut artifacts: Vec<Value> = Vec::new();
    if let Some(index) = text.find(WORKTREE_MARKER) {
        let head = text[..index].to_string();
        let tail = text[index + WORKTREE_MARKER.len()..].to_string();
        text = head.trim_end().to_string();
        let body = tail.trim();
        if !body.is_empty() {
            let content: String = redact_sensitive_text(body)
                .chars()
                .take(WORKTREE_ARTIFACT_CHARS)
                .collect();
            artifacts.push(json!({"type": "worktree", "content": content}));
        }
    }
    let mut summary = redact_sensitive_text(text.trim());
    if summary.chars().count() > summary_chars {
        summary = summary.chars().take(summary_chars).collect();
        summary.push_str(SUMMARY_TRUNCATED_SUFFIX);
    }
    PublicResult { summary, artifacts }
}

/// `_inject_subagent_notifications`：把新完成通知并入本轮临时 user 消息。
///
/// 返回是否真的注入了通知（无通知时不改动 `messages`）。
pub fn inject_notifications(messages: &mut Vec<Value>, notifications: &[Value]) -> bool {
    if notifications.is_empty() {
        return false;
    }
    let preview: Vec<Value> = notifications
        .iter()
        .take(NOTIFICATION_PREVIEW_ITEMS)
        .cloned()
        .collect();
    let rendered = crate::json::python_dumps(&Value::Array(preview), 0);
    let body: String = rendered.chars().take(NOTIFICATION_PREVIEW_CHARS).collect();
    let notification_text =
        format!("\n\n<subagent-notifications>\n{body}\n</subagent-notifications>");
    for message in messages.iter_mut().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let prefix = truthy_text(message.get("content"));
        if let Some(map) = message.as_object_mut() {
            map.insert(
                "content".to_string(),
                Value::String(format!("{prefix}{notification_text}")),
            );
        }
        return true;
    }
    messages.push(json!({
        "role": "user",
        "content": notification_text.trim(),
    }));
    true
}

/// `_tool_subagent` 的 `wants_review` 判定。
pub fn wants_review(arguments: &Value) -> bool {
    let Some(tasks) = arguments.get("tasks").and_then(Value::as_array) else {
        return false;
    };
    tasks.iter().any(|task| {
        task.is_object()
            && task
                .get("subagent_type")
                .map(|value| truthy_text(Some(value)).trim().to_lowercase() == "review")
                .unwrap_or(false)
    })
}

/// `run_subagent_task` 的结果投影：返回子任务全文，失败时给出安全错误。
pub fn require_completed_result(
    result_ok: bool,
    result_output: &str,
) -> Result<String, AgentError> {
    let payload: Value =
        serde_json::from_str(result_output).map_err(|_| AgentError::new(SUBAGENT_UNPARSABLE))?;
    if !result_ok || payload.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(AgentError::new(describe_run_failure(&payload, None)));
    }
    let results = payload
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(first) = results.first() else {
        return Err(AgentError::new(SUBAGENT_EMPTY_RESULT));
    };
    if first.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(AgentError::new(describe_run_failure(&payload, Some(first))));
    }
    if let Some(Value::String(full_text)) = first.get("full_text") {
        if !full_text.trim().is_empty() {
            return Ok(full_text.clone());
        }
    }
    Ok(truthy_text(first.get("summary")))
}

/// `未找到 SubAgent 定义：{agent_type}。当前可用：{available}。`
pub fn missing_definition_error(agent_type: &str, available: &[String]) -> AgentError {
    let joined = if available.is_empty() {
        "无".to_string()
    } else {
        available.join(", ")
    };
    AgentError::new(format!(
        "未找到 SubAgent 定义：{agent_type}。当前可用：{joined}。"
    ))
}

fn error_field(value: &Value) -> Value {
    value
        .get("error")
        .filter(|item| item.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

fn error_detail(error: &Value) -> String {
    let map = error.as_object();
    let message = truthy_text(map.and_then(|item| item.get("message")))
        .trim()
        .to_string();
    let code = truthy_text(map.and_then(|item| item.get("code")))
        .trim()
        .to_string();
    let (category, detail) = match map
        .and_then(|item| item.get("diagnostic"))
        .and_then(Value::as_object)
    {
        Some(diagnostic) => (
            truthy_text(diagnostic.get("category")).trim().to_string(),
            truthy_text(diagnostic.get("detail")).trim().to_string(),
        ),
        None => (String::new(), String::new()),
    };
    let mut parts: Vec<String> = Vec::new();
    if !detail.is_empty() && detail != message {
        parts.push(detail);
    } else if !message.is_empty() {
        parts.push(message);
    }
    let labels = [code, category];
    let labels: Vec<String> = labels.into_iter().filter(|item| !item.is_empty()).collect();
    if !labels.is_empty() {
        parts.push(format!("（{}）", labels.join("，")));
    }
    parts.concat()
}

fn truthy_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) if items.is_empty() => String::new(),
        Some(Value::Object(map)) if map.is_empty() => String::new(),
        Some(other @ Value::Number(_)) => {
            if other.as_f64() == Some(0.0) {
                String::new()
            } else {
                crate::json::python_number_text(other)
            }
        }
        Some(other) => crate::json::python_repr(other),
    }
}
