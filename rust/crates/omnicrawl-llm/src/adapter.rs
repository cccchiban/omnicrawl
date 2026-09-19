//! Provider 运行时工厂：按生效协议选运行时实现，并组装端点与模型能力。
//!
//! 语义基准是 Python `omnicrawl/llm/registry.py` 的 `build_runtime` 与各 Adapter 的
//! `create_runtime`：先解析生效协议（调用方指定 > Profile 默认 > Provider 默认），
//! 再用「Provider 保守默认 → 模型声明」合并能力、按模型描述覆盖上下文窗口；
//! Python 侧的 SDK 客户端在内核里换成 [`ChatEndpoint`]，`base_url` 留空时用 Provider 默认 API 根。
//!
//! 未搬：`discover_models`（各 Provider 的模型列表发现）与出网脱敏装饰器。

use omnicrawl_protocol::Protocol;

use crate::capabilities::{merge_capabilities, ModelCapabilities};
use crate::errors::ModelError;
use crate::registry::resolve_protocol;
use crate::runtime::{
    AnthropicRuntime, ChatEndpoint, GeminiRuntime, ModelRuntime, OpenAiChatRuntime,
    ResponsesRuntime,
};

/// 一次运行时构建所需的 Provider 侧配置（Python `ProviderProfile` 的内核子集）。
#[derive(Debug, Clone, Default)]
pub struct ProviderProfile {
    pub id: String,
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub user_agent: String,
    pub default_protocol: String,
}

/// 模型侧描述（Python `ModelDescriptor` 的内核子集）。
#[derive(Debug, Clone, Default)]
pub struct ModelDescriptor {
    pub model_id: String,
    pub protocol: String,
    pub capabilities: Option<ModelCapabilities>,
    pub context_window_tokens: i64,
    pub max_output_tokens: Option<u32>,
}

/// 构建结果：运行时本体 + 生效协议 + 组装后的能力与 API 根。
///
/// 能力门禁（`streaming` / `tools` / `prompt_cache` 开关）留在调用方，所以能力要一起交出去。
pub struct RuntimeBundle {
    pub protocol: Protocol,
    pub capabilities: ModelCapabilities,
    pub base_url: String,
    pub runtime: Box<dyn ModelRuntime>,
}

impl std::fmt::Debug for RuntimeBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeBundle")
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// Provider 的默认 API 根（Python 侧由各 SDK 兜底，内核显式给出）。
pub fn default_base_url(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChatCompletions | Protocol::OpenaiResponses => "https://api.openai.com/v1",
        Protocol::AnthropicMessages => "https://api.anthropic.com",
        Protocol::GeminiGenerateContent => "https://generativelanguage.googleapis.com",
    }
}

/// Provider 的保守默认能力（Python 每个 Adapter 的 `conservative_*_capabilities`）。
pub fn conservative_capabilities(protocol: Protocol) -> ModelCapabilities {
    match protocol {
        Protocol::OpenaiChatCompletions => ModelCapabilities::conservative_openai_chat(),
        Protocol::OpenaiResponses => ModelCapabilities::conservative_openai_responses(),
        Protocol::AnthropicMessages => ModelCapabilities::conservative_anthropic(),
        Protocol::GeminiGenerateContent => ModelCapabilities::conservative_gemini(),
    }
}

/// 组装一次运行时（Python `build_runtime` ＋ `create_runtime`）。
pub fn build_runtime(
    profile: &ProviderProfile,
    model: &ModelDescriptor,
) -> Result<RuntimeBundle, ModelError> {
    let protocol = resolve_protocol(
        &profile.provider,
        &profile.default_protocol,
        &model.protocol,
    )?;

    let mut capabilities = merge_capabilities(&[
        Some(conservative_capabilities(protocol)),
        model.capabilities,
    ]);
    // 模型描述的窗口只在正数时覆盖（与 Python `if model.context_window_tokens > 0` 同口径）。
    if model.context_window_tokens > 0 {
        capabilities.context_window_tokens = model.context_window_tokens;
    }

    let configured = profile.base_url.trim();
    let base_url = if configured.is_empty() {
        default_base_url(protocol).to_string()
    } else {
        configured.to_string()
    };
    let endpoint = ChatEndpoint {
        base_url: base_url.clone(),
        api_key: profile.api_key.clone(),
        user_agent: profile.user_agent.clone(),
    };

    let runtime: Box<dyn ModelRuntime> = match protocol {
        Protocol::OpenaiChatCompletions => Box::new(OpenAiChatRuntime::new(endpoint)),
        Protocol::OpenaiResponses => Box::new(ResponsesRuntime::new(endpoint)),
        Protocol::AnthropicMessages => {
            Box::new(AnthropicRuntime::new(endpoint, model.max_output_tokens))
        }
        Protocol::GeminiGenerateContent => Box::new(GeminiRuntime::new(endpoint)),
    };

    Ok(RuntimeBundle {
        protocol,
        capabilities,
        base_url,
        runtime,
    })
}
