//! `omnicrawl/agent/context_compaction/summary.py`：低成本模型结构化摘要调用、分块和 JSON 响应解析。
//!
//! 模型调用经 [`SummaryModelCaller`] 注入：本模块只负责提示词组装、事件索引分块、解析重试与
//! 用量累计。真实请求（复用主请求前缀、`tool_choice=none`）由宿主持有，内核侧实现见
//! `omnicrawl-compaction` 的 `SummaryModelAdapter`。

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::error::AgentError;
use crate::json::python_dumps_compact;
use crate::shared::python_split_lines;

use super::policy::{estimate_json_tokens, CompactionBatch, SourceEvent, TokenUsageSample};

/// 摘要请求与主请求共享同一份工具声明，只有 tool_choice 不同：工具块在 prompt 里排在
/// 前缀最前面，少带工具会让第一次摘要请求无法命中主请求已建立的前缀缓存。
pub const SUMMARY_TOOL_CHOICE: &str = "none";

/// 索引块的硬上限；运行态再按窗口余额放大（见 `SummaryModelAdapter::index_chunk_budget_tokens`）。
pub const MAX_INDEX_CHUNK_TOKENS: i64 = 128_000;

/// 摘要响应的输出预留：窗口余额要扣掉它才留给输入。
pub const SUMMARY_OUTPUT_RESERVE_TOKENS: i64 = 16_000;

/// 单块索引的默认预算：窗口余额不足时的下限。
pub const DEFAULT_MAX_INPUT_TOKENS: i64 = 64_000;

/// 事件索引每条预览的字符数：正文留在复用的原文前缀里。
const INDEX_PREVIEW_CHARS: usize = 200;

pub const SUMMARY_PROMPT_FILE: &str = "summary_prompt.md";

/// 摘要模型调用或结构化响应解析失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryGenerationError {
    message: String,
}

impl SummaryGenerationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SummaryGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SummaryGenerationError {}

impl From<SummaryGenerationError> for AgentError {
    fn from(value: SummaryGenerationError) -> Self {
        AgentError::new(value.message)
    }
}

/// 模型调用适配器返回的最小供应商无关结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SummaryModelResponse {
    pub content: String,
    pub usage: TokenUsageSample,
    pub profile: String,
    pub provider: String,
    pub tool_calls: i64,
}

impl SummaryModelResponse {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }
}

/// 摘要模型调用端口：接受逐字提示词，返回文本、用量与画像。
pub trait SummaryModelCall {
    fn call(&self, messages: &[Value]) -> Result<SummaryModelResponse, SummaryGenerationError>;
}

impl<F> SummaryModelCall for F
where
    F: Fn(&[Value]) -> Result<SummaryModelResponse, SummaryGenerationError>,
{
    fn call(&self, messages: &[Value]) -> Result<SummaryModelResponse, SummaryGenerationError> {
        self(messages)
    }
}

/// 已解析但尚未通过来源校验的模型结构化结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSummaryResult {
    pub structured: Value,
    pub usage: TokenUsageSample,
    pub profile: String,
    pub provider: String,
    pub attempts: i64,
}

/// 事件引用表：送给模型的索引里只出现 `E1`、`E2` 这类短引用，真事件 ID 只留在代码里。
///
/// 让模型逐字复制 24 位不透明十六进制 ID 是不可靠的（实测会整段编造出「像 ID 的串」，
/// 而校验只要发现一处不存在就让整份摘要作废）；短引用既短又易抄，展开成真 ID 后校验口径
/// 与「模型直接写 ID」完全相同：真 ID 是否属于本会话仍由 validator 判定。
#[derive(Debug, Default, Clone)]
struct EventRefs {
    /// 真事件 ID → 短引用。
    references: HashMap<String, String>,
    /// 短引用 → 真事件 ID。
    ids: HashMap<String, String>,
}

impl EventRefs {
    /// 按批次事件顺序编号（`E1`、`E2`…），跳过空 ID 与重复项。
    fn for_batch(batch: &CompactionBatch) -> Self {
        let mut refs = Self::default();
        for event in &batch.events {
            refs.register(&event.event_id);
        }
        refs
    }

    /// 登记一个事件 ID 并返回它的短引用；空白 ID 不参与（与 `covered_event_ids` 同样跳过）。
    fn register(&mut self, event_id: &str) -> Option<String> {
        if event_id.trim().is_empty() {
            return None;
        }
        if let Some(existing) = self.references.get(event_id) {
            return Some(existing.clone());
        }
        let reference = format!("E{}", self.references.len() + 1);
        self.references
            .insert(event_id.to_string(), reference.clone());
        self.ids.insert(reference.clone(), event_id.to_string());
        Some(reference)
    }

