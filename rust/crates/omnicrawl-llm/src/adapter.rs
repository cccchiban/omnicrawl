//! Provider 运行时工厂：按生效协议选运行时实现，并组装端点与模型能力。
//!
//! 语义基准是 Python `omnicrawl/llm/registry.py` 的 `build_runtime` 与各 Adapter 的
//! `create_runtime`：先解析生效协议（调用方指定 > Profile 默认 > Provider 默认），
//! 再用「Provider 保守默认 → 模型声明」合并能力、按模型描述覆盖上下文窗口；
//! Python 侧的 SDK 客户端在内核里换成 [`ChatEndpoint`]，`base_url` 留空时用 Provider 默认 API 根。
//!
//! 未搬：出网脱敏装饰器（`DesensitizationRuntime`）。

use omnicrawl_protocol::Protocol;
use serde_json::Value;

use crate::anthropic::ANTHROPIC_VERSION;
use crate::capabilities::{merge_capabilities, ModelCapabilities};
use crate::errors::ModelError;
use crate::json::text_of;
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

/// 模型发现的结果状态（Python `DiscoveryResult.status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryStatus {
    Ok,
    Unavailable,
    Unsupported,
}

/// 发现到的一个模型（Python `DiscoveryModel`）。
///
/// Python 每条发现结果都带上该协议的保守能力（`conservative_*_capabilities()`）；
/// 窗口值一律留 0，由调用方按 Profile 默认窗口回填（`model.context_window_tokens
/// or profile.default_context_window_tokens`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryModel {
    pub profile_id: String,
    pub provider: String,
    pub protocol: Protocol,
    pub model_id: String,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    pub context_window_tokens: i64,
}

/// 一次模型列表发现的结果（Python `DiscoveryResult`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryResult {
    pub profile_id: String,
    pub status: DiscoveryStatus,
    pub message: String,
    pub models: Vec<DiscoveryModel>,
}

impl DiscoveryResult {
    fn unavailable(profile_id: &str, message: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.to_string(),
            status: DiscoveryStatus::Unavailable,
            message: message.into(),
            models: Vec::new(),
        }
    }
}

/// Python 侧对列表条目的截断上限。
const DISCOVERY_LIMIT: usize = 500;

/// 模型列表发现（Python 各 Adapter 的 `discover_models`）。
///
/// Python 走 SDK 的 `models.list()`；内核按实测的线上形态直接发 GET——
/// OpenAI 两协议 `{base}/models` + `Bearer`、Anthropic `{base}/v1/models` +
/// `x-api-key` / `anthropic-version`、Gemini `{base}/v1beta/models` + `x-goog-api-key`。
/// 失败一律降级成 `unavailable`（发现模型不是关键路径，不该打断启动）。
pub fn discover_models(
    profile: &ProviderProfile,
    protocol: Protocol,
    timeout_seconds: f64,
) -> DiscoveryResult {
    if profile.api_key.trim().is_empty() {
        return DiscoveryResult::unavailable(&profile.id, "缺少 API Key，无法发现模型。");
    }

    let configured = profile.base_url.trim();
    let base = if configured.is_empty() {
        default_base_url(protocol).to_string()
    } else {
        configured.trim_end_matches('/').to_string()
    };
    let agent = crate::transport::build_agent();
    let user_agent = profile.user_agent.as_str();

    let (url, headers) = match protocol {
        Protocol::OpenaiChatCompletions | Protocol::OpenaiResponses => {
            let authorization = format!("Bearer {}", profile.api_key.trim());
            (
                format!("{base}/models"),
                vec![("Authorization", authorization)],
            )
        }
        Protocol::AnthropicMessages => (
            format!("{base}/v1/models"),
            vec![
                ("x-api-key", profile.api_key.trim().to_string()),
                ("anthropic-version", ANTHROPIC_VERSION.to_string()),
            ],
        ),
        Protocol::GeminiGenerateContent => (
            format!("{base}/v1beta/models"),
            vec![("x-goog-api-key", profile.api_key.trim().to_string())],
        ),
    };
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    let response = match crate::transport::get(&agent, &url, user_agent, timeout_seconds, &borrowed)
    {
        Ok(mut response) => {
            let status = response.status;
            let text = response.read_text();
            if status >= 400 {
                return DiscoveryResult::unavailable(
                    &profile.id,
                    discovery_failure_message(protocol, &text, "APIStatusError"),
                );
            }
            text
        }
        Err(failure) => {
            let (message, type_name) = failure.sdk_view();
            return DiscoveryResult::unavailable(
                &profile.id,
                discovery_failure_message(protocol, message, type_name),
            );
        }
    };

    let payload: Value = serde_json::from_str(&response).unwrap_or(Value::Null);
    let model_ids = match protocol {
        Protocol::GeminiGenerateContent => gemini_model_ids(&payload),
        _ => openai_style_model_ids(&payload),
    };

    let models = model_ids
        .into_iter()
        .take(DISCOVERY_LIMIT)
        .map(|model_id| DiscoveryModel {
            profile_id: profile.id.clone(),
            provider: profile.provider.clone(),
            protocol,
            display_name: model_id.clone(),
            model_id,
            capabilities: conservative_capabilities(protocol),
            context_window_tokens: 0,
        })
        .collect();

    DiscoveryResult {
        profile_id: profile.id.clone(),
        status: DiscoveryStatus::Ok,
        message: String::new(),
        models,
    }
}

