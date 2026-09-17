//! Provider 流的原始事件与一次回复的归并结果。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::message::{ConversationMessage, TokenUsage, ToolCallBlock};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDelta {
    pub text: String,
}

impl TextDelta {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningDelta {
    pub text: String,
}

impl ReasoningDelta {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallStarted {
    pub call_id: String,
    pub name: String,
}

impl ToolCallStarted {
    pub fn new(call_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallArgumentsDelta {
    pub call_id: String,
    pub delta: String,
}

impl ToolCallArgumentsDelta {
    pub fn new(call_id: impl Into<String>, delta: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            delta: delta.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallCompleted {
    pub call_id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
}

impl ToolCallCompleted {
    pub fn new(
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: Map<String, Value>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReported {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl UsageReported {
    pub fn to_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            reasoning_tokens: self.reasoning_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderWarning {
    pub code: String,
    pub message: String,
}

impl ProviderWarning {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Provider 流的归并输入，对应 Python 侧 `ModelStreamEvent` 联合类型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelStreamEvent {
    TextDelta(TextDelta),
    ReasoningDelta(ReasoningDelta),
    ToolCallStarted(ToolCallStarted),
    ToolCallArgumentsDelta(ToolCallArgumentsDelta),
    ToolCallCompleted(ToolCallCompleted),
    UsageReported(UsageReported),
    Finished { finish_reason: String },
    ProviderWarning(ProviderWarning),
}

/// 一次模型回复的归并结果（对应 Python 侧 `aggregate_stream_events` 的返回类型）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelReply {
    pub assistant_message: ConversationMessage,
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCallBlock>,
    pub usage: Option<TokenUsage>,
    pub finish_reason: String,
    pub content_streamed: bool,
    pub warnings: Vec<ProviderWarning>,
}
