//! `omnicrawl/agent/context_compaction/validation.py`：结构化摘要的 Schema、来源、精确证据与
//! 长度预算校验。
//!
//! 拒绝三类摘要：不可恢复（旧约束丢失）、无来源（引用了不存在的事件）、把未完成工具链或
//! 「该记没记」的文件/用户消息写成已完成。

use serde_json::{Map, Value};

use super::policy::{estimate_json_tokens, SourceEvent};

pub const PLAIN_LIST_FIELDS: [&str; 2] = ["objective", "current_state"];

pub const REFERENCED_LIST_FIELDS: [&str; 12] = [
    "constraints",
    "decisions",
    "completed",
    "open_issues",
    "artifacts",
    "exact_evidence",
    "failed_attempts",
    "excluded_approaches",
    "key_concepts",
    "problem_solving_process",
    "user_messages",
    "next_steps",
];

/// 新增的「过程与负信息」字段：缺失时按空数组宽容处理，不破坏旧摘要兼容。
pub const OPTIONAL_REFERENCED_FIELDS: [&str; 6] = [
    "failed_attempts",
    "excluded_approaches",
    "key_concepts",
    "problem_solving_process",
    "user_messages",
    "next_steps",
];

/// 写类工具：用于「该记的没记」完整性校验。
pub const FILE_CHANGE_TOOLS: [&str; 2] = ["write_file", "Edit_file"];

pub const FILE_LIST_FIELDS: [&str; 2] = ["read_files", "modified_files"];

/// 超过该长度的用户消息事件会被 summary 分块发送，因此原文强校验与覆盖检查对其豁免。
pub const LARGE_EVENT_SPLIT_EXEMPT_CHARS: usize = 60_000;

/// 校验结论：`normalized` 只在无错误时给出。
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryValidation {
    pub valid: bool,
    pub errors: Vec<String>,
    pub normalized: Option<Value>,
}

/// 校验入参。
pub struct SummaryValidationInput<'a> {
    pub structured: &'a Value,
    pub source_events: &'a [SourceEvent],
    pub target_summary_tokens: i64,
    pub previous_summary: Option<&'a Value>,
    pub preserve_exact_evidence: bool,
    pub completeness_events: Option<&'a [SourceEvent]>,
}

/// 校验入口：调用方只需给判定所需的事实，不必自行拼装结构体。
#[allow(clippy::too_many_arguments)]
pub fn validate_summary(
    structured: &Value,
    source_events: &[SourceEvent],
    target_summary_tokens: i64,
    previous_summary: Option<&Value>,
    preserve_exact_evidence: bool,
    completeness_events: Option<&[SourceEvent]>,
) -> SummaryValidation {
    let input = SummaryValidationInput {
        structured,
        source_events,
        target_summary_tokens,
        previous_summary,
        preserve_exact_evidence,
        completeness_events,
    };
    SummaryValidator.validate(&input)
}

/// 拒绝不可恢复、无来源或把未完成工具链写成完成的摘要。
#[derive(Debug, Clone, Default)]
pub struct SummaryValidator;

