//! 内核侧 `/undo` 的宿主实现：把 controllers 的 undo 判定面接到 git 与会话存储上。
//!
//! controllers/undo.rs 负责「哪些工具可回退 / 快照事件长什么样 / 快照能否安全应用」，并明确
//! 把 git 子进程、会话存储与锁留给宿主——这里补上这三件事。

use std::path::{Path, PathBuf};

use omnicrawl_controllers::undo::{
    self as undo_logic, ActiveTurnSnapshot, SnapshotError, SnapshotStore, TurnCaptureNeed,
    WorktreeSnapshot,
};
use omnicrawl_session::{utc_now, SessionStore, WorktreeSnapshotStore};
use serde_json::{Map, Value};

/// undo 快照目录名：与 controllers 里 `undo/xxx.patch` 的相对路径前缀一致。
const UNDO_DIR_NAME: &str = "undo";

/// 内核侧的 git 快照能力：转发给会话 crate 的实现（同一语义基准）。
pub struct KernelSnapshotStore {
    inner: WorktreeSnapshotStore,
}

impl Default for KernelSnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KernelSnapshotStore {
    pub fn new() -> Self {
        Self {
            inner: WorktreeSnapshotStore::new(),
        }
    }
}

fn to_logic(snapshot: omnicrawl_session::WorktreeSnapshot) -> WorktreeSnapshot {
    WorktreeSnapshot {
        patch: snapshot.patch,
        untracked: snapshot.untracked,
    }
}

fn to_session(snapshot: &WorktreeSnapshot) -> omnicrawl_session::WorktreeSnapshot {
    omnicrawl_session::WorktreeSnapshot {
        patch: snapshot.patch.clone(),
        untracked: snapshot.untracked.clone(),
        has_head: true,
    }
}

impl SnapshotStore for KernelSnapshotStore {
    fn has_head(&self, workspace: &Path) -> bool {
        self.inner.has_head(workspace)
    }

    fn capture(&self, workspace: &Path) -> Result<WorktreeSnapshot, SnapshotError> {
        self.inner
            .capture(workspace)
            .map(to_logic)
            .map_err(|error| SnapshotError::new(error.message()))
    }

    fn transition(
        &self,
        workspace: &Path,
        expected: &WorktreeSnapshot,
        target: &WorktreeSnapshot,
    ) -> Result<Vec<String>, SnapshotError> {
        self.inner
            .transition(workspace, &to_session(expected), &to_session(target))
            .map_err(|error| SnapshotError::new(error.message()))
    }
}

/// 一轮的 undo 账本：惰性起点、工具账本与收尾落盘。
pub struct TurnUndo {
    store: KernelSnapshotStore,
    snapshot: Option<ActiveTurnSnapshot>,
}

impl TurnUndo {
    /// 回合开始登记：工作区不是有 HEAD 的 Git 仓库时本轮不记账（与 Python 的降级一致：
    /// 只保留一次 rev-parse 探测，不立即拍快照）。
    pub fn begin(workspace: Option<&str>) -> Self {
        let store = KernelSnapshotStore::new();
        let snapshot = workspace
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| store.has_head(path))
            .map(|path| ActiveTurnSnapshot {
                snapshot_id: new_snapshot_id(),
                workspace: Some(path),
                ..ActiveTurnSnapshot::default()
            });
        Self { store, snapshot }
    }

    /// 记录一次实际执行的工具调用；首个可回退写工具执行前补捕获起点。
    pub fn record(&mut self, name: &str, arguments: &Map<String, Value>) {
        let Some(snapshot) = self.snapshot.as_mut() else {
            return;
        };
        if undo_logic::record_tool_execution(snapshot, name, arguments) == TurnCaptureNeed::Needed {
            undo_logic::ensure_turn_captured(snapshot, &self.store);
        }
    }

    /// 本轮已执行的工具名（按执行顺序，含重复）；没有账本时为空。
    pub fn executed_tools(&self) -> &[String] {
        match self.snapshot.as_ref() {
            Some(snapshot) => snapshot.executed_tools.as_slice(),
            None => &[],
        }
    }

    /// 回合收尾：捕获终点、写四个快照文件并落一条 `turn_snapshot` 事件。
    ///
    /// 纯读/纯对话轮次没有起点快照，直接标记完成、不落盘也不产生事件（与 Python 一致）。
    pub fn complete(&mut self, store: &SessionStore, session_id: &str) -> Result<(), String> {
        let Some(snapshot) = self.snapshot.as_mut() else {
            return Ok(());
        };
        if snapshot.completed {
            return Ok(());
        }
        let Some(before) = snapshot.before.clone() else {
            snapshot.completed = true;
            return Ok(());
        };
        let Some(workspace) = snapshot.workspace.clone() else {
            snapshot.completed = true;
            return Ok(());
        };
        let after = self
            .store
            .capture(&workspace)
            .map_err(|error| undo_logic::snapshot_write_error(error).to_string())?;
        let undo_dir = store
            .session_artifacts_dir(session_id)
            .map_err(|error| error.to_string())?
            .join(UNDO_DIR_NAME);
        undo_logic::write_snapshot_files(&undo_dir, &before, &after)
            .map_err(|error| error.to_string())?;
        let Some(payload) = undo_logic::snapshot_event_payload(snapshot) else {
            snapshot.completed = true;
            return Ok(());
        };
        let payload = payload.as_object().cloned().unwrap_or_default();
        store
            .append_event(
                session_id,
                undo_logic::SNAPSHOT_EVENT_TYPE,
                payload,
                None,
                utc_now(),
            )
            .map_err(|error| error.to_string())?;
        snapshot.completed = true;
        Ok(())
    }
}

