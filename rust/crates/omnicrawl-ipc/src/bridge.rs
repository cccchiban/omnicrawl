//! 方法语义：宿主 → 内核的命令、内核 → 宿主的请求与notifications。
//!
//! 方法名与负载字段是协议的一部分；改动即破坏性变更，必须同步 `docs/protocol-v1.md`
//! 与宿主实现。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::frame::{error_code, ErrorObject, Frame, Id};
use crate::version::PROTOCOL_VERSION;
use omnicrawl_core::{AgentLoopObservation, ToolCall, ToolResult};

/// 协议 v1 的方法名。
pub mod method {
    // 宿主 → 内核
    pub const INITIALIZE: &str = "initialize";
    pub const TURN_SUBMIT: &str = "turn.submit";
    pub const TURN_CANCEL: &str = "turn.cancel";
    pub const SHUTDOWN: &str = "shutdown";

    // 内核 → 宿主（请求，需要宿主回响应）
    pub const TOOL_BATCH: &str = "tool.batch";
    /// 过渡期：内核把模型请求转交宿主代答；内核自带 provider runtime 后不再使用。
    pub const MODEL_REPLY: &str = "model.reply";

    // 内核 → 宿主（通知，宿主不需要回响应）
    pub const TURN_DELTA: &str = "turn.delta";
    pub const TURN_REASONING_DELTA: &str = "turn.reasoning_delta";
    pub const TURN_STATUS: &str = "turn.status";
    pub const TURN_RETRY_STATUS: &str = "turn.retry_status";
    pub const TURN_PROTOCOL_WAIT: &str = "turn.protocol_wait";
    pub const TURN_STREAM_ROLLBACK: &str = "turn.stream_rollback";
    pub const TURN_TOKEN_USAGE: &str = "turn.token_usage";
    pub const TURN_FINISHED: &str = "turn.finished";
    pub const TOOL_STARTED: &str = "tool.started";
    pub const TOOL_FINISHED: &str = "tool.finished";
    pub const TOOL_OUTPUT_UPDATE: &str = "tool.output_update";
    pub const SUBAGENT_EVENT: &str = "subagent.event";
    pub const TODO_UPDATE: &str = "todo.update";
}

/// 桥接层错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// 该方法要求带 `id` 的请求，收到的却是通知。
    NotARequest,
    /// 该方法要求通知，收到的却是带 `id` 的请求。
    NotANotification,
    /// 方法名不在协议 v1 里。
    UnknownMethod(String),
    /// 方法名对，但负载字段不符合约定。
    InvalidParams(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARequest => write!(formatter, "该方法必须是带 id 的请求。"),
            Self::NotANotification => write!(formatter, "该方法必须是通知。"),
            Self::UnknownMethod(method) => write!(formatter, "未知方法：{method}"),
            Self::InvalidParams(detail) => write!(formatter, "负载字段不符：{detail}"),
        }
    }
}