impl SummaryValidator {
    pub fn validate(&self, input: &SummaryValidationInput<'_>) -> SummaryValidation {
        let mut errors: Vec<String> = Vec::new();
        let mut normalized = Map::new();
        let empty: Vec<SourceEvent> = Vec::new();
        let source_events = input.source_events;
        let _ = &empty;

        for field in PLAIN_LIST_FIELDS {
            let value = input.structured.get(field);
            let items = match value {
                Some(Value::Array(items))
                    if !items.iter().any(
                        |item| !matches!(item, Value::String(text) if !text.trim().is_empty()),
                    ) =>
                {
                    items
                        .iter()
                        .map(|item| Value::from(value_text(item).trim().to_string()))
                        .collect::<Vec<_>>()
                }
                _ => {
                    errors.push(format!("{field} 必须是非空字符串数组。"));
                    Vec::new()
                }
            };
            normalized.insert(field.to_string(), Value::Array(items));
        }
        if normalized
            .get("objective")
            .and_then(Value::as_array)
            .map(Vec::is_empty)
            .unwrap_or(true)
        {
            errors.push("objective 至少需要一项目标。".to_string());
        }

        for field in REFERENCED_LIST_FIELDS {
            let value = input.structured.get(field);
            let mut normalized_items: Vec<Value> = Vec::new();
            if (value.is_none() || matches!(value, Some(Value::Null)))
                && OPTIONAL_REFERENCED_FIELDS.contains(&field)
            {
                normalized.insert(field.to_string(), Value::Array(normalized_items));
                continue;
            }
            let Some(Value::Array(items)) = value else {
                errors.push(format!("{field} 必须是数组。"));
                normalized.insert(field.to_string(), Value::Array(normalized_items));
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                let Some(entry) = item.as_object() else {
                    errors.push(format!("{field}[{index}] 必须是对象。"));
                    continue;
                };
                let text = value_text(entry.get("text").unwrap_or(&Value::Null));
                if text.trim().is_empty() {
                    errors.push(format!("{field}[{index}].text 必须是非空字符串。"));
                    continue;
                }
                let refs = match entry.get("source_event_ids") {
                    Some(Value::Array(refs))
                        if !refs.is_empty()
                            && !refs.iter().any(
                                |item| !matches!(item, Value::String(value) if !value.is_empty()),
                            ) =>
                    {
                        refs.iter()
                            .map(|item| item.as_str().unwrap_or_default().to_string())
                            .collect::<Vec<_>>()
                    }
                    _ => {
                        errors.push(format!(
                            "{field}[{index}].source_event_ids 必须是非空字符串数组。"
                        ));
                        continue;
                    }
                };
                let unknown: Vec<&String> = refs
                    .iter()
                    .filter(|reference| {
                        !source_events
                            .iter()
                            .any(|event| event.event_id == ***reference)
                    })
                    .collect();
                if !unknown.is_empty() {
                    let joined = unknown
                        .iter()
                        .map(|item| item.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    errors.push(format!("{field}[{index}] 引用了不存在的事件：{joined}。"));
                }
                let mut normalized_entry = Map::new();
                normalized_entry.insert("text".to_string(), Value::from(text.trim().to_string()));
                normalized_entry.insert(
                    "source_event_ids".to_string(),
                    Value::Array(dedup_values(&refs)),
                );
                normalized_items.push(Value::Object(normalized_entry));
            }
            normalized.insert(field.to_string(), Value::Array(normalized_items));
        }

        for field in FILE_LIST_FIELDS {
            let value = input.structured.get(field);
            let mut normalized_items: Vec<Value> = Vec::new();
            if value.is_none() || matches!(value, Some(Value::Null)) {
                normalized.insert(field.to_string(), Value::Array(normalized_items));
                continue;
            }
            let Some(Value::Array(items)) = value else {
                errors.push(format!("{field} 必须是数组。"));
                normalized.insert(field.to_string(), Value::Array(normalized_items));
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                let Some(entry) = item.as_object() else {
                    errors.push(format!("{field}[{index}] 必须是对象。"));
                    continue;
                };
                let path = value_text(entry.get("path").unwrap_or(&Value::Null));
                let description = value_text(entry.get("description").unwrap_or(&Value::Null));
                if path.trim().is_empty() {
                    errors.push(format!("{field}[{index}].path 必须是非空字符串。"));
                    continue;
                }
                if description.trim().is_empty() {
                    errors.push(format!("{field}[{index}].description 必须是非空字符串。"));
                    continue;
                }
                let refs = match entry.get("source_event_ids") {
                    Some(Value::Array(refs))
                        if !refs.is_empty()
                            && !refs.iter().any(
                                |item| !matches!(item, Value::String(value) if !value.is_empty()),
                            ) =>
                    {
                        refs.iter()
                            .map(|item| item.as_str().unwrap_or_default().to_string())
                            .collect::<Vec<_>>()
                    }
                    _ => {
                        errors.push(format!(
                            "{field}[{index}].source_event_ids 必须是非空字符串数组。"
                        ));
                        continue;
                    }
                };
                let unknown: Vec<String> = refs
                    .iter()
                    .filter(|reference| {
                        !source_events
                            .iter()
                            .any(|event| event.event_id == **reference)
                    })
                    .cloned()
                    .collect();
                if !unknown.is_empty() {
                    errors.push(format!(
                        "{field}[{index}] 引用了不存在的事件：{}。",
                        unknown.join(", ")
                    ));
                }
                let mut normalized_entry = Map::new();
                normalized_entry.insert("path".to_string(), Value::from(path.trim().to_string()));
                normalized_entry.insert(
                    "description".to_string(),
                    Value::from(description.trim().to_string()),
                );
                normalized_entry.insert(
                    "source_event_ids".to_string(),
                    Value::Array(dedup_values(&refs)),
                );
                normalized_items.push(Value::Object(normalized_entry));
            }
            normalized.insert(field.to_string(), Value::Array(normalized_items));
        }

        let normalized_value = Value::Object(normalized.clone());
        validate_previous_constraints(input.previous_summary, &normalized_value, &mut errors);
        if input.preserve_exact_evidence {
            validate_exact_evidence(&normalized_value, source_events, &mut errors);
        } else {
            normalized.insert("exact_evidence".to_string(), Value::Array(Vec::new()));
        }
        let normalized_value = Value::Object(normalized.clone());
        validate_user_messages(&normalized_value, source_events, &mut errors);
        validate_completed_tool_chains(&normalized_value, source_events, &mut errors);
        validate_completeness(
            &normalized_value,
            input.completeness_events.unwrap_or(&empty),
            &mut errors,
        );
        // target_summary_tokens <= 0 表示无摘要预算上限：不因摘要长度拒绝。
        if input.target_summary_tokens > 0
            && estimate_json_tokens(&normalized_value) > input.target_summary_tokens
        {
            errors.push("结构化摘要超过 target_summary_tokens。".to_string());
        }
        SummaryValidation {
            valid: errors.is_empty(),
            normalized: if errors.is_empty() {
                Some(normalized_value)
            } else {
                None
            },
            errors,
        }
    }
}

fn dedup_values(values: &[String]) -> Vec<Value> {
    let mut seen: Vec<Value> = Vec::new();
    for value in values {
        let item = Value::from(value.clone());
        if !seen.contains(&item) {
            seen.push(item);
        }
    }
    seen
}

fn object_text(item: &Value, key: &str) -> String {
    item.get(key)
        .map(value_text)
        .unwrap_or_default()
        .to_string()
}

fn validate_previous_constraints(
    previous_summary: Option<&Value>,
    normalized: &Value,
    errors: &mut Vec<String>,
) {
    let Some(previous_summary) = previous_summary else {
        return;
    };
    let Some(previous_structured) = previous_summary
        .get("structured")
        .and_then(Value::as_object)
    else {
        return;
    };
    let Some(old_constraints) = previous_structured
        .get("constraints")
        .and_then(Value::as_array)
    else {
        return;
    };
    let current_text: Vec<String> = normalized
        .get("constraints")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .map(|item| object_text(item, "text"))
                .collect()
        })
        .unwrap_or_default();
    let decisions_text = normalized
        .get("decisions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .map(|item| object_text(item, "text"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    for item in old_constraints {
        if !item.is_object() {
            continue;
        }
        let text = object_text(item, "text").trim().to_string();
        if !text.is_empty() && !current_text.contains(&text) && !decisions_text.contains(&text) {
            errors.push(format!("既有约束未保留或说明失效：{text}"));
        }
    }
}