/// 快照 id：32 位十六进制，与 Python `uuid4().hex` 同形（只要求会话内唯一）。
fn new_snapshot_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_nanos())
        .unwrap_or(0);
    format!("{:032x}", nanos ^ u128::from(std::process::id()))
}

/// 这一批观察对应的工具调用是否记录进 undo 账本（用调用名与参数判定）。
pub fn record_calls(undo: &mut TurnUndo, calls: &[omnicrawl_core::ToolCall]) {
    for call in calls {
        undo.record(&call.name, &call.arguments);
    }
}

/// 撤销最近一轮的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
    /// `complete` | `incomplete`
    pub kind: String,
    pub message_count: u64,
    pub side_effects_reverted: bool,
    /// 被删除且没有内容副本、无法恢复的未跟踪文件。
    pub unrestorable: Vec<String>,
}

/// 撤销最近一轮：预检 → 恢复工作区副作用 → 提交会话回退（提交失败时反向恢复）。
///
/// 语义基准是 Python `SessionFacade.undo_last_turn` 与 `UndoMixin._restore_turn_side_effects`：
/// 副作用没有 Git 快照、或本轮跑过不可逆工具时拒绝整轮回退；提交失败要把工作区换回撤销前。
pub fn undo_last_turn(
    store: &SessionStore,
    session_id: &str,
    workspace: &Path,
) -> Result<UndoOutcome, String> {
    let plan = store
        .prepare_undo_last_turn(session_id)
        .map_err(|error| error.to_string())?;
    let events: Vec<undo_logic::UndoEvent> = plan
        .events
        .iter()
        .map(|event| undo_logic::UndoEvent {
            event_type: event.event_type.clone(),
            payload: Value::Object(event.payload.clone()),
        })
        .collect();
    let precheck =
        undo_logic::precheck_restore(&events, workspace).map_err(|error| error.to_string())?;

    let mut unrestorable: Vec<String> = Vec::new();
    let mut revert: Option<(WorktreeSnapshot, WorktreeSnapshot)> = None;
    if let undo_logic::RestorePrecheck::Snapshot(payload) = precheck {
        let artifact_root = store.artifacts_root();
        let before = undo_logic::load_workspace_snapshot(
            &artifact_root,
            session_id,
            &payload.begin_patch,
            &payload.begin_untracked,
            "begin",
        )
        .map_err(|error| format!("副作用回退失败：{}", error.message()))?;
        let after = undo_logic::load_workspace_snapshot(
            &artifact_root,
            session_id,
            &payload.end_patch,
            &payload.end_untracked,
            "end",
        )
        .map_err(|error| format!("副作用回退失败：{}", error.message()))?;
        unrestorable = KernelSnapshotStore::new()
            .transition(workspace, &after, &before)
            .map_err(|error| format!("副作用回退冲突或失败：{}", error.message()))?;
        revert = Some((before, after));
    }

    let reverted = revert.is_some();
    match store.commit_undo_plan(&plan, reverted, utc_now()) {
        Ok(_) => Ok(UndoOutcome {
            kind: plan.kind.clone(),
            message_count: plan.message_count(),
            side_effects_reverted: reverted,
            unrestorable,
        }),
        Err(error) => {
            if let Some((before, after)) = revert {
                if let Err(revert_error) =
                    KernelSnapshotStore::new().transition(workspace, &before, &after)
                {
                    return Err(format!(
                        "会话回退提交失败，且副作用反向恢复失败：{}",
                        revert_error.message()
                    ));
                }
            }
            Err(error.to_string())
        }
    }
}
