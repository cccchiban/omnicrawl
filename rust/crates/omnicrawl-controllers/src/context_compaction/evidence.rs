//! `omnicrawl/agent/context_compaction/evidence.py`：按当前有效摘要引用恢复 Session 精确证据。
//!
//! 「当前有效摘要」= 事件流里最后一条 `compact_summary`；只有它授权的来源事件才允许恢复，
//! 这样恢复工具不会变成绕过压缩取回全文的后门。artifact 正文由宿主注入的读取端提供
//! （内核不碰文件系统），读取失败只降级为元数据。

use serde_json::{Map, Value};

use crate::error::AgentError;

use super::policy::{estimate_json_tokens, estimate_text_tokens, SourceEvent};

pub const RECALL_SESSION_EVIDENCE_TOOL_NAME: &str = "recall_session_evidence";
pub const DEFAULT_EVIDENCE_MAX_ITEMS: usize = 8;
pub const DEFAULT_EVIDENCE_MAX_OUTPUT_TOKENS: i64 = 4_000;

const MAX_EVENT_ID_CHARS: usize = 128;

/// 允许随 artifact 一起回给模型的元数据字段（其余字段属正文，不在这里透出）。
const ARTIFACT_METADATA_KEYS: [&str; 12] = [
    "type",
    "title",
    "size_chars",
    "output_size_chars",
    "html_size_chars",
    "sha256",
    "output_sha256",
    "html_sha256",
    "truncated",
    "artifact_truncated",
    "redacted",
    "storage",
];

/// artifact 读取端的失败分类：非 UTF-8 文本与不可读取各自对应一段诊断文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactReadFailure {
    NotText,
    Unreadable,
}

/// artifact 读取端：路径归属与越界校验由宿主负责，这里只按成败分类。
pub type ArtifactReader = dyn Fn(&str) -> Result<String, ArtifactReadFailure>;

/// 只恢复最后一个有效摘要授权的当前 Session 事件。
#[derive(Debug, Clone)]
pub struct SessionEvidenceRecallService {
    pub max_items: usize,
    pub max_output_tokens: i64,
}

impl Default for SessionEvidenceRecallService {
    fn default() -> Self {
        Self {
            max_items: DEFAULT_EVIDENCE_MAX_ITEMS,
            max_output_tokens: DEFAULT_EVIDENCE_MAX_OUTPUT_TOKENS,
        }
    }
}

impl SessionEvidenceRecallService {
    pub fn new(max_items: usize, max_output_tokens: i64) -> Result<Self, AgentError> {
        if max_items == 0 {
            return Err(AgentError::new("max_items 必须是正整数。"));
        }
        if max_output_tokens <= 0 {
            return Err(AgentError::new("max_output_tokens 必须是正整数。"));
        }
        Ok(Self {
            max_items,
            max_output_tokens,
        })
    }