fn validate_exact_evidence(
    normalized: &Value,
    source_events: &[SourceEvent],
    errors: &mut Vec<String>,
) {
    let Some(items) = normalized.get("exact_evidence").and_then(Value::as_array) else {
        return;
    };
    for (index, item) in items.iter().enumerate() {
        let text = object_text(item, "text");
        let refs: Vec<String> = item
            .get("source_event_ids")
            .and_then(Value::as_array)
            .map(|refs| {
                refs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut source_text = String::new();
        for reference in refs {
            let Some(event) = source_events
                .iter()
                .find(|event| event.event_id == reference)
            else {
                continue;
            };
            for value in string_values(&event.payload) {
                source_text.push_str(&value);
                source_text.push('\n');
            }
        }
        if !text.is_empty() && !source_text.contains(&text) {
            errors.push(format!("exact_evidence[{index}] 与来源事件原文不一致。"));
        }
    }
}

fn validate_user_messages(
    normalized: &Value,
    source_events: &[SourceEvent],
    errors: &mut Vec<String>,
) {
    let Some(items) = normalized.get("user_messages").and_then(Value::as_array) else {
        return;
    };
    for (index, item) in items.iter().enumerate() {
        let text = object_text(item, "text").trim().to_string();
        let refs: Vec<String> = item
            .get("source_event_ids")
            .and_then(Value::as_array)
            .map(|refs| {
                refs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if text.is_empty() {
            errors.push(format!("user_messages[{index}].text 必须是非空字符串。"));
            continue;
        }
        for reference in refs {
            let Some(event) = source_events
                .iter()
                .find(|event| event.event_id == reference)
            else {
                continue;
            };
            if event.event_type != "user_message" {
                errors.push(format!(
                    "user_messages[{index}] 引用了非用户消息事件：{reference}（{}）。",
                    event.event_type
                ));
                continue;
            }
            let content = value_text(event.payload.get("content").unwrap_or(&Value::Null));
            if !content.trim().is_empty()
                && content.chars().count() < LARGE_EVENT_SPLIT_EXEMPT_CHARS
                && text != content.trim()
            {
                errors.push(format!(
                    "user_messages[{index}] 不是用户消息原文（事件 {reference}）。"
                ));
            }
        }
    }
}

fn validate_completeness(
    normalized: &Value,
    completeness_events: &[SourceEvent],
    errors: &mut Vec<String>,
) {
    if completeness_events.is_empty() {
        return;
    }
    let mut call_events: Vec<&SourceEvent> = Vec::new();
    let mut result_by_call_id: Vec<(String, &SourceEvent)> = Vec::new();
    for event in completeness_events {
        if event.event_type == "tool_call_requested" {
            call_events.push(event);
        } else if event.event_type == "tool_result" {
            let call_id = value_text(event.payload.get("tool_call_id").unwrap_or(&Value::Null));
            if !call_id.is_empty() {
                result_by_call_id.push((call_id, event));
            }
        }
    }

    // 1) 成功写入的文件必须被 modified_files 覆盖（引用事件 ID 或路径匹配任一）。
    let modified_items: Vec<&Value> = normalized
        .get("modified_files")
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default();
    let modified_refs: Vec<String> = modified_items
        .iter()
        .flat_map(|item| {
            item.get("source_event_ids")
                .and_then(Value::as_array)
                .map(|refs| {
                    refs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    let modified_paths: Vec<String> = modified_items
        .iter()
        .map(|item| object_text(item, "path").trim().to_string())
        .collect();
    for event in &call_events {
        let tool = value_text(event.payload.get("tool").unwrap_or(&Value::Null));
        if !FILE_CHANGE_TOOLS.contains(&tool.as_str()) {
            continue;
        }
        let call_id = value_text(event.payload.get("tool_call_id").unwrap_or(&Value::Null));
        let Some(result) = result_by_call_id
            .iter()
            .find(|(key, _)| *key == call_id)
            .map(|(_, event)| *event)
        else {
            continue;
        };
        if !result
            .payload
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if modified_refs.contains(&event.event_id) {
            continue;
        }
        let path = event
            .payload
            .get("arguments")
            .and_then(Value::as_object)
            .map(|arguments| value_text(arguments.get("path").unwrap_or(&Value::Null)))
            .unwrap_or_default()
            .trim()
            .to_string();
        if !path.is_empty()
            && modified_paths
                .iter()
                .any(|candidate| path_equivalent(&path, candidate))
        {
            continue;
        }
        errors.push(format!(
            "modified_files 缺少对成功写入文件的覆盖：{}。",
            if path.is_empty() {
                event.event_id.clone()
            } else {
                path
            }
        ));
    }

    // 2) 失败的工具调用必须被 failed_attempts 覆盖（引用调用或结果事件 ID）。
    let failed_refs: Vec<String> = normalized
        .get("failed_attempts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .flat_map(|item| {
                    item.get("source_event_ids")
                        .and_then(Value::as_array)
                        .map(|refs| {
                            refs.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    for event in &call_events {
        let call_id = value_text(event.payload.get("tool_call_id").unwrap_or(&Value::Null));
        let Some(result) = result_by_call_id
            .iter()
            .find(|(key, _)| *key == call_id)
            .map(|(_, event)| *event)
        else {
            continue;
        };
        if result
            .payload
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if failed_refs.contains(&event.event_id) || failed_refs.contains(&result.event_id) {
            continue;
        }
        let tool = value_text(event.payload.get("tool").unwrap_or(&Value::Null));
        errors.push(format!(
            "failed_attempts 缺少对失败工具调用的覆盖：{tool}（事件 {}）。",
            event.event_id
        ));
    }

    // 3) 被压缩窗口内的用户消息必须被 user_messages 原文覆盖（引用事件 ID）。
    let user_refs: Vec<String> = normalized
        .get("user_messages")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .flat_map(|item| {
                    item.get("source_event_ids")
                        .and_then(Value::as_array)
                        .map(|refs| {
                            refs.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    for event in completeness_events {
        if event.event_type != "user_message" {
            continue;
        }
        let content = value_text(event.payload.get("content").unwrap_or(&Value::Null));
        if content.trim().is_empty() || content.chars().count() >= LARGE_EVENT_SPLIT_EXEMPT_CHARS {
            continue;
        }
        if user_refs.contains(&event.event_id) {
            continue;
        }
        let head: String = content.trim().chars().take(40).collect();
        errors.push(format!(
            "user_messages 缺少对用户消息的覆盖：{head}…（事件 {}）。",
            event.event_id
        ));
    }
}

fn validate_completed_tool_chains(
    normalized: &Value,
    source_events: &[SourceEvent],
    errors: &mut Vec<String>,
) {
    let completed_refs: Vec<String> = normalized
        .get("completed")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .flat_map(|item| {
                    item.get("source_event_ids")
                        .and_then(Value::as_array)
                        .map(|refs| {
                            refs.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    let result_call_ids: Vec<String> = source_events
        .iter()
        .filter(|event| event.event_type == "tool_result")
        .map(|event| value_text(event.payload.get("tool_call_id").unwrap_or(&Value::Null)))
        .collect();
    for reference in completed_refs {
        let Some(event) = source_events
            .iter()
            .find(|event| event.event_id == reference)
        else {
            continue;
        };
        if event.event_type != "tool_call_requested" {
            continue;
        }
        let call_id = value_text(event.payload.get("tool_call_id").unwrap_or(&Value::Null));
        if !call_id.is_empty() && !result_call_ids.contains(&call_id) {
            errors.push(format!("completed 引用了未完成工具调用：{call_id}。"));
        }
    }
}

/// 宽松路径等价：统一分隔符、去掉 `./` 前缀后比较，允许相对/绝对差异。
pub fn path_equivalent(left: &str, right: &str) -> bool {
    fn normalize(value: &str) -> String {
        let mut result = value.replace('\\', "/");
        result = result.trim().to_string();
        while result.starts_with("./") {
            result = result[2..].to_string();
        }
        result.trim_end_matches('/').to_string()
    }

    let left_normalized = normalize(left);
    let right_normalized = normalize(right);
    if left_normalized.is_empty() || right_normalized.is_empty() {
        return false;
    }
    if left_normalized == right_normalized {
        return true;
    }
    left_normalized.ends_with(&format!("/{right_normalized}"))
        || right_normalized.ends_with(&format!("/{left_normalized}"))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.to_string(),
            None => number.to_string(),
        },
        Value::Array(items) => items.iter().map(value_text).collect::<Vec<_>>().join(", "),
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| format!("{key}: {}", value_text(item)))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn string_values(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map.values().flat_map(string_values).collect(),
        Value::Array(items) => items.iter().flat_map(string_values).collect(),
        Value::Null => Vec::new(),
        other => vec![value_text(other)],
    }
}

#[allow(dead_code)]
fn unused(_map: &Map<String, Value>) {}
