//! Provider 与协议的解析、默认值与一致性校验。
//!
//! 语义基准是 Python `omnicrawl/llm/registry.py` 的纯逻辑部分（`ProviderProfile.resolve_protocol`、
//! `protocol_for_provider`、`_validate_protocol_matches_provider`）：给定 provider、默认协议与调用方
//! 指定的协议，选出真正生效的协议，并拒绝「未知 Provider / 不支持的协议 / 协议与 Provider 不匹配」。
//!
//! 未搬：adapter 注册表与 `build_runtime` 工厂（依赖各 provider 的实现与脱敏装饰器）。

use omnicrawl_protocol::{Protocol, Provider};

use crate::errors::ModelError;

/// 解析生效协议：调用方指定 > Profile 默认 > Provider 默认，再校验与 Provider 匹配。
///
/// 判定顺序照 Python：空串才退到下一层，纯空白串会先在 `strip()` 后落到 Provider 默认。
pub fn resolve_protocol(
    provider: &str,
    default_protocol: &str,
    requested: &str,
) -> Result<Protocol, ModelError> {
    let raw = if requested.is_empty() {
        default_protocol
    } else {
        requested
    };
    let chosen = raw.trim();
    let protocol = if chosen.is_empty() {
        match Provider::parse(provider) {
            Some(known) => known.default_protocol(),
            None => {
                return Err(ModelError::configuration(format!(
                    "未知 Provider：{provider}"
                )))
            }
        }
    } else {
        Protocol::parse(chosen)
            .ok_or_else(|| ModelError::configuration(format!("不支持的协议：{chosen}")))?
    };
    validate_protocol_matches_provider(provider, protocol)?;
    Ok(protocol)
}

/// Provider 的默认协议（Python `protocol_for_provider`）。
pub fn protocol_for_provider(provider: &str, preferred: &str) -> Result<Protocol, ModelError> {
    resolve_protocol(provider, preferred, preferred)
}

/// 协议必须属于该 Provider 的允许集合。
pub fn validate_protocol_matches_provider(
    provider: &str,
    protocol: Protocol,
) -> Result<(), ModelError> {
    let allowed = match Provider::parse(provider) {
        Some(Provider::Openai) => matches!(
            protocol,
            Protocol::OpenaiChatCompletions | Protocol::OpenaiResponses
        ),
        Some(Provider::Anthropic) => matches!(protocol, Protocol::AnthropicMessages),
        Some(Provider::Gemini) => matches!(protocol, Protocol::GeminiGenerateContent),
        None => {
            return Err(ModelError::configuration(format!(
                "未知 Provider：{provider}"
            )))
        }
    };
    if allowed {
        return Ok(());
    }
    Err(ModelError::configuration(format!(
        "协议 {} 与 Provider {provider} 不匹配。",
        protocol.as_str()
    )))
}
