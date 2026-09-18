//! `omnicrawl/agent/context_compaction/projection.py`：结构化摘要、最近原文与工具事件的
//! 模型历史投影。

use serde_json::{Map, Value};

use super::policy::SourceEvent;
use crate::json::python_dumps_sorted;

use super::policy::COMPACT_SUMMARY_PREFIX;

/// 构建摘要在前、最近原文与最终回复锚点在后的稳定模型上下文。
#[derive(Debug, Clone, Default)]
pub struct ContextAssembler;

impl ContextAssembler {
    pub fn assemble(
        &self,
        structured: &Value,
        recent_events: &[SourceEvent],
        final_reply_event: Option<&SourceEvent>,
    ) -> Vec<Value> {
        let summary = render_summary_markdown(structured);
        let mut messages = vec![summary_message(&summary)];
        let anchor_id = final_reply_event.map(|event| event.event_id.clone());
        for event in recent_events {
            if Some(event.event_id.clone()) == anchor_id {
                continue;
            }
            if let Some(message) = event_to_model_message(event) {
                messages.push(message);
            }
        }
        if let Some(event) = final_reply_event {
            if let Some(message) = event_to_model_message(event) {
                messages.push(message);
            }
        }
        messages
    }

    pub fn recent_message_count(events: &[SourceEvent]) -> usize {
        events
            .iter()
            .filter(|event| event_to_model_message(event).is_some())
            .count()
    }
}

/// 装配入口：摘要在前、最近原文与最终回复锚点在后的模型上下文。
pub fn assemble_summary_history(
    structured: &Value,
    recent_events: &[SourceEvent],
    final_reply_event: Option<&SourceEvent>,
) -> Vec<Value> {
    ContextAssembler.assemble(structured, recent_events, final_reply_event)
}

/// 最近原文里能进模型上下文的事件条数。
pub fn recent_message_count(events: &[SourceEvent]) -> usize {
    ContextAssembler::recent_message_count(events)
}

fn summary_message(summary: &str) -> Value {
    let mut message = Map::new();
    message.insert("role".to_string(), Value::from("assistant"));
    message.insert(
        "content".to_string(),
        Value::from(format!("{COMPACT_SUMMARY_PREFIX}{summary}")),
    );
    Value::Object(message)
}

/// 返回压缩触发前最近一条非空的完整助手最终回复。
pub fn latest_final_reply_event(events: &[SourceEvent]) -> Option<&SourceEvent> {
    events.iter().rev().find(|event| {
        if event.event_type != "assistant_message" {
            return false;
        }
        let content = value_text(event.payload.get("content").unwrap_or(&Value::Null));
        !content.trim().is_empty()
    })
}

pub fn render_summary_markdown(structured: &Value) -> String {
    let mut lines = vec!["## 结构化工作摘要".to_string()];
    append_plain(&mut lines, "当前目标", structured.get("objective"));
    append_referenced(&mut lines, "关键技术概念", structured.get("key_concepts"));
    append_referenced(&mut lines, "约束", structured.get("constraints"));
    append_referenced(&mut lines, "已确认决策", structured.get("decisions"));
    append_referenced(&mut lines, "已完成与验证", structured.get("completed"));
    append_plain(&mut lines, "当前状态", structured.get("current_state"));
    append_referenced(
        &mut lines,
        "未完成事项与风险",
        structured.get("open_issues"),
    );
    append_referenced(&mut lines, "可能的下一步", structured.get("next_steps"));
    append_referenced(&mut lines, "文件、命令与产物", structured.get("artifacts"));
    append_file_items(&mut lines, "已读文件", structured.get("read_files"));
    append_file_items(&mut lines, "修改文件", structured.get("modified_files"));
    append_referenced(&mut lines, "失败尝试", structured.get("failed_attempts"));
    append_referenced(
        &mut lines,
        "问题解决过程",
        structured.get("problem_solving_process"),
    );
    append_referenced(
        &mut lines,
        "已排除方案",
        structured.get("excluded_approaches"),
    );
    append_referenced(&mut lines, "用户消息原文", structured.get("user_messages"));
    append_referenced(&mut lines, "精确证据", structured.get("exact_evidence"));
    lines.join("\n")
}