impl std::error::Error for BridgeError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPayload {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePayload {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsagePayload {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStartedPayload {
    pub step: usize,
    pub call: ToolCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolEventPayload {
    pub call: ToolCall,
    pub result: ToolResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentEventPayload {
    pub name: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoUpdatePayload {
    pub todos: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnFinishedPayload {
    pub turn_id: String,
    pub final_text: String,
    pub reasoning: String,
    pub model_turns: usize,
    pub tool_calls: usize,
    pub paused: bool,
}

/// 内核发给宿主的notifications。
///
/// 每个变体对应 Python 侧 `loop.py` 的一个 `run_stream` 回调（`turn.finished` 对应其返回值），
/// 映射表见 `docs/protocol-v1.md`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HostEvent {
    /// 可见文本增量。
    Delta(TextPayload),
    /// 思考文本增量。
    ReasoningDelta(TextPayload),
    /// 状态提示。
    Status(MessagePayload),
    /// 重试状态提示。
    RetryStatus(MessagePayload),
    /// 协议等待（宿主可忽略，用于界面提示）。
    ProtocolWait,
    /// 已展示的半截流式输出需要撤销。
    StreamRollback,
    /// 本回合最近一次模型请求的用量。
    TokenUsage(TokenUsagePayload),
    /// 某个工具调用开始。
    ToolStarted(ToolStartedPayload),
    /// 某个工具调用结束。
    ToolFinished(ToolEventPayload),
    /// 工具输出被压缩等操作改写后的更新。
    ToolOutputUpdate(ToolEventPayload),
    /// 子代理事件。
    SubagentEvent(SubagentEventPayload),
    /// 任务清单更新。
    TodoUpdate(TodoUpdatePayload),
    /// 回合结束。
    TurnFinished(TurnFinishedPayload),
}

impl HostEvent {
    /// 协议 v1 里内核可能发出的全部通知方法名。
    pub const METHODS: &'static [&'static str] = &[
        method::TURN_DELTA,
        method::TURN_REASONING_DELTA,
        method::TURN_STATUS,
        method::TURN_RETRY_STATUS,
        method::TURN_PROTOCOL_WAIT,
        method::TURN_STREAM_ROLLBACK,
        method::TURN_TOKEN_USAGE,
        method::TURN_FINISHED,
        method::TOOL_STARTED,
        method::TOOL_FINISHED,
        method::TOOL_OUTPUT_UPDATE,
        method::SUBAGENT_EVENT,
        method::TODO_UPDATE,
    ];

    pub fn method(&self) -> &'static str {
        match self {
            Self::Delta(_) => method::TURN_DELTA,
            Self::ReasoningDelta(_) => method::TURN_REASONING_DELTA,
            Self::Status(_) => method::TURN_STATUS,
            Self::RetryStatus(_) => method::TURN_RETRY_STATUS,
            Self::ProtocolWait => method::TURN_PROTOCOL_WAIT,
            Self::StreamRollback => method::TURN_STREAM_ROLLBACK,
            Self::TokenUsage(_) => method::TURN_TOKEN_USAGE,
            Self::ToolStarted(_) => method::TOOL_STARTED,
            Self::ToolFinished(_) => method::TOOL_FINISHED,
            Self::ToolOutputUpdate(_) => method::TOOL_OUTPUT_UPDATE,
            Self::SubagentEvent(_) => method::SUBAGENT_EVENT,
            Self::TodoUpdate(_) => method::TODO_UPDATE,
            Self::TurnFinished(_) => method::TURN_FINISHED,
        }
    }

    pub fn params(&self) -> Value {
        match self {
            Self::Delta(payload) | Self::ReasoningDelta(payload) => payload_value(payload),
            Self::Status(payload) | Self::RetryStatus(payload) => payload_value(payload),
            Self::ProtocolWait | Self::StreamRollback => json!({}),
            Self::TokenUsage(payload) => payload_value(payload),
            Self::ToolStarted(payload) => payload_value(payload),
            Self::ToolFinished(payload) | Self::ToolOutputUpdate(payload) => payload_value(payload),
            Self::SubagentEvent(payload) => payload_value(payload),
            Self::TodoUpdate(payload) => payload_value(payload),
            Self::TurnFinished(payload) => payload_value(payload),
        }
    }

    /// 组帧。
    pub fn to_frame(&self) -> Frame {
        Frame::notification(self.method(), self.params())
    }

    /// 解帧。
    pub fn from_frame(frame: &Frame) -> Result<Self, BridgeError> {
        if !frame.is_notification() {
            return Err(BridgeError::NotANotification);
        }
        let params = frame.params.clone().unwrap_or_else(|| json!({}));
        match frame.method().unwrap_or_default() {
            method::TURN_DELTA => Ok(Self::Delta(from_params(params)?)),
            method::TURN_REASONING_DELTA => Ok(Self::ReasoningDelta(from_params(params)?)),
            method::TURN_STATUS => Ok(Self::Status(from_params(params)?)),
            method::TURN_RETRY_STATUS => Ok(Self::RetryStatus(from_params(params)?)),
            method::TURN_PROTOCOL_WAIT => Ok(Self::ProtocolWait),
            method::TURN_STREAM_ROLLBACK => Ok(Self::StreamRollback),
            method::TURN_TOKEN_USAGE => Ok(Self::TokenUsage(from_params(params)?)),
            method::TOOL_STARTED => Ok(Self::ToolStarted(from_params(params)?)),
            method::TOOL_FINISHED => Ok(Self::ToolFinished(from_params(params)?)),
            method::TOOL_OUTPUT_UPDATE => Ok(Self::ToolOutputUpdate(from_params(params)?)),
            method::SUBAGENT_EVENT => Ok(Self::SubagentEvent(from_params(params)?)),
            method::TODO_UPDATE => Ok(Self::TodoUpdate(from_params(params)?)),
            method::TURN_FINISHED => Ok(Self::TurnFinished(from_params(params)?)),
            other => Err(BridgeError::UnknownMethod(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    /// 宿主自报的客户端信息，内核不解释，只用于日志与诊断。
    #[serde(default)]
    pub client: Value,
    /// 可选的模型配置：给了就由内核自己发模型请求，不给则退回 `model.reply` 代答。
    ///
    /// 装箱是因为它比同枚举里其他命令大一个量级，内联会让 Channel 每次投递都搬整块配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Box<KernelModelConfig>>,
    /// 可选的会话配置：给了就让内核自己持有会话（多轮历史、转录落盘与回合结束后的压缩）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Box<KernelSessionConfig>>,
}

/// 内核自己持有会话时需要的配置（与 Python 侧 `.agent_sessions` 同一套布局）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KernelSessionConfig {
    /// 会话目录。
    pub root: String,
    /// 续跑已有会话；空则由内核新建。
    #[serde(default)]
    pub session_id: String,
    /// 会话级记忆目录；空则不写记忆、不自动召回。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_root: Option<String>,
    /// 压缩策略；缺省用内核默认值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<KernelCompactionConfig>,
}

/// 压缩策略（对应 Python 侧 `config.context_compaction` 的关键字段）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct KernelCompactionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_turns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_summary_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_user_reserve_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_context_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emergency_context_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_exact_evidence: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_compacted_events: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_memory_recall: Option<bool>,
}

/// 宿主交给内核的模型配置，内核据此自己发起 Chat Completions 请求。
///
/// 凭据不进帧：这里只给环境变量名，内核在发请求时读环境。
/// 这样帧、日志与诊断输出里都不会出现 Key，Python 侧的 `api_key_env` 也是同一套约定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KernelModelConfig {
    pub model: String,
    /// Provider 名（`openai` / `anthropic` / `gemini`）；留空按 `openai` 处理，兼容旧宿主。
    #[serde(default)]
    pub provider: String,
    /// 协议名（如 `openai_responses` / `anthropic_messages` / `gemini_generate_content`）；
    /// 留空时用 Provider 的默认协议。
    #[serde(default)]
    pub protocol: String,
    /// 空则用运行时的默认地址。
    #[serde(default)]
    pub base_url: String,
    /// 存放 API Key 的环境变量名。
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub user_agent: String,
    /// 系统提示词。请求改由内核组装后，这份文本必须由宿主交进来。
    #[serde(default)]
    pub system_prompt: String,
    /// 静态工具声明（OpenAI functions 形状）；工具仍由宿主执行，内核只负责声明。
    #[serde(default)]
    pub tools: Vec<Value>,
    /// 生成选项，`GenerationOptions` 的 JSON 形状；缺省用默认值。
    #[serde(default)]
    pub options: Value,
    /// 单次请求的超时秒数；缺省或非正数时用运行时默认。
    #[serde(default)]
    pub request_timeout_seconds: Option<f64>,
    /// 模型声明的上下文窗口；正数时参与能力合并。
    #[serde(default)]
    pub context_window_tokens: i64,
    #[serde(default)]
    pub prompt_cache_capable: bool,
    #[serde(default)]
    pub prompt_cache_identity: BTreeMap<String, String>,
    /// 空响应与可重试错误的最大请求次数，默认 1（与 Python 侧主循环的默认一致）。
    #[serde(default = "default_request_retry_count")]
    pub request_retry_count: u32,
}

fn default_request_retry_count() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSubmitParams {
    pub turn_id: String,
    pub user_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCancelParams {
    pub turn_id: String,
}

/// 宿主发给内核的命令。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// 握手：声明版本。
    Initialize(InitializeParams),
    /// 提交一个回合的用户输入。
    TurnSubmit(TurnSubmitParams),
    /// 请求取消当前回合。
    TurnCancel(TurnCancelParams),
    /// 要求内核退出。
    Shutdown,
}

impl Command {
    pub const METHODS: &'static [&'static str] = &[
        method::INITIALIZE,
        method::TURN_SUBMIT,
        method::TURN_CANCEL,
        method::SHUTDOWN,
    ];

    pub fn method(&self) -> &'static str {
        match self {
            Self::Initialize(_) => method::INITIALIZE,
            Self::TurnSubmit(_) => method::TURN_SUBMIT,
            Self::TurnCancel(_) => method::TURN_CANCEL,
            Self::Shutdown => method::SHUTDOWN,
        }
    }

