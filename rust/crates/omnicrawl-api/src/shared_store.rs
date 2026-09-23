//! 跨进程共享的运行状态存储（`omnicrawl/api/shared_store.py` 的 Rust 等价实现）。
//!
//! Python 侧为多 worker（uvicorn `workers>1` 走 SO_REUSEPORT，内核按连接轮询、不保证粘性）而引入
//! SQLite：Run 元数据、事件流与人工决策只要留在进程内存里，多 worker 就会表现为随机 404、
//! 事件流断流与审批打不出去。Rust 服务端同样支持 `api.workers > 1` 的多进程监听
//!（监督进程 + SO_REUSEPORT，见 `main.rs` / `app.rs`），因此也不能把状态只放进进程内存。
//!
//! 与 Python 的差别只有存储介质：workspace 里没有 SQLite crate，引入 `rusqlite` 要新增依赖并编译
//! C 源（与本机禁编译、怕占盘的约束冲突），因此这里改成**单文件 JSON 快照 + 跨进程文件锁**：
//! 每次操作都在锁内完成「读文件 → 改内存 → 原子写回」，任何进程写完后其它进程立刻读得到，
//! 重启也不丢。语义与内存后端逐条对齐：单活动运行、事件递增 ID 与保留窗口、游标过期、
//! 审批/提问终态抢占（重复决议报冲突、未知 ID 报缺失）、取消把待决项一并置为终态。
//!
//! 与 Python 表一一对应的快照分区：`runs`（含内嵌的 events / confirmations / questions）、
//! `decisions`（跨进程决策投递）、`subagent_events` + `subagent_counters`（会话级后台事件
//! 跨进程可见，id 由独立计数器单调分配）、`task_sources`（task_id → 来源 Run / Session）。
//! 淘汰运行记录时连带清理它的 events / decisions / confirmations / questions / task_sources，
//! 与 Python `_delete_run` 同规则。

use std::path::{Path, PathBuf};

use omnicrawl_session::{atomic_write_text, ProcessFileLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runs::{
    ensure_cursor_available, now_seconds, RunEvent, RunRecord, RunRecordParts, RunStatus,
    RunStoreError, TodoSnapshot, DEFAULT_MAX_EVENTS_PER_RUN, DEFAULT_MAX_RETAINED_RUNS,
};
use crate::runs::{PendingConfirmation, PendingQuestion};

/// 跨进程锁的等待上限（对应 Python 的 `BUSY_TIMEOUT_MS`）。
const LOCK_TIMEOUT_SECONDS: f64 = 5.0;
/// 锁的重试间隔。
const LOCK_POLL_SECONDS: f64 = 0.02;

/// 孤儿运行收敛时的错误文案（对应 Python `reconcile_orphan_runs`）。
pub const ORPHAN_RUN_ERROR: &str = "拥有该生成任务的 API worker 已退出。";

/// 决策种类：取消 / 审批 / 提问（对应 Python `DECISION_*`）。
/// `target_id` 为对应的 `confirmation_id` 或 `question_id`，取消用空串。
pub const DECISION_CANCEL: &str = "cancel";
pub const DECISION_CONFIRM: &str = "confirm";
pub const DECISION_ANSWER: &str = "answer";

/// 文件名模板：按 `host:port` 区分，不同 API 实例互不串状态（与 Python 同规则）。
pub fn shared_run_store_filename(host: &str, port: u16) -> String {
    let safe_host: String = host
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '.' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    format!("api-runs-{safe_host}-{port}.json")
}

/// 任意 worker 投递给所有者的一条人工决策（对应 Python `decisions` 表与 `Decision`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub seq: u64,
    pub kind: String,
    pub target_id: String,
    pub payload: Value,
}

/// 跨进程共享的运行状态存储。
pub struct SharedRunStore {
    path: PathBuf,
    lock: ProcessFileLock,
    max_runs: usize,
    max_events: usize,
}

