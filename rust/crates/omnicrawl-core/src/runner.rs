//! Agent 模型循环执行器：语义基准 Python `omnicrawl/agent/runtime/execution.py`。
//!
//! 循环只做状态转移，不下探具体工具；同一次模型回复里的全部工具调用一次性交给
//! `ToolBatchHost`，由 Host 完成整批规范化、审批与调度，并按模型调用顺序返回观察。

use std::time::Instant;

use serde_json::Value;

use crate::types::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopResult, AgentModelReply, LoopError, ToolCall,
};

/// 单调时钟：Python 侧直接用 `time.monotonic()`，Rust 侧注入以便确定性测试。
pub trait Clock {
    fn now(&self) -> f64;
}

/// 系统单调时钟。
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

/// 模型回复来源：给出一次回复，失败时错误原样传播。
pub trait ReplySource {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError>;
}

/// 工具批次宿主：整批执行一次模型回复里的全部调用，按模型调用顺序返回观察。
///
/// 刻意不提供逐工具回调，防止 Host 在审批尚未完成时提前执行同批中的某个工具。
pub trait ToolBatchHost {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError>;
}

/// 循环边界回调：取消检查与停止检查。
pub struct LoopGuards<'a> {
    pub cancel_check: Option<&'a mut dyn FnMut() -> Result<(), LoopError>>,
    pub stop_check: Option<&'a mut dyn FnMut() -> bool>,
}

/// 执行独立且可取消、可设预算的 Agent 模型循环。
pub struct AgentLoopRunner {
    clock: Box<dyn Clock>,
}

impl AgentLoopRunner {
    pub fn new(clock: Box<dyn Clock>) -> Self {
        Self { clock }
    }

    /// 跑一轮「模型 ⇄ 工具」协议循环，直到模型不再请求工具或触发停止检查。
    pub fn run(
        &self,
        messages: &mut Vec<Value>,
        reply_source: &mut dyn ReplySource,
        tool_batch: &mut dyn ToolBatchHost,
        limits: AgentLoopLimits,
        guards: LoopGuards<'_>,
    ) -> Result<AgentLoopResult, LoopError> {
        let LoopGuards {
            mut cancel_check,
            mut stop_check,
        } = guards;
        let started_at = self.clock.now();
        let mut model_turns = 0usize;
        let mut tool_calls = 0usize;
        let mut reasoning_parts: Vec<String> = Vec::new();

        loop {
            check_boundary(&mut cancel_check, &limits, started_at, self.clock.as_ref())?;
            if let Some(max_model_turns) = limits.max_model_turns {
                if model_turns >= max_model_turns {
                    return Err(LoopError::BudgetExceeded(format!(
                        "Agent Loop 已达到模型回合预算 {}。",
                        max_model_turns
                    )));
                }
            }

            let reply = reply_source.request_reply(messages)?;
            model_turns += 1;
            if !reply.reasoning.is_empty() {
                reasoning_parts.push(reply.reasoning.clone());
            }

            if reply.tool_calls.is_empty() {
                return Ok(AgentLoopResult {
                    final_text: reply.content.trim().to_string(),
                    reasoning: reasoning_parts.join("\n"),
                    content_streamed: reply.content_streamed,
                    model_turns,
                    tool_calls,
                    last_reply: Some(reply),
                    paused: false,
                });
            }

            let next_tool_count = tool_calls + reply.tool_calls.len();
            if let Some(max_tool_calls) = limits.max_tool_calls {
                if next_tool_count > max_tool_calls {
                    return Err(LoopError::BudgetExceeded(format!(
                        "Agent Loop 工具调用预算为 {}，当前批次将使调用数达到 {}。",
                        max_tool_calls, next_tool_count
                    )));
                }
            }

            // assistant tool-call 消息必须与其对应的工具观察一起进入上下文。
            messages.push(reply.message.clone());
            let observations = tool_batch.execute_tool_batch(&reply.tool_calls, tool_calls + 1)?;
            if observations.len() != reply.tool_calls.len() {
                return Err(LoopError::ObservationMismatch(format!(
                    "工具批次观察数量与模型调用数量不一致：期望 {}，实际 {}。",
                    reply.tool_calls.len(),
                    observations.len()
                )));
            }
            // 全部 tool result 必须紧跟同一条 assistant tool_calls 消息：先回填整批，
            // 再追加各工具的补充观察，否则 Provider 会判定协议无效。
            for observation in &observations {
                messages.push(observation.message.clone());
            }
            for observation in &observations {
                for followup in &observation.followup_messages {
                    messages.push(followup.clone());
                }
            }
            tool_calls = next_tool_count;
            check_boundary(&mut cancel_check, &limits, started_at, self.clock.as_ref())?;
            if let Some(stop_check) = stop_check.as_mut() {
                if stop_check() {
                    return Ok(AgentLoopResult {
                        final_text: String::new(),
                        reasoning: reasoning_parts.join("\n"),
                        content_streamed: false,
                        model_turns,
                        tool_calls,
                        last_reply: None,
                        paused: true,
                    });
                }
            }
        }
    }
}

/// 取消检查总是执行；时间预算只在其被设置时读取时钟，与 Python 侧一致。
fn check_boundary(
    cancel_check: &mut Option<&mut dyn FnMut() -> Result<(), LoopError>>,
    limits: &AgentLoopLimits,
    started_at: f64,
    clock: &dyn Clock,
) -> Result<(), LoopError> {
    if let Some(cancel_check) = cancel_check.as_mut() {
        cancel_check()?;
    }
    if let Some(timeout_seconds) = limits.timeout_seconds {
        if clock.now() - started_at >= timeout_seconds {
            return Err(LoopError::BudgetExceeded(format!(
                "Agent Loop 已超过时间预算 {} 秒。",
                format_seconds(timeout_seconds)
            )));
        }
    }
    Ok(())
}

/// 复刻 Python `{value:g}` 的整数秒写法；`%g` 在极大/极小量级会切换指数写法，
/// 本内核只承诺整数秒与常规小数的文本一致。
fn format_seconds(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e6 {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    }
}