/// 把一条会话事件投影成模型侧消息；返回 `None` 表示该事件不进模型上下文。
pub fn event_to_model_message(event: &SourceEvent) -> Option<Value> {
    let payload = &event.payload;
    match event.event_type.as_str() {
        "user_message" | "assistant_message" => {
            let content = value_text(payload.get("content").unwrap_or(&Value::Null));
            if content.trim().is_empty() {
                return None;
            }
            let mut message = Map::new();
            message.insert(
                "role".to_string(),
                Value::from(if event.event_type == "user_message" {
                    "user"
                } else {
                    "assistant"
                }),
            );
            message.insert("content".to_string(), Value::from(content));
            Some(Value::Object(message))
        }
        "tool_call_requested" => {
            let tool = value_text(payload.get("tool").unwrap_or(&Value::Null))
                .trim()
                .to_string();
            if tool.is_empty() {
                return None;
            }
            let arguments = payload
                .get("arguments")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| Value::Object(Map::new()));
            let mut message = Map::new();
            message.insert("role".to_string(), Value::from("assistant"));
            message.insert(
                "content".to_string(),
                Value::from(format!(
                    "工具调用请求：{tool} 参数：{}",
                    python_dumps_sorted(&arguments)
                )),
            );
            Some(Value::Object(message))
        }
        "tool_result" | "tool_call_denied" => {
            let tool = value_text(payload.get("tool").unwrap_or(&Value::Null))
                .trim()
                .to_string();
            if tool.is_empty() {
                return None;
            }
            let content = if event.event_type == "tool_call_denied" {
                let raw = payload.get("reason").cloned().unwrap_or(Value::Null);
                let reason = if value_text(&raw).is_empty() {
                    "未批准。".to_string()
                } else {
                    value_text(&raw)
                };
                format!("工具执行结果：{tool} 失败，原因：{}", reason.trim())
            } else {
                let mut output = value_text(payload.get("model_output").unwrap_or(&Value::Null));
                if output.trim().is_empty() {
                    output = value_text(payload.get("output_preview").unwrap_or(&Value::Null));
                }
                if output.trim().is_empty() {
                    output = value_text(payload.get("output").unwrap_or(&Value::Null));
                }
                let status = if payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    "成功"
                } else {
                    "失败"
                };
                format!("工具执行结果：{tool} {status}\n{}", output.trim())
                    .trim()
                    .to_string()
            };
            let mut message = Map::new();
            message.insert("role".to_string(), Value::from("assistant"));
            message.insert("content".to_string(), Value::from(content));
            Some(Value::Object(message))
        }
        _ => None,
    }
}

fn append_file_items(lines: &mut Vec<String>, title: &str, items: Option<&Value>) {
    lines.push(format!("### {title}"));
    let values: &[Value] = items
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if values.is_empty() {
        lines.push("- 无".to_string());
        return;
    }
    for item in values {
        if !item.is_object() {
            continue;
        }
        let path = object_text(item, "path").trim().to_string();
        if path.is_empty() {
            continue;
        }
        let description = object_text(item, "description").trim().to_string();
        let ref_text = ref_text(item);
        let mut line = format!("- {path}");
        if !description.is_empty() {
            line.push_str(&format!("：{description}"));
        }
        if !ref_text.is_empty() {
            line.push_str(&format!("（来源：{ref_text}）"));
        }
        lines.push(line);
    }
}

fn append_plain(lines: &mut Vec<String>, title: &str, items: Option<&Value>) {
    lines.push(format!("### {title}"));
    let values: &[Value] = items
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if values.is_empty() {
        lines.push("- 无".to_string());
        return;
    }
    for item in values {
        lines.push(format!("- {}", value_text(item).trim()));
    }
}

fn append_referenced(lines: &mut Vec<String>, title: &str, items: Option<&Value>) {
    lines.push(format!("### {title}"));
    let values: &[Value] = items
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if values.is_empty() {
        lines.push("- 无".to_string());
        return;
    }
    for item in values {
        if !item.is_object() {
            continue;
        }
        let text = object_text(item, "text").trim().to_string();
        lines.push(format!("- {text}（来源：{}）", ref_text(item)));
    }
}

fn ref_text(item: &Value) -> String {
    item.get("source_event_ids")
        .and_then(Value::as_array)
        .map(|refs| refs.iter().map(value_text).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

fn object_text(item: &Value, key: &str) -> String {
    item.get(key).map(value_text).unwrap_or_default()
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
        other => other.to_string(),
    }
}
