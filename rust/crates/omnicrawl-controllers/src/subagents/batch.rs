//! `omnicrawl/agent/subagents/coordinator.py` 的批次编排状态面。
//!
//! Python 侧 `SubAgentCoordinator` 的同步批次用 `ThreadPoolExecutor` + `Future` 表达「哪些任务
//! 已经交给 worker」；本模块把这层状态收进内核：取消令牌、登记与完成计数、未登记索引、完成等待
//! 都是纯状态机，真正起线程或跑模型仍由宿主负责。
//!
//! 背景任务（`action=spawn`）的线程池在 [`crate::subagents::tasks`]，这里只放它的准入判定与
//! 汇总投影。

use std::sync::{Arc, Condvar, Mutex};

use omnicrawl_config::features::subagents::SubAgentConfig;
use serde_json::{json, Value};

use crate::subagents::coordinator::{top_level_error, PreparedTaskView};
use crate::subagents::definitions::AgentDefinition;

/// `run` 在功能未启用时的顶层错误。
pub const DISABLED_MESSAGE: &str =
    "SubAgent 功能未启用。请在配置中显式设置 subagents.enabled=true。";
/// 后台任务默认关闭时的顶层错误。
pub const BACKGROUND_DISABLED_MESSAGE: &str =
    "后台 SubAgent 默认关闭，请在配置中显式设置 allow_background=true。";
/// 后台批次拒绝 `fail_fast`。
pub const BACKGROUND_FAIL_FAST_MESSAGE: &str = "后台批次暂不支持 fail_fast=true。";
/// 关闭或切换工作区期间拒绝新批次。
pub const NOT_ACCEPTING_MESSAGE: &str = "SubAgent 正在关闭或切换工作区，暂不接受新任务。";
/// 父任务取消子任务时写入批次的原因。
pub const PARENT_CANCEL_REASON: &str = "父任务已取消子任务。";
/// 未登记任务在异常收尾时使用的原因。
pub const UNSUBMITTED_CANCEL_REASON: &str = "父任务已取消尚未调度的子任务。";
/// 批次取消时的默认原因。
pub const BATCH_CANCEL_REASON: &str = "子任务批次已取消。";

/// `_ActiveBatch`：同步批次的取消令牌与 worker 存活状态。
pub struct ActiveBatch {
    pub batch_id: String,
    pub task_count: usize,
    state: Mutex<BatchState>,
    done: Condvar,
}

#[derive(Debug, Default)]
struct BatchState {
    cancel_reason: String,
    cancelled: bool,
    cancel_dispatch_started: bool,
    registered: Vec<usize>,
    finished: Vec<usize>,
}

