//! `omnicrawl/agent/controllers/undo.py`：/undo 回合快照的安全判定、账本与恢复预检。
//!
//! 切割方式：crate 负责「哪些工具可回退」「快照事件长什么样」「快照能否安全应用」；
//! git 子进程、会话存储、线程与锁留给宿主。宿主只需实现 [`SnapshotStore`]。

use crate::error::AgentError;
use crate::shared::{MEMORY_UNDO_EXEMPT_TOOLS, READ_ONLY_UNDO_TOOLS, REVERSIBLE_UNDO_TOOLS};
use serde_json::{Map, Value};
use std::fmt;
use std::path::{Path, PathBuf};

pub const SNAPSHOT_EVENT_TYPE: &str = "turn_snapshot";

pub const SNAPSHOT_VERSION: i64 = 2;

pub const BEGIN_PATCH_FILE: &str = "undo/begin.patch";

pub const BEGIN_UNTRACKED_FILE: &str = "undo/begin.untracked.txt";

pub const END_PATCH_FILE: &str = "undo/end.patch";

pub const END_UNTRACKED_FILE: &str = "undo/end.untracked.txt";

/// `omnicrawl/state/turn_snapshot.py` 的 `SnapshotError`：git 起点/终点捕获失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotError {
    message: String,
}

impl SnapshotError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SnapshotError {}

/// 工作区的一次 git 快照：补丁字节与未跟踪文件清单。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    pub patch: Vec<u8>,
    pub untracked: Vec<String>,
}

/// 宿主注入的 git 快照能力（对应 `WorktreeSnapshotStore`）。
pub trait SnapshotStore {
    fn has_head(&self, workspace: &Path) -> bool;
    fn capture(&self, workspace: &Path) -> Result<WorktreeSnapshot, SnapshotError>;
    fn transition(
        &self,
        workspace: &Path,
        expected: &WorktreeSnapshot,
        target: &WorktreeSnapshot,
    ) -> Result<Vec<String>, SnapshotError>;
}

/// 当前模型轮次的快照占位与副作用账本（对应 `_ActiveTurnSnapshot`）。
///
/// Python 侧用 `capture_lock` 保证并发写工具只补捕获一次；Rust 侧由 `&mut` 独占
/// 所有权表达同一件事，锁留给持有该值的宿主。
#[derive(Debug, Clone, Default)]
pub struct ActiveTurnSnapshot {
    pub snapshot_id: String,
    pub workspace: Option<PathBuf>,
    pub before: Option<WorktreeSnapshot>,
    pub executed_tools: Vec<String>,
    pub irreversible_tools: Vec<String>,
    pub completed: bool,
    pub capture_attempted: bool,
    pub capture_failed: bool,
}

/// 记录一次工具执行后，调用方是否需要补捕获起点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCaptureNeed {
    No,
    Needed,
}

/// 工具调用是否不阻断事务式 undo：只读、可回退、记忆豁免与只读 action 的工具。
pub fn tool_is_undo_safe(name: &str, arguments: &Map<String, Value>) -> bool {
    if READ_ONLY_UNDO_TOOLS.contains(&name)
        || REVERSIBLE_UNDO_TOOLS.contains(&name)
        || MEMORY_UNDO_EXEMPT_TOOLS.contains(&name)
    {
        return true;
    }
    match name {
        "subagent" => action_of(arguments, "run")
            .is_some_and(|action| matches!(action.as_str(), "list" | "get" | "list_worktrees")),
        "monitor" => action_of(arguments, "list")
            .is_some_and(|action| matches!(action.as_str(), "list" | "poll")),
        "windows_window" => action_of(arguments, "list")
            .is_some_and(|action| matches!(action.as_str(), "list" | "get")),
        "windows_clipboard" => action_of(arguments, "read_text").is_some_and(|a| a == "read_text"),
        "windows_screenshot" => true,
        _ => false,
    }
}

fn action_of(arguments: &Map<String, Value>, default: &str) -> Option<String> {
    let raw = arguments.get("action");
    let text = match raw {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) | None => default.to_string(),
        Some(other) => python_str(other),
    };
    Some(text.trim().to_string())
}

/// 记录实际执行过的工具；未知或外部工具会阻止事务式 undo。
pub fn record_tool_execution(
    snapshot: &mut ActiveTurnSnapshot,
    name: &str,
    arguments: &Map<String, Value>,
) -> TurnCaptureNeed {
    snapshot.executed_tools.push(name.to_string());
    if REVERSIBLE_UNDO_TOOLS.contains(&name) {
        return TurnCaptureNeed::Needed;
    }
    if tool_is_undo_safe(name, arguments) {
        return TurnCaptureNeed::No;
    }
    snapshot.irreversible_tools.push(name.to_string());
    TurnCaptureNeed::No
}

