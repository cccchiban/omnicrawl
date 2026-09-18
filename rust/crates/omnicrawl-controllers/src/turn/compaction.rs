//! `omnicrawl/agent/controllers/turn/compaction.py` 的判定面：压缩通知文案、压缩记忆写请求、
//! 压缩后自动记忆召回与归档选取。
//!
//! 会话读写、模型调用、插件钩子与历史重建留在宿主；这里只产出「该写什么、该召回什么、
//! 该归档哪些事件」的可核验结果。

use serde_json::{Map, Value};

use omnicrawl_session::{MemorySearchResult, MemoryWriteRequest};

use crate::shared::{python_round_to_int, python_split_lines, python_str, python_truthy};

/// 压缩记忆的来源标记与两个固定落点目录。
pub const COMPACTION_MEMORY_SOURCE_EVENT: &str = "context_compaction";
pub const PROJECT_CONTEXT_DIRECTORY: &str = "project-context/general";
pub const TASK_HISTORY_DIRECTORY: &str = "task-history/general";

/// 自动召回：查询文本上限、单次检索条数与注入文本上限。
pub const RECALL_QUERY_CHARS: usize = 200;
pub const RECALL_MAX_RESULTS: usize = 3;
pub const RECALL_TEXT_LIMIT: usize = 1_200;

/// 生成「---已压缩 xxk~xxk ---」的分隔提示文本；数据缺失时返回 None。
///
/// 文本由 TUI 以灰色单独成行渲染，作为「上下文已被摘要替换」的可见边界。
pub fn format_compaction_notice(
    before_tokens: Option<i64>,
    after_tokens: Option<i64>,
) -> Option<String> {
    let before = before_tokens.unwrap_or(0);
    let after = after_tokens.unwrap_or(0);
    if before <= 0 || after <= 0 {
        return None;
    }
    let before_k = std::cmp::max(1, python_round_to_int(before as f64 / 1000.0));
    let after_k = std::cmp::max(1, python_round_to_int(after as f64 / 1000.0));
    Some(format!("---已压缩 {before_k}k~{after_k}k ---"))
}

