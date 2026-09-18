//! `omnicrawl/agent/context_compaction/summary.py`：低成本模型结构化摘要调用、分块和 JSON 响应解析。
//!
//! 模型调用经 [`SummaryModelCaller`] 注入：本模块只负责提示词组装、事件索引分块、解析重试与
//! 用量累计。真实请求（复用主请求前缀、`tool_choice=none`）由宿主持有，内核侧实现见
//! `omnicrawl-compaction` 的 `SummaryModelAdapter`。

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

        if partials.len() == 1 {
            return Ok(ModelSummaryResult {
                structured: partials.remove(0),
                usage: aggregate_usage,
                profile,
                provider,
                attempts,
            });
        }

        let merged = self.request_structured(&StructuredRequest {
            previous_summary: previous.as_ref(),
            source_events: &[],
            partial_summaries: partials,
            target_summary_tokens,
            validation_feedback,
            operation: "merge_chunks",
            chunk_index: 1,
            chunk_count: 1,
        })?;
        aggregate_usage = aggregate_usage.add(
            merged.usage.input_tokens,
            merged.usage.output_tokens,
            merged.usage.cached_input_tokens,
        );
        Ok(ModelSummaryResult {
            structured: merged.structured,
            usage: aggregate_usage,
            profile: if merged.profile.is_empty() {
                profile
            } else {
                merged.profile
            },
            provider: if merged.provider.is_empty() {
                provider
            } else {
                merged.provider
            },
            attempts: attempts + merged.attempts,
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
                    .map(|event| event.to_index_dict(INDEX_PREVIEW_CHARS))
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
