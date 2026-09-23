//! 运行记录与事件流：单进程（`workers=1`）语义下的唯一事实来源。
//!
//! 对应 `omnicrawl/api/models.py` 的 `RunState` / `RunEvent` / `PendingConfirmation` /
//! `PendingQuestion`，以及 `service.py` 里围绕它们的保留上限、事件游标与决策规则。
//! 跨进程共享存储（`shared_store.py` 的多 worker 语义）不在这里，见 crate README。

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::ApiError;

/// 运行记录保留上限（`max_retained_runs`），活动任务不因上限被回收。
pub const DEFAULT_MAX_RETAINED_RUNS: usize = 100;
/// 每个运行或会话级事件流保留的最近事件数（`max_events_per_run`）。
pub const DEFAULT_MAX_EVENTS_PER_RUN: usize = 2000;

/// 运行状态；字符串形状与 Python 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Pending,
    Running,
    WaitingConfirmation,
    WaitingUser,
    Completed,
    Cancelled,
    Failed,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::WaitingConfirmation => "waiting_confirmation",
            Self::WaitingUser => "waiting_user",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
    }

    /// 活动任务：占用「单活动生成」名额，也拦截修改类接口。
    pub fn is_active(self) -> bool {
        !self.is_terminal()
    }

    /// 字符串 → 状态（持久化后端回读用）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "waiting_confirmation" => Some(Self::WaitingConfirmation),
            "waiting_user" => Some(Self::WaitingUser),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// 一个 SSE 事件。
#[derive(Debug, Clone, PartialEq)]
pub struct RunEvent {
    pub id: u64,
    pub event: String,
    pub data: Value,
}

impl RunEvent {
    /// SSE 分帧：`id` / `event` / `data` 三行加空行，JSON 紧凑输出（与 Python 同形）。
    pub fn to_sse(&self) -> String {
        let payload = serde_json::to_string(&self.data).unwrap_or_else(|_| "null".to_string());
        format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.id, self.event, payload
        )
    }
}

/// 执行清单的一条（`update_todos` 的投影结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoSnapshot {
    pub id: String,
    pub step: String,
    pub completed: bool,
}

impl TodoSnapshot {
    pub fn to_value(&self) -> Value {
        json!({"id": self.id, "step": self.step, "completed": self.completed})
    }
}

/// 等待人工决定的工具审批。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingConfirmation {
    pub confirmation_id: String,
    pub tool: String,
    /// 已按 `public_tool_arguments` 投影过的参数，可直接进事件与响应体。
    pub arguments: Value,
    pub decision: Option<bool>,
}

impl PendingConfirmation {
    pub fn resolved(&self) -> bool {
        self.decision.is_some()
    }
}

/// 等待人工作答的提问；`resolved` 与 `answer` 分开记，因为「超时未作答」也算已处理。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingQuestion {
    pub question_id: String,
    pub kind: String,
    pub question: String,
    pub options: Vec<String>,
    pub answer: Option<String>,
    pub resolved: bool,
}

/// 一条运行记录。
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub run_id: String,
    pub message: String,
    pub session_id: String,
    pub status: RunStatus,
    pub created_at: f64,
    pub updated_at: f64,
    pub result: String,
    pub error: String,
    pub todos: Vec<TodoSnapshot>,
    pub cancel_requested: bool,
    pub events: Vec<RunEvent>,
    pub(crate) next_event_id: u64,
    pub(crate) confirmations: Vec<PendingConfirmation>,
    pub(crate) questions: Vec<PendingQuestion>,
}

/// 从各部件拼回一条记录：持久化后端（`shared_store`）与内存后端共用同一形状。
pub(crate) struct RunRecordParts {
    pub run_id: String,
    pub message: String,
    pub session_id: String,
    pub status: RunStatus,
    pub created_at: f64,
    pub updated_at: f64,
    pub result: String,
    pub error: String,
    pub todos: Vec<TodoSnapshot>,
    pub cancel_requested: bool,
    pub events: Vec<RunEvent>,
    pub next_event_id: u64,
    pub confirmations: Vec<PendingConfirmation>,
    pub questions: Vec<PendingQuestion>,
}

