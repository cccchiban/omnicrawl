//! 会话存储（Rust 内核）：事件与索引模型、命名与时间校验。
//!
//! 语义基准是 Python 侧 `omnicrawl/state/session_models.py`：会话 JSONL 事件、`index.json`
//! 索引条目、会话 id / 事件类型 / 相对路径的校验，以及时间戳的 ISO-8601（UTC、微秒）语义。
//! 本片只有纯逻辑、无文件 I/O：磁盘读写（追加转录、维护索引、锁与归档）在后续切片，
//! 但对外的字段名、错误文案与 JSONL 行字节都与 Python 侧对齐，避免两个实现互相读不懂对方的会话。

pub mod artifact;
pub mod error;
pub mod event;
pub mod history;
pub mod index;
pub mod locking;
pub mod memory;
pub mod memory_ranking;
pub mod memory_store;
pub mod naming;
pub mod projection;
pub mod redaction;
pub mod store;
pub mod time;

pub use artifact::{
    normalize_relative_artifact_path, preview_text, redact_sensitive_html, tool_output_summary,
    SessionArtifactStore, TOOL_RESULT_INLINE_OUTPUT_CHARS, TOOL_RESULT_PREVIEW_CHARS,
};
pub use error::SessionStoreError;
pub use event::SessionEvent;
pub use history::{
    project_compaction_boundary_history, project_history_messages, project_session_history,
    projection_only_event, TurnHistoryProjector, PROJECTION_ONLY_EVENT_ID_PREFIX,
};
pub use index::SessionIndexEntry;
pub use locking::{
    append_text_line, atomic_write_text, process_lock_for_root, DurableWritePolicy,
    ProcessFileLock, DEFAULT_LOCK_POLL_SECONDS, DEFAULT_LOCK_TIMEOUT_SECONDS, LOCK_FILE_NAME,
};
pub use memory::{
    body_of, dedupe_directories, dedupe_strings, format_memory_datetime, format_memory_markdown,
    normalize_content, normalize_directory, parse_memory_datetime, read_markdown_body,
    MemoryIndexEntry,
};
pub use memory_ranking::{
    classify_storage_directory, directories_overlap, directory_match_score, extract_search_tokens,
    local_now, make_summary, merge_memory_content, normalize_for_compare, score_related_entry,
    score_search_entry, text_similarity, DEFAULT_STORAGE_DIRECTORIES,
};
pub use memory_store::{
    migrate_legacy_memory, MemoryMigrationResult, MemoryRecord, MemorySearchResult, MemoryStore,
    MemoryWriteRequest,
};
pub use naming::{
    clean_title, event_type_from_json, new_session_id, normalize_event_type,
    normalize_relative_file_path, normalize_session_id, path_from_json,
    read_payload_non_negative_int, session_id_from_json, COMPACT_SUMMARY_PREFIX,
    EMPTY_SESSION_EVENT_TYPES, MESSAGE_EVENT_TYPES, MODEL_CONTEXT_EVENT_TYPES,
    SESSION_EVENT_VERSION, SUBAGENT_EVENT_TYPES,
};
pub use projection::{
    active_session_events, apply_run_guard_event, complete_tool_pairing, event_to_model_message,
    format_tool_result_content, function_tool_call, interrupted_tool_result_message,
    recover_run_guard_state, session_title_from_events, tool_result_message,
    tool_result_output_text, CANCELLED_TURN_DEFAULT_SUMMARY, INTERRUPTED_TOOL_RESULT_TEXT,
    RUN_GUARD_TODO_TOOL_NAME, TOOL_CALL_CONTEXT_PREFIX, TOOL_RESULT_CONTEXT_PREFIX,
    TURN_UNDONE_EVENT_TYPE,
};
pub use redaction::{redact_sensitive_text, redact_sensitive_values};
pub use store::{kernel_runtime_identity, CreatedSession, SessionStore};
pub use time::{datetime_from_json, datetime_to_millis, format_datetime, parse_datetime, utc_now};
