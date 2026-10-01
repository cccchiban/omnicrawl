//! `omnicrawl/agent/controllers/turn/loop.py` 的接线面：把宿主的模型请求与工具批次接到
//! `omnicrawl-core` 的回合循环，并把过程报告接到 `omnicrawl-ipc` 的宿主回调面。
//!
//! Python 把这些接线写在 `run_stream` 里：13 个回调形参、两个循环端口（`request_reply` /
//! `execute_tool_batch`）与两个守卫（`cancel_check` / `stop_check`），以及收尾时「最终回复
//! 是否需要补发」的判定。插件钩子、会话落盘、回合快照、压缩触发、run_guard 续跑与上下文
//! 超限恢复仍由宿主持有。

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Value};

use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopRunner, AgentModelReply, LoopError, LoopGuards,
    ReplySource, SystemClock, ToolBatchHost, ToolCall, ToolResult,
};
use omnicrawl_ipc::bridge::{
    HostEvent, MessagePayload, SubagentEventPayload, TextPayload, TodoUpdatePayload,
    TokenUsagePayload, ToolEventPayload, ToolStartedPayload, TurnFinishedPayload,
};

use crate::context_compaction::TokenUsageSample;
use crate::error::AgentError;

/// 13 回调面：与 Python `run_stream` 的回调形参一一对应。
///
/// `on_delta` 与 Python 一样是必需的（最终回复也经它输出）；其余回调缺省时静默丢弃，
/// 只有 `on_retry_status` 例外——Python 侧写的是 `on_retry_status or status`，因此缺省
/// 实现把重试提示交给 `on_status`。
pub trait TurnCallbacks {
    fn on_delta(&mut self, text: &str);

    fn on_status(&mut self, _message: &str) {}

    fn on_retry_status(&mut self, message: &str) {
        self.on_status(message);
    }

    fn on_protocol_wait(&mut self) {}

    fn on_stream_rollback(&mut self) {}

    fn on_token_usage(
        &mut self,
        _input_tokens: i64,
        _output_tokens: i64,
        _cached_input_tokens: i64,
    ) {
    }

    fn on_reasoning_delta(&mut self, _text: &str) {}

    fn on_tool_start(&mut self, _step: usize, _call: &ToolCall) {}

    fn on_tool_result(&mut self, _call: &ToolCall, _result: &ToolResult) {}

    fn on_tool_output_update(&mut self, _call: &ToolCall, _result: &ToolResult) {}

    fn on_subagent_event(&mut self, _name: &str, _payload: &Value) {}

    fn on_todo_update(&mut self, _todos: &Value) {}
}

/// 接线的报告句柄：宿主的原始模型请求与工具批次都通过它把过程交给回调面。
///
/// 与 Python 侧一致，用量取「本回合最近一次请求」与「累计」两份：最近一次输入 token 是
/// 请求输入的 `max(0, value)`，累计值把三个维度分别以 `max(0, value)` 后相加，而交给
/// `on_token_usage` 的仍是供应商给的原始值。
pub struct TurnReport<'a> {
    callbacks: &'a mut dyn TurnCallbacks,
    visible_output_seen: bool,
    tool_execution_seen: bool,
    turn_usage: TokenUsageSample,
    last_request_input_tokens: i64,
}

impl<'a> TurnReport<'a> {
    fn new(callbacks: &'a mut dyn TurnCallbacks) -> Self {
        Self {
            callbacks,
            visible_output_seen: false,
            tool_execution_seen: false,
            turn_usage: TokenUsageSample::default(),
            last_request_input_tokens: 0,
        }
    }

    /// 可见文本增量；非空增量记下「本回合已有可见输出」。
    pub fn delta(&mut self, text: &str) {
        if !text.is_empty() {
            self.visible_output_seen = true;
        }
        self.callbacks.on_delta(text);
    }

    pub fn status(&mut self, message: &str) {
        self.callbacks.on_status(message);
    }

    pub fn retry_status(&mut self, message: &str) {
        self.callbacks.on_retry_status(message);
    }

    pub fn protocol_wait(&mut self) {
        self.callbacks.on_protocol_wait();
    }

    pub fn stream_rollback(&mut self) {
        self.callbacks.on_stream_rollback();
    }

    pub fn reasoning_delta(&mut self, text: &str) {
        self.callbacks.on_reasoning_delta(text);
    }

