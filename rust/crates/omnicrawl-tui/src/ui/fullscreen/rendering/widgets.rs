//! 工具卡正文的缩略常量。
//!
//! 这里只留**真正被装配层使用**的部分：`ui/conversation.rs` 用这几个常量决定工具卡正文
//! 的缩略与展开行为。原先一并搬运的 `ToolDisclosure` / `AssistantMessage` /
//! `ReasoningDisclosure` / `ConfirmationScreen` / `RuntimeStatus` / `TodoPlan` 等组件状态机
//! 从未挂载到渲染路径（渲染由 `conversation.rs` 与 `panels.rs` 直接实现），已删除；
//! 其余零引用常量与辅助函数同样删除。

/// 工具展开正文的行数上限（不含标题行）。
pub const MAX_EXPANDED_BODY_LINES: usize = 5;
/// 正文被缩略时首部与尾部各保留的有效行数。
pub const HEAD_BODY_LINES: usize = 2;
pub const TAIL_BODY_LINES: usize = 2;
/// 方案6：正文相对标题的缩进宽度（4 空格）。
pub const BODY_INDENT: &str = "    ";
/// 省略区提示行文案：占位符是中间被省略的有效行数。
pub const EXPAND_HINT: &str = "点击展开 {lines} 行";