    pub fn params(&self) -> Value {
        match self {
            Self::Initialize(payload) => payload_value(payload),
            Self::TurnSubmit(payload) => payload_value(payload),
            Self::TurnCancel(payload) => payload_value(payload),
            Self::Shutdown => json!({}),
        }
    }

    pub fn to_frame(&self, id: Id) -> Frame {
        Frame::request(id, self.method(), self.params())
    }

    /// 解帧；返回 `id` 便于内核把响应关联回原始请求。
    pub fn from_frame(frame: &Frame) -> Result<PendingCommand, BridgeError> {
        if !frame.is_request() {
            return Err(BridgeError::NotARequest);
        }
        let id = frame.id().cloned().expect("is_request 已确认 id 存在");
        let params = frame.params.clone().unwrap_or_else(|| json!({}));
        let command = match frame.method().unwrap_or_default() {
            method::INITIALIZE => Command::Initialize(from_params(params)?),
            method::TURN_SUBMIT => Command::TurnSubmit(from_params(params)?),
            method::TURN_CANCEL => Command::TurnCancel(from_params(params)?),
            method::SHUTDOWN => Command::Shutdown,
            other => return Err(BridgeError::UnknownMethod(other.to_string())),
        };
        Ok(PendingCommand { id, command })
    }
}

/// 已解出的命令与constructible响应的 `id`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingCommand {
    pub id: Id,
    pub command: Command,
}

/// 内核请求宿主执行一整批工具调用。
///
/// 批次是不可分割的余为本：宿主必须先完成整批规范化与审批，再按模型调用顺序
/// 返回观察，内核不接受逐工具的回调。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolBatch {
    pub turn_id: String,
    pub step: usize,
    pub calls: Vec<ToolCall>,
}

impl ToolBatch {
    pub fn to_frame(&self, id: Id) -> Frame {
        Frame::request(id, method::TOOL_BATCH, payload_value(self))
    }