impl SharedRunStore {
    /// 打开（必要时创建）快照文件；父目录不存在时一并创建。
    pub fn open(
        path: impl Into<PathBuf>,
        max_retained_runs: usize,
        max_events_per_run: usize,
    ) -> Result<Self, RunStoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                RunStoreError::StoreFailure(format!("创建目录失败：{}，{error}", parent.display()))
            })?;
        }
        let lock = ProcessFileLock::new(path.with_extension("lock"));
        let store = Self {
            path,
            lock,
            max_runs: max_retained_runs.max(1),
            max_events: max_events_per_run.max(10),
        };
        // 首次打开就把快照落盘：后续操作都假定文件可读可写（这一步同样在锁内，避免与
        // 另一个进程的首次打开互相覆盖）。
        {
            let _guard = store.acquire()?;
            let document = store.read_locked()?;
            store.write_locked(&document)?;
        }
        Ok(store)
    }

    /// 默认保留上限的快捷构造。
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, RunStoreError> {
        Self::open(path, DEFAULT_MAX_RETAINED_RUNS, DEFAULT_MAX_EVENTS_PER_RUN)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // ---- 运行记录 ---------------------------------------------------------

    /// 受理一个新运行；已有活动运行或存储关闭时拒绝。
    ///
    /// 所有者 PID 记录为本进程：worker 崩溃后由 [`Self::reconcile_orphan_runs`] 收敛。
    pub fn create(
        &self,
        run_id: &str,
        message: &str,
        session_id: &str,
    ) -> Result<RunRecord, RunStoreError> {
        self.create_with_owner_pid(run_id, message, session_id, std::process::id() as i64)
    }

    /// 指定所有者 PID 的受理入口（测试与跨进程收敛用）。
    pub fn create_with_owner_pid(
        &self,
        run_id: &str,
        message: &str,
        session_id: &str,
        owner_pid: i64,
    ) -> Result<RunRecord, RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        if let Some(active) = active_of(&document) {
            return Err(RunStoreError::RunActive(active.run_id.clone()));
        }
        let now = now_seconds();
        document.runs.push(StoredRun {
            run_id: run_id.to_string(),
            message: message.to_string(),
            session_id: session_id.to_string(),
            owner_pid,
            status: RunStatus::Pending.as_str().to_string(),
            created_at: now,
            updated_at: now,
            result: String::new(),
            error: String::new(),
            todo_items: Vec::new(),
            cancel_requested: false,
            next_event_id: 1,
            events: Vec::new(),
            confirmations: Vec::new(),
            questions: Vec::new(),
        });
        prune(&mut document, self.max_runs);
        self.write_locked(&document)?;
        let record = document
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        record.to_record()
    }

    pub fn get(&self, run_id: &str) -> Result<RunRecord, RunStoreError> {
        let _guard = self.acquire()?;
        let document = self.read_locked()?;
        find_run(&document, run_id)?.to_record()
    }

    /// 把所有者进程已消失的活动运行标记为失败，避免客户端无限等待。
    ///
    /// worker 崩溃后它的内存运行状态随之消失，若不收敛，SSE 客户端会一直挂在
    /// `running` 上轮询。返回被收敛的运行数量（对应 Python `reconcile_orphan_runs`）。
    /// 没有孤儿时不写盘。
    pub fn reconcile_orphan_runs(
        &self,
        pid_alive: impl Fn(i64) -> bool,
    ) -> Result<usize, RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        let now = now_seconds();
        let mut reconciled = 0usize;
        for run in document.runs.iter_mut() {
            let active = RunStatus::parse(&run.status)
                .map(|status| status.is_active())
                .unwrap_or(false);
            if !active || pid_alive(run.owner_pid) {
                continue;
            }
            run.status = RunStatus::Failed.as_str().to_string();
            run.error = ORPHAN_RUN_ERROR.to_string();
            run.updated_at = now;
            reconciled += 1;
        }
        if reconciled > 0 {
            self.write_locked(&document)?;
        }
        Ok(reconciled)
    }

    /// 最近更新的运行记录（跨进程取 `updated_at` 最大者）。
    ///
    /// 会话级事件流用它解析「当前 API 可见的 Session」：多 worker 下产生事件的 Run
    /// 可能由别的 worker 持有，不能取本进程内核的会话。
    pub fn latest_run(&self) -> Result<Option<RunRecord>, RunStoreError> {
        let _guard = self.acquire()?;
        let document = self.read_locked()?;
        match document.runs.iter().max_by(|left, right| {
            left.updated_at
                .partial_cmp(&right.updated_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) {
            Some(run) => Ok(Some(run.to_record()?)),
            None => Ok(None),
        }
    }

    pub fn active_run_id(&self) -> Option<String> {
        let _guard = match self.acquire() {
            Ok(guard) => guard,
            Err(_) => return None,
        };
        let document = match self.read_locked() {
            Ok(document) => document,
            Err(_) => return None,
        };
        active_of(&document).map(|run| run.run_id.clone())
    }

    /// 活动运行仍处于活动状态时返回错误（修改类接口的守卫）。
    pub fn ensure_mutation_allowed(&self) -> Result<(), RunStoreError> {
        let _guard = self.acquire()?;
        let document = self.read_locked()?;
        match active_of(&document) {
            Some(run) => match RunStatus::parse(&run.status) {
                Some(status) if status.is_active() => {
                    Err(RunStoreError::RunActive(run.run_id.clone()))
                }
                _ => Ok(()),
            },
            None => Ok(()),
        }
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) -> Result<(), RunStoreError> {
        self.mutate(run_id, |run| {
            run.status = status.as_str().to_string();
            run.updated_at = now_seconds();
        })
    }

    /// 终态收尾：写状态、结果或错误。
    pub fn finish(
        &self,
        run_id: &str,
        status: RunStatus,
        result: &str,
        error: &str,
    ) -> Result<(), RunStoreError> {
        self.mutate(run_id, |run| {
            run.status = status.as_str().to_string();
            run.result = result.to_string();
            run.error = error.to_string();
            run.updated_at = now_seconds();
        })
    }

    pub fn record_todos(
        &self,
        run_id: &str,
        todos: Vec<TodoSnapshot>,
    ) -> Result<(), RunStoreError> {
        self.mutate(run_id, |run| {
            run.todo_items = todos.iter().map(TodoSnapshot::to_value).collect();
        })
    }

    /// 追加一个事件并分配递增 ID；超出保留窗口时丢最旧的。
    pub fn append_event(
        &self,
        run_id: &str,
        event: &str,
        data: Value,
    ) -> Result<RunEvent, RunStoreError> {
        let max_events = self.max_events;
        let mut recorded: Option<RunEvent> = None;
        self.mutate(run_id, |run| {
            let id = run.next_event_id.max(1);
            run.next_event_id = id + 1;
            run.events.push(StoredEvent {
                id,
                event: event.to_string(),
                data: data.clone(),
            });
            if run.events.len() > max_events {
                let excess = run.events.len() - max_events;
                run.events.drain(..excess);
            }
            run.updated_at = now_seconds();
            recorded = Some(RunEvent {
                id,
                event: event.to_string(),
                data,
            });
        })?;
        recorded.ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))
    }

    /// 读取游标之后的事件；游标早于保留窗口时按契约报错。
    pub fn events_after(
        &self,
        run_id: &str,
        last_event_id: u64,
    ) -> Result<Vec<RunEvent>, RunStoreError> {
        let _guard = self.acquire()?;
        let document = self.read_locked()?;
        let run = find_run(&document, run_id)?;
        let events: Vec<RunEvent> = run.events.iter().map(StoredEvent::to_event).collect();
        ensure_cursor_available(&events, last_event_id)?;
        Ok(events
            .into_iter()
            .filter(|event| event.id > last_event_id)
            .collect())
    }

    /// 取消：置位取消标记，并把待决审批与提问置为终态（迟到的决定会得到 409）。
    pub fn request_cancel(&self, run_id: &str) -> Result<(), RunStoreError> {
        self.mutate(run_id, |run| {
            run.cancel_requested = true;
            for confirmation in run.confirmations.iter_mut() {
                if confirmation.decision.is_none() {
                    confirmation.decision = Some(false);
                }
            }
            for question in run.questions.iter_mut() {
                question.resolved = true;
            }
        })
    }

    pub fn cancel_requested(&self, run_id: &str) -> Result<bool, RunStoreError> {
        Ok(self.get(run_id)?.cancel_requested)
    }

    // ---- 跨进程决策 -------------------------------------------------------

    /// 投递一条人工决策（取消 / 审批 / 提问）；序号单调递增。
    ///
    /// 对应 Python `push_decision`：非所有者 worker 处理完 HTTP 请求后把决定写进
    /// 共享存储，正在阻塞等待的所有者进程按 `run_id` 取走。
    pub fn push_decision(
        &self,
        run_id: &str,
        kind: &str,
        target_id: &str,
        payload: Value,
    ) -> Result<u64, RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        let seq = document.next_decision_seq.max(1);
        document.next_decision_seq = seq + 1;
        document.decisions.push(StoredDecision {
            seq,
            run_id: run_id.to_string(),
            kind: kind.to_string(),
            target_id: target_id.to_string(),
            payload,
            created_at: now_seconds(),
        });
        self.write_locked(&document)?;
        Ok(seq)
    }

    /// 取出并删除该 run 的待处理决策（只有所有者会调用，因此不会互相抢）。
    pub fn take_decisions(&self, run_id: &str) -> Result<Vec<Decision>, RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        let mut taken: Vec<Decision> = document
            .decisions
            .iter()
            .filter(|item| item.run_id == run_id)
            .map(|item| Decision {
                seq: item.seq,
                kind: item.kind.clone(),
                target_id: item.target_id.clone(),
                payload: item.payload.clone(),
            })
            .collect();
        if taken.is_empty() {
            return Ok(taken);
        }
        taken.sort_by_key(|item| item.seq);
        let highest = taken.last().map(|item| item.seq).unwrap_or_default();
        document
            .decisions
            .retain(|item| !(item.run_id == run_id && item.seq <= highest));
        self.write_locked(&document)?;
        Ok(taken)
    }

    // ---- 会话级后台任务事件 -----------------------------------------------

    /// 分配（并预留）下一个 Session 事件 id。
    ///
    /// 用独立计数器而不是「最大 id + 1」：不同 worker 可能为同一 Session 追加事件，
    /// 计数器在锁内自增天然互斥；事件被保留窗口裁剪后也不会让 id 从头开始。
    pub fn allocate_subagent_event_id(&self, session_id: &str) -> Result<u64, RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        let allocated = match document
            .subagent_counters
            .iter_mut()
            .find(|item| item.session_id == session_id)
        {
            Some(counter) => {
                let id = counter.next_id.max(1);
                counter.next_id = id + 1;
                id
            }
            None => {
                document.subagent_counters.push(StoredSubagentCounter {
                    session_id: session_id.to_string(),
                    next_id: 2,
                });
                1
            }
        };
        self.write_locked(&document)?;
        Ok(allocated)
    }

    /// 追加一个会话级后台事件；超出保留窗口时丢最旧的。
    pub fn append_subagent_event(
        &self,
        session_id: &str,
        event_id: u64,
        event: &str,
        data: Value,
    ) -> Result<(), RunStoreError> {
        let max_events = self.max_events as u64;
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        document
            .subagent_events
            .retain(|item| !(item.session_id == session_id && item.event_id == event_id));
        document.subagent_events.push(StoredSubagentEvent {
            session_id: session_id.to_string(),
            event_id,
            event: event.to_string(),
            data,
        });
        let cutoff = event_id.saturating_sub(max_events);
        document
            .subagent_events
            .retain(|item| item.session_id != session_id || item.event_id > cutoff);
        self.write_locked(&document)
    }

    /// 读取会话级事件游标之后的条目，按 id 升序。
    pub fn subagent_events_after(
        &self,
        session_id: &str,
        last_event_id: u64,
    ) -> Result<Vec<RunEvent>, RunStoreError> {
        let _guard = self.acquire()?;
        let document = self.read_locked()?;
        let mut events: Vec<RunEvent> = document
            .subagent_events
            .iter()
            .filter(|item| item.session_id == session_id && item.event_id > last_event_id)
            .map(StoredSubagentEvent::to_event)
            .collect();
        events.sort_by_key(|event| event.id);
        Ok(events)
    }

    /// 廉价的存在性检查：SSE 等待循环不需要把事件全部取回。
    pub fn has_subagent_events_after(&self, session_id: &str, last_event_id: u64) -> bool {
        let Ok(_guard) = self.acquire() else {
            return false;
        };
        match self.read_locked() {
            Ok(document) => document
                .subagent_events
                .iter()
                .any(|item| item.session_id == session_id && item.event_id > last_event_id),
            Err(_) => false,
        }
    }

    /// 该会话保留窗口里最早的事件 id；没有事件时为 `None`。
    pub fn earliest_subagent_event_id(&self, session_id: &str) -> Option<u64> {
        let _guard = self.acquire().ok()?;
        let document = self.read_locked().ok()?;
        document
            .subagent_events
            .iter()
            .filter(|item| item.session_id == session_id)
            .map(|item| item.event_id)
            .min()
    }

    // ---- 后台任务来源 -----------------------------------------------------

    /// 记录后台任务的来源 Run / Session（跨进程可见）。
    pub fn record_task_source(
        &self,
        task_id: &str,
        run_id: &str,
        session_id: &str,
    ) -> Result<(), RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        document.task_sources.retain(|item| item.task_id != task_id);
        document.task_sources.push(StoredTaskSource {
            task_id: task_id.to_string(),
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
        });
        self.write_locked(&document)
    }

    /// 查询任务来源；未记录时为 `None`。
    pub fn task_source(&self, task_id: &str) -> Result<Option<(String, String)>, RunStoreError> {
        let _guard = self.acquire()?;
        let document = self.read_locked()?;
        Ok(document
            .task_sources
            .iter()
            .find(|item| item.task_id == task_id)
            .map(|item| (item.run_id.clone(), item.session_id.clone())))
    }

    /// 任务进入终态后清除来源映射。
    pub fn drop_task_source(&self, task_id: &str) -> Result<(), RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        document.task_sources.retain(|item| item.task_id != task_id);
        self.write_locked(&document)
    }

    // ---- 审批与提问 -------------------------------------------------------

    pub fn register_confirmation(
        &self,
        run_id: &str,
        confirmation_id: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<(), RunStoreError> {
        self.mutate(run_id, |run| {
            run.confirmations
                .retain(|item| item.confirmation_id != confirmation_id);
            run.confirmations.push(StoredConfirmation {
                confirmation_id: confirmation_id.to_string(),
                tool: tool.to_string(),
                arguments,
                decision: None,
            });
        })
    }

    /// 提交审批决议；已处理过的 id 报 409，不认识的 id 报 404。
    pub fn resolve_confirmation(
        &self,
        run_id: &str,
        confirmation_id: &str,
        approved: bool,
    ) -> Result<PendingConfirmation, RunStoreError> {
        let mut resolved: Option<PendingConfirmation> = None;
        let mut failure: Option<RunStoreError> = None;
        self.mutate(run_id, |run| {
            let Some(index) = run
                .confirmations
                .iter()
                .position(|item| item.confirmation_id == confirmation_id)
            else {
                failure = Some(RunStoreError::ConfirmationNotFound(
                    confirmation_id.to_string(),
                ));
                return;
            };
            if run.confirmations[index].decision.is_some() {
                failure = Some(RunStoreError::ConfirmationResolved);
                return;
            }
            run.confirmations[index].decision = Some(approved);
            run.updated_at = now_seconds();
            resolved = Some(PendingConfirmation {
                confirmation_id: run.confirmations[index].confirmation_id.clone(),
                tool: run.confirmations[index].tool.clone(),
                arguments: run.confirmations[index].arguments.clone(),
                decision: Some(approved),
            });
        })?;
        match (resolved, failure) {
            (Some(confirmation), _) => Ok(confirmation),
            (None, Some(error)) => Err(error),
            (None, None) => Err(RunStoreError::ConfirmationNotFound(
                confirmation_id.to_string(),
            )),
        }
    }

    pub fn register_question(
        &self,
        run_id: &str,
        question_id: &str,
        kind: &str,
        question: &str,
        options: Vec<String>,
    ) -> Result<(), RunStoreError> {
        self.mutate(run_id, |run| {
            run.questions.retain(|item| item.question_id != question_id);
            run.questions.push(StoredQuestion {
                question_id: question_id.to_string(),
                kind: kind.to_string(),
                question: question.to_string(),
                options,
                answer: None,
                resolved: false,
            });
        })
    }

    /// 提问超时或取消：置为已处理但没有答案（迟到的作答一律得到 409）。
    pub fn expire_question(&self, run_id: &str, question_id: &str) -> Result<(), RunStoreError> {
        let mut failure: Option<RunStoreError> = None;
        self.mutate(run_id, |run| {
            let Some(index) = run
                .questions
                .iter()
                .position(|item| item.question_id == question_id)
            else {
                failure = Some(RunStoreError::QuestionNotFound(question_id.to_string()));
                return;
            };
            run.questions[index].resolved = true;
            run.updated_at = now_seconds();
        })?;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// 提交提问答案；`select` 只能提交已声明的选项。
    pub fn resolve_question(
        &self,
        run_id: &str,
        question_id: &str,
        answer: &str,
    ) -> Result<PendingQuestion, RunStoreError> {
        let text = answer.trim();
        if text.is_empty() {
            return Err(RunStoreError::InvalidAnswer);
        }
        let mut resolved: Option<PendingQuestion> = None;
        let mut failure: Option<RunStoreError> = None;
        self.mutate(run_id, |run| {
            let Some(index) = run
                .questions
                .iter()
                .position(|item| item.question_id == question_id)
            else {
                failure = Some(RunStoreError::QuestionNotFound(question_id.to_string()));
                return;
            };
            let question = &run.questions[index];
            if question.resolved {
                failure = Some(RunStoreError::QuestionResolved);
                return;
            }
            if question.kind == "select" && !question.options.iter().any(|item| item == text) {
                failure = Some(RunStoreError::InvalidAnswer);
                return;
            }
            run.questions[index].answer = Some(text.to_string());
            run.questions[index].resolved = true;
            run.updated_at = now_seconds();
            resolved = Some(PendingQuestion {
                question_id: run.questions[index].question_id.clone(),
                kind: run.questions[index].kind.clone(),
                question: run.questions[index].question.clone(),
                options: run.questions[index].options.clone(),
                answer: Some(text.to_string()),
                resolved: true,
            });
        })?;
        match (resolved, failure) {
            (Some(question), _) => Ok(question),
            (None, Some(error)) => Err(error),
            (None, None) => Err(RunStoreError::QuestionNotFound(question_id.to_string())),
        }
    }

    // ---- 内部：锁、读改写的公共路径 ---------------------------------------

    /// 持锁执行一次「读 → 改 → 原子写回」。
    fn mutate(
        &self,
        run_id: &str,
        change: impl FnOnce(&mut StoredRun),
    ) -> Result<(), RunStoreError> {
        let _guard = self.acquire()?;
        let mut document = self.read_locked()?;
        {
            let run = document
                .runs
                .iter_mut()
                .find(|run| run.run_id == run_id)
                .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
            change(run);
        }
        self.write_locked(&document)
    }

    fn acquire(&self) -> Result<omnicrawl_session::locking::ProcessLockGuard<'_>, RunStoreError> {
        self.lock
            .acquire(LOCK_TIMEOUT_SECONDS, LOCK_POLL_SECONDS)
            .map_err(|error| RunStoreError::StoreFailure(error.message().to_string()))
    }

    fn read_locked(&self) -> Result<StoreDocument, RunStoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(StoreDocument::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                RunStoreError::StoreFailure(format!(
                    "共享状态文件损坏：{}，{error}",
                    self.path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(StoreDocument::default())
            }
            Err(error) => Err(RunStoreError::StoreFailure(format!(
                "读取共享状态失败：{}，{error}",
                self.path.display()
            ))),
        }
    }

    fn write_locked(&self, document: &StoreDocument) -> Result<(), RunStoreError> {
        let text = serde_json::to_string(document)
            .map_err(|error| RunStoreError::StoreFailure(format!("序列化共享状态失败：{error}")))?;
        atomic_write_text(&self.path, &text, true).map_err(|error| {
            RunStoreError::StoreFailure(format!(
                "写入共享状态失败：{}，{error}",
                self.path.display()
            ))
        })
    }
}

