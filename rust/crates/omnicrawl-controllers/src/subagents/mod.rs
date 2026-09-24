//! `omnicrawl/agent/subagents/` 的移植（按 Python 包结构分模块）。
//!
//! 已搬：定义解析与来源发现、worktree 的判定面、orchestration 的判定/投影面、
//! 只读命令策略，以及**任务生命周期本体**（`tasks.rs` 的 `SubAgentTaskManager`：
//! 提交/查询/取消、回收恢复、保留期清理）。
//! 留在宿主：模型运行时引导与线程池（`omnicrawl-cli/src/subagent.rs`）、
//! worktree 的 git 操作（`omnicrawl-cli/src/worktree.rs`）、Session 落盘与事件观察者转发。

pub mod batch;
pub mod coordinator;
pub mod definitions;
pub mod execution;
pub mod orchestration;
pub mod read_only;
pub mod recovery;
pub mod tasks;
pub mod verify;
pub mod worktrees;

pub use batch::*;
pub use coordinator::*;
pub use definitions::*;
pub use execution::*;
pub use orchestration::*;
pub use read_only::*;
pub use recovery::*;
pub use tasks::*;
pub use verify::*;
pub use worktrees::*;
