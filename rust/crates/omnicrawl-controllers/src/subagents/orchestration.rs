//! `omnicrawl/agent/controllers/subagents/orchestration.py` 的判定与投影面。
//!
//! 失败描述、Fork 上下文冻结、子任务结果公开投影、后台通知注入与「是否需要完整评审报告」
//! 判定收进内核（任务生命周期本体在同目录 `tasks.rs`）；模型运行时引导、线程池、
//! worktree 的 git 操作与 Session 落盘留在宿主（`omnicrawl-cli`）。

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

/// `_tool_subagent` 的 `wants_review` 判定。
pub fn wants_review(arguments: &Value) -> bool {
    let Some(task) = arguments.get("task") else {
        return false;
    };
    task.is_object()
        && task
            .get("subagent_type")
            .map(|value| truthy_text(Some(value)).trim().to_lowercase() == "review")
            .unwrap_or(false)
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
