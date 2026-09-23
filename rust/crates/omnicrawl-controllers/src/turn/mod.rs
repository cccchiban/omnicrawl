//! `omnicrawl/agent/controllers/turn/` 的移植（按 Python 包结构分模块）。
//! 已搬：`loop.py` 的接线面（13 回调面、两个循环端口与守卫、收尾与失败判定）、
//! `loop.py` 的 run_guard 续跑判定面（理由、上限、终态与事件载荷）、
//! `compaction.py` 的判定面（通知文案、记忆写请求、自动召回、归档选取）。
//! 另含 `agent/context/environment.py` 的移植（[`environment`]）：注入给模型的
//! 运行环境摘要与窗口/终端探测。
//! 未搬：`loop.py` 的编排壳（插件钩子、会话事件、回合快照、压缩触发、上下文超限恢复）
//! 与 `compaction.py` 的会话/模型编排——它们需要会话设施、模型调用与其它子系统。
//! 续跑判定面由内核回合循环（`omnicrawl-cli`）驱动，模型调用与事件落盘留在那里。

pub mod compaction;
pub mod context_messages;
pub mod continuation;
pub mod environment;
pub mod prompt_context;
pub mod skill_command;
pub mod tool_events;
pub mod turn_loop;
pub mod turn_text;

pub use compaction::*;
pub use context_messages::*;
pub use continuation::*;
pub use environment::*;
pub use prompt_context::*;
pub use skill_command::*;
pub use tool_events::*;
pub use turn_loop::*;
pub use turn_text::*;
