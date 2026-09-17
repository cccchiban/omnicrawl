//! 循环的数据契约：对应 Python `omnicrawl/agent/types.py` 与
//! `omnicrawl/agent/runtime/execution.py` 的公开类型。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 模型请求执行的一次工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub function_name: String,
}

/// 工具调用返回给模型的结构化结果。
///
/// 只保留模型上下文使用的字段：UI 展示用的 `ui_artifact`、`completed_at` 与视觉附件
/// `model_images` 属于 Host 侧的展示与视觉通路，循环本身从不读取，因此不搬。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub full_output: String,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}

/// Agent 层一次模型回复。
///
/// 与 `omnicrawl_protocol::ModelReply` 同名但不同层：那是 Provider 流事件的归并结果，
/// 本类型是「已定型、可直接回填上下文」的一条回复，多一个 `message` 原文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentModelReply {
    /// 原样追加进上下文的 assistant 消息。
    pub message: Value,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub content_streamed: bool,
}

/// 可选循环预算；`None` 表示该维度不设上限。
///
/// 主 Agent 使用默认值以保持既有无限工具循环语义；SubAgent 可传入有界预算。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentLoopLimits {
    pub max_model_turns: Option<usize>,
    pub max_tool_calls: Option<usize>,
    pub timeout_seconds: Option<f64>,
}

impl AgentLoopLimits {
    /// 构造并校验预算，错误消息与 Python 侧的校验保持一致。
    pub fn new(
        max_model_turns: Option<usize>,
        max_tool_calls: Option<usize>,
        timeout_seconds: Option<f64>,
    ) -> Result<Self, LoopError> {
        for (name, value) in [
            ("max_model_turns", max_model_turns),
            ("max_tool_calls", max_tool_calls),
        ] {
            if value == Some(0) {
                return Err(LoopError::InvalidBudget(format!(
                    "{} 必须是正整数或 None。",
                    name
                )));
            }
        }
        if let Some(timeout) = timeout_seconds {
            if timeout <= 0.0 {
                return Err(LoopError::InvalidBudget(
                    "timeout_seconds 必须是正数或 None。".to_string(),
                ));
            }
        }
        Ok(Self {
            max_model_turns,
            max_tool_calls,
            timeout_seconds,
        })
    }
}

/// 一个已执行工具调用及其回填给模型的观察消息。
///
/// `followup_messages` 用于工具结果之后的补充观察（例如视觉截图）：循环会先回填同一
/// 批次的全部 tool 消息，再逐条追加这些 user 消息，保持工具协议要求的顺序。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentLoopObservation {
    pub tool_call: ToolCall,
    pub result: ToolResult,
    pub message: Value,
    #[serde(default)]
    pub followup_messages: Vec<Value>,
}

/// 循环完成后的结果。
///
/// Python 侧结果内嵌 `messages` 本身；Rust 侧消息列表由调用方持有，结果不复制，
/// 避免整段上下文的无谓克隆。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentLoopResult {
    pub final_text: String,
    pub reasoning: String,
    pub content_streamed: bool,
    pub model_turns: usize,
    pub tool_calls: usize,
    pub last_reply: Option<AgentModelReply>,
    pub paused: bool,
}

/// 循环错误。
///
/// 预算与批次校验错误在循环内产生；取消、模型回复、工具批次的错误由 Host 回调产生，
/// 循环原样传播、不做包装。
#[derive(Debug, Clone, PartialEq)]
pub enum LoopError {
    /// 预算参数非法。
    InvalidBudget(String),
    /// 超过模型回合、工具调用或时间预算。
    BudgetExceeded(String),
    /// 工具批次观察数量与模型调用数量不一致。
    ObservationMismatch(String),
    /// 取消检查触发。
    Cancelled(String),
    /// 模型回复来源失败。
    ReplySource(String),
    /// 工具批次执行失败。
    ToolBatch(String),
}

impl LoopError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidBudget(message)
            | Self::BudgetExceeded(message)
            | Self::ObservationMismatch(message)
            | Self::Cancelled(message)
            | Self::ReplySource(message)
            | Self::ToolBatch(message) => message,
        }
    }

    /// 稳定标签，供跨语言 parity 对照使用。
    pub fn tag(&self) -> &'static str {
        match self {
            Self::InvalidBudget(_) => "invalid_budget",
            Self::BudgetExceeded(_) => "budget_exceeded",
            Self::ObservationMismatch(_) => "observation_mismatch",
            Self::Cancelled(_) => "cancelled",
            Self::ReplySource(_) => "reply_source",
            Self::ToolBatch(_) => "tool_batch",
        }
    }
}