    pub fn token_usage(&mut self, input_tokens: i64, output_tokens: i64, cached_input_tokens: i64) {
        self.last_request_input_tokens = input_tokens.max(0);
        self.turn_usage = self
            .turn_usage
            .add(input_tokens, output_tokens, cached_input_tokens);
        self.callbacks
            .on_token_usage(input_tokens, output_tokens, cached_input_tokens);
    }

    pub fn tool_start(&mut self, step: usize, call: &ToolCall) {
        self.callbacks.on_tool_start(step, call);
    }

    pub fn tool_result(&mut self, call: &ToolCall, result: &ToolResult) {
        self.callbacks.on_tool_result(call, result);
    }

    pub fn tool_output_update(&mut self, call: &ToolCall, result: &ToolResult) {
        self.callbacks.on_tool_output_update(call, result);
    }

    pub fn subagent_event(&mut self, name: &str, payload: &Value) {
        self.callbacks.on_subagent_event(name, payload);
    }

    pub fn todo_update(&mut self, todos: &Value) {
        self.callbacks.on_todo_update(todos);
    }

    /// 收尾补发：`on_delta` 直接输出最终回复，不改写「已有可见输出」的判定。
    fn flush_final_text(&mut self, text: &str) {
        self.callbacks.on_delta(text);
    }

    pub fn visible_output_seen(&self) -> bool {
        self.visible_output_seen
    }

    pub fn tool_execution_seen(&self) -> bool {
        self.tool_execution_seen
    }

    pub fn turn_usage(&self) -> TokenUsageSample {
        self.turn_usage
    }

    pub fn last_request_input_tokens(&self) -> i64 {
        self.last_request_input_tokens
    }

    fn mark_tool_execution(&mut self) {
        self.tool_execution_seen = true;
    }
}

/// 宿主端口：原始模型请求、整批工具执行与两个循环守卫。
///
/// 两个端口的实现都拿到 [`TurnReport`]：Python 侧 `_request_agent_reply` 与
/// `_execute_tool_batch` 也是这样接收接线传进来的报告回调的。工具批次仍是一次性的整批
/// 调用，接线不改变批次顺序与观察回填规则。
pub struct StreamPorts<'a> {
    pub request_reply: ReplyPort<'a>,
    pub execute_tool_batch: BatchPort<'a>,
    pub cancel_check: Option<&'a mut dyn FnMut() -> Result<(), LoopError>>,
    pub stop_check: Option<&'a mut dyn FnMut() -> bool>,
}

/// 宿主的模型请求端口：`messages` 是循环当前的完整上下文，报告句柄用于外发增量与用量。
pub type ReplyPort<'a> =
    &'a mut dyn FnMut(&mut Vec<Value>, &mut TurnReport<'a>) -> Result<AgentModelReply, LoopError>;

/// 宿主的工具批次端口：整批调用 + 起始步号（1 基），报告句柄用于外发工具生命周期。
pub type BatchPort<'a> = &'a mut dyn FnMut(
    &[ToolCall],
    usize,
    &mut TurnReport<'a>,
) -> Result<Vec<AgentLoopObservation>, LoopError>;

/// 一个回合接线完成后的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamOutcome {
    /// 最终回复：已按 Python 的收尾规则组装，`content_streamed` 为假时也已补发过。
    pub final_text: String,
    pub reasoning: String,
    /// 最终回复是否已经流式输出过。
    pub content_streamed: bool,
    pub model_turns: usize,
    pub tool_calls: usize,
    pub paused: bool,
    pub turn_usage: TokenUsageSample,
    pub last_request_input_tokens: i64,
}

impl StreamOutcome {
    /// `turn.finished` 通知：协议 v1 里这一步对应 `run_stream` 的返回值而不是某个回调。
    pub fn finished_event(&self, turn_id: &str) -> HostEvent {
        HostEvent::TurnFinished(TurnFinishedPayload {
            turn_id: turn_id.to_string(),
            final_text: self.final_text.clone(),
            reasoning: self.reasoning.clone(),
            model_turns: self.model_turns,
            tool_calls: self.tool_calls,
            paused: self.paused,
            // 压缩在回合收尾之后由内核自己的会话尾段完成，这里无从得知。
            post_compaction_context_tokens: None,
        })
    }
}

/// 接线失败。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnFailure {
    pub error: TurnError,
    /// 失败前是否已产生可见输出。
    pub visible_output_seen: bool,
    /// 失败前是否已执行过工具批次。
    pub tool_execution_seen: bool,
}