impl ActiveBatch {
    pub fn new(batch_id: impl Into<String>, task_count: usize) -> Self {
        Self {
            batch_id: batch_id.into(),
            task_count,
            state: Mutex::new(BatchState::default()),
            done: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BatchState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// `register_future`：登记 worker，并返回登记时批次是否已经进入取消态。
    pub fn register(&self, index: usize) -> bool {
        let mut state = self.lock();
        if !state.registered.contains(&index) {
            state.registered.push(index);
        }
        state.cancelled
    }

    /// `begin_cancel`：立即设置取消令牌，并告知调用方是否需启动一次异步收尾。
    pub fn begin_cancel(&self, reason: &str) -> bool {
        let mut state = self.lock();
        if state.cancel_reason.is_empty() {
            state.cancel_reason = reason.to_string();
        }
        state.cancelled = true;
        if state.cancel_dispatch_started {
            return false;
        }
        state.cancel_dispatch_started = true;
        true
    }

    /// `mark_finished`：记录一个任务不再占用 worker；全部结束时置位完成状态。
    pub fn mark_finished(&self, index: usize) -> bool {
        let finished = {
            let mut state = self.lock();
            if !state.finished.contains(&index) {
                state.finished.push(index);
            }
            state.finished.len() == self.task_count
        };
        if finished {
            self.done.notify_all();
        }
        finished
    }

    /// `untracked_indexes`：尚未登记为 worker 的任务索引。
    pub fn untracked_indexes(&self) -> Vec<usize> {
        let state = self.lock();
        (0..self.task_count)
            .filter(|index| !state.registered.contains(index) && !state.finished.contains(index))
            .collect()
    }

    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// `batch.cancel_reason` 的原始取值（未取消时为空串）。
    pub fn raw_cancel_reason(&self) -> String {
        self.lock().cancel_reason.clone()
    }

    /// 收尾文案使用的原因；未取消时回落到默认原因。
    pub fn cancel_reason(&self) -> String {
        let state = self.lock();
        if state.cancel_reason.is_empty() {
            BATCH_CANCEL_REASON.to_string()
        } else {
            state.cancel_reason.clone()
        }
    }

    pub fn is_done(&self) -> bool {
        self.lock().finished.len() == self.task_count
    }

    /// 有界等待整批 worker 收尾。
    pub fn wait_done(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.lock();
        while state.finished.len() != self.task_count {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (next, _) = self
                .done
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner());
            state = next;
        }
        true
    }
}

/// `_requires_shared_writer_lock`：shared isolation + standard 写权限才需要主工作区单写锁。
pub fn requires_shared_writer_lock(
    definition: &AgentDefinition,
    isolation: &str,
    tool_names: &[String],
) -> bool {
    let isolation = if isolation.is_empty() {
        definition.isolation.as_str()
    } else {
        isolation
    };
    if isolation != "shared" {
        return false;
    }
    if definition.permission_mode != "standard" {
        return false;
    }
    crate::subagents::coordinator::STANDARD_WRITE_TOOL_NAMES
        .iter()
        .any(|name| tool_names.iter().any(|item| item == name))
}

/// `_is_cancellation`：异常类型名里含 cancel。
pub fn is_cancellation_error(exception_type: &str) -> bool {
    exception_type.to_lowercase().contains("cancel")
}

/// `run` 在功能未启用时的返回值。
pub fn disabled_error() -> Value {
    top_level_error("SUBAGENT_DISABLED", DISABLED_MESSAGE)
}

/// 后台批次的准入判定；`Some` 表示拒绝。
pub fn background_rejection(config: &SubAgentConfig, fail_fast: bool) -> Option<Value> {
    if !config.allow_background {
        return Some(top_level_error(
            "SUBAGENT_BACKGROUND_DISABLED",
            BACKGROUND_DISABLED_MESSAGE,
        ));
    }
    if fail_fast {
        return Some(top_level_error(
            "SUBAGENT_PERMISSION_DENIED",
            BACKGROUND_FAIL_FAST_MESSAGE,
        ));
    }
    None
}

/// 批次整体状态：全完成 `completed`、部分完成 `partial`、否则 `failed`。
pub fn batch_status(results: &[Value]) -> &'static str {
    let completed = results
        .iter()
        .filter(|item| item.get("status").and_then(Value::as_str) == Some("completed"))
        .count();
    if completed == results.len() {
        "completed"
    } else if completed > 0 {
        "partial"
    } else {
        "failed"
    }
}

/// `run` 成功路径的返回值（`ok` 只在整批完成时为真）。
pub fn batch_summary(batch_id: &str, results: Vec<Value>) -> (bool, Value) {
    let status = batch_status(&results);
    (
        status == "completed",
        json!({
            "batch_id": batch_id,
            "status": status,
            "results": results,
        }),
    )
}

/// `_spawn_background` 的受理投影。
pub fn spawn_acknowledgement(batch_id: &str, task_ids: &[String]) -> Value {
    json!({
        "batch_id": batch_id,
        "task_ids": task_ids,
        "status": "queued",
    })
}

/// 批次创建事件的 payload。
pub fn batch_created_payload(
    batch_id: &str,
    task_count: usize,
    max_concurrency: i64,
    fail_fast: Option<bool>,
) -> Value {
    let mut payload = json!({
        "batch_id": batch_id,
        "status": "queued",
        "task_count": task_count,
        "max_concurrency": max_concurrency,
    });
    if let Some(flag) = fail_fast {
        if let Some(object) = payload.as_object_mut() {
            object.insert("fail_fast".to_string(), Value::Bool(flag));
        }
    }
    payload
}

/// `_run_all` 的取消收尾投影：被取消但未开始的索引列表。
pub fn cancelled_indexes(batch: &ActiveBatch) -> Vec<usize> {
    batch.untracked_indexes()
}

/// 同步批次的共享入口；宿主用它登记与收尾，内核不持有线程。
pub type SharedBatch = Arc<ActiveBatch>;

/// 用任务视图构造批次（索引与 `tasks` 顺序一一对应）。
pub fn build_batch(batch_id: &str, tasks: &[PreparedTaskView]) -> SharedBatch {
    Arc::new(ActiveBatch::new(batch_id, tasks.len()))
}
