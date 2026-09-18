//! 上下文压缩的会话与记忆编排（Python 侧 `controllers/turn/compaction.py` 的宿主壳）。
//!
//! 判定与文案在 `omnicrawl-controllers`，会话与记忆的读写在这里：回合结束边界的测量事件落盘、
//! 按阈值触发的压缩、被压缩窗口的事件归档、压缩结果的记忆回写与自动召回，以及压缩后的历史重建。
//!
//! 摘要模型调用经端口注入（内核侧实现见 `omnicrawl-cli`）。

pub mod adapter;
pub mod driver;

pub use adapter::{SummaryAdapterSettings, SummaryModelAdapter};
pub use driver::{AfterTurnReport, CompactionConfig, CompactionDriver, TurnBoundary};