/// 快照文档：运行记录 + 四张跨进程表（决策、会话级事件、事件计数器、任务来源）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreDocument {
    #[serde(default)]
    runs: Vec<StoredRun>,
    /// 跨进程人工决策投递队列（对应 Python `decisions`）。
    #[serde(default)]
    decisions: Vec<StoredDecision>,
    #[serde(default = "default_next_decision_seq")]
    next_decision_seq: u64,
    /// 会话级后台任务事件（对应 Python `subagent_events`）。
    #[serde(default)]
    subagent_events: Vec<StoredSubagentEvent>,
    /// 会话级事件 id 计数器（对应 Python `subagent_counters`）。
    #[serde(default)]
    subagent_counters: Vec<StoredSubagentCounter>,
    /// 后台任务来源映射（对应 Python `task_sources`）。
    #[serde(default)]
    task_sources: Vec<StoredTaskSource>,
}

fn default_next_decision_seq() -> u64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRun {
    run_id: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    session_id: String,
    status: String,
    /// 所有者进程 PID；崩溃后用于收敛孤儿运行。旧快照缺字段时按 0 处理（视为孤儿）。
    #[serde(default)]
    owner_pid: i64,
    #[serde(default)]
    created_at: f64,
    #[serde(default)]
    updated_at: f64,
    #[serde(default)]
    result: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    todo_items: Vec<Value>,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default = "default_next_event_id")]
    next_event_id: u64,
    #[serde(default)]
    events: Vec<StoredEvent>,
    #[serde(default)]
    confirmations: Vec<StoredConfirmation>,
    #[serde(default)]
    questions: Vec<StoredQuestion>,
}

