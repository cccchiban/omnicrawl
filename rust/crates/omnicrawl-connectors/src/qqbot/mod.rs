//! QQ 机器人远程接入（对齐 QQ 开放平台 API v2 与「第三方 Agent 接入」文档）。
//!
//! 与 Telegram / 飞书同构：只做平台 I/O 与显示映射，回合、斜杠命令与配置持久化由宿主侧
//! （[`crate::agent::AgentDriver`]）负责。平台形态差异集中在 [`bot`]：QQ 只允许被动回复，
//! 因此没有消息编辑与流式预览。

pub mod api;
pub mod bind;
pub mod bot;
pub mod config;
pub mod dispatch;
pub mod files;
pub mod gateway;
pub mod markdown;
pub mod media;

pub use crate::http::{HttpReply, HttpTransport, UreqTransport};
pub use api::{
    QqApi, QqApiError, Target, FILE_TYPE_FILE, FILE_TYPE_IMAGE, FILE_TYPE_VIDEO, FILE_TYPE_VOICE,
    MAX_DIRECT_UPLOAD_BYTES, MAX_REPLIES_PER_MESSAGE, MSG_TYPE_MARKDOWN, MSG_TYPE_MEDIA,
    MSG_TYPE_TEXT, USER_AGENT,
};
pub use bind::{
    bind_with, bind_with_console, build_connect_url, create_bind_task, credentials_from,
    decrypt_secret, generate_bind_key, poll_bind_result, qr_terminal, BindCredentials, BindError,
    BindResult, BindStatus, BindTask, CONNECT_PAGE, DEFAULT_BIND_TIMEOUT, POLL_INTERVAL,
};
pub use bot::{
    describe, split_content, QqBot, QqPort, ReplyHandle, MAX_CONTENT_CHARS,
    MAX_INTERMEDIATE_REPLIES,
};
pub use config::{
    check_config, load_qqbot_config, mask_secret, validate_config, AccessPolicy, AccessScope,
    QqBotConfig, DEFAULT_BASE_URL, DEFAULT_CONFIRM_TIMEOUT_SECONDS, DEFAULT_INTENTS,
    SANDBOX_BASE_URL,
};
pub use dispatch::{
    attachments_of, classify, command_name, parse_message, parse_message_with_bot,
    parse_thinking_command, route_event, route_event_with_bot, strip_mention, workspace_argument,
    BotIdentity, Command, EventRoute, Inbound, ThinkingCommand, EVENT_C2C_MESSAGE,
    EVENT_GROUP_AT_MESSAGE, EVENT_GROUP_MESSAGE, EVENT_GUILD_AT_MESSAGE,
};
pub use files::{
    classify_file_name, destination, fallback_name, relative_display_path, safe_name,
    suffix_for_mime,
};
pub use gateway::{
    heartbeat_payload, identify_payload, parse_hello_interval, parse_signal, resume_payload,
    GatewayConnection, GatewaySignal, DEFAULT_HEARTBEAT,
};
pub use markdown::{starts_block, to_markdown, MAX_MARKDOWN_CHARS};
pub use media::{kind_of_path, MediaKind, MediaUpload, MAX_MEDIA_BYTES};