impl TurnFailure {
    /// 输入为空：与 Python 一样在回合开始前拦下，宿主要按普通错误处理、不落回合事件。
    pub fn is_input_error(&self) -> bool {
        matches!(self.error, TurnError::Input(_))
    }

    /// UI 主动取消：对应 Python `_is_turn_cancel_exception` 收敛到内核后的判定。
    pub fn is_cancelled(&self) -> bool {
        matches!(self.error, TurnError::Loop(LoopError::Cancelled(_)))
    }

    /// 与 `LoopError::tag()` 同口径的稳定标签；输入为空时是 `input`。
    pub fn tag(&self) -> &'static str {
        match &self.error {
            TurnError::Input(_) => "input",
            TurnError::Loop(error) => error.tag(),
        }
    }

    pub fn message(&self) -> &str {
        match &self.error {
            TurnError::Input(error) => error.message(),
            TurnError::Loop(error) => error.message(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnError {
    /// 用户输入为空：与 Python 一样在接线最前面拦下，宿主按普通错误处理。
    Input(AgentError),
    /// 循环失败：取消、预算、模型回复来源与工具批次都走这里，宿主用 `tag()` 分支。
    Loop(LoopError),
}

/// 把回调面接到 `omnicrawl-ipc` 的宿主事件上：一个发射器就够。
///
/// 这是协议 v1 的线上形态；`on_retry_status` 覆盖了缺省回落，重试提示发
/// `turn.retry_status`（宿主侧再决定它是否落到自己的 `on_status`）。用量字段在协议里是
/// 非负计数，负值按 0 发。
pub struct HostEventCallbacks<'a> {
    emit: &'a mut dyn FnMut(HostEvent),
}

impl<'a> HostEventCallbacks<'a> {
    pub fn new(emit: &'a mut dyn FnMut(HostEvent)) -> Self {
        Self { emit }
    }
}

impl TurnCallbacks for HostEventCallbacks<'_> {
    fn on_delta(&mut self, text: &str) {
        (self.emit)(HostEvent::Delta(TextPayload {
            text: text.to_string(),
        }));
    }

    fn on_status(&mut self, message: &str) {
        (self.emit)(HostEvent::Status(MessagePayload {
            message: message.to_string(),
        }));
    }

    fn on_retry_status(&mut self, message: &str) {
        (self.emit)(HostEvent::RetryStatus(MessagePayload {
            message: message.to_string(),
        }));
    }

    fn on_protocol_wait(&mut self) {
        (self.emit)(HostEvent::ProtocolWait);
    }

    fn on_stream_rollback(&mut self) {
        (self.emit)(HostEvent::StreamRollback);
    }

    fn on_token_usage(&mut self, input_tokens: i64, output_tokens: i64, cached_input_tokens: i64) {
        // 负值原样透传（Python 的用量字段是无符号语义之外的有符号 int；这里不归零）。
        (self.emit)(HostEvent::TokenUsage(TokenUsagePayload {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        }));
    }

    fn on_reasoning_delta(&mut self, text: &str) {
        (self.emit)(HostEvent::ReasoningDelta(TextPayload {
            text: text.to_string(),
        }));
    }

    fn on_tool_start(&mut self, step: usize, call: &ToolCall) {
        (self.emit)(HostEvent::ToolStarted(ToolStartedPayload {
            step,
            call: call.clone(),
        }));
    }

    fn on_tool_result(&mut self, call: &ToolCall, result: &ToolResult) {
        (self.emit)(HostEvent::ToolFinished(ToolEventPayload {
            call: call.clone(),
            result: result.clone(),
        }));
    }

    fn on_tool_output_update(&mut self, call: &ToolCall, result: &ToolResult) {
        (self.emit)(HostEvent::ToolOutputUpdate(ToolEventPayload {
            call: call.clone(),
            result: result.clone(),
        }));
    }

    fn on_subagent_event(&mut self, name: &str, payload: &Value) {
        (self.emit)(HostEvent::SubagentEvent(SubagentEventPayload {
            name: name.to_string(),
            payload: payload.clone(),
        }));
    }

    fn on_todo_update(&mut self, todos: &Value) {
        (self.emit)(HostEvent::TodoUpdate(TodoUpdatePayload {
            todos: todos.clone(),
        }));
    }
}

/// 跑一轮「模型 ⇄ 工具」：上下文 + 历史 + 用户输入进循环，最终回复按收尾规则输出。
///
/// 报告句柄在两个端口之间共享，因此这里用 `Rc<RefCell<_>>`——`omnicrawl-core` 的两个端口
/// 都要 `&mut` 借用同一个报告（Python 侧两者共用 `self`，等价）。这个共享只存在于接线
/// 内部，宿主看不到。
pub fn run_stream<'a>(
    context_messages: &[Value],
    history: &[Value],
    user_text: &str,
    limits: AgentLoopLimits,
    ports: StreamPorts<'a>,
    callbacks: &'a mut dyn TurnCallbacks,
) -> Result<StreamOutcome, TurnFailure> {
    let text = user_text.trim();
    if text.is_empty() {
        return Err(TurnFailure {
            error: TurnError::Input(AgentError::new("用户输入为空，无法发送给 Agent。")),
            visible_output_seen: false,
            tool_execution_seen: false,
        });
    }

    let mut messages: Vec<Value> = Vec::with_capacity(context_messages.len() + history.len() + 1);
    messages.extend_from_slice(context_messages);
    messages.extend_from_slice(history);
    messages.push(json!({"role": "user", "content": text}));

    let StreamPorts {
        request_reply,
        execute_tool_batch,
        mut cancel_check,
        stop_check,
    } = ports;
    // 与 Python 一样，进入循环前先查一次取消：用户提交后立刻要求停止时也要收得住，
    // 否则这一个检查点会落在第一次模型请求之后。
    if let Some(check) = cancel_check.as_mut() {
        if let Err(error) = check() {
            return Err(TurnFailure {
                error: TurnError::Loop(error),
                visible_output_seen: false,
                tool_execution_seen: false,
            });
        }
    }
    let report = Rc::new(RefCell::new(TurnReport::new(callbacks)));
    let mut reply_port = ReplyWiring {
        report: Rc::clone(&report),
        request_reply,
    };
    let mut batch_port = BatchWiring {
        report: Rc::clone(&report),
        execute_tool_batch,
    };
    let guards = LoopGuards {
        cancel_check,
        stop_check,
    };
    let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));

    match runner.run(
        &mut messages,
        &mut reply_port,
        &mut batch_port,
        limits,
        guards,
    ) {
        Ok(result) => {
            let mut report = report.borrow_mut();
            let final_text = result.final_text.trim().to_string();
            let final_content_streamed = !final_text.is_empty() && result.content_streamed;
            if !final_text.is_empty() && !final_content_streamed {
                report.flush_final_text(&final_text);
            }
            Ok(StreamOutcome {
                final_text,
                reasoning: result.reasoning,
                content_streamed: final_content_streamed,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                paused: result.paused,
                turn_usage: report.turn_usage(),
                last_request_input_tokens: report.last_request_input_tokens(),
            })
        }
        Err(error) => {
            let report = report.borrow();
            Err(TurnFailure {
                error: TurnError::Loop(error),
                visible_output_seen: report.visible_output_seen(),
                tool_execution_seen: report.tool_execution_seen(),
            })
        }
    }
}

