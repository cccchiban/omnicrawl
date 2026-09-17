//! 会话存储（Rust 内核）：事件与索引模型、命名与时间校验。
//!
//! 语义基准是 Python 侧 `omnicrawl/state/session_models.py`：会话 JSONL 事件、`index.json`
//! 索引条目、会话 id / 事件类型 / 相对路径的校验，以及时间戳的 ISO-8601（UTC、微秒）语义。
//! 本片只有纯逻辑、无文件 I/O：磁盘读写（追加转录、维护索引、锁与归档）在后续切片，
//! 但对外的字段名、错误文案与 JSONL 行字节都与 Python 侧对齐，避免两个实现互相读不懂对方的会话。

pub mod error;
pub mod event;
pub mod index;
pub mod naming;
pub mod time;

pub use error::SessionStoreError;
pub use event::SessionEvent;
pub use index::SessionIndexEntry;
pub use naming::{
    clean_title, event_type_from_json, new_session_id, normalize_event_type,
    normalize_relative_file_path, normalize_session_id, path_from_json,
    read_payload_non_negative_int, session_id_from_json, COMPACT_SUMMARY_PREFIX,
    EMPTY_SESSION_EVENT_TYPES, MESSAGE_EVENT_TYPES, MODEL_CONTEXT_EVENT_TYPES,
    SESSION_EVENT_VERSION, SUBAGENT_EVENT_TYPES,
};
pub use time::{datetime_from_json, datetime_to_millis, format_datetime, parse_datetime, utc_now};