    pub fn recall(
        &self,
        events: &[SourceEvent],
        event_ids: &Value,
        artifact_reader: &ArtifactReader,
    ) -> Value {
        let (requested, diagnostics, request_truncated) = self.normalize_event_ids(event_ids);
        let latest_summary = events
            .iter()
            .rev()
            .find(|event| event.event_type == "compact_summary");
        let authorized_ids = match latest_summary {
            Some(summary) => summary_source_event_ids(&summary.payload),
            None => Vec::new(),
        };
        let item_budget = std::cmp::max(
            64,
            python_floor_div(
                self.max_output_tokens - 600 - (requested.len() as i64) * 80,
                std::cmp::max(1, requested.len() as i64),
            ),
        );

        let mut items: Vec<Value> = Vec::new();
        let mut content_truncated = false;
        for event_id in &requested {
            if latest_summary.is_none() {
                items.push(diagnostic_item(
                    event_id,
                    "no_active_summary",
                    "当前 Session 没有可用于证据恢复的有效摘要。",
                ));
                continue;
            }
            if !authorized_ids.contains(event_id) {
                items.push(diagnostic_item(
                    event_id,
                    "unauthorized",
                    "事件未被当前有效摘要引用。",
                ));
                continue;
            }
            let found = events.iter().find(|event| &event.event_id == event_id);
            let Some(event) = found else {
                items.push(diagnostic_item(
                    event_id,
                    "missing",
                    "当前有效事件流中找不到该摘要来源事件。",
                ));
                continue;
            };
            let item = self.event_item(event, item_budget, artifact_reader);
            content_truncated = content_truncated || item["content_truncated"] == Value::Bool(true);
            if let Some(artifacts) = item["artifacts"].as_array() {
                content_truncated = content_truncated
                    || artifacts.iter().any(|artifact| {
                        artifact.get("content_truncated") == Some(&Value::Bool(true))
                    });
            }
            items.push(item);
        }

        let mut budget = Map::new();
        budget.insert("max_items".to_string(), Value::from(self.max_items as i64));
        budget.insert(
            "max_output_tokens".to_string(),
            Value::from(self.max_output_tokens),
        );
        let mut result = Map::new();
        result.insert("schema_version".to_string(), Value::from(1));
        result.insert(
            "ok".to_string(),
            Value::from(
                items
                    .iter()
                    .any(|item| item.get("status") == Some(&Value::from("ok"))),
            ),
        );
        result.insert(
            "summary_event_id".to_string(),
            match latest_summary {
                Some(summary) => Value::from(summary.event_id.clone()),
                None => Value::Null,
            },
        );
        result.insert(
            "requested_count".to_string(),
            Value::from(requested.len() as i64),
        );
        result.insert("items".to_string(), Value::Array(items));
        result.insert("diagnostics".to_string(), Value::Array(diagnostics));
        result.insert(
            "truncated".to_string(),
            Value::from(request_truncated || content_truncated),
        );
        result.insert("budget".to_string(), Value::Object(budget));
        result.insert("estimated_tokens".to_string(), Value::from(0));

        let mut result = Value::Object(result);
        self.fit_result_budget(&mut result);
        let estimated = estimate_json_tokens(&result);
        set_estimated_tokens(&mut result, estimated);
        // estimated_tokens 自身会使序列化长度变化几个字符，再做一次最终守卫。
        if estimated > self.max_output_tokens {
            self.fit_result_budget(&mut result);
            let estimated = estimate_json_tokens(&result);
            set_estimated_tokens(&mut result, estimated);
        }
        result
    }

    fn normalize_event_ids(&self, event_ids: &Value) -> (Vec<String>, Vec<Value>, bool) {
        let mut diagnostics: Vec<Value> = Vec::new();
        let Some(raw_ids) = event_ids.as_array() else {
            diagnostics.push(diagnostic(
                "invalid_event_ids",
                "event_ids 必须是字符串数组。",
                None,
                None,
            ));
            return (Vec::new(), diagnostics, false);
        };

        let mut normalized: Vec<String> = Vec::new();
        for (index, raw_event_id) in raw_ids.iter().enumerate() {
            let raw = raw_event_id.as_str().filter(|text| !text.trim().is_empty());
            let Some(raw) = raw else {
                diagnostics.push(diagnostic(
                    "invalid_event_id",
                    "事件 ID 必须是非空字符串。",
                    Some(index),
                    None,
                ));
                continue;
            };
            let event_id = raw.trim();
            if event_id.chars().count() > MAX_EVENT_ID_CHARS {
                diagnostics.push(diagnostic(
                    "invalid_event_id",
                    &format!("事件 ID 长度不能超过 {MAX_EVENT_ID_CHARS}。"),
                    Some(index),
                    None,
                ));
                continue;
            }
            if normalized.iter().any(|item| item == event_id) {
                continue;
            }
            normalized.push(event_id.to_string());
        }

        let truncated = normalized.len() > self.max_items;
        if truncated {
            let omitted = normalized.len() - self.max_items;
            diagnostics.push(diagnostic(
                "item_limit_exceeded",
                &format!("单次最多恢复 {} 个事件，其余引用未处理。", self.max_items),
                None,
                Some(omitted),
            ));
            normalized.truncate(self.max_items);
        }
        (normalized, diagnostics, truncated)
    }

