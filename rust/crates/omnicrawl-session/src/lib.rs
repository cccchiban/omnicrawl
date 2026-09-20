//! 会话存储（Rust 内核）：事件与索引模型、命名与时间校验。
//!
//! 语义基准是 Python 侧 `omnicrawl/state/session_models.py`：会话 JSONL 事件、`index.json`
//! 索引条目、会话 id / 事件类型 / 相对路径的校验，以及时间戳的 ISO-8601（UTC、微秒）语义。
//! 本片只有纯逻辑、无文件 I/O：磁盘读写（追加转录、维护索引、锁与归档）在后续切片，
//! 但对外的字段名、错误文案与 JSONL 行字节都与 Python 侧对齐，避免两个实现互相读不懂对方的会话。

pub mod artifact;
pub mod consistency;
pub mod error;
pub mod event;
pub mod history;
pub mod index;
pub mod locking;
pub mod memory;
pub mod memory_ranking;
pub mod memory_store;
pub mod naming;
pub mod project;
pub mod projection;
pub mod prompt_history;
pub mod records;
pub mod redaction;
pub mod store;
pub mod time;
pub mod turn_snapshot;
pub mod undo;

pub use artifact::{
    normalize_relative_artifact_path, preview_text, redact_sensitive_html, tool_output_summary,
    SessionArtifactStore, TOOL_RESULT_INLINE_OUTPUT_CHARS, TOOL_RESULT_PREVIEW_CHARS,
};
pub use consistency::{
    build_index_entry_from_events, compare_index_entry, discover_artifact_session_ids,
    discover_transcripts, SessionConsistencyIssue, TranscriptLocation, ISSUE_ARCHIVED_MISMATCH,
    ISSUE_EVENT_COUNT_MISMATCH, ISSUE_LAST_EVENT_MISMATCH, ISSUE_MESSAGE_COUNT_MISMATCH,
    ISSUE_PATH_MISMATCH, ISSUE_TITLE_MISMATCH, ISSUE_UPDATED_AT_MISMATCH, ISSUE_WORKSPACE_MISMATCH,
    SEVERITY_ERROR, SEVERITY_WARNING,
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
pub use project::{
    clean_project_name, expand_vars, git_root, is_scan_excluded, normalize_project_path, path_key,
    under_agent_worktrees, OverviewSession, OverviewSessionEntry, ProjectEntry, ProjectOverview,
    ProjectStore, AGENT_WORKTREES_DIR_NAME, PROJECTS_FILE_NAME,
};
pub use projection::{
    active_session_events, apply_run_guard_event, complete_tool_pairing, event_to_model_message,
    format_tool_result_content, function_tool_call, interrupted_tool_result_message,
    recover_run_guard_state, session_title_from_events, tool_result_message,
    tool_result_output_text, CANCELLED_TURN_DEFAULT_SUMMARY, INTERRUPTED_TOOL_RESULT_TEXT,
    RUN_GUARD_TODO_TOOL_NAME, TOOL_CALL_CONTEXT_PREFIX, TOOL_RESULT_CONTEXT_PREFIX,
    TURN_UNDONE_EVENT_TYPE,
};
pub use prompt_history::{
    clean_prompt_display, PromptHistoryEntry, PromptHistoryStore, MAX_PROMPT_HISTORY_DISPLAY_CHARS,
};
pub use records::{
    build_index_document, decode_session_event_dict, decode_session_event_line,
    log_record_diagnostics, migrate_event_dict, parse_index_document,
    read_session_events_with_diagnostics, split_lines_python, SessionEventReadResult,
    SessionRecordDiagnostic, DIAG_INVALID_FIELDS, DIAG_INVALID_JSON, DIAG_LEGACY_MIGRATED,
    DIAG_NOT_OBJECT, DIAG_PROMPT_INVALID, DIAG_SESSION_ID_MISMATCH, DIAG_TRAILING_INCOMPLETE,
    DIAG_UNSUPPORTED_VERSION, SESSION_INDEX_SCHEMA_VERSION, SEVERITY_INFO,
    SUPPORTED_EVENT_VERSIONS, TRANSCRIPT_MMAP_THRESHOLD_BYTES,
};
pub use redaction::{redact_sensitive_text, redact_sensitive_values};
pub use store::{kernel_runtime_identity, CreatedSession, SessionListQuery, SessionStore};
pub use time::{datetime_from_json, datetime_to_millis, format_datetime, parse_datetime, utc_now};
pub use turn_snapshot::{
    SnapshotError, WorktreeSnapshot, WorktreeSnapshotStore, GIT_COMMAND_TIMEOUT_SECONDS,
};
pub use undo::{build_undo_plan, SessionUndoPlan, UNDO_KIND_COMPLETE, UNDO_KIND_INCOMPLETE};