    fn reference_of(&self, event_id: &str) -> Option<&str> {
        self.references.get(event_id).map(String::as_str)
    }
}

/// 上次摘要里的真 ID 也要先登记：这些事件通常已不在本次批次里，照原样发过去等于
/// 又把一堆 24 位 ID 摆在模型面前让它抄。
fn register_previous_refs(references: &mut EventRefs, previous: &Value) {
    let mut collected: Vec<String> = Vec::new();
    collect_source_event_ids(previous, &mut collected);
    for event_id in collected {
        references.register(&event_id);
    }
}

/// 收集摘要结构里所有 `source_event_ids` 的取值（按出现顺序，含嵌套字段）。
fn collect_source_event_ids(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(entries) => {
            for (key, item) in entries.iter() {
                if key == "source_event_ids" {
                    if let Value::Array(values) = item {
                        for entry in values.iter() {
                            if let Some(text) = entry.as_str() {
                                out.push(text.to_string());
                            }
                        }
                    }
                }
                collect_source_event_ids(item, out);
            }
        }
        Value::Array(items) => {
            for item in items.iter() {
                collect_source_event_ids(item, out);
            }
        }
        _ => {}
    }
}

/// 深度遍历摘要结构，把 `source_event_ids` 的取值按 `mapping` 改写：未命中的保持原样，
/// 于是未知取值依旧会被 validator 判为「引用了不存在的事件」，不会静默放过。
fn map_source_event_ids(value: &mut Value, mapping: &HashMap<String, String>) {
    match value {
        Value::Object(entries) => {
            for (key, item) in entries.iter_mut() {
                if key == "source_event_ids" {
                    if let Value::Array(values) = item {
                        for entry in values.iter_mut() {
                            let mapped = entry
                                .as_str()
                                .and_then(|text| mapping.get(text))
                                .map(|text| Value::from(text.as_str()));
                            if let Some(mapped) = mapped {
                                *entry = mapped;
                            }
                        }
                    }
                }
                map_source_event_ids(item, mapping);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                map_source_event_ids(item, mapping);
            }
        }
        _ => {}
    }
}

/// 只负责生成结构化摘要；来源与事实校验由 validator 执行。
pub struct ModelSummaryCompactor {
    call_model: Box<dyn SummaryModelCall>,
    summary_prompt: String,
    max_input_tokens: i64,
    /// 窗口余额预算：返回 `None` 表示不可用，此时退回默认预算。
    budget_provider: Option<Box<dyn Fn() -> Option<i64>>>,
}

impl ModelSummaryCompactor {
    pub fn new(
        call_model: Box<dyn SummaryModelCall>,
        summary_prompt: impl Into<String>,
        max_input_tokens: i64,
    ) -> Result<Self, AgentError> {
        if max_input_tokens <= 0 {
            return Err(AgentError::new("max_input_tokens 必须是正整数。"));
        }
        Ok(Self {
            call_model,
            summary_prompt: summary_prompt.into().trim().to_string(),
            max_input_tokens,
            budget_provider: None,
        })
    }

    pub fn with_budget_provider(mut self, provider: Box<dyn Fn() -> Option<i64>>) -> Self {
        self.budget_provider = Some(provider);
        self
    }

    /// 单块索引预算：默认值与窗口余额取大者，并受硬上限约束。
    fn chunk_budget(&self) -> i64 {
        let mut budget = self.max_input_tokens;
        if let Some(provider) = &self.budget_provider {
            if let Some(extra) = provider() {
                budget = std::cmp::max(budget, std::cmp::max(0, extra));
            }
        }
        std::cmp::min(budget, MAX_INDEX_CHUNK_TOKENS)
    }