impl RunRecord {
    /// 按部件构造（持久化后端的反序列化入口）。
    pub(crate) fn from_parts(parts: RunRecordParts) -> Self {
        Self {
            run_id: parts.run_id,
            message: parts.message,
            session_id: parts.session_id,
            status: parts.status,
            created_at: parts.created_at,
            updated_at: parts.updated_at,
            result: parts.result,
            error: parts.error,
            todos: parts.todos,
            cancel_requested: parts.cancel_requested,
            events: parts.events,
            next_event_id: parts.next_event_id,
            confirmations: parts.confirmations,
            questions: parts.questions,
        }
    }
    fn new(run_id: String, message: String, session_id: String, now: f64) -> Self {
        Self {
            run_id,
            message,
            session_id,
            status: RunStatus::Pending,
            created_at: now,
            updated_at: now,
            result: String::new(),
            error: String::new(),
            todos: Vec::new(),
            cancel_requested: false,
            events: Vec::new(),
            next_event_id: 1,
            confirmations: Vec::new(),
            questions: Vec::new(),
        }
    }

    pub fn confirmations(&self) -> &[PendingConfirmation] {
        &self.confirmations
    }

    pub fn questions(&self) -> &[PendingQuestion] {
        &self.questions
    }

    /// `GET /runs/{id}` 的响应体（对应 Python 的 `RunState.summary`）。
    pub fn summary(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "session_id": self.session_id,
            "status": self.status.as_str(),
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "result": self.result,
            "error": self.error,
            "todo_items": self.todos.iter().map(TodoSnapshot::to_value).collect::<Vec<Value>>(),
        })
    }
}

/// 运行记录层的错误；每个变体都带着对外的错误码与文案。
#[derive(Debug, Clone, PartialEq)]
pub enum RunStoreError {
    RunNotFound(String),
    RunActive(String),
    CursorExpired(u64),
    ConfirmationNotFound(String),
    ConfirmationResolved,
    QuestionNotFound(String),
    QuestionResolved,
    InvalidAnswer,
    /// 持久化后端不可用（读、写、加锁失败）。
    StoreFailure(String),
}

impl RunStoreError {
    pub fn to_api_error(&self) -> ApiError {
        match self {
            Self::RunNotFound(run_id) => ApiError::new(
                "RUN_NOT_FOUND",
                format!("生成任务不存在：{run_id}"),
                StatusCode::NOT_FOUND,
                None,
            ),
            Self::RunActive(run_id) => ApiError::new(
                "RUN_ACTIVE",
                "当前已有生成任务运行，暂不能开始新的生成任务。",
                StatusCode::CONFLICT,
                Some(json!({"run_id": run_id})),
            ),
            Self::CursorExpired(earliest) => ApiError::new(
                "EVENT_CURSOR_EXPIRED",
                "请求的事件游标已超出内存保留窗口，请重新获取任务状态。",
                StatusCode::CONFLICT,
                Some(json!({"earliest_event_id": earliest})),
            ),
            Self::ConfirmationNotFound(id) => ApiError::new(
                "CONFIRMATION_NOT_FOUND",
                format!("确认请求不存在：{id}"),
                StatusCode::NOT_FOUND,
                None,
            ),
            Self::ConfirmationResolved => ApiError::new(
                "CONFIRMATION_RESOLVED",
                "该确认请求已经处理。",
                StatusCode::CONFLICT,
                None,
            ),
            Self::QuestionNotFound(id) => ApiError::new(
                "QUESTION_NOT_FOUND",
                format!("提问请求不存在：{id}"),
                StatusCode::NOT_FOUND,
                None,
            ),
            Self::QuestionResolved => ApiError::new(
                "QUESTION_RESOLVED",
                "该提问请求已经处理。",
                StatusCode::CONFLICT,
                None,
            ),
            Self::InvalidAnswer => {
                ApiError::bad_request("INVALID_ANSWER", "answer 必须是 options 中的选项。")
            }
            Self::StoreFailure(message) => ApiError::new(
                "RUN_STORE_FAILED",
                format!("运行状态存储不可用：{message}"),
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
            ),
        }
    }
}

