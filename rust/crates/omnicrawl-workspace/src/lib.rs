//! 工作区层（`omnicrawl/workspace/` 的 Rust 移植）。
//!
//! 已落地：
//!
//! - [`slug`]：`slug.py` 的通用路径段安全校验（fail-closed，供隔离区实例 ID、
//!   元数据文件名、git 分支名等拼路径的地方复用）；
//! - [`agent_isolation`]：`agent_isolation.py` 的主 Agent 隔离工作区——worktree / local
//!   两种模式的创建与复用、变更应用（patch + 三方）、四层清理门禁、退出收尾与启动清扫；
//! - [`temp`]：`temp.py` 的 Agent 临时工作区——分类子目录、按间隔清理、启动补清理与后台
//!   清理线程；
//! - [`connector_singleton`]：`connector_singleton.py` 的连接器跨进程单例锁（粘滞接管）；
//! - [`process_control`]：`process_control.py` 的进程树控制（Windows Job Object / Unix
//!   进程组）与跨平台 PID 存活探测；
//! - [`paths`]：`Path.resolve()` / `expanduser()` / `home()` 的可用子集。
//!
//! 尚未移植的同包模块（见 `README.md`）：`context`、`monitor`、`search_backend`、`tools`。

pub mod agent_isolation;
pub mod connector_singleton;
pub mod paths;
pub mod process_control;
pub mod slug;
pub mod temp;

pub use agent_isolation::{
    apply_isolation_changes, cleanup_eligible, cleanup_isolation_session, count_changed,
    create_isolation_session, default_patch_dir, default_worktrees_root,
    finalize_isolation_session, finalize_subagent_worktrees, is_git_repository, is_relative_to,
    isolation_metadata_path, now_seconds, patch_files, prepare_isolated_workspace,
    process_instance_id, read_isolation_metadata, register_isolation_session,
    registered_isolation_sessions, resolve_repo_root, resolve_worktree_head,
    session_from_sweep_entry, start_background_isolation_sweep, sweep_expired_isolation_sessions,
    unmerged_conflicts, unregister_isolation_session, AgentIsolationError, IsolationOptions,
    IsolationSession, IsolationSweepResult, SweepOptions, DEFAULT_SWEEP_MAX_AGE_SECONDS,
    ENV_SCRIPT_TIMEOUT_SECONDS, MIN_KEEP_SECONDS, WORKTREES_DIRNAME,
};
pub use connector_singleton::{
    connector_lock_path, locked_pid, sanitize_platform_name, ConnectorInstanceLock,
    LOCK_FILENAME_PREFIX, LOCK_FILENAME_SUFFIX, LOCK_POLL_SECONDS, LOCK_TIMEOUT_SECONDS,
    STALE_RECLAIM_WAIT_SECONDS,
};
pub use paths::{home_directory, resolve_path};
pub use process_control::{
    assign_process_to_kill_on_close_job, close_windows_handle, pid_is_running,
    terminate_process_tree, wait_with_timeout, ManagedProcess, ProcessState,
    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS, JOB_OBJECT_LIMIT_KILL_ON_CLOSE,
};
pub use slug::{is_safe_slug, validate_slug, SlugSafetyError, DEFAULT_MAX_LENGTH};
pub use temp::{
    agent_temp_status_label, delete_temp_entry, format_seconds, load_agent_temp_workspace_config,
    local_to_system_time, preserved_root_names, resolve_agent_temp_dir, resolve_temp_child,
    system_time_to_local, temp_workspace_readme, AgentTempCleanupResult, AgentTempWorkspace,
    AgentTempWorkspaceConfig, AgentTempWorkspaceError, NowFactory, TempCore,
    DEFAULT_AGENT_TEMP_CLEANUP_INTERVAL_HOURS, DEFAULT_AGENT_TEMP_DIRECTORY,
    DEFAULT_AGENT_TEMP_SUBDIRECTORIES, LAST_CLEANUP_FILENAME, PRESERVED_ROOT_NAMES,
};
