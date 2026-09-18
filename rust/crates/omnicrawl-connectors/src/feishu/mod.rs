//! 飞书连接器（Python 侧 `omnicrawl/connectors/fsapp.py` 与 `feishu_inbox.py` 的 Rust 移植）。
//!
//! 与 Telegram 侧同构：只做平台 I/O 与显示映射（卡片时间线、工具摘要、执行计划、子任务进度、
//! 提问卡片与工具审批），回合驱动由宿主侧（[`crate::agent::AgentDriver`]）负责。

pub mod api;
pub mod bot;
pub mod config;
pub mod dedupe;
pub mod files;
pub mod render;
pub mod text;
pub mod timeline;
pub mod ws;

pub use api::{
    ClientConfig, FeishuApi, FeishuApiError, WsEndpoint, DEFAULT_BASE_URL, WS_ENDPOINT_PATH,
};
pub use bot::{ActiveTask, FeishuBot, STREAM_PATCH_INTERVAL_SECONDS};
pub use config::{check_config, load_feishu_config, mask_secret, ConfigSource, FeishuConfig};
pub use dedupe::{inbox_dedupe_key, SeenMessages, DEDUP_MAX_ENTRIES, DEDUP_TTL_SECONDS};
pub use files::{
    classify_filename, file_marker_paths, post_text_and_images, resolve_temp_destination,
    resource_file_key, resource_file_name, FILE_TYPE_MAP, MESSAGE_RESOURCE_TYPES,
};
pub use render::{
    card_json, format_duration, format_elapsed, markdown_card, normalize_todos, question_card_json,
    question_resolved_card_json, reasoning_panel, render_tool_record, subagents_text, todos_text,
    tool_body, tool_status_icon, tool_summary, SubagentNode,
};
pub use text::{
    clean_text, display_text, parse_json_object, resolve_final_text, split_segment_for_card,
    split_text, text_value, MAX_TEXT_CHARS, SEGMENT_MAX_CHARS,
};
pub use timeline::{
    MessagePort, PlanMessage, ReasoningMessage, SubAgentMessage, TextMessage, TimelineMessage,
    ToolMessage, ToolRecord,
};
pub use ws::{ping_interval_seconds, ReconnectPolicy};
