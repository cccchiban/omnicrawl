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
    pub const TURN_UNDO: &str = "turn.undo";
    /// 运行期更新内核对后续回合生效的设置（模型、工具声明、上下文与压缩阈值）。
    pub const SESSION_SETTINGS: &str = "session.settings";
    /// 显式压缩当前会话：不做阈值判定，直接请求一次模型摘要。
    pub const SESSION_COMPACT: &str = "session.compact";
    /// 查询／取消内核持有的后台 SubAgent 任务（不做任务创建与分发）。
    pub const SUBAGENT_QUERY: &str = "subagent.query";
    /// 列出当前工作区最近的会话（`archived=false`）或已归档会话（`archived=true`）。
    pub const SESSION_LIST: &str = "session.list";
    /// 重命名当前会话。
    pub const SESSION_RENAME: &str = "session.rename";
    /// 归档当前会话，并立即开启一个新的空会话。
    pub const SESSION_ARCHIVE: &str = "session.archive";
    /// 查询用户提示历史（只读展示，不注入模型上下文）。
    pub const SESSION_HISTORY: &str = "session.history";
    /// 读取当前会话的有效事件流（只读回放，供宿主重建对话视图）。
    pub const SESSION_EVENTS: &str = "session.events";
    /// 清空当前对话并开启新会话。
    pub const SESSION_NEW: &str = "session.new";
    /// 恢复指定会话：内核切到该会话并用转录重建运行期历史。
    pub const SESSION_RESUME: &str = "session.resume";
    /// 派生一个子 Agent 任务并等它结束，返回子 Agent 的原始输出文本。
    pub const SUBAGENT_RUN: &str = "subagent.run";
    /// 把宿主产生的文本作为 assistant 消息注入内核会话历史（下一轮请求可见）。
    pub const SESSION_APPEND: &str = "session.append";
    /// 运行中切换工作区：内核把自持会话的工作区切到新根并转录 `workspace_switched`。
    pub const WORKSPACE_SWITCH: &str = "workspace.switch";
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
    /// 回合收尾触发了上下文压缩时的计量通知（宿主据此分发 `context.compaction.after_turn`）。
    pub const TURN_CONTEXT_COMPACTION: &str = "turn.context_compaction";
    /// 一次模型请求成功返回（宿主据此分发 `model.response.after`）。
    pub const TURN_MODEL_RESPONSE_AFTER: &str = "turn.model_response_after";
    /// 一次模型请求以协议错误终结（宿主据此分发 `model.request.error`）。
    pub const TURN_MODEL_REQUEST_ERROR: &str = "turn.model_request_error";
    pub const TURN_TOOL_CALL_STARTED: &str = "turn.tool_call_started";
    pub const TURN_TOOL_CALL_ARGUMENTS: &str = "turn.tool_call_arguments";
    pub const TURN_TOOL_OUTPUT_COMPRESSION: &str = "turn.tool_output_compression";

    /// 内核在发出模型请求前请宿主运行 `model.request.before` 插件 Hook。
    ///
    /// 只在宿主于 `initialize` 声明 `plugin_model_hooks` 时使用；未声明的宿主永远收不到。
    pub const MODEL_HOOK: &str = "model.hook";
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

/// `turn.token_usage` 的载荷：与 Python 的用量字段同为有符号整数（负值原样透传）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsagePayload {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
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

/// 模型开始吐一个工具调用（尚未执行）。
///
/// 与执行期的 `tool.started` 不同：这条发生在**模型还在写参数**的时候，宿主据此先把卡片
/// 立起来，后续用 [`ToolCallArgumentsPayload`] 逐段补参数，做到「调用随 API 流一起长大」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallStartedPayload {
    pub call_id: String,
    pub tool: String,
}

/// 工具调用参数的增量（模型流里的 arguments 分片，可能是半截 JSON）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallArgumentsPayload {
    pub call_id: String,
    pub delta: String,
}

/// 工具输出压缩的阶段通知：`started` 表示正在压缩，`finished` 带上压缩前后的字符数。
///
/// 字符数用 `usize`：与内核 `compression` 旁路里 `chars().count()` 同口径（字符，不是字节）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutputCompressionPayload {
    pub call_id: String,
    pub tool: String,
    /// `started` / `finished`。
    pub phase: String,
    #[serde(default)]
    pub before_chars: usize,
    #[serde(default)]
    pub after_chars: usize,
    /// 压缩后的正文（只有 `phase = finished` 才带）：宿主用它替换卡片里的原始输出。
    #[serde(default)]
    pub output: String,
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

