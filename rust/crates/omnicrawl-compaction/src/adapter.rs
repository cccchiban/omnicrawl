//! `omnicrawl/agent/context_compaction/summary.py` 的模型面：把摘要请求接到内核运行时。
//!
//! 与主请求共享同一份系统提示词、消息前缀与工具声明（逐字复用才能命中前缀缓存），
//! 只有 `tool_choice` 不同：工具面照带，但模型不允许调用工具。没有前缀时不发请求——
//! 待压缩正文只存在于复用前缀里，缺前缀既命不中缓存也拿不到正文。

use std::collections::BTreeMap;

use omnicrawl_controllers::context_compaction::{
    estimate_messages_tokens, estimate_text_tokens, SummaryGenerationError, SummaryModelCall,
    SummaryModelResponse, TokenUsageSample, MAX_INDEX_CHUNK_TOKENS, SUMMARY_OUTPUT_RESERVE_TOKENS,
    SUMMARY_TOOL_CHOICE,
};
use omnicrawl_llm::{ChatRequestInput, ModelRuntime, SinkFlow, TurnSink};
use omnicrawl_protocol::{
    conversation_from_openai_messages, tool_spec_from_openai_item, GenerationOptions,
    ModelStreamEvent,
};
use serde_json::Value;

/// 适配器的构造参数：模型身份、与主请求逐字一致的复用前缀与工具面、缓存身份与窗口。
pub struct SummaryAdapterSettings {
    pub runtime: Box<dyn ModelRuntime>,
    pub model: String,
    pub provider: String,
    pub system_prompt: String,
    pub prefix: Vec<Value>,
    pub tools: Vec<Value>,
    pub options: GenerationOptions,
    pub prompt_cache_identity: BTreeMap<String, String>,
    pub context_window_tokens: i64,
}

/// 摘要模型适配器：复用主请求前缀与工具面，按 `tool_choice=none` 发一次请求。
pub struct SummaryModelAdapter {
    runtime: Box<dyn ModelRuntime>,
    model: String,
    provider: String,
    system_prompt: String,
    /// 最近一次主请求的逐字消息；为空表示没有可复用前缀。
    prefix: Vec<Value>,
    /// 与主请求逐字相同的工具声明（不同源时由调用方给空）。
    tools: Vec<Value>,
    options: GenerationOptions,
    prompt_cache_identity: BTreeMap<String, String>,
    context_window_tokens: i64,
}

impl SummaryModelAdapter {
    pub fn new(settings: SummaryAdapterSettings) -> Self {
        Self {
            runtime: settings.runtime,
            model: settings.model,
            provider: settings.provider,
            system_prompt: settings.system_prompt,
            prefix: settings.prefix,
            tools: settings.tools,
            options: settings.options,
            prompt_cache_identity: settings.prompt_cache_identity,
            context_window_tokens: settings.context_window_tokens,
        }
    }

    /// 复用前缀的 token 估算：系统提示词 + 逐字复用的主请求消息。
    pub fn prefix_token_estimate(&self) -> i64 {
        estimate_text_tokens(&self.system_prompt) + estimate_messages_tokens(&self.prefix)
    }

    /// 单块事件索引预算：窗口一半扣掉复用前缀与输出预留；窗口缺失或余额不足返回 None。
    pub fn index_chunk_budget_tokens(&self) -> Option<i64> {
        if self.context_window_tokens <= 0 {
            return None;
        }
        let budget = self.context_window_tokens / 2
            - self.prefix_token_estimate()
            - SUMMARY_OUTPUT_RESERVE_TOKENS;
        if budget <= 0 {
            return None;
        }
        Some(std::cmp::min(budget, MAX_INDEX_CHUNK_TOKENS))
    }
}

impl SummaryModelCall for SummaryModelAdapter {
    fn call(&self, messages: &[Value]) -> Result<SummaryModelResponse, SummaryGenerationError> {
        if self.prefix.is_empty() {
            return Err(SummaryGenerationError::new(
                "没有可复用的主请求前缀，已跳过本次模型摘要（降级为确定性压缩）。",
            ));
        }
        let mut all = self.prefix.clone();
        all.extend(messages.iter().cloned());
        let conversation = conversation_from_openai_messages(&all);
        let tools: Vec<_> = self
            .tools
            .iter()
            .filter_map(tool_spec_from_openai_item)
            .collect();
        let options = GenerationOptions {
            tool_choice: SUMMARY_TOOL_CHOICE.to_string(),
            ..self.options.clone()
        };
        let input = ChatRequestInput {
            model: self.model.as_str(),
            system_prompt: self.system_prompt.as_str(),
            messages: &conversation,
            tools: &tools,
            options: &options,
            profile_request_timeout_seconds: self.options.request_timeout_seconds,
            prompt_cache_capable: !self.prompt_cache_identity.is_empty(),
            prompt_cache_identity: &self.prompt_cache_identity,
        };
        let mut sink = UsageSink::default();
        let reply = self.runtime.run_turn(&input, &mut sink).map_err(|error| {
            SummaryGenerationError::new(format!("摘要模型请求失败：{}", error.message))
        })?;
        Ok(SummaryModelResponse {
            content: reply.content,
            usage: sink.usage,
            profile: self.model.clone(),
            provider: self.provider.clone(),
            tool_calls: reply.tool_calls.len() as i64,
        })
    }
}

/// 只收集用量、丢弃增量：摘要调用不面向界面。
#[derive(Default)]
struct UsageSink {
    usage: TokenUsageSample,
}

impl TurnSink for UsageSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        if let ModelStreamEvent::UsageReported(usage) = event {
            let to_tokens = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
            self.usage = self.usage.add(
                to_tokens(usage.input_tokens),
                to_tokens(usage.output_tokens),
                to_tokens(usage.cached_input_tokens),
            );
        }
        SinkFlow::Continue
    }
}
