//! `omnicrawl/agent/toolkit/tools.py` 与 `host_tools.py` 的参数层。
//!
//! 每次工具调用都要经过这一层：工具名/参数名归一化 → 参数投影（进 Session、确认页、
//! SSE 的可见形状）→ 紧凑 Schema → Schema 二次校验 → 结果信封。这里收的是全部判定与
//! 格式；真正的工具执行、MCP 调用与目录快照仍是宿主的活。

use crate::json::{python_dumps, python_number_text, python_repr};
use crate::types::ToolResult;
use serde_json::{Map, Value};

pub const TODO_TOOL_NAME: &str = "update_todos";

pub const ASK_USER_TOOL_NAME: &str = "ask_user";

pub const PAUSE_WORK_TOOL_NAME: &str = "pause_work";

pub const ADVISOR_TOOL_NAME: &str = "advisor";

pub const INVOKE_TOOL_NAME: &str = "invoke_tool";

/// 模型常见误写的工具名 → 真实工具名。
pub const TOOL_NAME_ALIASES: [(&str, &str); 5] = [
    ("bashcommand", "bash"),
    ("monitorcommand", "monitor"),
    ("powershellcommand", "powershell"),
    ("readimage", "read_image"),
    ("writefile", "write_file"),
];

/// 模型常见误写的参数名 → 真实参数名（`startline` / `maxLines` / `tabId` 这类）。
pub const ARGUMENT_NAME_ALIASES: [(&str, &str); 48] = [
    ("cmd", "command"),
    ("caseSensitive", "case_sensitive"),
    ("casesensitive", "case_sensitive"),
    ("maxLines", "max_lines"),
    ("maxlines", "max_lines"),
    ("maxResults", "max_results"),
    ("maxresults", "max_results"),
    ("newText", "new_text"),
    ("newtext", "new_text"),
    ("oldText", "old_text"),
    ("oldtext", "old_text"),
    ("startLine", "start_line"),
    ("startline", "start_line"),
    ("function", "function_name"),
    ("functionName", "function_name"),
    ("functionname", "function_name"),
    ("textSnippet", "text"),
    ("textsnippet", "text"),
    ("contextLines", "context_lines"),
    ("contextlines", "context_lines"),
    ("monitorId", "monitor_id"),
    ("monitorid", "monitor_id"),
    ("maxEvents", "max_events"),
    ("maxevents", "max_events"),
    ("tabId", "tab"),
    ("tabid", "tab"),
    ("timeoutSeconds", "timeout_seconds"),
    ("timeoutseconds", "timeout_seconds"),
    ("windowHandle", "window_handle"),
    ("windowhandle", "window_handle"),
    ("titleContains", "title_contains"),
    ("titlecontains", "title_contains"),
    ("className", "class_name"),
    ("classname", "class_name"),
    ("classNameContains", "class_name_contains"),
    ("classnamecontains", "class_name_contains"),
    ("visibleOnly", "visible_only"),
    ("visibleonly", "visible_only"),
    ("includeUntitled", "include_untitled"),
    ("includeuntitled", "include_untitled"),
    ("automationId", "automation_id"),
    ("automationid", "automation_id"),
    ("controlType", "control_type"),
    ("controltype", "control_type"),
    ("wheelDelta", "wheel_delta"),
    ("wheeldelta", "wheel_delta"),
    ("maxDimension", "max_dimension"),
    ("maxdimension", "max_dimension"),
];

pub fn tool_name_alias(name: &str) -> Option<&'static str> {
    TOOL_NAME_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, target)| *target)
}

pub fn argument_name_alias(name: &str) -> Option<&'static str> {
    ARGUMENT_NAME_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, target)| *target)
}

/// `re.sub(r"[\s_-]+", "", value).lower()`：比较工具名/参数名时忽略空白、下划线与连字符。
pub fn normalize_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|item| !item.is_whitespace() && *item != '_' && *item != '-')
        .flat_map(|item| item.to_lowercase())
        .collect()
}