struct ReplyWiring<'a> {
    report: Rc<RefCell<TurnReport<'a>>>,
    request_reply: ReplyPort<'a>,
}

impl ReplySource for ReplyWiring<'_> {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        let Self {
            report,
            request_reply,
        } = self;
        let mut report = report.borrow_mut();
        request_reply(messages, &mut report)
    }
}

struct BatchWiring<'a> {
    report: Rc<RefCell<TurnReport<'a>>>,
    execute_tool_batch: BatchPort<'a>,
}

impl ToolBatchHost for BatchWiring<'_> {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
        let Self {
            report,
            execute_tool_batch,
        } = self;
        let mut report = report.borrow_mut();
        report.mark_tool_execution();
        execute_tool_batch(calls, first_step, &mut report)
    }
}

// ------------------------------------------------------------------ 视觉能力

/// 运行时是否具备视觉能力：快照缺失、没有 runtime/capabilities 或未声明时一律为假。
pub fn model_supports_vision(vision_capability: Option<bool>) -> bool {
    vision_capability.unwrap_or(false)
}

/// 是否优先使用原生视觉：显式开关优先，未配置时按运行时能力。
///
/// 配置里写 `false` 是「明确关掉」，与「没配置」不是一回事——后者才回落到运行时能力。
pub fn native_vision_enabled(override_value: Option<bool>, supports_vision: bool) -> bool {
    match override_value {
        Some(value) => value,
        None => supports_vision,
    }
}