    fn event_item(
        &self,
        event: &SourceEvent,
        item_budget: i64,
        artifact_reader: &ArtifactReader,
    ) -> Value {
        let references = artifact_references(&event.payload);
        let event_content_budget = if references.is_empty() {
            item_budget
        } else {
            std::cmp::max(48, python_floor_div(item_budget, 3))
        };
        let serialized_payload = crate::json::python_dumps_compact_sorted(&event.payload);
        let (content, content_was_truncated) =
            truncate_to_token_budget(&serialized_payload, event_content_budget);

        let mut artifacts: Vec<Value> = Vec::new();
        let artifact_budget = std::cmp::max(
            32,
            python_floor_div(
                item_budget - estimate_text_tokens(&content) - 40,
                std::cmp::max(1, references.len() as i64),
            ),
        );
        for (path, metadata) in references {
            let mut artifact = Map::new();
            artifact.insert("path".to_string(), Value::from(path.clone()));
            artifact.insert("metadata".to_string(), metadata);
            match artifact_reader(&path) {
                Err(ArtifactReadFailure::NotText) => {
                    artifact.insert("status".to_string(), Value::from("metadata_only"));
                    artifact.insert(
                        "diagnostic".to_string(),
                        diagnostic(
                            "artifact_not_text",
                            "artifact 不是 UTF-8 文本，仅返回元数据。",
                            None,
                            None,
                        ),
                    );
                }
                Err(ArtifactReadFailure::Unreadable) => {
                    artifact.insert("status".to_string(), Value::from("metadata_only"));
                    artifact.insert(
                        "diagnostic".to_string(),
                        diagnostic(
                            "artifact_unreadable",
                            "artifact 不存在、越界或不可读取，仅返回元数据。",
                            None,
                            None,
                        ),
                    );
                }
                Ok(text) => {
                    let (artifact_content, artifact_truncated) =
                        truncate_to_token_budget(&text, artifact_budget);
                    artifact.insert("status".to_string(), Value::from("ok"));
                    artifact.insert("content".to_string(), Value::from(artifact_content));
                    artifact.insert(
                        "content_truncated".to_string(),
                        Value::from(artifact_truncated),
                    );
                }
            }
            artifacts.push(Value::Object(artifact));
        }

        let mut item = Map::new();
        item.insert("event_id".to_string(), Value::from(event.event_id.clone()));
        item.insert(
            "event_type".to_string(),
            Value::from(event.event_type.clone()),
        );
        item.insert("status".to_string(), Value::from("ok"));
        item.insert("content".to_string(), Value::from(content));
        item.insert(
            "content_truncated".to_string(),
            Value::from(content_was_truncated),
        );
        item.insert("artifacts".to_string(), Value::Array(artifacts));
        Value::Object(item)
    }

    /// 最终按实际 JSON Token 估算收紧内容，避免元数据挤破总预算。
    fn fit_result_budget(&self, result: &mut Value) {
        for _attempt in 0..32 {
            if estimate_json_tokens(result) <= self.max_output_tokens {
                return;
            }
            let Some(items) = result.get("items").and_then(Value::as_array) else {
                return;
            };
            let mut best: Option<ContentTarget> = None;
            for (item_index, item) in items.iter().enumerate() {
                if let Some(text) = item.get("content").and_then(Value::as_str) {
                    let length = text.chars().count();
                    if best.as_ref().is_none_or(|current| length > current.length) {
                        best = Some(ContentTarget {
                            item_index,
                            artifact_index: None,
                            length,
                        });
                    }
                }
                if let Some(artifacts) = item.get("artifacts").and_then(Value::as_array) {
                    for (artifact_index, artifact) in artifacts.iter().enumerate() {
                        if let Some(text) = artifact.get("content").and_then(Value::as_str) {
                            let length = text.chars().count();
                            if best.as_ref().is_none_or(|current| length > current.length) {
                                best = Some(ContentTarget {
                                    item_index,
                                    artifact_index: Some(artifact_index),
                                    length,
                                });
                            }
                        }
                    }
                }
            }
            let Some(target) = best else {
                return;
            };
            if target.length <= 32 {
                return;
            }
            let keep = std::cmp::max(16, python_floor_div((target.length as i64) * 3, 4)) as usize;
            let Some(slot) = content_slot(result, &target) else {
                return;
            };
            let current = slot
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            slot["content"] = Value::from(shorten_text(&current, keep));
            slot["content_truncated"] = Value::from(true);
            result["truncated"] = Value::from(true);
        }
    }
}

/// 待收紧的正文位置：条目正文，或某个条目下的 artifact 正文。
struct ContentTarget {
    item_index: usize,
    artifact_index: Option<usize>,
    length: usize,
}

fn content_slot<'a>(result: &'a mut Value, target: &ContentTarget) -> Option<&'a mut Value> {
    let item = result
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .and_then(|items| items.get_mut(target.item_index))?;
    match target.artifact_index {
        None => Some(item),
        Some(index) => item
            .get_mut("artifacts")
            .and_then(Value::as_array_mut)
            .and_then(|artifacts| artifacts.get_mut(index)),
    }
}

fn set_estimated_tokens(result: &mut Value, estimated: i64) {
    if let Some(map) = result.as_object_mut() {
        map.insert("estimated_tokens".to_string(), Value::from(estimated));
    }
}

/// Python 的 `//`：向负无穷取整（Rust 的 `/` 向零取整）。
fn python_floor_div(left: i64, right: i64) -> i64 {
    if right == 0 {
        return 0;
    }
    let quotient = left / right;
    if (left % right != 0) && ((left < 0) != (right < 0)) {
        quotient - 1
    } else {
        quotient
    }
}