/// 把 `bashcommand` / `readimage` 这类误写映射为当前工具表里的真实名字。
pub fn normalize_tool_name(raw_name: &str, tool_names: &[&str]) -> String {
    let name: String = raw_name
        .trim()
        .chars()
        .filter(|item| !item.is_whitespace())
        .collect();
    if tool_names.contains(&name.as_str()) {
        return name;
    }
    if let Some(alias) = tool_name_alias(&name) {
        return alias.to_string();
    }
    let normalized = normalize_identifier(&name);
    if let Some(alias) = tool_name_alias(&normalized) {
        return alias.to_string();
    }
    if !tool_names.is_empty() {
        let matches: Vec<&&str> = tool_names
            .iter()
            .filter(|tool_name| normalize_identifier(tool_name) == normalized)
            .collect();
        if matches.len() == 1 {
            return matches[0].to_string();
        }
    }
    name
}

fn schema_of<'a>(tool_name: &str, tools: &'a [(&'a str, &'a str)]) -> Option<&'a str> {
    tools
        .iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, schema)| *schema)
}

fn parse_schema(schema: &str) -> Option<Value> {
    serde_json::from_str::<Value>(schema)
        .ok()
        .filter(Value::is_object)
}

/// Schema 里声明的参数名集合（`properties` 的键；无 `properties` 时退化为顶层键）。
pub fn tool_argument_keys(tool_name: &str, tools: &[(&str, &str)]) -> Vec<String> {
    let Some(raw) = schema_of(tool_name, tools) else {
        return Vec::new();
    };
    let Some(schema) = parse_schema(raw) else {
        return Vec::new();
    };
    match schema.get("properties") {
        Some(Value::Object(properties)) => properties.keys().cloned().collect(),
        _ => schema
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default(),
    }
}