/// 运行记录仓库：单进程内的唯一写者，超出保留窗口的历史按时间淘汰。
pub struct RunStore {
    inner: Mutex<Inner>,
    max_runs: usize,
    max_events: usize,
}

#[derive(Default)]
struct Inner {
    runs: Vec<RunRecord>,
    active_run_id: String,
}

impl RunStore {
    pub fn new(max_retained_runs: usize, max_events_per_run: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_runs: max_retained_runs.max(1),
            max_events: max_events_per_run.max(10),
        }
    }

    /// 受理一个新运行；已有活动运行或服务关闭时拒绝。
    pub fn create(
        &self,
        run_id: &str,
        message: &str,
        session_id: &str,
    ) -> Result<RunRecord, RunStoreError> {
        let mut inner = self.lock();
        if let Some(active) = inner.active() {
            return Err(RunStoreError::RunActive(active.run_id.clone()));
        }
        let record = RunRecord::new(
            run_id.to_string(),
            message.to_string(),
            session_id.to_string(),
            now_seconds(),
        );
        inner.runs.push(record.clone());
        inner.active_run_id = run_id.to_string();
        prune(&mut inner, self.max_runs);
        Ok(record)
    }

    pub fn get(&self, run_id: &str) -> Result<RunRecord, RunStoreError> {
        self.lock()
            .find(run_id)
            .cloned()
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))
    }

    /// 最近更新的运行记录（按 `updated_at`）。
    pub fn latest_run(&self) -> Result<Option<RunRecord>, RunStoreError> {
        Ok(self
            .lock()
            .runs
            .iter()
            .max_by(|left, right| {
                left.updated_at
                    .partial_cmp(&right.updated_at)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned())
    }

    pub fn active_run_id(&self) -> Option<String> {
        self.lock().active().map(|run| run.run_id.clone())
    }

    /// 活动运行仍处于活动状态时返回错误（修改类接口的守卫）。
    pub fn ensure_mutation_allowed(&self) -> Result<(), RunStoreError> {
        match self.lock().active() {
            Some(run) if run.status.is_active() => {
                Err(RunStoreError::RunActive(run.run_id.clone()))
            }
            _ => Ok(()),
        }
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) -> Result<(), RunStoreError> {
        self.update(run_id, |run| {
            run.status = status;
            run.updated_at = now_seconds();
        })
    }

    /// 终态收尾：写状态、结果或错误，并让出活动名额。
    pub fn finish(
        &self,
        run_id: &str,
        status: RunStatus,
        result: &str,
        error: &str,
    ) -> Result<(), RunStoreError> {
        let mut inner = self.lock();
        let active = inner.active_run_id.clone();
        let run = inner
            .find_mut(run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        run.status = status;
        run.result = result.to_string();
        run.error = error.to_string();
        run.updated_at = now_seconds();
        if active == run_id {
            inner.active_run_id.clear();
        }
        prune(&mut inner, self.max_runs);
        Ok(())
    }

    pub fn record_todos(
        &self,
        run_id: &str,
        todos: Vec<TodoSnapshot>,
    ) -> Result<(), RunStoreError> {
        self.update(run_id, |run| run.todos = todos)
    }

    /// 追加一个事件并分配递增 ID；超出保留窗口时丢最旧的。
    pub fn append_event(
        &self,
        run_id: &str,
        event: &str,
        data: Value,
    ) -> Result<RunEvent, RunStoreError> {
        let max_events = self.max_events;
        let mut inner = self.lock();
        let run = inner
            .find_mut(run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        let recorded = RunEvent {
            id: run.next_event_id,
            event: event.to_string(),
            data,
        };
        run.next_event_id += 1;
        run.events.push(recorded.clone());
        if run.events.len() > max_events {
            let excess = run.events.len() - max_events;
            run.events.drain(..excess);
        }
        run.updated_at = now_seconds();
        Ok(recorded)
    }

    /// 读取游标之后的事件；游标早于保留窗口时按契约报错。
    pub fn events_after(
        &self,
        run_id: &str,
        last_event_id: u64,
    ) -> Result<Vec<RunEvent>, RunStoreError> {
        let inner = self.lock();
        let run = inner
            .find(run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        ensure_cursor_available(&run.events, last_event_id)?;
        Ok(run
            .events
            .iter()
            .filter(|event| event.id > last_event_id)
            .cloned()
            .collect())
    }

    /// 取消：置位取消标记，并把待决审批与提问置为终态（迟到的决定会得到 409）。
    pub fn request_cancel(&self, run_id: &str) -> Result<(), RunStoreError> {
        self.update(run_id, |run| {
            run.cancel_requested = true;
            for confirmation in run.confirmations.iter_mut() {
                if confirmation.decision.is_none() {
                    confirmation.decision = Some(false);
                }
            }
            for question in run.questions.iter_mut() {
                if !question.resolved {
                    question.resolved = true;
                }
            }
        })
    }

    pub fn cancel_requested(&self, run_id: &str) -> Result<bool, RunStoreError> {
        Ok(self.get(run_id)?.cancel_requested)
    }

    pub fn register_confirmation(
        &self,
        run_id: &str,
        confirmation_id: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<(), RunStoreError> {
        self.update(run_id, |run| {
            run.confirmations.push(PendingConfirmation {
                confirmation_id: confirmation_id.to_string(),
                tool: tool.to_string(),
                arguments,
                decision: None,
            })
        })
    }

    /// 提交审批决议；已处理过的 id 报 409，不认识的 id 报 404。
    pub fn resolve_confirmation(
        &self,
        run_id: &str,
        confirmation_id: &str,
        approved: bool,
    ) -> Result<PendingConfirmation, RunStoreError> {
        let mut inner = self.lock();
        let run = inner
            .find_mut(run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        let confirmation = run
            .confirmations
            .iter_mut()
            .find(|item| item.confirmation_id == confirmation_id)
            .ok_or_else(|| RunStoreError::ConfirmationNotFound(confirmation_id.to_string()))?;
        if confirmation.resolved() {
            return Err(RunStoreError::ConfirmationResolved);
        }
        confirmation.decision = Some(approved);
        run.updated_at = now_seconds();
        Ok(confirmation.clone())
    }

    /// 提问超时或取消：置为已处理但没有答案（迟到的作答一律得到 409）。
    pub fn expire_question(&self, run_id: &str, question_id: &str) -> Result<(), RunStoreError> {
        let mut inner = self.lock();
        let run = inner
            .find_mut(run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        let question = run
            .questions
            .iter_mut()
            .find(|item| item.question_id == question_id)
            .ok_or_else(|| RunStoreError::QuestionNotFound(question_id.to_string()))?;
        question.resolved = true;
        run.updated_at = now_seconds();
        Ok(())
    }

    pub fn register_question(
        &self,
        run_id: &str,
        question_id: &str,
        kind: &str,
        question: &str,
        options: Vec<String>,
    ) -> Result<(), RunStoreError> {
        self.update(run_id, |run| {
            run.questions.push(PendingQuestion {
                question_id: question_id.to_string(),
                kind: kind.to_string(),
                question: question.to_string(),
                options,
                answer: None,
                resolved: false,
            })
        })
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
        let mut inner = self.lock();
        let run = inner
            .find_mut(run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        let question = run
            .questions
            .iter_mut()
            .find(|item| item.question_id == question_id)
            .ok_or_else(|| RunStoreError::QuestionNotFound(question_id.to_string()))?;
        if question.resolved {
            return Err(RunStoreError::QuestionResolved);
        }
        if question.kind == "select" && !question.options.iter().any(|item| item == text) {
            return Err(RunStoreError::InvalidAnswer);
        }
        question.answer = Some(text.to_string());
        question.resolved = true;
        run.updated_at = now_seconds();
        Ok(question.clone())
    }

    fn update(
        &self,
        run_id: &str,
        change: impl FnOnce(&mut RunRecord),
    ) -> Result<(), RunStoreError> {
        let mut inner = self.lock();
        let run = inner
            .find_mut(run_id)
            .ok_or_else(|| RunStoreError::RunNotFound(run_id.to_string()))?;
        change(run);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // 锁只在短临界区里持有，中毒说明别处 panic；继续用会掩盖真相，直接恢复现场。
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn find(&self, run_id: &str) -> Option<&RunRecord> {
        self.runs.iter().find(|run| run.run_id == run_id)
    }

    fn find_mut(&mut self, run_id: &str) -> Option<&mut RunRecord> {
        self.runs.iter_mut().find(|run| run.run_id == run_id)
    }

    fn active(&self) -> Option<&RunRecord> {
        if self.active_run_id.is_empty() {
            return None;
        }
        self.find(&self.active_run_id)
    }
}

/// 运行状态后端：默认单进程内存；`api.workers > 1` 时换成跨进程共享存储。
///
/// 两条路径的方法签名与错误类型完全一致，服务层因此只依赖这一层，不必分支；
/// 单进程（默认）仍是内存实现，行为与历史版本一致。
pub enum RunBackend {
    Memory(RunStore),
    Shared(crate::shared_store::SharedRunStore),
}

impl RunBackend {
    /// 单进程内存后端（默认）。
    pub fn memory(max_retained_runs: usize, max_events_per_run: usize) -> Self {
        Self::Memory(RunStore::new(max_retained_runs, max_events_per_run))
    }

    pub fn create(
        &self,
        run_id: &str,
        message: &str,
        session_id: &str,
    ) -> Result<RunRecord, RunStoreError> {
        match self {
            Self::Memory(store) => store.create(run_id, message, session_id),
            Self::Shared(store) => store.create(run_id, message, session_id),
        }
    }

    pub fn get(&self, run_id: &str) -> Result<RunRecord, RunStoreError> {
        match self {
            Self::Memory(store) => store.get(run_id),
            Self::Shared(store) => store.get(run_id),
        }
    }

    /// 跨进程共享后端句柄；内存后端返回 `None`。
    ///
    /// 会话级后台事件、任务来源与人工决策只有共享存储需要跨进程，单进程内存路径
    /// 仍由服务自己的会话事件流承载，因此统一从这里分支（对应 Python 服务里
    /// `self._store is not None` 的判断）。
    pub fn shared(&self) -> Option<&crate::shared_store::SharedRunStore> {
        match self {
            Self::Memory(_) => None,
            Self::Shared(store) => Some(store),
        }
    }

    /// 最近更新的运行记录（会话级事件流用它解析当前 Session）。
    pub fn latest_run(&self) -> Result<Option<RunRecord>, RunStoreError> {
        match self {
            Self::Memory(store) => store.latest_run(),
            Self::Shared(store) => store.latest_run(),
        }
    }

    pub fn active_run_id(&self) -> Option<String> {
        match self {
            Self::Memory(store) => store.active_run_id(),
            Self::Shared(store) => store.active_run_id(),
        }
    }

    pub fn ensure_mutation_allowed(&self) -> Result<(), RunStoreError> {
        match self {
            Self::Memory(store) => store.ensure_mutation_allowed(),
            Self::Shared(store) => store.ensure_mutation_allowed(),
        }
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) -> Result<(), RunStoreError> {
        match self {
            Self::Memory(store) => store.set_status(run_id, status),
            Self::Shared(store) => store.set_status(run_id, status),
        }
    }

    pub fn finish(
        &self,
        run_id: &str,
        status: RunStatus,
        result: &str,
        error: &str,
    ) -> Result<(), RunStoreError> {
        match self {
            Self::Memory(store) => store.finish(run_id, status, result, error),
            Self::Shared(store) => store.finish(run_id, status, result, error),
        }
    }

    pub fn record_todos(
        &self,
        run_id: &str,
        todos: Vec<TodoSnapshot>,
    ) -> Result<(), RunStoreError> {
        match self {
            Self::Memory(store) => store.record_todos(run_id, todos),
            Self::Shared(store) => store.record_todos(run_id, todos),
        }
    }

    pub fn append_event(
        &self,
        run_id: &str,
        event: &str,
        data: Value,
    ) -> Result<RunEvent, RunStoreError> {
        match self {
            Self::Memory(store) => store.append_event(run_id, event, data),
            Self::Shared(store) => store.append_event(run_id, event, data),
        }
    }

    pub fn events_after(&self, run_id: &str, cursor: u64) -> Result<Vec<RunEvent>, RunStoreError> {
        match self {
            Self::Memory(store) => store.events_after(run_id, cursor),
            Self::Shared(store) => store.events_after(run_id, cursor),
        }
    }

    pub fn request_cancel(&self, run_id: &str) -> Result<(), RunStoreError> {
        match self {
            Self::Memory(store) => store.request_cancel(run_id),
            Self::Shared(store) => store.request_cancel(run_id),
        }
    }

    pub fn cancel_requested(&self, run_id: &str) -> Result<bool, RunStoreError> {
        match self {
            Self::Memory(store) => store.cancel_requested(run_id),
            Self::Shared(store) => store.cancel_requested(run_id),
        }
    }

    pub fn register_confirmation(
        &self,
        run_id: &str,
        confirmation_id: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<(), RunStoreError> {
        match self {
            Self::Memory(store) => {
                store.register_confirmation(run_id, confirmation_id, tool, arguments)
            }
            Self::Shared(store) => {
                store.register_confirmation(run_id, confirmation_id, tool, arguments)
            }
        }
    }

    pub fn resolve_confirmation(
        &self,
        run_id: &str,
        confirmation_id: &str,
        approved: bool,
    ) -> Result<PendingConfirmation, RunStoreError> {
        match self {
            Self::Memory(store) => store.resolve_confirmation(run_id, confirmation_id, approved),
            Self::Shared(store) => store.resolve_confirmation(run_id, confirmation_id, approved),
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
        match self {
            Self::Memory(store) => {
                store.register_question(run_id, question_id, kind, question, options)
            }
            Self::Shared(store) => {
                store.register_question(run_id, question_id, kind, question, options)
            }
        }
    }

    pub fn expire_question(&self, run_id: &str, question_id: &str) -> Result<(), RunStoreError> {
        match self {
            Self::Memory(store) => store.expire_question(run_id, question_id),
            Self::Shared(store) => store.expire_question(run_id, question_id),
        }
    }

    pub fn resolve_question(
        &self,
        run_id: &str,
        question_id: &str,
        answer: &str,
    ) -> Result<PendingQuestion, RunStoreError> {
        match self {
            Self::Memory(store) => store.resolve_question(run_id, question_id, answer),
            Self::Shared(store) => store.resolve_question(run_id, question_id, answer),
        }
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn memory_backend_keeps_single_active_run() {
        let backend = RunBackend::memory(DEFAULT_MAX_RETAINED_RUNS, DEFAULT_MAX_EVENTS_PER_RUN);
        let first = backend.create("run-1", "第一条", "s1").expect("受理");
        assert_eq!(first.status, RunStatus::Pending);
        assert_eq!(backend.active_run_id().as_deref(), Some("run-1"));
        assert!(matches!(
            backend.create("run-2", "第二条", "s1"),
            Err(RunStoreError::RunActive(run_id)) if run_id == "run-1"
        ));
    }
}

/// 保留窗口淘汰：按创建时间留最新的，活动运行永不淘汰。
fn prune(inner: &mut Inner, max_runs: usize) {
    if inner.runs.len() <= max_runs {
        return;
    }
    let active = inner.active_run_id.clone();
    let mut order: Vec<usize> = (0..inner.runs.len()).collect();
    order.sort_by(|left, right| {
        inner.runs[*left]
            .created_at
            .partial_cmp(&inner.runs[*right].created_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut removable = inner.runs.len() - max_runs;
    let mut drop_flags = vec![false; inner.runs.len()];
    for index in order {
        if removable == 0 {
            break;
        }
        if !active.is_empty() && inner.runs[index].run_id == active {
            continue;
        }
        drop_flags[index] = true;
        removable -= 1;
    }
    let mut index = 0;
    inner.runs.retain(|_| {
        let keep = !drop_flags[index];
        index += 1;
        keep
    });
}

/// 游标早于保留窗口时拒绝重放（与 Python `_ensure_event_cursor_available` 同规则）。
pub(crate) fn ensure_cursor_available(
    events: &[RunEvent],
    last_event_id: u64,
) -> Result<(), RunStoreError> {
    let Some(earliest) = events.first().map(|event| event.id) else {
        return Ok(());
    };
    if last_event_id == 0 || last_event_id >= earliest.saturating_sub(1) {
        return Ok(());
    }
    Err(RunStoreError::CursorExpired(earliest))
}

/// 当前时间（epoch 秒，带小数），与 Python `time.time()` 同尺度。
pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or_default()
}

/// 24 位十六进制的不透明 ID（对应 Python 的 `secrets.token_hex(12)` 形状）。
///
/// 不是密码学随机源：ID 只需要在本机不撞车，凭据绝不走这条路径。
pub fn random_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = Sha256::new();
    hasher.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let digest = hasher.finalize();
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> RunStore {
        RunStore::new(DEFAULT_MAX_RETAINED_RUNS, DEFAULT_MAX_EVENTS_PER_RUN)
    }

    #[test]
    fn create_allows_only_one_active_run() {
        let store = store();
        store
            .create("run-1", "你好", "s1")
            .expect("首个运行应当受理");
        assert_eq!(store.active_run_id().as_deref(), Some("run-1"));

        let error = store
            .create("run-2", "再问", "s1")
            .expect_err("第二个必须被拒");
        assert_eq!(error, RunStoreError::RunActive("run-1".to_string()));
        assert_eq!(
            error.to_api_error().code,
            "RUN_ACTIVE".to_string(),
            "冲突码必须稳定"
        );

        store
            .finish("run-1", RunStatus::Completed, "完成", "")
            .expect("收尾");
        assert_eq!(store.active_run_id(), None);
        store.create("run-2", "再问", "s1").expect("活动名额已让出");
    }

    #[test]
    fn events_are_numbered_and_serialised_as_sse() {
        let store = store();
        store.create("run-1", "你好", "s1").expect("受理");
        let first = store
            .append_event("run-1", "run.started", json!({"run_id": "run-1"}))
            .expect("追加");
        let second = store
            .append_event("run-1", "assistant.delta", json!({"delta": "你好"}))
            .expect("追加");

        assert_eq!((first.id, second.id), (1, 2));
        assert_eq!(
            first.to_sse(),
            "id: 1\nevent: run.started\ndata: {\"run_id\":\"run-1\"}\n\n"
        );
        let replay = store.events_after("run-1", 1).expect("重放");
        assert_eq!(replay, vec![second]);
        assert!(store.events_after("run-1", 0).expect("全量").len() == 2);
    }

    #[test]
    fn event_retention_expires_old_cursors() {
        let store = RunStore::new(DEFAULT_MAX_RETAINED_RUNS, 10);
        store.create("run-1", "你好", "s1").expect("受理");
        for index in 0..15 {
            store
                .append_event("run-1", "assistant.delta", json!({"i": index}))
                .expect("追加");
        }
        let kept = store.events_after("run-1", 0).expect("全量");
        assert_eq!(kept.len(), 10);
        assert_eq!(kept[0].id, 6);
        assert!(store.events_after("run-1", 5).is_ok(), "窗口边界仍可重放");
        assert_eq!(
            store.events_after("run-1", 4).expect_err("过期游标"),
            RunStoreError::CursorExpired(6)
        );
    }

    #[test]
    fn confirmations_resolve_once() {
        let store = store();
        store.create("run-1", "你好", "s1").expect("受理");
        store
            .register_confirmation("run-1", "c1", "bash", json!({"command": "rm -rf /"}))
            .expect("登记");
        store
            .resolve_confirmation("run-1", "c1", true)
            .expect("首次决议应当成功");

        assert_eq!(
            store
                .resolve_confirmation("run-1", "c1", false)
                .expect_err("重复决议"),
            RunStoreError::ConfirmationResolved
        );
        assert_eq!(
            store
                .resolve_confirmation("run-1", "missing", true)
                .expect_err("未知 id"),
            RunStoreError::ConfirmationNotFound("missing".to_string())
        );
    }

    #[test]
    fn cancel_settles_pending_decisions() {
        let store = store();
        store.create("run-1", "你好", "s1").expect("受理");
        store
            .register_confirmation("run-1", "c1", "bash", json!({}))
            .expect("登记");
        store
            .register_question("run-1", "q1", "select", "选哪个？", vec!["A".into()])
            .expect("登记");

        store.request_cancel("run-1").expect("取消");
        assert!(store.cancel_requested("run-1").expect("读标记"));
        let run = store.get("run-1").expect("读记录");
        assert_eq!(run.confirmations()[0].decision, Some(false));
        assert!(run.questions()[0].resolved, "取消后提问也算已处理");
        assert_eq!(
            store
                .resolve_question("run-1", "q1", "A")
                .expect_err("已处理的提问不能再答"),
            RunStoreError::QuestionResolved
        );
    }

    #[test]
    fn select_questions_only_accept_declared_options() {
        let store = store();
        store.create("run-1", "你好", "s1").expect("受理");
        store
            .register_question(
                "run-1",
                "q1",
                "select",
                "选哪个？",
                vec!["A".into(), "B".into()],
            )
            .expect("登记");

        assert_eq!(
            store
                .resolve_question("run-1", "q1", "C")
                .expect_err("不在选项里"),
            RunStoreError::InvalidAnswer
        );
        let answered = store.resolve_question("run-1", "q1", " B ").expect("作答");
        assert_eq!(answered.answer.as_deref(), Some("B"));
    }

    #[test]
    fn free_form_questions_accept_any_answer() {
        let store = store();
        store.create("run-1", "你好", "s1").expect("受理");
        store
            .register_question("run-1", "q1", "question", "补充信息", Vec::new())
            .expect("登记");
        let answered = store
            .resolve_question("run-1", "q1", "自定义答案")
            .expect("作答");
        assert_eq!(answered.answer.as_deref(), Some("自定义答案"));
        assert_eq!(
            store
                .resolve_question("run-1", "q1", "  ")
                .expect_err("空答案"),
            RunStoreError::InvalidAnswer
        );
    }

    #[test]
    fn unknown_run_reports_not_found() {
        let store = store();
        let error = store.get("missing").expect_err("未知运行");
        assert_eq!(error, RunStoreError::RunNotFound("missing".to_string()));
        let api = error.to_api_error();
        assert_eq!(api.status, StatusCode::NOT_FOUND);
        assert_eq!(api.message, "生成任务不存在：missing");
    }

    #[test]
    fn summary_carries_todos_and_status() {
        let store = store();
        store.create("run-1", "你好", "s1").expect("受理");
        store
            .record_todos(
                "run-1",
                vec![TodoSnapshot {
                    id: "1".to_string(),
                    step: "写测试".to_string(),
                    completed: false,
                }],
            )
            .expect("记录清单");
        store
            .set_status("run-1", RunStatus::Running)
            .expect("置状态");
        let summary = store.get("run-1").expect("读记录").summary();
        assert_eq!(summary["status"], "running");
        assert_eq!(summary["todo_items"][0]["step"], "写测试");
        assert_eq!(summary["session_id"], "s1");
    }

    #[test]
    fn retention_keeps_newest_runs() {
        let store = RunStore::new(2, DEFAULT_MAX_EVENTS_PER_RUN);
        store.create("run-1", "一", "s1").expect("受理");
        store
            .finish("run-1", RunStatus::Completed, "", "")
            .expect("收尾");
        store.create("run-2", "二", "s1").expect("受理");
        store
            .finish("run-2", RunStatus::Completed, "", "")
            .expect("收尾");
        store.create("run-3", "三", "s1").expect("受理");

        assert_eq!(
            store.get("run-1").expect_err("最旧的记录应当被淘汰"),
            RunStoreError::RunNotFound("run-1".to_string())
        );
        assert!(store.get("run-2").is_ok());
        assert!(store.get("run-3").is_ok());
    }

    #[test]
    fn random_ids_are_unique_hex() {
        let first = random_id();
        let second = random_id();
        assert_eq!(first.len(), 24);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