/// 回合收尾的上下文压缩计量：对应 Python `_trigger_context_compaction_after_turn`
/// 在 `snapshot.trigger_reached` 时发给插件的载荷。只在真正触发压缩的回合发出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextCompactionPayload {
    /// 压缩前测得的回合结束后实际上下文 Token。
    pub post_turn_context_tokens: i64,
    /// 触发压缩的阈值 Token。
    pub trigger_context_tokens: i64,
    /// 触发压缩的回合 id；缺省为空串。
    #[serde(default)]
    pub turn_id: String,
}

/// 一次模型请求成功返回的通知：对应 Python `model.response.after` 的载荷。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponseAfterPayload {
    pub model: String,
    pub content: String,
    pub tool_call_count: usize,
}

/// 一次模型请求以协议错误终结的通知：对应 Python `model.request.error` 的载荷。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequestErrorPayload {
    pub error: String,
    pub model: String,
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
    /// 回合收尾触发了上下文压缩（携带计量，宿主据此分发插件 Hook）。
    ContextCompaction(ContextCompactionPayload),
    /// 一次模型请求成功返回（宿主据此分发 `model.response.after`）。
    ModelResponseAfter(ModelResponseAfterPayload),
    /// 一次模型请求以协议错误终结（宿主据此分发 `model.request.error`）。
    ModelRequestError(ModelRequestErrorPayload),
    /// 模型开始吐一个工具调用（参数还在流里）。
    ToolCallStarted(ToolCallStartedPayload),
    /// 工具调用参数的增量。
    ToolCallArguments(ToolCallArgumentsPayload),
    /// 工具输出压缩的阶段通知。
    ToolOutputCompression(ToolOutputCompressionPayload),
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
        method::TURN_CONTEXT_COMPACTION,
        method::TURN_MODEL_RESPONSE_AFTER,
        method::TURN_MODEL_REQUEST_ERROR,
        method::TURN_TOOL_CALL_STARTED,
        method::TURN_TOOL_CALL_ARGUMENTS,
        method::TURN_TOOL_OUTPUT_COMPRESSION,
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
            Self::ContextCompaction(_) => method::TURN_CONTEXT_COMPACTION,
            Self::ModelResponseAfter(_) => method::TURN_MODEL_RESPONSE_AFTER,
            Self::ModelRequestError(_) => method::TURN_MODEL_REQUEST_ERROR,
            Self::ToolCallStarted(_) => method::TURN_TOOL_CALL_STARTED,
            Self::ToolCallArguments(_) => method::TURN_TOOL_CALL_ARGUMENTS,
            Self::ToolOutputCompression(_) => method::TURN_TOOL_OUTPUT_COMPRESSION,
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
            Self::ContextCompaction(payload) => payload_value(payload),
            Self::ModelResponseAfter(payload) => payload_value(payload),
            Self::ModelRequestError(payload) => payload_value(payload),
            Self::ToolCallStarted(payload) => payload_value(payload),
            Self::ToolCallArguments(payload) => payload_value(payload),
            Self::ToolOutputCompression(payload) => payload_value(payload),
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
            method::TURN_CONTEXT_COMPACTION => Ok(Self::ContextCompaction(from_params(params)?)),
            method::TURN_MODEL_RESPONSE_AFTER => Ok(Self::ModelResponseAfter(from_params(params)?)),
            method::TURN_MODEL_REQUEST_ERROR => Ok(Self::ModelRequestError(from_params(params)?)),
            method::TURN_TOOL_CALL_STARTED => Ok(Self::ToolCallStarted(from_params(params)?)),
            method::TURN_TOOL_CALL_ARGUMENTS => Ok(Self::ToolCallArguments(from_params(params)?)),
            method::TURN_TOOL_OUTPUT_COMPRESSION => {
                Ok(Self::ToolOutputCompression(from_params(params)?))
            }
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
    /// 宿主是否支持插件模型 Hook（内核据此决定是否发 `model.hook` 请求）。
    ///
    /// 缺省 false：未声明的宿主（如只实现协议最小集的兼容宿主）永远收不到该请求，
    /// 内核也就不会因等不到响应而阻住回合。false 时不序列化该字段，保持与旧宿主的
    /// 帧形状逐字一致。
    #[serde(default, skip_serializing_if = "is_false")]
    pub plugin_model_hooks: bool,
}