fn diagnostic(code: &str, message: &str, index: Option<usize>, omitted: Option<usize>) -> Value {
    let mut map = Map::new();
    map.insert("code".to_string(), Value::from(code));
    if let Some(index) = index {
        map.insert("index".to_string(), Value::from(index as i64));
    }
    map.insert("message".to_string(), Value::from(message));
    if let Some(omitted) = omitted {
        map.insert("omitted_count".to_string(), Value::from(omitted as i64));
    }
    Value::Object(map)
}

fn diagnostic_item(event_id: &str, status: &str, message: &str) -> Value {
    let mut map = Map::new();
    map.insert("event_id".to_string(), Value::from(event_id));
    map.insert("status".to_string(), Value::from(status));
    map.insert(
        "diagnostic".to_string(),
        diagnostic(status, message, None, None),
    );
    Value::Object(map)
}

/// 最后一个有效摘要授权的事件 ID：覆盖清单 + 结构化摘要里逐条引用的来源。
fn summary_source_event_ids(payload: &Value) -> Vec<String> {
    let mut source_ids: Vec<String> = Vec::new();
    let mut push = |source_ids: &mut Vec<String>, candidate: &str| {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() && !source_ids.iter().any(|item| item == trimmed) {
            source_ids.push(trimmed.to_string());
        }
    };

    fn visit(
        value: &Value,
        source_ids: &mut Vec<String>,
        push: &mut dyn FnMut(&mut Vec<String>, &str),
    ) {
        match value {
            Value::Object(map) => {
                if let Some(refs) = map.get("source_event_ids").and_then(Value::as_array) {
                    for event_id in refs {
                        if let Some(event_id) = event_id.as_str() {
                            push(source_ids, event_id);
                        }
                    }
                }
                for nested in map.values() {
                    visit(nested, source_ids, push);
                }
            }
            Value::Array(items) => {
                for nested in items {
                    visit(nested, source_ids, push);
                }
            }
            _ => {}
        }
    }

    if let Some(covered) = payload.get("covered_event_ids").and_then(Value::as_array) {
        for event_id in covered {
            if let Some(event_id) = event_id.as_str() {
                push(&mut source_ids, event_id);
            }
        }
    }
    if let Some(structured) = payload.get("structured") {
        if structured.is_object() {
            visit(structured, &mut source_ids, &mut push);
        }
    }
    source_ids
}

/// payload 里出现的 artifact 引用：路径去重，元数据只留可透出的标量字段。
fn artifact_references(payload: &Value) -> Vec<(String, Value)> {
    let mut references: Vec<(String, Value)> = Vec::new();

    fn visit(value: &Value, references: &mut Vec<(String, Value)>) {
        match value {
            Value::Object(map) => {
                if let Some(raw_path) = map.get("artifact_path").and_then(Value::as_str) {
                    let path = raw_path.trim();
                    if !path.is_empty() && !references.iter().any(|(item, _)| item == path) {
                        let mut metadata = Map::new();
                        for (key, nested) in map {
                            let scalar = matches!(
                                nested,
                                Value::String(_) | Value::Number(_) | Value::Bool(_)
                            );
                            if scalar && ARTIFACT_METADATA_KEYS.contains(&key.as_str()) {
                                metadata.insert(key.clone(), nested.clone());
                            }
                        }
                        references.push((path.to_string(), Value::Object(metadata)));
                    }
                }
                for nested in map.values() {
                    visit(nested, references);
                }
            }
            Value::Array(items) => {
                for nested in items {
                    visit(nested, references);
                }
            }
            _ => {}
        }
    }

    visit(payload, &mut references);
    references
}

const TRUNCATED_SUFFIX: &str = "\n... 证据已截断，可按更少的事件 ID 分批恢复。";

/// 按 Token 估算截断文本：二分出能放下的最长前缀，再挂截断说明。
fn truncate_to_token_budget(text: &str, max_tokens: i64) -> (String, bool) {
    if estimate_text_tokens(text) <= max_tokens {
        return (text.to_string(), false);
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut low, mut high) = (0usize, chars.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate: String = chars[..middle].iter().collect::<String>() + TRUNCATED_SUFFIX;
        if estimate_text_tokens(&candidate) <= max_tokens {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    (
        chars[..low].iter().collect::<String>() + TRUNCATED_SUFFIX,
        true,
    )
}

fn shorten_text(text: &str, keep: usize) -> String {
    let head: String = text.chars().take(keep).collect();
    format!("{head}\n... 证据已按总预算截断。")
}