/// `{"data":[{"id": …}]}`（OpenAI / Anthropic）：无 `data` 时按顶层数组读，与 Python 同口径。
fn openai_style_model_ids(payload: &Value) -> Vec<String> {
    let items = payload
        .get("data")
        .filter(|value| !value.is_null())
        .and_then(Value::as_array)
        .or_else(|| payload.as_array());
    collect_model_ids(items, "id")
}

/// `{"models":[{"name":"models/gemini-x"}]}`：`name` 取最后一段（Python 的 `split("/", 1)[-1]`）。
fn gemini_model_ids(payload: &Value) -> Vec<String> {
    let items = payload.get("models").and_then(Value::as_array);
    let raw = collect_model_ids(items, "name");
    raw.into_iter()
        .map(|name| {
            // Python 是 `name.split("/", 1)[-1]`：从左切第一个 `/`。
            name.split_once('/')
                .map(|(_, tail)| tail.to_string())
                .unwrap_or(name)
        })
        .collect()
}

fn collect_model_ids(items: Option<&Vec<Value>>, key: &str) -> Vec<String> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut result: Vec<String> = Vec::new();
    for item in items.into_iter().flatten() {
        let raw = item.get(key).map(text_of).unwrap_or_default();
        let model_id = raw.trim();
        if model_id.is_empty() || !seen.insert(model_id.to_string()) {
            continue;
        }
        result.push(model_id.to_string());
    }
    result
}

/// 发现失败文案：前缀按 Provider，内容走各家的错误阶梯。
fn discovery_failure_message(protocol: Protocol, message: &str, type_name: &str) -> String {
    match protocol {
        Protocol::OpenaiChatCompletions | Protocol::OpenaiResponses => {
            let view = crate::errors::ExceptionView {
                message,
                type_name,
                ..crate::errors::ExceptionView::default()
            };
            let mapped = crate::errors::map_exception(&view, &[]);
            format!("模型列表发现失败：{}", mapped.message)
        }
        Protocol::AnthropicMessages => format!(
            "Claude 模型列表发现失败：{}",
            crate::anthropic::format_anthropic_error(message, type_name)
        ),
        Protocol::GeminiGenerateContent => format!(
            "Gemini 模型列表发现失败：{}",
            crate::gemini::format_gemini_error(message, type_name)
        ),
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