    pub fn compact(
        &self,
        batch: &CompactionBatch,
        target_summary_tokens: i64,
        validation_feedback: &[String],
    ) -> Result<ModelSummaryResult, SummaryGenerationError> {
        let previous = previous_structured(batch.previous_summary.as_ref());
        // 引用表在分块之前建好：分块只是把索引切开，引用编号对整批事件稳定，
        // 合并步骤里模型看到的也只是这些短引用。
        let mut references = EventRefs::for_batch(batch);
        let previous = previous.map(|value| {
            register_previous_refs(&mut references, &value);
            let mut value = value;
            map_source_event_ids(&mut value, &references.references);
            value
        });
        let chunks = chunk_events(&batch.events, self.chunk_budget());
        if chunks.is_empty() {
            return Err(SummaryGenerationError::new("没有可供模型摘要的事件。"));
        }

        let mut aggregate_usage = TokenUsageSample::default();
        let mut attempts = 0i64;
        let mut partials: Vec<Value> = Vec::new();
        let mut profile = String::new();
        let mut provider = String::new();
        let chunk_count = chunks.len() as i64;
        for (index, chunk) in chunks.iter().enumerate() {
            let chunk_index = index as i64 + 1;
            let result = self.request_structured(&StructuredRequest {
                previous_summary: if chunk_index == 1 {
                    previous.as_ref()
                } else {
                    None
                },
                source_events: chunk,
                partial_summaries: Vec::new(),
                target_summary_tokens,
                validation_feedback,
                operation: if chunk_count > 1 {
                    "extract_chunk"
                } else {
                    "merge_summary"
                },
                chunk_index,
                chunk_count,
                references: &references,
            })?;
            aggregate_usage = aggregate_usage.add(
                result.usage.input_tokens,
                result.usage.output_tokens,
                result.usage.cached_input_tokens,
            );
            attempts += result.attempts;
            if !result.profile.is_empty() {
                profile = result.profile.clone();
            }
            if !result.provider.is_empty() {
                provider = result.provider.clone();
            }
            partials.push(result.structured);
        }

        let (mut structured, profile, provider, attempts) = if partials.len() == 1 {
            (partials.remove(0), profile, provider, attempts)
        } else {
            let merged = self.request_structured(&StructuredRequest {
                previous_summary: previous.as_ref(),
                source_events: &[],
                partial_summaries: partials,
                target_summary_tokens,
                validation_feedback,
                operation: "merge_chunks",
                chunk_index: 1,
                chunk_count: 1,
                references: &references,
            })?;
            aggregate_usage = aggregate_usage.add(
                merged.usage.input_tokens,
                merged.usage.output_tokens,
                merged.usage.cached_input_tokens,
            );
            (
                merged.structured,
                if merged.profile.is_empty() {
                    profile
                } else {
                    merged.profile
                },
                if merged.provider.is_empty() {
                    provider
                } else {
                    merged.provider
                },
                attempts + merged.attempts,
            )
        };
        // 短引用在这里还原成真事件 ID：后续的校验、投影与落盘拿到的都是真 ID，
        // 与早先「模型直接写 ID」的输入形状完全一致。
        map_source_event_ids(&mut structured, &references.ids);
        Ok(ModelSummaryResult {
            structured,
            usage: aggregate_usage,
            profile,
            provider,
            attempts,
        })
    }

    fn request_structured(
        &self,
        request: &StructuredRequest<'_>,
    ) -> Result<ModelSummaryResult, SummaryGenerationError> {
        let mut base = Map::new();
        base.insert("operation".to_string(), Value::from(request.operation));
        // 0 表示无摘要预算上限：向模型传 null + budget_limited=false，
        // 由 summary_prompt.md 规则 9 引导完整性优先。
        base.insert(
            "target_summary_tokens".to_string(),
            if request.target_summary_tokens > 0 {
                Value::from(request.target_summary_tokens)
            } else {
                Value::Null
            },
        );
        base.insert(
            "budget_limited".to_string(),
            Value::from(request.target_summary_tokens > 0),
        );
        base.insert("chunk_index".to_string(), Value::from(request.chunk_index));
        base.insert("chunk_count".to_string(), Value::from(request.chunk_count));
        base.insert(
            "previous_summary".to_string(),
            request.previous_summary.cloned().unwrap_or(Value::Null),
        );
        base.insert(
            "events_index".to_string(),
            Value::Array(
                request
                    .source_events
                    .iter()
                    .filter_map(|event| {
                        request
                            .references
                            .reference_of(&event.event_id)
                            .map(|reference| {
                                event.to_ref_index_dict(reference, INDEX_PREVIEW_CHARS)
                            })
                    })
                    .collect(),
            ),
        );
        base.insert(
            "partial_summaries".to_string(),
            Value::Array(request.partial_summaries.clone()),
        );
        base.insert(
            "validation_feedback".to_string(),
            Value::Array(
                request
                    .validation_feedback
                    .iter()
                    .map(|item| Value::from(item.clone()))
                    .collect(),
            ),
        );

        let mut usage = TokenUsageSample::default();
        let mut latest_profile = String::new();
        let mut latest_provider = String::new();
        let mut parse_error = String::new();
        for attempt in 1..=2i64 {
            let mut payload = base.clone();
            if !parse_error.is_empty() {
                payload.insert(
                    "response_error".to_string(),
                    Value::from(parse_error.clone()),
                );
                payload.insert(
                    "instruction".to_string(),
                    Value::from("上次响应不是合法 JSON 对象，请严格按 Schema 重试。"),
                );
            }
            let prompt = format!(
                "{}\n\n输入：\n{}",
                self.summary_prompt,
                python_dumps_compact(&Value::Object(payload))
            );
            let response = self
                .call_model
                .call(&[json!({"role": "user", "content": prompt})])?;
            usage = usage.add(
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.cached_input_tokens,
            );
            if !response.profile.is_empty() {
                latest_profile = response.profile.clone();
            }
            if !response.provider.is_empty() {
                latest_provider = response.provider.clone();
            }
            if response.tool_calls != 0 {
                parse_error =
                    "摘要响应里出现了工具调用；本请求禁止调用工具，只允许输出 JSON 对象。"
                        .to_string();
                continue;
            }
            match parse_structured_summary(&response.content) {
                Ok(structured) => {
                    return Ok(ModelSummaryResult {
                        structured,
                        usage,
                        profile: latest_profile,
                        provider: latest_provider,
                        attempts: attempt,
                    })
                }
                Err(error) => {
                    parse_error = error.message().to_string();
                    continue;
                }
            }
        }
        Err(SummaryGenerationError::new(format!(
            "摘要模型连续返回无效结构：{parse_error}"
        )))
    }
}