/// 压缩结果要写入的会话级记忆：项目上下文与任务状态各一条，没有内容的条目不产生。
pub fn compaction_memory_requests(compact_payload: &Value) -> Vec<MemoryWriteRequest> {
    let project_sections: Vec<(&'static str, Option<Value>)>;
    let task_sections: Vec<(&'static str, Option<Value>)>;
    match compact_payload.get("structured") {
        Some(Value::Object(structured)) => {
            project_sections = vec![
                ("项目目标", structured.get("objective").cloned()),
                ("项目约束", structured.get("constraints").cloned()),
                ("关键技术概念", structured.get("key_concepts").cloned()),
                ("关键决策", structured.get("decisions").cloned()),
                ("当前状态", structured.get("current_state").cloned()),
                ("文件与产物", structured.get("artifacts").cloned()),
                ("已读文件", structured.get("read_files").cloned()),
                ("修改文件", structured.get("modified_files").cloned()),
            ];
            task_sections = vec![
                ("完成状态", structured.get("completed").cloned()),
                ("失败尝试", structured.get("failed_attempts").cloned()),
                (
                    "问题解决过程",
                    structured.get("problem_solving_process").cloned(),
                ),
                ("已排除方案", structured.get("excluded_approaches").cloned()),
                ("后续事项", structured.get("open_issues").cloned()),
                ("可能的下一步", structured.get("next_steps").cloned()),
                ("用户消息原文", structured.get("user_messages").cloned()),
            ];
        }
        _ => {
            let summary = python_str(compact_payload.get("content").unwrap_or(&Value::Null));
            let summary = summary.trim().to_string();
            let mut project_lines: Vec<String> = Vec::new();
            let mut task_lines: Vec<String> = Vec::new();
            for line in python_split_lines(&summary) {
                let stripped = line.trim();
                let project_prefixes = ["- 既有摘要：", "- 原始目标：", "- 已压缩的用户后续要求："];
                let task_prefixes = ["- 已完成/已回复要点：", "- 压缩前状态：", "- 下一步："];
                if project_prefixes
                    .iter()
                    .any(|prefix| stripped.starts_with(prefix))
                {
                    project_lines.push(trimmed_after_bullet(stripped));
                } else if task_prefixes
                    .iter()
                    .any(|prefix| stripped.starts_with(prefix))
                {
                    task_lines.push(trimmed_after_bullet(stripped));
                }
            }
            project_sections = vec![(
                "项目目标与用户要求",
                Some(Value::Array(
                    project_lines.into_iter().map(Value::from).collect(),
                )),
            )];
            task_sections = vec![(
                "完成状态与后续事项",
                Some(Value::Array(
                    task_lines.into_iter().map(Value::from).collect(),
                )),
            )];
        }
    }

    let project_content = render_memory("压缩会话中的项目上下文", &project_sections);
    let task_content = render_memory("压缩会话中的任务状态", &task_sections);
    let mut requests: Vec<MemoryWriteRequest> = Vec::new();
    if !project_content.is_empty() {
        requests.push(MemoryWriteRequest {
            content: project_content,
            related_directories: vec![
                PROJECT_CONTEXT_DIRECTORY.to_string(),
                TASK_HISTORY_DIRECTORY.to_string(),
            ],
            storage_directory: Some(PROJECT_CONTEXT_DIRECTORY.to_string()),
            source_event: Some(COMPACTION_MEMORY_SOURCE_EVENT.to_string()),
        });
    }
    if !task_content.is_empty() {
        requests.push(MemoryWriteRequest {
            content: task_content,
            related_directories: vec![
                TASK_HISTORY_DIRECTORY.to_string(),
                PROJECT_CONTEXT_DIRECTORY.to_string(),
            ],
            storage_directory: Some(TASK_HISTORY_DIRECTORY.to_string()),
            source_event: Some(COMPACTION_MEMORY_SOURCE_EVENT.to_string()),
        });
    }
    requests
}

/// 压缩后的自动召回查询：摘要目标 + 当前状态，空格连接后截到 200 字符。
pub fn compaction_recall_query(compact_payload: &Value) -> Option<String> {
    let structured = compact_payload.get("structured")?;
    if !structured.is_object() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for field in ["objective", "current_state"] {
        let Some(Value::Array(items)) = structured.get(field) else {
            continue;
        };
        for item in items {
            let text = python_str(item).trim().to_string();
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(take_chars(&parts.join(" "), RECALL_QUERY_CHARS))
}

/// 召回事件的命中清单：只留 ID、落点目录与 200 字摘要。
pub fn compaction_recall_hits(results: &[MemorySearchResult]) -> Vec<Value> {
    results
        .iter()
        .map(|result| {
            let mut hit = Map::new();
            hit.insert("id".to_string(), Value::from(result.id.clone()));
            hit.insert(
                "storage_directory".to_string(),
                Value::from(result.storage_directory.clone()),
            );
            hit.insert(
                "summary".to_string(),
                Value::from(take_chars(&result.summary, RECALL_QUERY_CHARS)),
            );
            Value::Object(hit)
        })
        .collect()
}

/// 召回事件的载荷：查询文本 + 命中清单。
pub fn compaction_recall_event_payload(query: &str, hits: &[Value]) -> Value {
    let mut payload = Map::new();
    payload.insert("query".to_string(), Value::from(query));
    payload.insert("hits".to_string(), Value::Array(hits.to_vec()));
    Value::Object(payload)
}

/// 注入父模型上下文的召回文本；超过 1200 字符时截断并加省略标记。
pub fn compaction_recall_text(results: &[MemorySearchResult]) -> String {
    let mut lines: Vec<String> = vec!["记忆检索（压缩后自动补强）：".to_string()];
    for (index, result) in results.iter().enumerate() {
        let summary = result.summary.trim();
        if !summary.is_empty() {
            lines.push(format!("{}. {summary}", index + 1));
        }
    }
    let text = lines.join("\n").trim().to_string();
    if text.chars().count() > RECALL_TEXT_LIMIT {
        return format!("{}\n...", take_chars(&text, RECALL_TEXT_LIMIT));
    }
    text
}

/// 本次要归档的被压缩事件 ID：未启用归档或没有事件时为空。
pub fn archive_compacted_event_ids(archive_enabled: bool, compact_payload: &Value) -> Vec<String> {
    if !archive_enabled {
        return Vec::new();
    }
    let Some(ids) = compact_payload
        .get("compacted_event_ids")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut wanted: Vec<String> = Vec::new();
    for id in ids {
        if let Some(id) = id.as_str() {
            if !wanted.iter().any(|item| item == id) {
                wanted.push(id.to_string());
            }
        }
    }
    wanted
}

fn trimmed_after_bullet(line: &str) -> String {
    line.strip_prefix("- ").unwrap_or(line).trim().to_string()
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn render_memory(title: &str, sections: &[(&str, Option<Value>)]) -> String {
    let mut lines: Vec<String> = vec![format!("## {title}")];
    for (heading, raw_items) in sections {
        let Some(items) = raw_items.as_ref().and_then(Value::as_array) else {
            continue;
        };
        let mut rendered: Vec<String> = Vec::new();
        for item in items {
            let text = match item {
                Value::Object(map) => {
                    let null = Value::Null;
                    let picked = match map.get("path") {
                        Some(value) if python_truthy(value) => value,
                        _ => match map.get("text") {
                            Some(value) if python_truthy(value) => value,
                            _ => &null,
                        },
                    };
                    python_str(picked).trim().to_string()
                }
                other => {
                    if python_truthy(other) {
                        python_str(other).trim().to_string()
                    } else {
                        String::new()
                    }
                }
            };
            if !text.is_empty() {
                rendered.push(text);
            }
        }
        if rendered.is_empty() {
            continue;
        }
        lines.push(format!("### {heading}"));
        lines.extend(rendered.into_iter().map(|item| format!("- {item}")));
    }
    if lines.len() > 1 {
        lines.join("\n")
    } else {
        String::new()
    }
}
