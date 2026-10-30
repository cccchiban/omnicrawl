//! OmniCrawl 统一启动入口（Python `omnicrawl/entry.py` 的 Rust 移植）。
//!
//! | Python | Rust |
//! | --- | --- |
//! | `entry.py` 的路由与子进程生命周期 | `src/main.rs`（二进制 `omnicrawl-host`） |
//!
//! 边界：本 crate 不复制 TUI 与内核的业务逻辑。启动编排（首次配置向导、连接器自动启动、
//! 配置诊断与退出回收）在 [`startup`] 与 [`channel_setup`]。

pub mod channel_setup;
pub mod startup;

pub use channel_setup::{default_base_url, run_channel_setup, ChannelSetupOutcome};
pub use startup::{
    bootstrap, connector_specs, interactive_terminal, locate_templates_dir, start_connectors,
    StartupOutcome,
};