fn default_next_event_id() -> u64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEvent {
    id: u64,
    event: String,
    #[serde(default)]
    data: Value,
}

impl StoredEvent {
    fn to_event(&self) -> RunEvent {
        RunEvent {
            id: self.id,
            event: self.event.clone(),
            data: self.data.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredConfirmation {
    confirmation_id: String,
    tool: String,
    #[serde(default)]
    arguments: Value,
    #[serde(default)]
    decision: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredQuestion {
    question_id: String,
    kind: String,
    question: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    resolved: bool,
}

/// `decisions` 表的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDecision {
    seq: u64,
    run_id: String,
    kind: String,
    #[serde(default)]
    target_id: String,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    created_at: f64,
}

/// `subagent_events` 表的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSubagentEvent {
    session_id: String,
    event_id: u64,
    event: String,
    #[serde(default)]
    data: Value,
}

impl StoredSubagentEvent {
    fn to_event(&self) -> RunEvent {
        RunEvent {
            id: self.event_id,
            event: self.event.clone(),
            data: self.data.clone(),
        }
    }
}

/// `subagent_counters` 表的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSubagentCounter {
    session_id: String,
    #[serde(default = "default_next_event_id")]
    next_id: u64,
}

/// `task_sources` 表的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTaskSource {
    task_id: String,
    run_id: String,
    session_id: String,
}

