//! 工具输出压缩的旁路调用：把超长工具观察交给独立小模型压成精简文本。
//!
//! 语义基准是 Python `agent/controllers/tools/compression.py` 的批量编排与
//! `agent/runtime/tool_output_compressor.py` 的请求构造：只有合格结果才压缩、模型没真正压小
//! 就不采纳、失败保留原文。
//!
//! 模型连接沿用内核 `initialize.model` 给的那条（与 `subagent::child_model_config` 同一做法），
//! 只把 model 名换成 `[tool_output_compression].model_key`；请求是旁路调用：不流式、不写会话。

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::tool_output_compression::load_tool_output_compression_config;
use omnicrawl_controllers::compression as compression_logic;
use omnicrawl_ipc::bridge::KernelModelConfig;
use omnicrawl_llm::{ChatRequestInput, DiscardSink, ModelRuntime};
use omnicrawl_protocol::{
    conversation_from_openai_messages, ConversationMessage, GenerationOptions,
};
use std::collections::BTreeMap;

use crate::session::{build_model_runtime_with_key, read_api_key};

pub struct KernelCompressor {
    runtime: Box<dyn ModelRuntime>,
    model: String,
    system_prompt: String,
    min_chars: usize,
    max_input_chars: usize,
    max_output_chars: usize,
    timeout_seconds: f64,
    options: GenerationOptions,
}

impl KernelCompressor {
    /// 从配置与主模型连接装配压缩器；未启用或装配失败返回 `None`（压缩是可选旁路）。
    pub fn load(model: &KernelModelConfig) -> Option<Self> {
        let env = ConfigEnvironment::from_process();
        let config = load_tool_output_compression_config(&env, None).ok()?;
        if !config.active() {
            return None;
        }

        let selection = config.model_key.trim().to_string();
        let mut child = model.clone();
        if !selection.is_empty() {
            child.model = selection.clone();
        }
        child.system_prompt = String::new();
        child.tools = Vec::new();
        let api_key = read_api_key(&child).ok()?;
        let runtime = build_model_runtime_with_key(&child, api_key).ok()?;

        let effort = compression_logic::effective_reasoning_effort(
            config.thinking_enabled,
            &config.reasoning_effort,
        );
        let options = GenerationOptions {
            reasoning_effort: effort,
            ..GenerationOptions::default()
        };

        Some(Self {
            runtime,
            model: if selection.is_empty() {
                child.model.clone()
            } else {
                selection
            },
            system_prompt: compression_logic::system_prompt_text(),
            min_chars: config.min_chars.max(0) as usize,
            max_input_chars: config.max_input_chars.max(0) as usize,
            max_output_chars: config.max_output_chars.max(0) as usize,
            timeout_seconds: config.timeout_seconds.max(0) as f64,
            options,
        })
    }

    /// 该工具结果是否值得压缩（与 Python `_should_compact` 同规则）。
    pub fn should_compress(&self, tool_name: &str, output: &str) -> bool {
        compression_logic::should_compact(tool_name, output, self.min_chars)
    }

    /// 单次压缩；模型返回工具调用或空文本、请求失败都返回 `Err`，由调用方保留原文。
    pub fn compress(
        &self,
        tool_name: &str,
        arguments_summary: &str,
        task_hint: &str,
        output: &str,
    ) -> Result<String, String> {
        if output.trim().is_empty() {
            return Err("工具输出为空，无需压缩。".to_string());
        }
        let sampled = compression_logic::sample_output(output, self.max_input_chars);
        let messages =
            compression_logic::build_messages(tool_name, arguments_summary, task_hint, &sampled);
        let conversation: Vec<ConversationMessage> = conversation_from_openai_messages(&messages);
        let identity: BTreeMap<String, String> = BTreeMap::from([
            (
                "scope".to_string(),
                "omnicrawl-tool-output-compression".to_string(),
            ),
            ("model".to_string(), self.model.clone()),
        ]);
        let input = ChatRequestInput {
            model: &self.model,
            system_prompt: &self.system_prompt,
            messages: &conversation,
            tools: &[],
            options: &self.options,
            profile_request_timeout_seconds: self.timeout_seconds,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let reply = self
            .runtime
            .run_turn(&input, &mut DiscardSink)
            .map_err(|error| error.message.clone())?;
        if !reply.tool_calls.is_empty() {
            return Err("压缩模型返回了工具调用。".to_string());
        }
        let text = compression_logic::clean_reply_text(reply.content.as_str());
        if text.is_empty() {
            return Err("压缩模型返回了空文本。".to_string());
        }
        Ok(compression_logic::bound_text(&text, self.max_output_chars))
    }

    /// 把压缩结果写回观察：模型没压小就保留原文（与 Python `_compress_one` 同规则）。
    ///
    /// 返回真正采纳的下标，供调用方记录/展示。
    pub fn apply_observations(
        &self,
        observations: &mut [omnicrawl_core::AgentLoopObservation],
        task_hint: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Vec<usize> {
        let mut applied = Vec::new();
        for (index, observation) in observations.iter_mut().enumerate() {
            if cancelled() {
                break;
            }
            let tool_name = observation.tool_call.name.clone();
            let output = observation.result.output.clone();
            if !self.should_compress(&tool_name, &output) {
                continue;
            }
            let summary = compression_logic::arguments_summary(&serde_json::Value::Object(
                observation.tool_call.arguments.clone(),
            ));
            let compacted = match self.compress(&tool_name, &summary, task_hint, &output) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("[kernel] 工具输出压缩失败，保留原始输出：{tool_name}：{error}");
                    continue;
                }
            };
            if compacted.chars().count() >= output.chars().count() {
                eprintln!("[kernel] 工具输出压缩未缩小结果，保留原始输出：{tool_name}");
                continue;
            }
            let full = compression_logic::compacted_display(
                &compacted,
                compacted.chars().count(),
                output.chars().count(),
                &self.model,
            );
            let tool_call = observation.tool_call.clone();
            observation.result.output = compacted.clone();
            observation.result.full_output = full;
            observation.message = omnicrawl_session::tool_result_message(
                &tool_call.name,
                observation.result.ok,
                &compacted,
                &tool_call.id,
            );
            applied.push(index);
        }
        applied
    }
}