/// `skip_serializing_if` 辅助：false 时省掉该字段。
fn is_false(value: &bool) -> bool {
    !*value
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
    /// 宿主工作区根：内核据此在回合内拍工作区快照，供 `/undo` 回滚副作用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
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

/// 宿主在运行期更新内核持有的设置（协议 v1 `session.settings`）。
///
/// 只覆盖给出的字段，其余保持原值；任一字段非法时整体不生效（原子），
/// 错误响应的 `data.kind` 给出可判定原因（见 `docs/protocol-v1.md`）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionSettingsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Box<SessionModelSettings>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<KernelCompactionConfig>,
}

/// `session.settings.model` 的可改字段：`None` 表示不动该字段。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 生成选项（`GenerationOptions` 的 JSON 形状），整体替换。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    /// 推理强度：只写进生成选项的 `reasoning_effort` 字段，不整体替换 `options`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// system prompt，整体替换（模式切换会改写它）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// system 之外的上下文消息，整体替换（Skill 重扫、工作区切换后用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_messages: Option<Vec<Value>>,
    /// 静态工具声明，整体替换；工具仍由宿主执行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<i64>,
    // ---- 渠道字段：切换模型渠道时一并下发，空串视作不改 ----
    /// Provider 名（`openai` / `anthropic` / `gemini`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 协议名（如 `openai_responses`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// 渠道的基地址；空则沿用运行时默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 存放 API Key 的环境变量名（凭据本身不进帧）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
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
    /// system 之外的上下文消息（项目规范、Skill 索引、工具能力说明、运行环境）。
    ///
    /// 这些消息由宿主按「稳定 → 动态」组装好整段交进来，内核每轮把它们原样插在历史之前；
    /// 空数组表示旧宿主不给（行为与迁移前一致）。
    #[serde(default)]
    pub context_messages: Vec<Value>,
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
    /// 主模型是否用原生视觉。为真时内核**不**启用独立视觉模型代理——带图观察直送主模型；
    /// 为假时交给 `[vision].models` 代理；两者都不可用时内核把图片观察摄掉。
    ///
    /// 优先级与 Python `route_image_result` 一致（原生视觉优先于代理）；旧宿主不给这个字段
    /// 时按假处理，行为是「图片走代理」，与迁移前的默认一致。
    #[serde(default)]
    pub native_vision: bool,
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

/// `session.list` 负载：`archived` 为真时只看归档，`limit` 由内核收敛到 1..=100。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListParams {
    #[serde(default)]
    pub archived: bool,
    #[serde(default = "default_session_list_limit")]
    pub limit: u32,
}

fn default_session_list_limit() -> u32 {
    10
}

/// `session.rename` 负载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRenameParams {
    pub title: String,
}

/// `session.history` 负载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryParams {
    #[serde(default)]
    pub query: String,
    #[serde(default = "default_history_limit")]
    pub limit: u32,
}

fn default_history_limit() -> u32 {
    20
}

/// `session.resume` 负载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeParams {
    pub session_id: String,
}

/// `subagent.run` 负载：派生单个子 Agent 并等它跑完（`/review` 这类命令用）。
///
/// 与 `subagent` 工具共用同一套定义发现与工具白名单：`agent_type` 取定义目录里注册的
/// 名字（如 `review`），`prompt` 是子 Agent 的任务指令。工具批次照旧回到宿主执行，
/// 因此宿主必须保持事件循环可响应（不能同步等待本方法的响应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRunParams {
    pub agent_type: String,
    #[serde(default)]
    pub description: String,
    pub prompt: String,
}

/// `session.append` 负载：把宿主产生的文本注入内核会话历史。
///
/// 只允许 `role = "assistant"`：宿主注入的是**自己产生的**说明或报告文本（如 `/review`
/// 的评审报告），不借这个入口伪造用户输入或工具结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAppendParams {
    #[serde(default = "default_append_role")]
    pub role: String,
    pub content: String,
}