impl StoredRun {
    /// 磁盘记录 → 运行记录；状态字符串无法识别时按失败处理（不静默改写状态）。
    fn to_record(&self) -> Result<RunRecord, RunStoreError> {
        let status = RunStatus::parse(&self.status).ok_or_else(|| {
            RunStoreError::StoreFailure(format!("无法识别的运行状态：{}", self.status))
        })?;
        Ok(RunRecord::from_parts(RunRecordParts {
            run_id: self.run_id.clone(),
            message: self.message.clone(),
            session_id: self.session_id.clone(),
            status,
            created_at: self.created_at,
            updated_at: self.updated_at,
            result: self.result.clone(),
            error: self.error.clone(),
            todos: self.todo_items.iter().filter_map(todo_from_value).collect(),
            cancel_requested: self.cancel_requested,
            events: self.events.iter().map(StoredEvent::to_event).collect(),
            next_event_id: self.next_event_id.max(1),
            confirmations: self
                .confirmations
                .iter()
                .map(|item| PendingConfirmation {
                    confirmation_id: item.confirmation_id.clone(),
                    tool: item.tool.clone(),
                    arguments: item.arguments.clone(),
                    decision: item.decision,
                })
                .collect(),
            questions: self
                .questions
                .iter()
                .map(|item| PendingQuestion {
                    question_id: item.question_id.clone(),
                    kind: item.kind.clone(),
                    question: item.question.clone(),
                    options: item.options.clone(),
                    answer: item.answer.clone(),
                    resolved: item.resolved,
                })
                .collect(),
        }))
    }
}

