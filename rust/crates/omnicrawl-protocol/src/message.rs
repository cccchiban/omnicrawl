//! Provider 无关的消息块、工具调用、消息与生成选项。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// 内联图片的细节等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    High,
}

impl ImageDetail {
    pub const fn as_str(self) -> &'static str {
        match self {
            ImageDetail::Auto => "auto",
            ImageDetail::Low => "low",
            ImageDetail::High => "high",
        }
    }

    /// Python 侧对 detail 做精确匹配，其他取值一律回落 `auto`。
    pub fn parse(value: &str) -> Self {
        match value {
            "low" => ImageDetail::Low,
            "high" => ImageDetail::High,
            _ => ImageDetail::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
}

impl TextBlock {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Provider 无关的内联图片块。
///
/// 只接收 Host 生成的 Base64 数据，不支持远程 URL，避免模型请求在未审批的情况下触发
/// 额外网络读取。图片仅存在于当前工具循环，不写入长期会话历史。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub media_type: String,
    pub data_base64: String,
    pub detail: ImageDetail,
}

impl ImageBlock {
    pub fn new(
        media_type: impl Into<String>,
        data_base64: impl Into<String>,
        detail: ImageDetail,
    ) -> Self {
        Self {
            media_type: media_type.into(),
            data_base64: data_base64.into(),
            detail,
        }
    }

    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data_base64)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBlock {
    pub call_id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
    #[serde(default)]
    pub provider_call_id: String,
}

impl ToolCallBlock {
    pub fn new(
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: Map<String, Value>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            arguments,
            provider_call_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub call_id: String,
    pub ok: bool,
    pub content: String,
}

impl ToolResultBlock {
    pub fn new(call_id: impl Into<String>, ok: bool, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            ok,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageBlock {
    Text(TextBlock),
    Image(ImageBlock),
    ToolCall(ToolCallBlock),
    ToolResult(ToolResultBlock),
}

/// 消息角色。闭集之外的取值原样透传：OpenAI Chat 分支会把 role 直接写进请求体。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    Other(String),
}

impl Role {
    pub fn parse(value: &str) -> Self {
        match value {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => Role::Other(value.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Other(value) => value.as_str(),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Role {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Role::parse(&String::deserialize(deserializer)?))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: Role,
    #[serde(default)]
    pub blocks: Vec<MessageBlock>,
    /// 思考模式思维链；回传历史时须原样携带。
    #[serde(default)]
    pub reasoning: String,
    /// system 消息可携带动态加载的工具声明。
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
}

impl ConversationMessage {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            blocks: Vec::new(),
            reasoning: String::new(),
            tools: Vec::new(),
        }
    }

    /// 对应 Python 侧 `ConversationMessage.text`：只拼接文本块。
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                MessageBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Map<String, Value>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenerationOptions {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub reasoning_effort: String,
    /// 空字符串＝沿用 Provider 默认（有工具面时为 auto）。
    pub tool_choice: String,
    pub request_timeout_seconds: f64,
    pub request_retry_count: u32,
    pub provider_options: Map<String, Value>,
}

impl Default for GenerationOptions {
    fn default() -> Self {
        Self {
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: String::new(),
            tool_choice: String::new(),
            request_timeout_seconds: 180.0,
            request_retry_count: 5,
            provider_options: Map::new(),
        }
    }
}

/// 一次模型请求的用量。
///
/// 字段为**有符号**整数，与 Python `omnicrawl/llm/protocol.py` 的 `TokenUsage`（普通 `int`）
/// 同口径：上游给出负值时原样保留，不在此层归零（是否参与统计由消费方决定，例如
/// `context_compaction` 的累加器会 clamp 到 0）。真实 Provider 不会给出负值。
///
/// 仍未对齐的一处：Python 是任意精度整数，超出 `i64` 的取值这里解析不出来（按缺失处理）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
}

impl TokenUsage {
    pub fn new(input_tokens: i64, output_tokens: i64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            ..Self::default()
        }
    }

    /// 兼容旧 UI 回调签名 (input, output, cached_input)。
    pub fn as_tuple(self) -> (i64, i64, i64) {
        (
            self.input_tokens,
            self.output_tokens,
            self.cached_input_tokens,
        )
    }
}