/// 非必填、且 schema 显式声明 `minLength >= 1` 的字符串字段。
///
/// 这类字段「提供就必须非空」，因此空串与未提供等价；未声明 `minLength` 的可选字段
/// （如替换文本）保留原值，避免改变行为。
pub fn optional_blank_ignored_keys(tool_name: &str, tools: &[(&str, &str)]) -> Vec<String> {
    let Some(raw) = schema_of(tool_name, tools) else {
        return Vec::new();
    };
    let Some(schema) = parse_schema(raw) else {
        return Vec::new();
    };
    let Some(Value::Object(properties)) = schema.get("properties") else {
        return Vec::new();
    };
    let required: Vec<String> = match schema.get("required") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    properties
        .iter()
        .filter(|(key, child)| {
            if required.contains(key) {
                return false;
            }
            let Some(child) = child.as_object() else {
                return false;
            };
            if child.get("type").and_then(Value::as_str) != Some("string") {
                return false;
            }
            child
                .get("minLength")
                .and_then(Value::as_i64)
                .is_some_and(|value| value >= 1)
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// 按工具 schema 归一化参数名，并丢弃「可选 + 显式空串」的字段。
pub fn normalize_tool_arguments(
    tool_name: &str,
    arguments: &Map<String, Value>,
    tools: &[(&str, &str)],
) -> Map<String, Value> {
    let canonical_keys = tool_argument_keys(tool_name, tools);
    let optional_blank = optional_blank_ignored_keys(tool_name, tools);
    let mut normalized = Map::new();
    for (key, value) in arguments {
        let normalized_key = normalize_identifier(key);
        let canonical_key = match argument_name_alias(key)
            .filter(|alias| canonical_keys.contains(&alias.to_string()))
        {
            Some(alias) => alias.to_string(),
            None => match argument_name_alias(&normalized_key)
                .filter(|alias| canonical_keys.contains(&alias.to_string()))
            {
                Some(alias) => alias.to_string(),
                None => canonical_keys
                    .iter()
                    .find(|candidate| normalize_identifier(candidate) == normalized_key)
                    .cloned()
                    .unwrap_or_else(|| key.clone()),
            },
        };
        if optional_blank.contains(&canonical_key)
            && matches!(value, Value::String(text) if text.trim().is_empty())
        {
            continue;
        }
        normalized.insert(canonical_key, value.clone());
    }
    normalized
}

/// 执行前的工具调用归一化：先修工具名，再按该工具的 schema 修参数名。
pub fn normalize_tool_call(
    raw_name: &str,
    arguments: &Map<String, Value>,
    tools: &[(&str, &str)],
) -> (String, Map<String, Value>) {
    let tool_names: Vec<&str> = tools.iter().map(|(name, _)| *name).collect();
    let name = normalize_tool_name(raw_name, &tool_names);
    let normalized = normalize_tool_arguments(&name, arguments, tools);
    (name, normalized)
}

/// 可进入 Session、确认页与 SSE 的工具参数投影。
pub fn public_tool_arguments(tool_name: &str, arguments: &Map<String, Value>) -> Value {
    if tool_name == INVOKE_TOOL_NAME {
        let target = truncate_chars(
            &arguments
                .get("tool_name")
                .map(python_text)
                .unwrap_or_default(),
            200,
        );
        let mut public = Map::new();
        public.insert("tool_name".to_string(), Value::from(target));
        match arguments.get("arguments") {
            Some(Value::Object(inner)) => {
                let mut keys: Vec<String> = inner
                    .keys()
                    .map(|key| truncate_chars(&python_text(&Value::from(key.clone())), 100))
                    .collect();
                keys.sort();
                keys.truncate(50);
                public.insert(
                    "argument_keys".to_string(),
                    Value::Array(keys.into_iter().map(Value::from).collect()),
                );
                public.insert("argument_count".to_string(), Value::from(inner.len()));
            }
            _ => {
                public.insert("arguments_valid".to_string(), Value::from(false));
            }
        }
        return Value::Object(public);
    }

    if matches!(
        tool_name,
        "windows_window"
            | "windows_control"
            | "windows_input"
            | "windows_clipboard"
            | "windows_screenshot"
    ) {
        return public_windows_desktop_arguments(tool_name, arguments);
    }

    if tool_name != "subagent" {
        return Value::Object(arguments.clone());
    }

    let action = {
        let text = arguments
            .get("action")
            .map(python_text)
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            "run".to_string()
        } else {
            text
        }
    };
    // worktree 控制面用**原始** action 判定；只有任务分支才做闭集收敛。
    if matches!(
        action.as_str(),
        "apply_worktree" | "discard_worktree" | "list_worktrees"
    ) {
        let mut public = Map::new();
        public.insert("action".to_string(), Value::from(action));
        for key in ["task_id", "batch_id", "branch", "strategy"] {
            let Some(value) = arguments.get(key) else {
                continue;
            };
            let raw = python_text(value);
            let text = omnicrawl_session::redaction::redact_sensitive_text(raw.trim());
            if !text.is_empty() {
                public.insert(key.to_string(), Value::from(truncate_chars(&text, 200)));
            }
        }
        if arguments.contains_key("cleanup") {
            public.insert(
                "cleanup".to_string(),
                Value::from(python_bool(arguments.get("cleanup"))),
            );
        }
        if arguments.contains_key("remove_branch") {
            public.insert(
                "remove_branch".to_string(),
                Value::from(python_bool(arguments.get("remove_branch"))),
            );
        }
        return Value::Object(public);
    }

    let tasks = arguments.get("tasks");
    let is_task_list = matches!(tasks, Some(Value::Array(_)));
    if !is_task_list && arguments.contains_key("task_count") {
        let descriptions_source: Vec<Value> = match arguments.get("descriptions") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        let agent_types_source: Vec<Value> = match arguments.get("agent_types") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        let task_count = arguments.get("task_count");
        let valid_task_count = matches!(task_count, Some(Value::Number(number)) if number.as_i64().is_some_and(|value| value >= 0));
        let mut public = Map::new();
        public.insert(
            "action".to_string(),
            Value::from(normalized_action(&action)),
        );
        public.insert(
            "task_count".to_string(),
            if valid_task_count {
                task_count.cloned().unwrap_or(Value::from(0))
            } else {
                Value::from(0)
            },
        );
        public.insert(
            "descriptions".to_string(),
            Value::Array(
                descriptions_source
                    .iter()
                    .take(4)
                    .map(|item| {
                        Value::from(truncate_chars(
                            &omnicrawl_session::redaction::redact_sensitive_text(&python_text(
                                item,
                            )),
                            120,
                        ))
                    })
                    .collect(),
            ),
        );
        public.insert(
            "agent_types".to_string(),
            Value::Array(
                agent_types_source
                    .iter()
                    .take(4)
                    .map(|item| {
                        Value::from(truncate_chars(
                            &omnicrawl_session::redaction::redact_sensitive_text(&python_text(
                                item,
                            )),
                            120,
                        ))
                    })
                    .collect(),
            ),
        );
        public.insert(
            "max_concurrency".to_string(),
            arguments
                .get("max_concurrency")
                .cloned()
                .unwrap_or(Value::Null),
        );
        public.insert(
            "fail_fast".to_string(),
            Value::from(
                arguments
                    .get("fail_fast")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
        );
        return Value::Object(public);
    }

    let safe_tasks: Vec<Value> = match tasks {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let mut descriptions: Vec<Value> = Vec::new();
    let mut agent_types: Vec<Value> = Vec::new();
    for item in safe_tasks.iter().take(4) {
        let Some(task) = item.as_object() else {
            continue;
        };
        let raw_description = task.get("description").map(python_text).unwrap_or_default();
        let description =
            omnicrawl_session::redaction::redact_sensitive_text(raw_description.trim());
        if !description.is_empty() {
            descriptions.push(Value::from(truncate_chars(&description, 120)));
        }
        let raw_agent_type = task
            .get("subagent_type")
            .map(python_text)
            .unwrap_or_default();
        let agent_type = omnicrawl_session::redaction::redact_sensitive_text(raw_agent_type.trim());
        if !agent_type.is_empty() {
            agent_types.push(Value::from(truncate_chars(&agent_type, 120)));
        }
    }
    let mut public = Map::new();
    public.insert(
        "action".to_string(),
        Value::from(normalized_action(&action)),
    );
    public.insert("task_count".to_string(), Value::from(safe_tasks.len()));
    public.insert("descriptions".to_string(), Value::Array(descriptions));
    public.insert("agent_types".to_string(), Value::Array(agent_types));
    public.insert(
        "max_concurrency".to_string(),
        arguments
            .get("max_concurrency")
            .cloned()
            .unwrap_or(Value::Null),
    );
    public.insert(
        "fail_fast".to_string(),
        Value::from(
            arguments
                .get("fail_fast")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    Value::Object(public)
}

/// 桌面自动化参数投影：输入文本与剪贴板内容只暴露长度。
fn public_windows_desktop_arguments(tool_name: &str, arguments: &Map<String, Value>) -> Value {
    let action = arguments
        .get("action")
        .map(python_text)
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut public = Map::new();
    public.insert("action".to_string(), Value::from(action));
    const KEYS: [&str; 24] = [
        "window_handle",
        "title_contains",
        "class_name_contains",
        "visible_only",
        "include_untitled",
        "max_results",
        "name",
        "automation_id",
        "class_name",
        "control_type",
        "index",
        "x",
        "y",
        "button",
        "clicks",
        "wheel_delta",
        "key",
        "keys",
        "presses",
        "max_chars",
        "target",
        "width",
        "height",
        "max_dimension",
    ];
    for key in KEYS {
        if let Some(value) = arguments.get(key) {
            public.insert(key.to_string(), value.clone());
        }
    }
    if tool_name == "windows_control" {
        if let Some(Value::String(text)) = arguments.get("value") {
            public.insert(
                "value_length".to_string(),
                Value::from(text.chars().count()),
            );
        }
    }
    if matches!(tool_name, "windows_input" | "windows_clipboard") {
        if let Some(Value::String(text)) = arguments.get("text") {
            public.insert("text_length".to_string(), Value::from(text.chars().count()));
        }
    }
    Value::Object(public)
}

fn normalized_action(action: &str) -> String {
    match action {
        "run" | "spawn" | "list" | "get" | "cancel" => action.to_string(),
        _ => "run".to_string(),
    }
}

fn python_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

fn python_bool(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|item| item != 0.0),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        _ => false,
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

// ------------------------------------------------------------------ Schema 压缩与校验

const COMPACT_SCHEMA_KEYS: [&str; 19] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "oneOf",
    "anyOf",
    "allOf",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "pattern",
];

/// 折叠工具说明里的空白（多行拼接的换行与缩进归并为单空格）。
pub fn compact_tool_description(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 删除长描述与默认值，只保留模型填写参数所需的 Schema 信息。
///
/// 列表字段（`enum`/`required`/`oneOf`…）不做长度截断：它们要么是语义契约、要么是
/// 单元素描述，截断任何一项都可能改变契约语义。入参是工具的 `argument_schema` 原文；
/// 解析失败或不是对象时回落成空对象 schema（与 Python 的 `tool_parameters_schema` 一致）。
pub fn compact_tool_schema(argument_schema: &str) -> Value {
    compact_schema_node(&parse_schema_or_default(argument_schema), 0)
}

fn parse_schema_or_default(argument_schema: &str) -> Value {
    serde_json::from_str::<Value>(argument_schema)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}))
}

fn compact_schema_node(value: &Value, depth: usize) -> Value {
    if depth > 6 {
        let mut fallback = Map::new();
        fallback.insert("type".to_string(), Value::from("object"));
        return Value::Object(fallback);
    }
    match value {
        Value::Object(map) => {
            let mut compacted = Map::new();
            for (key, child) in map {
                if !COMPACT_SCHEMA_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if key == "enum" {
                    compacted.insert(key.clone(), child.clone());
                } else if key == "properties" {
                    match child {
                        Value::Object(properties) => {
                            let mut next = Map::new();
                            for (name, property) in properties {
                                next.insert(name.clone(), compact_schema_node(property, depth + 1));
                            }
                            compacted.insert(key.clone(), Value::Object(next));
                        }
                        other => {
                            compacted.insert(key.clone(), other.clone());
                        }
                    }
                } else {
                    compacted.insert(key.clone(), compact_schema_node(child, depth + 1));
                }
            }
            Value::Object(compacted)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| compact_schema_node(item, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// 参数校验问题：`path` 是出错位置，`message` 是与 Python 逐字一致的中文说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolIssue {
    pub path: String,
    pub message: String,
}

impl ToolIssue {
    fn new(path: &str, message: String) -> Self {
        Self {
            path: path.to_string(),
            message,
        }
    }

    fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("path".to_string(), Value::from(self.path.clone()));
        map.insert("message".to_string(), Value::from(self.message.clone()));
        Value::Object(map)
    }
}

/// 按压缩后的 Schema 校验参数，返回全部问题（空表示通过）。
pub fn validate_tool_arguments(argument_schema: &str, arguments: &Value) -> Vec<ToolIssue> {
    let compacted = compact_tool_schema(argument_schema);
    let mut issues = Vec::new();
    validate_schema_node(arguments, &compacted, "arguments", &mut issues);
    issues
}

fn validate_schema_node(value: &Value, schema: &Value, path: &str, issues: &mut Vec<ToolIssue>) {
    let Some(schema_map) = schema.as_object() else {
        return;
    };

    if let Some(Value::Array(enum_values)) = schema_map.get("enum") {
        if !enum_values.contains(value) {
            issues.push(ToolIssue::new(
                path,
                format!(
                    "必须是以下值之一：{}",
                    python_repr(&Value::Array(enum_values.clone()))
                ),
            ));
            return;
        }
    }
    if let Some(expected) = schema_map.get("const") {
        if value != expected {
            issues.push(ToolIssue::new(
                path,
                format!("必须等于 {}", python_repr(expected)),
            ));
            return;
        }
    }

    let alternatives = schema_map
        .get("oneOf")
        .or_else(|| schema_map.get("anyOf"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty());
    if let Some(alternatives) = alternatives {
        let valid = alternatives.iter().any(|alternative| {
            let mut candidate = Vec::new();
            validate_schema_node(value, alternative, path, &mut candidate);
            candidate.is_empty()
        });
        if !valid {
            issues.push(ToolIssue::new(
                path,
                "不符合任一允许的参数结构。".to_string(),
            ));
        }
        return;
    }

    match schema_map.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(value_map) = value.as_object() else {
                issues.push(ToolIssue::new(path, "必须是 object。".to_string()));
                return;
            };
            let empty = Map::new();
            let properties = schema_map
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            if let Some(Value::Array(required)) = schema_map.get("required") {
                for key in required.iter().filter_map(Value::as_str) {
                    if !value_map.contains_key(key) {
                        issues.push(ToolIssue::new(
                            &format!("{path}.{key}"),
                            "缺少必填字段。".to_string(),
                        ));
                    }
                }
            }
            if schema_map.get("additionalProperties") == Some(&Value::Bool(false)) {
                for key in value_map.keys() {
                    if !properties.contains_key(key) {
                        issues.push(ToolIssue::new(
                            &format!("{path}.{key}"),
                            "不是声明的字段。".to_string(),
                        ));
                    }
                }
            }
            validate_number_bound(
                value,
                schema_map,
                path,
                issues,
                "minProperties",
                "属性至少为",
            );
            validate_number_bound(
                value,
                schema_map,
                path,
                issues,
                "maxProperties",
                "属性最多为",
            );
            for (key, child_schema) in properties {
                if let Some(child_value) = value_map.get(key) {
                    validate_schema_node(
                        child_value,
                        child_schema,
                        &format!("{path}.{key}"),
                        issues,
                    );
                }
            }
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                issues.push(ToolIssue::new(path, "必须是 array。".to_string()));
                return;
            };
            validate_number_bound(value, schema_map, path, issues, "minItems", "至少包含");
            validate_number_bound(value, schema_map, path, issues, "maxItems", "最多包含");
            if let Some(item_schema) = schema_map.get("items") {
                if item_schema.is_object() {
                    for (index, item) in items.iter().enumerate() {
                        validate_schema_node(
                            item,
                            item_schema,
                            &format!("{path}[{index}]"),
                            issues,
                        );
                    }
                }
            }
        }
        Some("string") => {
            let Some(text) = value.as_str() else {
                issues.push(ToolIssue::new(path, "必须是 string。".to_string()));
                return;
            };
            validate_number_bound(value, schema_map, path, issues, "minLength", "长度至少为");
            validate_number_bound(value, schema_map, path, issues, "maxLength", "长度最多为");
            if let Some(pattern) = schema_map.get("pattern").and_then(Value::as_str) {
                if !pattern_is_literal(pattern) {
                    // 正则子集未搬：当前工具 Schema 未使用 pattern（见 crate README）。
                } else if !text.contains(pattern) {
                    issues.push(ToolIssue::new(path, "不符合字段格式要求。".to_string()));
                }
            }
        }
        Some("integer") => {
            if !matches!(value, Value::Number(number) if number.is_i64() || number.is_u64()) {
                issues.push(ToolIssue::new(path, "必须是 integer。".to_string()));
                return;
            }
            validate_number_bound(value, schema_map, path, issues, "minimum", "不能小于");
            validate_number_bound(value, schema_map, path, issues, "maximum", "不能大于");
        }
        Some("number") => {
            if !value.is_number() {
                issues.push(ToolIssue::new(path, "必须是 number。".to_string()));
                return;
            }
            validate_number_bound(value, schema_map, path, issues, "minimum", "不能小于");
            validate_number_bound(value, schema_map, path, issues, "maximum", "不能大于");
        }
        Some("boolean") => {
            if !value.is_boolean() {
                issues.push(ToolIssue::new(path, "必须是 boolean。".to_string()));
            }
        }
        Some("null") if !value.is_null() => {
            issues.push(ToolIssue::new(path, "必须是 null。".to_string()));
        }
        _ => {}
    }
}

fn pattern_is_literal(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern
            .chars()
            .all(|item| item.is_alphanumeric() || item == '_' || item == '-')
}

fn validate_number_bound(
    value: &Value,
    schema: &Map<String, Value>,
    path: &str,
    issues: &mut Vec<ToolIssue>,
    key: &str,
    message_prefix: &str,
) {
    let Some(bound) = schema.get(key).filter(|item| item.is_number()) else {
        return;
    };
    let comparable = match key {
        "minLength" | "maxLength" => match value.as_str() {
            Some(text) => text.chars().count() as f64,
            None => return,
        },
        "minItems" | "maxItems" => match value.as_array() {
            Some(items) => items.len() as f64,
            None => return,
        },
        "minProperties" | "maxProperties" => match value.as_object() {
            Some(map) => map.len() as f64,
            None => return,
        },
        _ => match value.as_f64() {
            Some(number) => number,
            None => return,
        },
    };
    let target = bound.as_f64().unwrap_or_default();
    let violated = if key.starts_with("min") {
        comparable < target
    } else if key.starts_with("max") {
        comparable > target
    } else if key == "minimum" {
        comparable < target
    } else if key == "maximum" {
        comparable > target
    } else {
        false
    };
    if violated {
        issues.push(ToolIssue::new(
            path,
            format!("{message_prefix} {}。", python_number_text(bound)),
        ));
    }
}

/// 目录检索用的有界整数：只认真正的整数（布尔与非整数一律回落默认值）。
pub fn bounded_int(value: Option<&Value>, default: i64, minimum: i64, maximum: i64) -> i64 {
    let Some(value) = value else {
        return default;
    };
    let Some(parsed) = value.as_i64() else {
        return default;
    };
    parsed.clamp(minimum, maximum)
}

// --------------------------------------------------------------------- 结果信封

/// 结构化错误信封：`schema_version` + `error{code,message,retryable}`。
pub fn error_result(
    code: &str,
    message: &str,
    tool_name: &str,
    retryable: bool,
    extra: Option<&Map<String, Value>>,
) -> ToolResult {
    let mut error = Map::new();
    error.insert("code".to_string(), Value::from(code));
    error.insert("message".to_string(), Value::from(message));
    error.insert("retryable".to_string(), Value::from(retryable));
    if let Some(extra) = extra {
        for (key, value) in extra {
            error.insert(key.clone(), value.clone());
        }
    }
    let mut payload = Map::new();
    payload.insert("schema_version".to_string(), Value::from(1));
    payload.insert("ok".to_string(), Value::from(false));
    payload.insert("error".to_string(), Value::Object(error));
    if !tool_name.is_empty() {
        payload.insert("tool_name".to_string(), Value::from(tool_name));
    }
    let text = python_dumps(&Value::Object(payload), 2);
    ToolResult {
        ok: false,
        output: text.clone(),
        full_output: text,
        ..ToolResult::default()
    }
}

/// 把 Host 二次校验失败转换成可供模型修正的结构化结果。
pub fn tool_validation_error_result(
    tool_name: &str,
    argument_schema: &str,
    issues: &[ToolIssue],
) -> ToolResult {
    let mut extra = Map::new();
    extra.insert(
        "issues".to_string(),
        Value::Array(issues.iter().map(ToolIssue::to_value).collect()),
    );
    extra.insert("contract".to_string(), compact_tool_schema(argument_schema));
    error_result(
        "invalid_arguments",
        &format!("工具 {tool_name} 的参数未通过 Schema 校验。"),
        tool_name,
        true,
        Some(&extra),
    )
}

/// `json.dumps(data, ensure_ascii=False, indent=2)` 形态的成功结果。
pub fn json_tool_result(data: &Value) -> ToolResult {
    ToolResult {
        ok: true,
        output: python_dumps(data, 2),
        ..ToolResult::default()
    }
}

pub fn mcp_prompt_arguments_error() -> ToolResult {
    ToolResult {
        ok: false,
        output: "arguments 必须是 JSON 对象。".to_string(),
        ..ToolResult::default()
    }
}

/// MCP 调用结果的可见字段（对应 `MCPToolResult` 等）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpResultFields {
    pub ok: bool,
    pub server_name: String,
    pub item_name: String,
    pub audit_id: String,
    pub duration_ms: i64,
    pub error_code: String,
    pub retryable: bool,
    pub output: String,
    pub full_output: String,
}

/// 组装 MCP 结果字段（供宿主与对照测试使用；不涉及任何网络调用）。
#[allow(clippy::too_many_arguments)]
pub fn mcp_fields(
    ok: bool,
    server_name: &str,
    item_name: &str,
    audit_id: &str,
    duration_ms: i64,
    error_code: &str,
    retryable: bool,
    output: &str,
    full_output: &str,
) -> McpResultFields {
    McpResultFields {
        ok,
        server_name: server_name.to_string(),
        item_name: item_name.to_string(),
        audit_id: audit_id.to_string(),
        duration_ms,
        error_code: error_code.to_string(),
        retryable,
        output: output.to_string(),
        full_output: full_output.to_string(),
    }
}

/// MCP 结果文本拼接：返回 (可见输出, 完整输出)，两者相同时复用同一字符串。
pub fn join_mcp_output(
    header_parts: &[String],
    output: &str,
    full_output: &str,
) -> (String, String) {
    let text = format!("{}\n输出：\n{output}", header_parts.join("\n"));
    if full_output.is_empty() || full_output == output {
        return (text.clone(), text);
    }
    let full_text = format!("{}\n输出：\n{full_output}", header_parts.join("\n"));
    (text, full_text)
}

fn mcp_result(fields: McpResultFields, kind: &str) -> ToolResult {
    let label = match kind {
        "tool" => format!("MCP Tool：{}.{}", fields.server_name, fields.item_name),
        "resource" => format!("MCP Resource：{}:{}", fields.server_name, fields.item_name),
        _ => format!("MCP Prompt：{}.{}", fields.server_name, fields.item_name),
    };
    let mut parts = vec![label];
    if kind == "tool" {
        parts.push(format!("审计 ID：{}", fields.audit_id));
    }
    parts.push(format!("耗时：{} ms", fields.duration_ms));
    if !fields.error_code.is_empty() {
        parts.push(format!("错误码：{}", fields.error_code));
    }
    if fields.retryable {
        parts.push("可重试：是".to_string());
    }
    let (text, full_text) = join_mcp_output(&parts, &fields.output, &fields.full_output);
    ToolResult {
        ok: fields.ok,
        output: text,
        full_output: full_text,
        ..ToolResult::default()
    }
}

pub fn mcp_tool_result(fields: McpResultFields) -> ToolResult {
    mcp_result(fields, "tool")
}

pub fn mcp_resource_result(fields: McpResultFields) -> ToolResult {
    mcp_result(fields, "resource")
}

pub fn mcp_prompt_result(fields: McpResultFields) -> ToolResult {
    mcp_result(fields, "prompt")
}

/// 必填字符串列表：非列表或空项一律丢弃，逐项 `strip()`。
pub fn read_required_string_list(arguments: &Map<String, Value>, key: &str) -> Vec<String> {
    let Some(Value::Array(items)) = arguments.get(key) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect()
}

/// 可选字符串列表：缺省或非列表返回 `None`，其余与必填同规则。
pub fn read_optional_string_list(arguments: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    match arguments.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::Array(_)) => Some(read_required_string_list(arguments, key)),
        _ => None,
    }
}

/// 工具参数里的有界整数：`int(value)` 语义（字符串可解析、浮点截断），
/// 布尔与不可解析值回落默认值，其余夹到 `[1, maximum]`。
pub fn read_limited_int(
    arguments: &Map<String, Value>,
    key: &str,
    default: i64,
    maximum: i64,
) -> i64 {
    let Some(value) = arguments.get(key) else {
        return default;
    };
    let parsed = match value {
        Value::Bool(_) => return default,
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.trunc() as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    parsed
        .map(|item| item.clamp(1, std::cmp::max(1, maximum)))
        .unwrap_or(default)
}