/// 首个可回退写工具执行前捕获工作区起点；失败只降级为「本轮无事务式 undo」。
pub fn ensure_turn_captured(snapshot: &mut ActiveTurnSnapshot, store: &dyn SnapshotStore) {
    if snapshot.before.is_some() || snapshot.capture_attempted {
        return;
    }
    snapshot.capture_attempted = true;
    let workspace = match snapshot.workspace.clone() {
        Some(path) => path,
        None => {
            snapshot.capture_failed = true;
            return;
        }
    };
    match store.capture(&workspace) {
        Ok(before) => snapshot.before = Some(before),
        Err(_) => snapshot.capture_failed = true,
    }
}

/// `turn_snapshot` 会话事件的载荷；工作区缺失（占位快照）时不产生事件。
pub fn snapshot_event_payload(snapshot: &ActiveTurnSnapshot) -> Option<Value> {
    let workspace = snapshot.workspace.as_ref()?;
    let mut payload = Map::new();
    payload.insert("version".to_string(), Value::from(SNAPSHOT_VERSION));
    payload.insert(
        "snapshot_id".to_string(),
        Value::from(snapshot.snapshot_id.clone()),
    );
    payload.insert(
        "workspace".to_string(),
        Value::from(workspace.to_string_lossy().to_string()),
    );
    payload.insert("begin_patch".to_string(), Value::from(BEGIN_PATCH_FILE));
    payload.insert(
        "begin_untracked".to_string(),
        Value::from(BEGIN_UNTRACKED_FILE),
    );
    payload.insert("end_patch".to_string(), Value::from(END_PATCH_FILE));
    payload.insert("end_untracked".to_string(), Value::from(END_UNTRACKED_FILE));
    payload.insert(
        "executed_tools".to_string(),
        Value::Array(
            snapshot
                .executed_tools
                .iter()
                .map(|name| Value::from(name.clone()))
                .collect(),
        ),
    );
    payload.insert(
        "irreversible_tools".to_string(),
        Value::Array(
            dedup_preserve_order(&snapshot.irreversible_tools)
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    );
    Some(Value::Object(payload))
}

/// 本轮结束时的快照落盘失败文案。
pub fn snapshot_write_error(error: impl fmt::Display) -> AgentError {
    AgentError::new(format!(
        "本轮结束 Git 快照失败，副作用无法安全回退：{error}"
    ))
}

/// 原子写入四个快照文件（先写 `.tmp` 再替换），避免半截 undo 状态。
pub fn write_snapshot_files(
    undo_dir: &Path,
    before: &WorktreeSnapshot,
    after: &WorktreeSnapshot,
) -> Result<(), AgentError> {
    std::fs::create_dir_all(undo_dir).map_err(snapshot_write_error)?;
    write_bytes_atomic(&undo_dir.join("begin.patch"), &before.patch)?;
    write_untracked_atomic(&undo_dir.join("begin.untracked.txt"), &before.untracked)?;
    write_bytes_atomic(&undo_dir.join("end.patch"), &after.patch)?;
    write_untracked_atomic(&undo_dir.join("end.untracked.txt"), &after.untracked)?;
    Ok(())
}

fn write_bytes_atomic(path: &Path, content: &[u8]) -> Result<(), AgentError> {
    let temporary = temporary_path(path);
    std::fs::write(&temporary, content).map_err(snapshot_write_error)?;
    std::fs::rename(&temporary, path).map_err(snapshot_write_error)
}

fn write_untracked_atomic(path: &Path, untracked: &[String]) -> Result<(), AgentError> {
    let temporary = temporary_path(path);
    std::fs::write(&temporary, untracked.join("\n")).map_err(snapshot_write_error)?;
    std::fs::rename(&temporary, path).map_err(snapshot_write_error)
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.tmp"))
}

/// 计划里的一个事件：`type` 与 `payload` 是 undo 预检真正读到的字段。
#[derive(Debug, Clone)]
pub struct UndoEvent {
    pub event_type: String,
    pub payload: Value,
}

/// 回退预检结论。
#[derive(Debug, Clone, PartialEq)]
pub enum RestorePrecheck {
    NoSnapshot,
    Snapshot(SnapshotPayload),
}

/// `turn_snapshot` 事件载荷的已校验视图。
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotPayload {
    pub snapshot_id: String,
    pub workspace: String,
    pub begin_patch: String,
    pub begin_untracked: String,
    pub end_patch: String,
    pub end_untracked: String,
    pub irreversible_tools: Vec<String>,
}

/// 预检并恢复计划中的快照：返回可应用快照，或在拒绝时给出与 Python 同文案的错误。
///
/// 只覆盖到「可以安全应用」为止：真正应用补丁需要 [`SnapshotStore::transition`]。
pub fn precheck_restore(
    events: &[UndoEvent],
    current_workspace: &Path,
) -> Result<RestorePrecheck, AgentError> {
    let snapshot_events: Vec<&UndoEvent> = events
        .iter()
        .filter(|event| event.event_type == SNAPSHOT_EVENT_TYPE)
        .collect();

    if snapshot_events.is_empty() {
        let potential_side_effects = potential_side_effects(events);
        if !potential_side_effects.is_empty() {
            return Err(AgentError::new(format!(
                "该旧轮次存在副作用但没有 Git 快照，已拒绝回退：{}",
                dedup_preserve_order(&potential_side_effects).join("、")
            )));
        }
        return Ok(RestorePrecheck::NoSnapshot);
    }
    if snapshot_events.len() != 1 {
        return Err(AgentError::new(
            "当前轮次包含多个 Git 快照事件，无法安全回退。",
        ));
    }

    let payload = &snapshot_events[0].payload;
    if !json_int_like(payload.get("version"), SNAPSHOT_VERSION) {
        return Err(AgentError::new(
            "该轮次使用旧版影子对象库快照（version 1），已随 git diff 重构移除，\
             无法自动回退；请手工还原文件后重试。",
        ));
    }

    let irreversible = payload
        .get("irreversible_tools")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let Value::Array(irreversible) = irreversible else {
        return Err(AgentError::new("轮次快照的不可逆工具账本格式无效。"));
    };
    let blocker_names: Vec<String> = irreversible
        .iter()
        .map(python_str)
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    if !blocker_names.is_empty() {
        return Err(AgentError::new(format!(
            "该轮执行了无法由 Git 证明可逆的操作，已拒绝整轮回退：{}",
            dedup_preserve_order(&blocker_names).join("、")
        )));
    }

    let recorded_workspace = payload
        .get("workspace")
        .map(python_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if !recorded_workspace.is_empty()
        && resolve_path(Path::new(&recorded_workspace)) != resolve_path(current_workspace)
    {
        return Err(AgentError::new(
            "该轮次的工作区与当前工作区不一致，已拒绝回退。",
        ));
    }

    Ok(RestorePrecheck::Snapshot(SnapshotPayload {
        snapshot_id: payload
            .get("snapshot_id")
            .map(python_str)
            .unwrap_or_default(),
        workspace: recorded_workspace,
        begin_patch: string_field(payload, "begin_patch"),
        begin_untracked: string_field(payload, "begin_untracked"),
        end_patch: string_field(payload, "end_patch"),
        end_untracked: string_field(payload, "end_untracked"),
        irreversible_tools: blocker_names,
    }))
}

fn string_field(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .map(python_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn potential_side_effects(events: &[UndoEvent]) -> Vec<String> {
    let mut requested_calls: Vec<(String, Value)> = Vec::new();
    for event in events {
        if event.event_type != "tool_call_requested" {
            continue;
        }
        let key = event
            .payload
            .get("tool_call_id")
            .map(python_str)
            .unwrap_or_default();
        if key.is_empty() {
            continue;
        }
        requested_calls.retain(|(existing, _)| existing != &key);
        requested_calls.push((key, event.payload.clone()));
    }

    let mut result = Vec::new();
    for event in events {
        if event.event_type == "compact_summary" {
            result.push("context_compaction".to_string());
        }
        if event.event_type != "tool_result" || event.payload.get("ok") == Some(&Value::Bool(false))
        {
            continue;
        }
        let tool_name = event
            .payload
            .get("tool")
            .map(python_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let key = event
            .payload
            .get("tool_call_id")
            .map(python_str)
            .unwrap_or_default();
        let arguments = requested_calls
            .iter()
            .find(|(existing, _)| existing == &key)
            .and_then(|(_, payload)| payload.get("arguments"))
            .and_then(|value| match value {
                Value::Object(map) => Some(map.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if !tool_name.is_empty()
            && (!tool_is_undo_safe(&tool_name, &arguments)
                || REVERSIBLE_UNDO_TOOLS.contains(&tool_name.as_str()))
        {
            result.push(tool_name);
        }
    }
    result
}

/// 读取轮次快照：校验事件里的相对路径仍位于 `artifacts/<session_id>` 内。
pub fn load_workspace_snapshot(
    artifact_root: &Path,
    session_id: &str,
    patch_relative: &str,
    untracked_relative: &str,
    prefix: &str,
) -> Result<WorktreeSnapshot, SnapshotError> {
    let patch_relative = patch_relative.trim();
    let untracked_relative = untracked_relative.trim();
    if patch_relative.is_empty() || untracked_relative.is_empty() {
        return Err(SnapshotError::new(format!(
            "轮次快照缺少 {prefix} 补丁文件引用。"
        )));
    }
    let session_artifacts = resolve_path(&artifact_root.join(session_id));
    let patch_path = resolve_artifact_path(&session_artifacts, patch_relative)?;
    let untracked_path = resolve_artifact_path(&session_artifacts, untracked_relative)?;
    let patch = std::fs::read(&patch_path)
        .map_err(|error| SnapshotError::new(format!("读取轮次快照文件失败：{error}")))?;
    let untracked_text = std::fs::read_to_string(&untracked_path)
        .map_err(|error| SnapshotError::new(format!("读取轮次快照文件失败：{error}")))?;
    let untracked = untracked_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect();
    Ok(WorktreeSnapshot { patch, untracked })
}

/// 把事件中的相对路径解析为 root 内的绝对路径（防目录穿越）。
pub fn resolve_artifact_path(root: &Path, relative: &str) -> Result<PathBuf, SnapshotError> {
    let value = relative.replace('\\', "/");
    let value = value.trim_matches('/').to_string();
    if value.is_empty() {
        return Err(SnapshotError::new(format!("快照文件路径无效：{relative}")));
    }
    let mut candidate = root.to_path_buf();
    for part in value.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(SnapshotError::new(format!("快照文件路径无效：{relative}")));
        }
        candidate.push(part);
    }
    let path = resolve_path(&candidate);
    if !is_relative_to(&path, root) {
        return Err(SnapshotError::new(format!(
            "快照文件越出会话目录：{relative}"
        )));
    }
    Ok(path)
}

/// Python 3.9 的 `Path.relative_to`：parent 必须是 path 的祖先或等值。
pub fn is_relative_to(path: &Path, parent: &Path) -> bool {
    path.starts_with(parent)
}

/// 解析后的路径是否落在 root 之内（含 root 自身）。
pub fn resolved_under(path: &Path, root: &Path) -> bool {
    let resolved = resolve_path(path);
    let root = resolve_path(root);
    resolved == root || is_relative_to(&resolved, &root)
}

/// 绝对化并按字面展开 `.` 与 `..`。
///
/// 与 Python `Path.resolve()` 的差别：不做符号链接解析（见 crate README 的已知差异）。
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    let mut depth: Vec<String> = Vec::new();
    for component in absolute.components() {
        match component {
            std::path::Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                depth.pop();
            }
            std::path::Component::Normal(value) => {
                depth.push(value.to_string_lossy().to_string());
            }
            std::path::Component::Prefix(prefix) => {
                normalized.push(prefix.as_os_str());
            }
        }
    }
    for part in depth {
        normalized.push(part);
    }
    normalized
}

pub fn dedup_preserve_order(values: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for value in values {
        if !seen.contains(value) {
            seen.push(value.clone());
        }
    }
    seen
}

fn json_int_like(value: Option<&Value>, expected: i64) -> bool {
    match value {
        Some(Value::Number(number)) => {
            number.as_i64() == Some(expected) || number.as_f64() == Some(expected as f64)
        }
        _ => false,
    }
}

/// `str(value)` 的可用子集：标量按 Python 写法，容器退化为 JSON 文本。
pub fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// 读取快照文件失败时的包装文案（`_restore_turn_side_effects` 的读取段）。
pub fn restore_load_failed(cause: impl fmt::Display) -> AgentError {
    AgentError::new(format!("副作用回退失败：{cause}"))
}

/// 应用补丁冲突或失败时的包装文案（`_restore_turn_side_effects` 的过渡段）。
pub fn restore_conflict_failed(cause: impl fmt::Display) -> AgentError {
    AgentError::new(format!("副作用回退冲突或失败：{cause}"))
}

/// 未启用会话时无法持久化轮次快照（`_complete_turn_snapshot` 的前置拒绝）。
pub fn turn_snapshot_session_required() -> AgentError {
    AgentError::new("Session 未启用，无法持久化轮次快照。")
}
