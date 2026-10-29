//! 消息平台连接器（Rust 内核）：Telegram Bot 轮询、飞书自建应用长连接与 QQ 机器人网关。
//!
//! 语义基准是 Python 侧 `omnicrawl/connectors/telegram.py` 与 `omnicrawl/connectors/fsapp.py`：
//! 配置解析、白名单、文本分段与裁剪、文件接收与分类、命令分发、审批与提问桥、显示映射都在这里；
//! 回合驱动由宿主侧实现（见 [`agent`]），连接器不持有 Agent 回合逻辑。
pub mod agent;
pub mod autostart;
pub mod feishu;
pub mod http;
pub mod json;
pub mod mqtt;
pub mod qqbot;
pub mod render;
pub mod telegram;
pub mod text;
pub mod ws;

pub use agent::{
    AgentDriver, AgentStatus, AskUserHandler, ConfirmHandler, ToolCall, ToolResult, TurnError,
    TurnEvent, TurnOutcome, WorkspaceSwitch,
};
pub use autostart::{
    auto_start_decision, child_environment, connector_log_path, current_environment,
    default_connector_specs, default_log_root, safe_error_text, start_configured_connectors,
    AutoStartDecision, ConnectorManagerOptions, ConnectorProcessManager, ConnectorSpec,
    OutputTarget, ProcessSpawner, SpawnRequest, StdProcessSpawner, AUTO_START_ENV,
    CONNECTOR_LOG_DIRNAME, CONNECTOR_SUBCOMMAND, DISABLED_VALUES, ENABLED_VALUES, FEISHU_PLATFORM,
    MQTT_PLATFORM, QQ_PLATFORM, TELEGRAM_PLATFORM, WATCHER_JOIN_TIMEOUT_SECONDS, WATCH_POLL_SECONDS,
};