fn default_append_role() -> String {
    "assistant".to_string()
}

/// `workspace.switch` 的负载：宿主已解析并校验过的新工作区绝对路径。
///
/// 内核只负责会话侧的一致性：把自持会话的工作区指向新根，并按 Python
/// `_append_session_event("workspace_switched", ...)` 的口径转录 `{from, to}`。
/// 工具表、MCP、临时目录等宿主侧资源由宿主自己重建，协议不传。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSwitchParams {
    pub path: String,
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
    /// 撤销最近一轮：恢复工作区副作用并回退会话逻辑。
    TurnUndo,
    /// 运行期更新设置（模型、工具声明、上下文与压缩阈值）。
    ///
    /// 装箱是因为它（含工具声明）比同枚举里其他命令大一个量级。
    SessionSettings(Box<SessionSettingsParams>),
    /// 显式压缩当前会话（无参数）。
    SessionCompact,
    /// 查询／取消后台 SubAgent 任务：`action` 取 `list` / `get` / `cancel`。
    SubagentQuery(SubagentQueryParams),
    /// 列出最近会话或已归档会话。
    SessionList(SessionListParams),
    /// 重命名当前会话。
    SessionRename(SessionRenameParams),
    /// 归档当前会话并开启新会话。
    SessionArchive,
    /// 查询用户提示历史。
    SessionHistory(SessionHistoryParams),
    /// 读取当前会话的有效事件流（回退投影后的视图，与 UI 回放同源）。
    SessionEvents,
    /// 清空当前对话并开启新会话。
    SessionNew,
    /// 恢复指定会话。
    SessionResume(SessionResumeParams),
    /// 派生单个子 Agent 并等它结束。
    SubagentRun(SubagentRunParams),
    /// 把宿主产生的 assistant 文本追加到内核会话历史。
    SessionAppend(SessionAppendParams),
    /// 运行中切换工作区：更新自持会话的工作区并转录 `workspace_switched`。
    WorkspaceSwitch(WorkspaceSwitchParams),
    /// 要求内核退出。
    Shutdown,
}

/// `subagent.query` 的负载：动作与可选任务 ID（空串表示不指定）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentQueryParams {
    pub action: String,
    #[serde(default)]
    pub task_id: String,
}