    pub fn from_frame(frame: &Frame) -> Result<Self, BridgeError> {
        if !frame.is_request() {
            return Err(BridgeError::NotARequest);
        }
        match frame.method() {
            Some(method::TOOL_BATCH) => {
                from_params(frame.params.clone().unwrap_or_else(|| json!({})))
            }
            Some(other) => Err(BridgeError::UnknownMethod(other.to_string())),
            None => Err(BridgeError::NotARequest),
        }
    }
}

/// 宿主对 `tool.batch` 的响应负载。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolBatchResult {
    pub observations: Vec<AgentLoopObservation>,
}

impl ToolBatchResult {
    pub fn to_result(&self) -> Value {
        payload_value(self)
    }

    pub fn from_result(result: &Value) -> Result<Self, BridgeError> {
        from_params(result.clone())
    }
}

/// 内核请求宿主代答一次模型回复。
///
/// 过渡形态：`omnicrawl-core` 的 `request_reply` 由宿主承担（Python 过渡宿主本来就把模型客户端放在宿主侧）。
/// 内核自带 provider runtime 后，这个方法不再被使用，协议方法本身保留。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub turn_id: String,
    pub messages: Vec<Value>,
}

impl ModelRequest {
    pub fn to_frame(&self, id: Id) -> Frame {
        Frame::request(id, method::MODEL_REPLY, payload_value(self))
    }

    pub fn from_frame(frame: &Frame) -> Result<Self, BridgeError> {
        if !frame.is_request() {
            return Err(BridgeError::NotARequest);
        }
        match frame.method() {
            Some(method::MODEL_REPLY) => {
                from_params(frame.params.clone().unwrap_or_else(|| json!({})))
            }
            Some(other) => Err(BridgeError::UnknownMethod(other.to_string())),
            None => Err(BridgeError::NotARequest),
        }
    }
}

/// `initialize` 的成功响应负载。
pub fn initialize_result() -> Value {
    json!({ "protocol_version": PROTOCOL_VERSION })
}

/// `initialize` 失败时的错误对象：宿主换一个主版本重试。
pub fn unsupported_version_error(host_version: &str) -> ErrorObject {
    ErrorObject::with_data(
        error_code::UNSUPPORTED_PROTOCOL_VERSION,
        format!("不支持宿主协议版本 {host_version}。"),
        json!({ "supported": PROTOCOL_VERSION, "host": host_version }),
    )
}

fn payload_value<T: Serialize>(payload: &T) -> Value {
    serde_json::to_value(payload).expect("负载是serde_json::Value字段，必须可序列化")
}

fn from_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, BridgeError> {
    serde_json::from_value(params).map_err(|error| BridgeError::InvalidParams(error.to_string()))
}