struct StructuredRequest<'a> {
    previous_summary: Option<&'a Value>,
    source_events: &'a [SourceEvent],
    partial_summaries: Vec<Value>,
    target_summary_tokens: i64,
    validation_feedback: &'a [String],
    operation: &'a str,
    chunk_index: i64,
    chunk_count: i64,
    /// 事件索引与上次摘要都说短引用（`E1`…），真 ID 不发给模型。
    references: &'a EventRefs,
}

/// 读取摘要提示词模板；空模板按空串返回（提示词组装方负责判定）。
pub fn read_summary_prompt(templates_dir: &Path) -> Result<String, AgentError> {
    let path = templates_dir.join(SUMMARY_PROMPT_FILE);
    let bytes = std::fs::read(&path)
        .map_err(|error| AgentError::new(format!("读取 {SUMMARY_PROMPT_FILE} 失败：{error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| AgentError::new(format!("{SUMMARY_PROMPT_FILE} 必须是 UTF-8 文本。")))?;
    Ok(text.trim().to_string())
}

/// 解析模型返回的结构化摘要：容忍 ``` 围栏，顶层必须是 JSON 对象。
pub fn parse_structured_summary(content: &str) -> Result<Value, SummaryGenerationError> {
    let mut text = content.trim().to_string();
    if text.starts_with("```") {
        let mut lines = python_split_lines(&text);
        if lines
            .first()
            .map(|line| line.trim().starts_with("```"))
            .unwrap_or(false)
        {
            lines.remove(0);
        }
        if lines
            .last()
            .map(|line| line.trim() == "```")
            .unwrap_or(false)
        {
            lines.pop();
        }
        text = lines.join("\n").trim().to_string();
    }
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            return Err(SummaryGenerationError::new(format!(
                "摘要响应不是合法 JSON：{error}"
            )))
        }
    };
    if !value.is_object() {
        return Err(SummaryGenerationError::new(
            "摘要响应顶层必须是 JSON 对象。",
        ));
    }
    Ok(value)
}

/// 交给摘要模型的上次摘要：优先结构化形状，旧版纯文本包成 `legacy_content`。
pub fn previous_structured(payload: Option<&Value>) -> Option<Value> {
    let payload = payload?;
    if let Some(structured) = payload.get("structured") {
        if structured.is_object() {
            return Some(structured.clone());
        }
    }
    let content = payload.get("content").and_then(Value::as_str);
    match content {
        Some(content) if !content.trim().is_empty() => {
            let mut map = Map::new();
            map.insert(
                "legacy_content".to_string(),
                Value::from(content.trim().to_string()),
            );
            Some(Value::Object(map))
        }
        _ => None,
    }
}

/// 按单块预算把事件切成索引块：正文随复用的原请求前缀发送，这里只排目录。
pub fn chunk_events(events: &[SourceEvent], max_input_tokens: i64) -> Vec<Vec<SourceEvent>> {
    let mut chunks: Vec<Vec<SourceEvent>> = Vec::new();
    let mut current: Vec<SourceEvent> = Vec::new();
    let mut current_tokens = 0i64;
    for event in events {
        let event_tokens = estimate_json_tokens(&event.to_index_dict(INDEX_PREVIEW_CHARS));
        if !current.is_empty() && current_tokens + event_tokens > max_input_tokens {
            chunks.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current.push(event.clone());
        current_tokens += event_tokens;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}
