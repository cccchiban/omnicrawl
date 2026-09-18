//! Telegram Bot 远程接入（Python 侧 `omnicrawl/connectors/telegram.py` 的 Rust 移植）。
//!
//! 只做平台 I/O 与显示映射：`getUpdates` 长轮询、命令分发、流式消息、工具确认桥与文件接收；
//! Agent 回合、斜杠命令与配置持久化由宿主侧（[`crate::agent::AgentDriver`]）负责。

pub mod api;
pub mod bot;
pub mod config;
pub mod dispatch;
pub mod files;
pub mod format;

pub use crate::http::{HttpReply, HttpTransport, UreqTransport};
pub use api::{TelegramApi, TelegramApiError};
pub use bot::{ActiveTask, TelegramBot, CLOSE_TASK_JOIN_TIMEOUT};
pub use config::{
    load_telegram_config, validate_config, TelegramConfig, DEFAULT_CONFIRM_TIMEOUT_SECONDS,
    POLLING_TIMEOUT_SECONDS,
};
pub use dispatch::{
    classify, parse_thinking_command, route_update, workspace_argument, Command, ThinkingCommand,
    UpdateRoute,
};
pub use files::{
    classify_file_name, extract_telegram_file, relative_display_path, temp_destination,
    TelegramFile,
};
pub use format::{
    plan_abort, plan_finalize, split_message, truncate_for_stream, AbortPlan, FinalizePlan,
    MAX_MESSAGE_LEN, STREAM_EDIT_INTERVAL_SECONDS,
};
