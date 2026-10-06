//! Provider 无关的 Agent 回合循环。
//!
//! 语义基准是 Python 侧 `omnicrawl/agent/runtime/execution.py`：只承载
//! 「模型回复 → 整批工具观察 → 下一次模型请求」的纯状态转移。用户输入、会话落盘、
//! 插件钩子、审批、并发调度与具体工具执行仍由 Host 拥有，本 crate 通过 trait 接收。
//!
//! 与 `omnicrawl-protocol` 的边界：该 crate 负责把 Provider 流事件归并成一条模型回复，
//! 本 crate 负责拿这条回复跑完整批工具循环；两者不共享类型。

mod runner;
mod types;

pub mod diagnostics;

pub use runner::{AgentLoopRunner, Clock, LoopGuards, ReplySource, SystemClock, ToolBatchHost};
pub use types::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopResult, AgentModelReply, LoopError, ToolCall,
    ToolResult,
};