impl Command {
    pub const METHODS: &'static [&'static str] = &[
        method::INITIALIZE,
        method::TURN_SUBMIT,
        method::TURN_CANCEL,
        method::TURN_UNDO,
        method::SESSION_SETTINGS,
        method::SESSION_COMPACT,
        method::SUBAGENT_QUERY,
        method::SESSION_LIST,
        method::SESSION_RENAME,
        method::SESSION_ARCHIVE,
        method::SESSION_HISTORY,
        method::SESSION_EVENTS,
        method::SESSION_NEW,
        method::SESSION_RESUME,
        method::SUBAGENT_RUN,
        method::SESSION_APPEND,
        method::WORKSPACE_SWITCH,
        method::SHUTDOWN,
    ];

    pub fn method(&self) -> &'static str {
        match self {
            Self::Initialize(_) => method::INITIALIZE,
            Self::TurnSubmit(_) => method::TURN_SUBMIT,
            Self::TurnCancel(_) => method::TURN_CANCEL,
            Self::TurnUndo => method::TURN_UNDO,
            Self::SessionSettings(_) => method::SESSION_SETTINGS,
            Self::SessionCompact => method::SESSION_COMPACT,
            Self::SubagentQuery(_) => method::SUBAGENT_QUERY,
            Self::SessionList(_) => method::SESSION_LIST,
            Self::SessionRename(_) => method::SESSION_RENAME,
            Self::SessionArchive => method::SESSION_ARCHIVE,
            Self::SessionHistory(_) => method::SESSION_HISTORY,
            Self::SessionEvents => method::SESSION_EVENTS,
            Self::SessionNew => method::SESSION_NEW,
            Self::SessionResume(_) => method::SESSION_RESUME,
            Self::SubagentRun(_) => method::SUBAGENT_RUN,
            Self::SessionAppend(_) => method::SESSION_APPEND,
            Self::WorkspaceSwitch(_) => method::WORKSPACE_SWITCH,
            Self::Shutdown => method::SHUTDOWN,
        }
    }

    pub fn params(&self) -> Value {
        match self {
            Self::Initialize(payload) => payload_value(payload),
            Self::TurnSubmit(payload) => payload_value(payload),
            Self::TurnCancel(payload) => payload_value(payload),
            Self::TurnUndo => json!({}),
            Self::SessionSettings(payload) => payload_value(payload),
            Self::SessionCompact => json!({}),
            Self::SubagentQuery(payload) => payload_value(payload),
            Self::SessionList(payload) => payload_value(payload),
            Self::SessionRename(payload) => payload_value(payload),
            Self::SessionArchive => json!({}),
            Self::SessionHistory(payload) => payload_value(payload),
            Self::SessionEvents => json!({}),
            Self::SessionNew => json!({}),
            Self::SessionResume(payload) => payload_value(payload),
            Self::SubagentRun(payload) => payload_value(payload),
            Self::SessionAppend(payload) => payload_value(payload),
            Self::WorkspaceSwitch(payload) => payload_value(payload),
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
            method::TURN_UNDO => Command::TurnUndo,
            method::SESSION_SETTINGS => Command::SessionSettings(from_params(params)?),
            method::SESSION_COMPACT => Command::SessionCompact,
            method::SUBAGENT_QUERY => Command::SubagentQuery(from_params(params)?),
            method::SESSION_LIST => Command::SessionList(from_params(params)?),
            method::SESSION_RENAME => Command::SessionRename(from_params(params)?),
            method::SESSION_ARCHIVE => Command::SessionArchive,
            method::SESSION_HISTORY => Command::SessionHistory(from_params(params)?),
            method::SESSION_EVENTS => Command::SessionEvents,
            method::SESSION_NEW => Command::SessionNew,
            method::SESSION_RESUME => Command::SessionResume(from_params(params)?),
            method::SUBAGENT_RUN => Command::SubagentRun(from_params(params)?),
            method::SESSION_APPEND => Command::SessionAppend(from_params(params)?),
            method::WORKSPACE_SWITCH => Command::WorkspaceSwitch(from_params(params)?),
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
    /// 隔离根：子任务的工具批次要在这个目录下执行（`None` 表示用宿主自己的工作区）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
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

/// 宿主拒绝执行工具调用时回填的 `result.error_code`。
///
/// 拒绝事实只走观察（`tool.batch` 响应），不额外加协议方法：内核据此把拒绝落成
/// `tool_call_denied` 会话事件，因此两侧取值必须同源。
pub const DENIED_ERROR_CODE: &str = "denied";

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

/// 内核在发出模型请求前，请宿主运行 `model.request.before` 插件 Hook。
///
/// 载荷与 Python 同形（`{messages, model}`）；仅当宿主在 `initialize` 声明
/// `plugin_model_hooks` 时才发出。宿主拒绝时以错误响应回填插件的拒绝文案。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelHookRequest {
    pub messages: Vec<Value>,
    pub model: String,
}

impl ModelHookRequest {
    pub fn to_frame(&self, id: Id) -> Frame {
        Frame::request(id, method::MODEL_HOOK, payload_value(self))
    }

    pub fn from_frame(frame: &Frame) -> Result<Self, BridgeError> {
        if !frame.is_request() {
            return Err(BridgeError::NotARequest);
        }
        match frame.method() {
            Some(method::MODEL_HOOK) => {
                from_params(frame.params.clone().unwrap_or_else(|| json!({})))
            }
            Some(other) => Err(BridgeError::UnknownMethod(other.to_string())),
            None => Err(BridgeError::NotARequest),
        }
    }
}

/// 宿主对 `model.hook` 的响应：Hook 放行后的最终消息集（可被插件改写）。
///
/// 插件拒绝时不走这里：宿主以错误响应回填 `HookDecision` 的拒绝文案，内核据此中止本轮。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelHookResult {
    pub messages: Vec<Value>,
}

impl ModelHookResult {
    pub fn to_result(&self) -> Value {
        payload_value(self)
    }

    pub fn from_result(result: &Value) -> Result<Self, BridgeError> {
        from_params(result.clone())
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
