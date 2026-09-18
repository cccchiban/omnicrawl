//! Agent 控制器域（`omnicrawl/agent/controllers/`）的 Rust 移植。
//!
//! Python 侧把这些逻辑写成挂在 `LocalToolAgent` 上的 Mixin：判定、校验、文案与预算
//! 计算和宿主副作用（git、线程池、文件系统、UI 回调）混在同一个方法里。本 crate 只收
//! 走前者，后者一律留给宿主，经参数或 trait 注入：
//!
//! - 需要 git 的地方走 [`undo::SnapshotStore`]，不在 crate 内起子进程；
//! - 需要并发/超时的地方用标准库线程，调用方负责取消与生命周期；
//! - 需要其它子系统的数据（会话事件、工具表、LLM 客户端）由调用方按字段传入。
//!
//! 与 Python 的对应关系、尚未搬的宿主粘合层见 `README.md`。

pub mod advisor;
pub mod approval;
pub mod building;
pub mod compression;
pub mod context_compaction;
pub mod control;
pub mod error;
pub mod json;
pub mod memory;
pub mod output;
pub mod plugins;
pub mod settings;
pub mod sha1;
pub mod shared;
pub mod tool_args;
pub mod tool_catalog;
pub mod types;
pub mod undo;
pub mod workspace;

pub use error::AgentError;
pub use types::{ToolCall, ToolImageAttachment, ToolResult};
