//! `omnicrawl/agent/controllers/subagents/` 的移植（按 Python 包结构分模块）。
//!
//! 已搬：`worktrees.py` 的判定面与 `orchestration.py` 的判定/投影面。
//! 未搬：Coordinator/TaskManager 生命周期、模型运行时引导、worktree 的 git 操作、
//! Session 落盘与事件观察者转发——它们需要宿主 I/O、线程/进程管理或会话设施。

pub mod orchestration;
pub mod worktrees;

pub use orchestration::*;
pub use worktrees::*;