/// 从存档条目还原执行清单项；形状不对的条目直接丢弃（与内存后端「只保留 dict」同义）。
fn todo_from_value(value: &Value) -> Option<TodoSnapshot> {
    let object = value.as_object()?;
    let step = object.get("step").and_then(Value::as_str)?;
    Some(TodoSnapshot {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        step: step.to_string(),
        completed: object
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn find_run<'a>(document: &'a StoreDocument, run_id: &str) -> Result<&'a StoredRun, RunStoreError> {
    document
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))
}

fn active_of(document: &StoreDocument) -> Option<&StoredRun> {
    document.runs.iter().rev().find(|run| {
        RunStatus::parse(&run.status)
            .map(|status| status.is_active())
            .unwrap_or(false)
    })
}

/// 保留窗口淘汰：按创建时间留最新的（与内存后端同规则）。
fn prune(document: &mut StoreDocument, max_runs: usize) {
    if document.runs.len() <= max_runs {
        return;
    }
    let mut order: Vec<usize> = (0..document.runs.len()).collect();
    order.sort_by(|left, right| {
        document.runs[*left]
            .created_at
            .partial_cmp(&document.runs[*right].created_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut removable = document.runs.len() - max_runs;
    let mut drop_flags = vec![false; document.runs.len()];
    for index in order {
        if removable == 0 {
            break;
        }
        let active = RunStatus::parse(&document.runs[index].status)
            .map(|status| status.is_active())
            .unwrap_or(false);
        if active {
            continue;
        }
        drop_flags[index] = true;
        removable -= 1;
    }
    let mut dropped: Vec<String> = Vec::new();
    let mut index = 0;
    document.runs.retain(|run| {
        let keep = !drop_flags[index];
        index += 1;
        if !keep {
            dropped.push(run.run_id.clone());
        }
        keep
    });
    if dropped.is_empty() {
        return;
    }
    // 连带清理被淘汰 run 的决策与任务来源（事件、审批、提问内嵌在 run 里自然消失）。
    document
        .decisions
        .retain(|item| !dropped.iter().any(|run_id| run_id == &item.run_id));
    document
        .task_sources
        .retain(|item| !dropped.iter().any(|run_id| run_id == &item.run_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunStatus;
    use serde_json::json;

    fn store(tag: &str) -> SharedRunStore {
        let path =
            std::env::temp_dir().join(format!("oc-api-shared-{}-{tag}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        SharedRunStore::open(path, DEFAULT_MAX_RETAINED_RUNS, DEFAULT_MAX_EVENTS_PER_RUN)
            .expect("打开共享存储")
    }

    #[test]
    fn decisions_round_trip_in_sequence_order() {
        let store = store("decisions");
        store
            .push_decision("r1", DECISION_CANCEL, "", json!({}))
            .expect("投递取消");
        store
            .push_decision("r1", DECISION_CONFIRM, "c1", json!({"approved": true}))
            .expect("投递审批");

        let taken = store.take_decisions("r1").expect("取走决策");
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].kind, DECISION_CANCEL);
        assert_eq!(taken[1].target_id, "c1");
        assert_eq!(taken[1].payload["approved"], json!(true));
        assert!(store.take_decisions("r1").expect("再取").is_empty());
    }

    #[test]
    fn subagent_event_ids_are_monotonic_per_session() {
        let store = store("subagent");
        assert_eq!(store.allocate_subagent_event_id("s1").expect("分配"), 1);
        assert_eq!(store.allocate_subagent_event_id("s1").expect("分配"), 2);
        store
            .append_subagent_event("s1", 1, "subagent.task.running", json!({"task_id": "t1"}))
            .expect("追加");
        store
            .append_subagent_event("s1", 2, "subagent.task.completed", json!({"task_id": "t1"}))
            .expect("追加");

        assert!(store.has_subagent_events_after("s1", 1));
        assert_eq!(store.earliest_subagent_event_id("s1"), Some(1));
        let events = store.subagent_events_after("s1", 1).expect("读取");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 2);
        // 会话隔离：别的会话看不到这些事件，各自的计数器也独立。
        assert!(store
            .subagent_events_after("s2", 0)
            .expect("读取")
            .is_empty());
        assert_eq!(store.allocate_subagent_event_id("s2").expect("分配"), 1);
    }

    #[test]
    fn task_sources_are_recorded_and_dropped() {
        let store = store("sources");
        store.record_task_source("t1", "r1", "s1").expect("记录");
        assert_eq!(
            store.task_source("t1").expect("查询"),
            Some(("r1".to_string(), "s1".to_string()))
        );
        store.drop_task_source("t1").expect("清除");
        assert_eq!(store.task_source("t1").expect("查询"), None);
    }

    #[test]
    fn pruning_runs_clears_related_decisions_and_sources() {
        let path =
            std::env::temp_dir().join(format!("oc-api-shared-{}-prune.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SharedRunStore::open(path, 1, DEFAULT_MAX_EVENTS_PER_RUN).expect("打开");
        store.create("r1", "第一条", "s1").expect("受理");
        store
            .finish("r1", RunStatus::Completed, "", "")
            .expect("收尾");
        store
            .push_decision("r1", DECISION_CANCEL, "", json!({}))
            .expect("投递");
        store.record_task_source("t1", "r1", "s1").expect("记录");

        // 第二个运行触发淘汰：最旧的终态 run 应连带清掉决策与任务来源。
        store.create("r2", "第二条", "s1").expect("受理");
        assert_eq!(
            store.get("r1").expect_err("应被淘汰"),
            RunStoreError::RunNotFound("r1".to_string())
        );
        assert!(store.take_decisions("r1").expect("取走").is_empty());
        assert_eq!(store.task_source("t1").expect("查询"), None);
    }

    #[test]
    fn create_records_the_owner_pid() {
        let store = store("owner-pid");
        let record = store.create("r1", "你好", "s1").expect("受理");
        assert_eq!(record.status, RunStatus::Pending);
        // 快照里记下本进程 PID：别人的收敛不会误杀活着的运行。
        let raw = std::fs::read_to_string(store.path()).expect("读取快照");
        let document: StoreDocument = serde_json::from_str(&raw).expect("解析快照");
        assert_eq!(document.runs[0].owner_pid, std::process::id() as i64);
    }

    #[test]
    fn reconcile_marks_runs_whose_owner_process_is_gone() {
        let store = store("reconcile");
        store
            .create_with_owner_pid("alive", "活着", "s1", 4242)
            .expect("受理");

        // 所有者仍在：不动。
        assert_eq!(
            store
                .reconcile_orphan_runs(|pid| pid == 4242)
                .expect("收敛"),
            0
        );
        assert_eq!(store.get("alive").expect("读取").status, RunStatus::Pending);

        // 所有者消失：标记为失败并保留可读原因，让客户端不再空等。
        assert_eq!(store.reconcile_orphan_runs(|_| false).expect("收敛"), 1);
        let record = store.get("alive").expect("读取");
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(record.error, ORPHAN_RUN_ERROR);
        // 收敛后活动名额让出，可以受理新运行。
        store.create("next", "再来", "s1").expect("受理新运行");
    }

    #[test]
    fn reconcile_ignores_terminal_runs_and_missing_owner() {
        let store = store("reconcile-terminal");
        store
            .create_with_owner_pid("done", "已完成", "s1", 4242)
            .expect("受理");
        store
            .finish("done", RunStatus::Completed, "好的", "")
            .expect("收尾");
        // 终态不受影响，即便所有者已消失。
        assert_eq!(store.reconcile_orphan_runs(|_| false).expect("收敛"), 0);
        assert_eq!(
            store.get("done").expect("读取").status,
            RunStatus::Completed
        );
        // 旧快照缺 owner_pid 时按 0 处理；`pid_is_running(0)` 为假，因此会被收敛。
        store
            .create_with_owner_pid("legacy", "旧记录", "s1", 0)
            .expect("受理");
        assert_eq!(store.reconcile_orphan_runs(|pid| pid > 0).expect("收敛"), 1);
        assert_eq!(store.get("legacy").expect("读取").status, RunStatus::Failed);
    }
}
