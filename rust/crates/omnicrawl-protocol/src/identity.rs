//! Provider 与传输协议的闭集、模型唯一身份。

use std::fmt;

use serde::{Deserialize, Serialize};

/// Provider 闭集，取值与 Python 侧 `PROVIDER_*` 常量一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Anthropic,
    Gemini,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Openai, Provider::Anthropic, Provider::Gemini];

    pub const fn as_str(self) -> &'static str {
        match self {
            Provider::Openai => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Gemini => "gemini",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.as_str() == value)
    }

    /// 未显式指定协议时的默认协议（对应 Python 侧 `PROVIDER_DEFAULT_PROTOCOL`）。
    pub const fn default_protocol(self) -> Protocol {
        match self {
            Provider::Openai => Protocol::OpenaiChatCompletions,
            Provider::Anthropic => Protocol::AnthropicMessages,
            Provider::Gemini => Protocol::GeminiGenerateContent,
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 传输协议闭集，取值对应 Python 侧 `PROTOCOL_*` 常量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    OpenaiResponses,
    OpenaiChatCompletions,
    AnthropicMessages,
    GeminiGenerateContent,
}

impl Protocol {
    pub const ALL: [Protocol; 4] = [
        Protocol::OpenaiResponses,
        Protocol::OpenaiChatCompletions,
        Protocol::AnthropicMessages,
        Protocol::GeminiGenerateContent,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenaiResponses => "openai_responses",
            Protocol::OpenaiChatCompletions => "openai_chat_completions",
            Protocol::AnthropicMessages => "anthropic_messages",
            Protocol::GeminiGenerateContent => "gemini_generate_content",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.as_str() == value)
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 模型唯一身份：同一 model_id 可来自不同 Profile / 协议。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub profile_id: String,
    pub provider: Provider,
    pub protocol: Protocol,
    pub model_id: String,
    #[serde(default)]
    pub catalog_key: String,
}

impl ModelIdentity {
    pub fn new(
        profile_id: impl Into<String>,
        provider: Provider,
        protocol: Protocol,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            profile_id: profile_id.into(),
            provider,
            protocol,
            model_id: model_id.into(),
            catalog_key: String::new(),
        }
    }

    /// 对应 Python 侧 `ModelIdentity.triple`：(profile_id, protocol, model_id)。
    pub fn triple(&self) -> (&str, Protocol, &str) {
        (
            self.profile_id.as_str(),
            self.protocol,
            self.model_id.as_str(),
        )
    }

    /// 对应 Python 侧 `ModelIdentity.as_ref`：`catalog_key` 优先，否则 `profile_id/model_id`。
    pub fn reference(&self) -> String {
        if !self.catalog_key.is_empty() {
            return self.catalog_key.clone();
        }
        format!("{}/{}", self.profile_id, self.model_id)
    }
}
