//! `omnicrawl/agent/controllers/tools/implementations.py` 的判定面。
//!
//! 工具实现本体是 I/O（文件系统、HTTP、子进程、MCP、TTS），留在宿主；这里收的是
//! 参数解析与安全投影：`update_todos` 的清单投影与输出信封、`ask_user` 的入参校验
//! 与回答信封，以及记忆工具的 `scope` 解析。

use serde_json::{json, Map, Value};

use crate::error::AgentError;
use crate::json::python_dumps;
use crate::shared::{python_str, python_truthy};

// --------------------------------------------------------------- update_todos

pub const TODOS_NOT_ARRAY: &str = "todos 必须是数组。";

/// 投影上限：模型给多了只取前 20 条。
pub const MAX_TODO_ITEMS: usize = 20;

pub const MAX_TODO_STEP_CHARS: usize = 240;

pub const MAX_TODO_ID_CHARS: usize = 80;

/// 把状态词也当作「已完成」的取值（模型不一定填 `completed`）。
pub const TODO_COMPLETED_STATUSES: [&str; 3] = ["completed", "done", "complete"];

/// 投影后的执行清单条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub step: String,
    pub completed: bool,
}

/// `update_todos` 的安全投影：最多 20 条，逐条取 `step`/`description`/`title` 的首个非空值。
///
/// 非对象条目与空步骤直接跳过，所以投影结果是「能显示的清单」而不是原样回显。
pub fn project_todos(raw: &[Value]) -> Vec<TodoItem> {
    let mut items = Vec::new();
    for (offset, raw_item) in raw.iter().take(MAX_TODO_ITEMS).enumerate() {
        let Some(object) = raw_item.as_object() else {
            continue;
        };
        let step = first_truthy_text(object, &["step", "description", "title"]);
        if step.is_empty() {
            continue;
        }
        let index = offset + 1;
        let status = truthy_str(object.get("status"))
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let completed = object.get("completed").map(python_truthy).unwrap_or(false)
            || TODO_COMPLETED_STATUSES.contains(&status.as_str());
        items.push(TodoItem {
            id: todo_id(object, index),
            step: take_chars(&step, MAX_TODO_STEP_CHARS),
            completed,
        });
    }
    items
}

/// 工具的模型可见输出：`{"updated": <条数>, "todos": [...]}` 的 Python 风格 JSON。
pub fn todos_output(items: &[TodoItem]) -> String {
    let payload = json!({
        "updated": items.len(),
        "todos": items.iter().map(todo_value).collect::<Vec<_>>(),
    });
    python_dumps(&payload, 0)
}

fn todo_value(item: &TodoItem) -> Value {
    json!({"id": item.id, "step": item.step, "completed": item.completed})
}

fn todo_id(object: &Map<String, Value>, index: usize) -> String {
    let raw = match object.get("id") {
        Some(value) if python_truthy(value) => python_str(value),
        _ => index.to_string(),
    };
    let trimmed = take_chars(raw.trim(), MAX_TODO_ID_CHARS);
    if trimmed.is_empty() {
        index.to_string()
    } else {
        trimmed
    }
}

fn first_truthy_text(object: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| truthy_str(object.get(*key)))
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

/// Python `str(value or "")`：falsy 一律变成空串。
fn truthy_str(value: Option<&Value>) -> Option<String> {
    value.filter(|item| python_truthy(item)).map(python_str)
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

// -------------------------------------------------------------------- ask_user

pub const ASK_USER_KINDS: [&str; 3] = ["question", "select", "confirm"];

pub const ASK_USER_KIND_INVALID: &str = "kind 必须是 question、select 或 confirm。";

pub const ASK_USER_QUESTION_EMPTY: &str = "question 不能为空。";

pub const ASK_USER_OPTIONS_NOT_ARRAY: &str = "options 必须是数组。";

pub const ASK_USER_OPTIONS_EMPTY: &str = "ask_user 必须提供至少一个非空 options 选项。";

pub const ASK_USER_UNANSWERED: &str = "用户未回答该问题。";

/// 校验并归一化后的提问请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskUserRequest {
    pub kind: String,
    pub question: String,
    pub options: Vec<String>,
    pub request_id: String,
}

/// 解析 `ask_user` 入参；`Err` 就是模型的失败文案。
pub fn parse_ask_user(arguments: &Map<String, Value>) -> Result<AskUserRequest, String> {
    let kind = truthy_str(arguments.get("kind"))
        .unwrap_or_else(|| "question".to_string())
        .trim()
        .to_lowercase();
    if !ASK_USER_KINDS.contains(&kind.as_str()) {
        return Err(ASK_USER_KIND_INVALID.to_string());
    }
    let question = match arguments.get("question") {
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        _ => return Err(ASK_USER_QUESTION_EMPTY.to_string()),
    };
    let options: Vec<String> = match arguments.get("options") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .collect(),
        Some(_) => return Err(ASK_USER_OPTIONS_NOT_ARRAY.to_string()),
    };
    if options.is_empty() {
        return Err(ASK_USER_OPTIONS_EMPTY.to_string());
    }
    let request_id = truthy_str(arguments.get("request_id"))
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(AskUserRequest {
        kind,
        question,
        options,
        request_id,
    })
}

/// 用户给出答案后的输出信封。
pub fn ask_user_answer_output(request: &AskUserRequest, answer: &str) -> String {
    let payload = json!({
        "kind": request.kind,
        "question": request.question,
        "options": request.options,
        "answer": answer.trim(),
    });
    python_dumps(&payload, 0)
}

// ----------------------------------------------------------------- memory scope

/// 记忆工具 `scope` 参数的作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Project,
    Session,
    User,
}

impl MemoryScope {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryScope::Project => "project",
            MemoryScope::Session => "session",
            MemoryScope::User => "user",
        }
    }
}

/// 解析 `scope`：缺省 `project`，大小写与空白不敏感，其余取值一律拒绝。
pub fn parse_memory_scope(raw: Option<&Value>) -> Result<MemoryScope, AgentError> {
    let scope = truthy_str(raw)
        .unwrap_or_else(|| "project".to_string())
        .trim()
        .to_lowercase();
    match scope.as_str() {
        "project" => Ok(MemoryScope::Project),
        "session" => Ok(MemoryScope::Session),
        "user" => Ok(MemoryScope::User),
        _ => Err(AgentError::new(format!(
            "scope 仅支持 project、session、user；收到：{scope}。"
        ))),
    }
}

/// 某作用域未启用（宿主没有注入对应存储）时的拒绝文案。
pub fn memory_store_missing_error(scope: MemoryScope) -> AgentError {
    AgentError::new(format!("{} 级记忆系统未启用。", scope.as_str()))
}
