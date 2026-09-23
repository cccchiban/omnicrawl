//! 斜杠命令框架与内置命令（`omnicrawl/commands/` 的 Rust 移植）。
//!
//! - [`framework`]：注册、解析、分发与列表派生，对映 `commands/framework.py`；
//! - [`agent`]：命令处理器依赖的[`CommandAgent`] 能力面与命令用到的值类型；
//! - [`slash`]：内置命令声明与处理器，对映 `commands/slash.py`。
//!
//! 依赖方向是单向的：本 crate 不依赖任何宿主（`omnicrawl-host` / `omnicrawl-tui` / 连接器 /
//! 本地 API）。需要宿主副作用（会话生命周期、子代理、MCP、Skill、工具表重建）的地方
//! 一律经 [`CommandAgent`] 注入；宿主缺这项能力时必须返回明确的 [`AgentError`]，
//! 不允许静默成功。配置读写（审批模式、推理强度、模型选择、顾问、工作区）直接用
//! `omnicrawl-config` 已搬好的写回函数，与 Python 侧 `slash.py` 直接调配置函数的结构一致。
//!
//! 与 Python 的两处必要差异、尚未接线的宿主入口见 `README.md`。

pub mod agent;
pub mod framework;
pub mod slash;

pub use agent::{CommandAgent, SessionSummary, SubAgentRun, SubagentEventCallback};
pub use framework::{
    Channel, Command, CommandContext, CommandOption, CommandParseError, CommandRegistry,
    CommandResult, CommandType, DeferredCommand, Handler, ParsedCommand, COMMAND_CATEGORY,
    DEFAULT_COMMAND_DESCRIPTION, SKILL_CATEGORY,
};
// AgentError 是 Python `AgentError` 的 Rust 对应物（文案保真、不做分类）；命令层与宿主
// 实现共用同一个错误类型，重新导出便于实现方只依赖本 crate。
pub use omnicrawl_controllers::AgentError;
