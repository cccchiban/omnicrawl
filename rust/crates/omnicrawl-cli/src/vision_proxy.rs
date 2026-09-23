//! 独立视觉模型代理的内核执行体：把带图观察交给 `[vision].models` 里的模型，取回文本分析。
//!
//! 语义基准是 Python `agent/runtime/vision_proxy.py`：按配置顺序尝试，前一个候选失败就换下一个；
//! 每个候选沿用 `initialize.model` 给的连接、只换 model 名（与工具输出压缩同一做法）。请求是旁路
//! 调用：不流式、不写会话、无工具与系统提示，`reasoning_effort=none`。

use std::collections::BTreeMap;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::models::vision::load_vision_configuration;
use omnicrawl_controllers::output::{
    vision_display_text, vision_failure_error, vision_followup_message,
};
use omnicrawl_controllers::vision_proxy as vision_logic;
use omnicrawl_core::AgentLoopObservation;
use omnicrawl_ipc::bridge::KernelModelConfig;
use omnicrawl_llm::{ChatRequestInput, DiscardSink, ModelRuntime};
use omnicrawl_protocol::{conversation_from_openai_messages, GenerationOptions};
use serde_json::Value;

use crate::session::{build_model_runtime_with_key, read_api_key};

/// 视觉代理成功返回的文本与实际使用的模型标签。
pub struct VisionAnalysis {
    pub text: String,
    pub model: String,
}

struct VisionCandidate {
    label: String,
    model: String,
    timeout_seconds: f64,
    runtime: Box<dyn ModelRuntime>,
}

pub struct KernelVisionProxy {
    candidates: Vec<VisionCandidate>,
    max_output_chars: usize,
}

/// 这批观察里有没有带图观察：有才值得装配代理（与 Python 的按需构造一致）。
pub fn has_vision_observation(observations: &[AgentLoopObservation]) -> bool {
    observations
        .iter()
        .any(|observation| vision_logic::vision_followup_content(observation).is_some())
}

impl KernelVisionProxy {
    /// 从 `[vision]` 配置与主模型连接装配代理；未启用或没有可用候选时返回 `None`（代理是可选旁路）。
    pub fn load(model: &KernelModelConfig) -> Option<Self> {
        let env = ConfigEnvironment::from_process();
        let config = load_vision_configuration(&env, None).ok()?;
        if !config.enabled || config.models.is_empty() {
            return None;
        }
        let mut candidates = Vec::new();
        for reference in &config.models {
            let Ok(selection) = vision_logic::model_ref_selection(reference) else {
                continue;
            };
            let mut child = model.clone();
            child.model = selection;
            if reference.source == "detected" && !reference.protocol.is_empty() {
                child.protocol = reference.protocol.clone();
            }
            child.system_prompt = String::new();
            child.tools = Vec::new();
            // 故障转移不该被单个候选的长重试拖住（与 Python 一致：每个候选最多两次）。
            child.request_retry_count = child.request_retry_count.clamp(1, 2);
            let Ok(api_key) = read_api_key(&child) else {
                continue;
            };
            let Ok(runtime) = build_model_runtime_with_key(&child, api_key) else {
                continue;
            };
            candidates.push(VisionCandidate {
                label: vision_logic::model_ref_label(reference),
                model: child.model.clone(),
                timeout_seconds: child.request_timeout_seconds.unwrap_or(0.0).max(0.0),
                runtime,
            });
        }
        if candidates.is_empty() {
            return None;
        }
        Some(Self {
            candidates,
            max_output_chars: vision_logic::VISION_MAX_OUTPUT_CHARS,
        })
    }

    /// 按配置顺序调用视觉模型；当前候选失败就换下一个，全部失败返回汇总文案。
    pub fn analyze(&self, content: &Value) -> Result<VisionAnalysis, String> {
        let mut errors: Vec<String> = Vec::new();
        for candidate in &self.candidates {
            match candidate.request(content, self.max_output_chars) {
                Ok(text) => {
                    return Ok(VisionAnalysis {
                        text,
                        model: candidate.label.clone(),
                    })
                }
                Err(error) => errors.push(vision_logic::candidate_error(&candidate.label, &error)),
            }
        }
        Err(vision_logic::all_candidates_failed_error(&errors))
    }

    /// 把带图观察换成视觉结论，返回真正改写的下标。
    ///
    /// 成功：正文换成不可信文本观察，展示文本追加「视觉模型分析（模型）：…」。
    /// 失败：结果整体变成错误文案，图片观察不再注入（与 Python 的失败分支同义）。
    /// 观察里没有图片时原样保留——宿主没把图片交出来，内核不插手。
    pub fn apply_observations(
        &self,
        observations: &mut [AgentLoopObservation],
        cancelled: &dyn Fn() -> bool,
    ) -> Vec<usize> {
        let mut applied = Vec::new();
        for (index, observation) in observations.iter_mut().enumerate() {
            if cancelled() {
                break;
            }
            let Some(content) = vision_logic::vision_followup_content(observation) else {
                continue;
            };
            match self.analyze(&content) {
                Ok(analysis) => {
                    let original = if observation.result.full_output.is_empty() {
                        observation.result.output.clone()
                    } else {
                        observation.result.full_output.clone()
                    };
                    observation.result.full_output =
                        vision_display_text(&original, &analysis.model, &analysis.text);
                    observation.followup_messages =
                        vec![vision_followup_message(&analysis.model, &analysis.text)];
                }
                Err(error) => {
                    let text = vision_failure_error(&error);
                    observation.result.ok = false;
                    observation.result.output = text.clone();
                    observation.result.full_output = text.clone();
                    observation.followup_messages.clear();
                    observation.message = omnicrawl_session::tool_result_message(
                        &observation.tool_call.name,
                        false,
                        &text,
                        &observation.tool_call.id,
                    );
                }
            }
            applied.push(index);
        }
        applied
    }
}

impl VisionCandidate {
    fn request(&self, content: &Value, max_output_chars: usize) -> Result<String, String> {
        let messages = vec![vision_logic::vision_request_message(content.clone())];
        let conversation = conversation_from_openai_messages(&messages);
        let identity: BTreeMap<String, String> = BTreeMap::from([
            ("scope".to_string(), "omnicrawl-vision-proxy".to_string()),
            ("model".to_string(), self.model.clone()),
        ]);
        let options = GenerationOptions {
            reasoning_effort: "none".to_string(),
            ..GenerationOptions::default()
        };
        let input = ChatRequestInput {
            model: &self.model,
            system_prompt: "",
            messages: &conversation,
            tools: &[],
            options: &options,
            profile_request_timeout_seconds: self.timeout_seconds,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let reply = self
            .runtime
            .run_turn(&input, &mut DiscardSink)
            .map_err(|error| error.message.clone())?;
        let text = reply.content.trim().to_string();
        if text.is_empty() {
            return Err(vision_logic::VISION_PROXY_EMPTY_TEXT_ERROR.to_string());
        }
        Ok(vision_logic::bound_text(&text, max_output_chars))
    }
}
