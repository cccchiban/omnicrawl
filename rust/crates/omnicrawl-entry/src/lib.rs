//! OmniCrawl 统一启动入口（Python `omnicrawl/entry.py` 与 `omnicrawl/cli.py` 的 Rust 移植）。
//!
//! | Python | Rust |
//! | --- | --- |
//! | `entry.py` 的路由与子进程生命周期 | `src/main.rs`（二进制 `omnicrawl-host`） |
//! | `cli.py` 的 plugin 子命令 | [`cli`] |
//!
//! `entry.py` 里 `plugin ...` 只做分发，真正的参数面与子命令在 `cli.py`；Rust 侧沿用同一分层：
//! [`cli`] 只依赖 `omnicrawl-extensions`（注册表 / 安装 / 诊断）与 `omnicrawl-config`
//! （`plugins` 段读写），不碰 TUI、API 或内核进程，便于单独对照测试。
//!
//! 边界：本 crate 不复制 TUI 与内核的业务逻辑。启动编排（首次配置向导、连接器自动启动、
//! 配置诊断与退出回收）在 [`startup`] 与 [`channel_setup`]；插件 CLI 在 [`cli`]。

pub mod channel_setup;
pub mod cli;
pub mod startup;

pub use channel_setup::{
    default_api_key_env, default_base_url, run_channel_setup, ChannelSetupOutcome,
};
pub use startup::{
    bootstrap, connector_specs, interactive_terminal, locate_templates_dir, start_connectors,
    StartupOutcome,
};

pub use cli::{
    parse_plugin_args, resolve_scope, run_plugin_cli, run_plugin_command, runner_dir, runner_path,
    worker_launcher, CliError, PluginArgs, EXIT_ATOMIC, EXIT_MANIFEST, EXIT_NODE, EXIT_OK,
    EXIT_REGISTRY_NET, EXIT_SMOKE, EXIT_USAGE, EXIT_USER_CANCEL, RUNNER_DIR_ENV,
};
