//! 触发记录与事件流：服务进程内的唯一事实来源。
//!
//! 一条记录就是一次「拉起 loop」的完整生命周期：入队 → 运行 → 终态。事件流与本地 API 的
//! `/runs/{id}/events` 同形（`id` / `event` / `data`），客户端可以复用同一套解析。
//!
//! 与运行记录（`omnicrawl_api::RunRecord`）的差别只有两处，都来自无人值守：
//! 没有人工审批与提问（因此没有 `waiting_*` 状态），并且并发由 slot 池而不是单活动名额控制。

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// 每个触发保留的最近事件数。超出时丢最旧的，终态事件永远是最新一条。
pub const MAX_EVENTS_PER_TRIGGER: usize = 2000;
/// 去重窗口：同一 `idempotency_key` 在这段时间内只受理一次。
pub const IDEMPOTENCY_WINDOW_SECONDS: f64 = 24.0 * 3600.0;
/// 记录正文进响应体时的截断上限：一条 transcript 不该把调用方的终端刷满。
pub const MAX_RESULT_CHARS: usize = 20000;

/// 触发状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TriggerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// 一条 SSE 事件。
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerEvent {
    pub id: u64,
    pub event: String,
    pub data: Value,
}

impl TriggerEvent {
    /// SSE 分帧：与本地 API 的 `RunEvent::to_sse` 同一格式。
    pub fn to_sse(&self) -> String {
        let payload = serde_json::to_string(&self.data).unwrap_or_else(|_| "null".to_string());
        format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.id, self.event, payload
        )
    }
}

/// 终态投递目标与结果。
#[derive(Debug, Clone, PartialEq)]
pub struct NotifySpec {
    /// `telegram` / `feishu` / `qqbot` / `none`。
    pub channel: String,
    /// 目标会话 ID；`telegram` 是整数，其余是字符串。
    pub target: String,
    /// `completed` / `failed` / `always`。
    pub on: String,
    /// 投递正文上限。
    pub max_chars: usize,
}

impl NotifySpec {
    /// 是否该在这个终态上投递。
    pub fn matches(&self, state: TriggerState) -> bool {
        if self.channel == "none" {
            return false;
        }
        match self.on.as_str() {
            "always" => true,
            "failed" => state == TriggerState::Failed,
            _ => state == TriggerState::Completed,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({"type": self.channel, "target": self.target, "on": self.on})
    }
}

/// 投递结果：`delivered` 为假时 `error` 给原因（不改触发状态）。
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyOutcome {
    pub delivered: bool,
    pub error: Option<String>,
}

impl NotifyOutcome {
    pub fn delivered() -> Self {
        Self {
            delivered: true,
            error: None,
        }
    }

    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            delivered: false,
            error: Some(reason.into()),
        }
    }

    pub fn skipped() -> Self {
        Self {
            delivered: false,
            error: None,
        }
    }
}

/// 一条触发记录。
#[derive(Debug, Clone)]
pub struct TriggerRecord {
    pub trigger_id: String,
    pub title: String,
    /// 注入后的完整输入（`[外部触发] …` + 正文），也是写进会话的用户消息。
    pub prompt: String,
    /// 调用方原始 `message`，用于列表展示与排障。
    pub raw_message: String,
    pub state: TriggerState,
    pub created_at: f64,
    pub started_at: f64,
    pub finished_at: f64,
    pub session_id: String,
    pub workspace: String,
    pub timeout_seconds: i64,
    pub session_key: String,
    pub idempotency_key: String,
    pub tags: Map<String, Value>,
    pub notify: Option<NotifySpec>,
    pub notify_outcome: Option<NotifyOutcome>,
    pub result: String,
    pub error: String,
    pub cancel_requested: bool,
    pub model_turns: u64,
    pub tool_calls: u64,
    events: Vec<TriggerEvent>,
    next_event_id: u64,
}

impl TriggerRecord {
    fn new(
        trigger_id: String,
        title: String,
        prompt: String,
        raw_message: String,
        workspace: String,
        timeout_seconds: i64,
        session_key: String,
        idempotency_key: String,
        tags: Map<String, Value>,
        notify: Option<NotifySpec>,
        now: f64,
    ) -> Self {
        Self {
            trigger_id,
            title,
            prompt,
            raw_message,
            state: TriggerState::Queued,
            created_at: now,
            started_at: 0.0,
            finished_at: 0.0,
            session_id: String::new(),
            workspace,
            timeout_seconds,
            session_key,
            idempotency_key,
            tags,
            notify,
            notify_outcome: None,
            result: String::new(),
            error: String::new(),
            cancel_requested: false,
            model_turns: 0,
            tool_calls: 0,
            events: Vec::new(),
            next_event_id: 1,
        }
    }

    /// 列表与状态端点共用的投影；`result` 按上限截断。
    pub fn summary(&self) -> Value {
        let mut body = json!({
            "trigger_id": self.trigger_id,
            "state": self.state.as_str(),
            "title": self.title,
            "message": truncate(&self.raw_message, MAX_RESULT_CHARS),
            "created_at": self.created_at,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "session_id": self.session_id,
            "result": truncate(&self.result, MAX_RESULT_CHARS),
            "error": self.error,
            "usage": {"model_turns": self.model_turns, "tool_calls": self.tool_calls},
            "tags": Value::Object(self.tags.clone()),
        });
        if let Some(notify) = &self.notify {
            let outcome = self
                .notify_outcome
                .clone()
                .unwrap_or(NotifyOutcome::skipped());
            if let Some(map) = body.as_object_mut() {
                map.insert(
                    "notify".to_string(),
                    json!({
                        "type": notify.channel,
                        "target": notify.target,
                        "on": notify.on,
                        "delivered": outcome.delivered,
                        "error": outcome.error,
                    }),
                );
            }
        }
        body
    }
}

/// 一条新触发的受理参数。
pub struct NewTrigger {
    pub trigger_id: String,
    pub title: String,
    pub prompt: String,
    pub raw_message: String,
    pub workspace: String,
    pub timeout_seconds: i64,
    pub session_key: String,
    pub idempotency_key: String,
    pub tags: Map<String, Value>,
    pub notify: Option<NotifySpec>,
}

/// 记录层错误，每个变体都带对外错误码与文案。
#[derive(Debug, Clone, PartialEq)]
pub enum RecordError {
    NotFound(String),
    QueueFull(usize),
    CursorExpired(u64),
}

impl RecordError {
    pub fn to_api_error(&self) -> crate::app::ApiError {
        match self {
            Self::NotFound(id) => crate::app::ApiError::not_found(
                "TRIGGER_NOT_FOUND",
                format!("触发记录不存在或已过保留期：{id}"),
            ),
            Self::QueueFull(limit) => crate::app::ApiError::new(
                "QUEUE_FULL",
                format!("排队已满（上限 {limit} 条），请稍后重试。"),
                axum::http::StatusCode::TOO_MANY_REQUESTS,
            ),
            Self::CursorExpired(earliest) => crate::app::ApiError::cursor_expired(*earliest),
        }
    }
}

/// 触发记录仓库：单进程内的唯一写者。
pub struct TriggerStore {
    inner: Mutex<Inner>,
    wake: Condvar,
    queue_limit: usize,
    retention_seconds: f64,
}

#[derive(Default)]
struct Inner {
    records: Vec<TriggerRecord>,
    /// 入队顺序（`trigger_id`）；出队即从队列里摘掉。
    queue: VecDeque<String>,
    /// 正在占用 slot 的 `trigger_id`。
    running: Vec<String>,
}

impl TriggerStore {
    pub fn new(queue_limit: usize, retention_hours: i64) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            wake: Condvar::new(),
            queue_limit: queue_limit.max(1),
            retention_seconds: (retention_hours.max(1) as f64) * 3600.0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 受理一个新触发。命中 24 小时内的相同 `idempotency_key` 时返回原记录并置 `duplicate`。
    ///
    /// 队列满时返回 [`RecordError::QueueFull`]，**不静默丢弃**：调用方据此重试或告警。
    pub fn submit(&self, spec: NewTrigger) -> Result<(TriggerRecord, bool), RecordError> {
        let mut inner = self.lock();
        purge_expired(&mut inner, self.retention_seconds);
        if !spec.idempotency_key.is_empty() {
            if let Some(existing) = inner
                .records
                .iter()
                .find(|record| record.idempotency_key == spec.idempotency_key)
                .cloned()
            {
                return Ok((existing, true));
            }
        }
        if inner.queue.len() >= self.queue_limit {
            return Err(RecordError::QueueFull(self.queue_limit));
        }
        let record = TriggerRecord::new(
            spec.trigger_id.clone(),
            spec.title,
            spec.prompt,
            spec.raw_message,
            spec.workspace,
            spec.timeout_seconds,
            spec.session_key,
            spec.idempotency_key,
            spec.tags,
            spec.notify,
            now_seconds(),
        );
        inner.queue.push_back(spec.trigger_id.clone());
        inner.records.push(record.clone());
        drop(inner);
        self.wake.notify_all();
        Ok((record, false))
    }

    pub fn get(&self, trigger_id: &str) -> Result<TriggerRecord, RecordError> {
        self.lock()
            .records
            .iter()
            .find(|record| record.trigger_id == trigger_id)
            .cloned()
            .ok_or_else(|| RecordError::NotFound(trigger_id.to_string()))
    }

    /// 列表：按创建时间倒序，可按状态与标签过滤。`tag` 形如 `k=v`。
    pub fn list(
        &self,
        state: Option<TriggerState>,
        tag: Option<(&str, &str)>,
        limit: usize,
    ) -> Vec<TriggerRecord> {
        let inner = self.lock();
        let mut records: Vec<TriggerRecord> = inner
            .records
            .iter()
            .filter(|record| state.is_none_or(|want| record.state == want))
            .filter(|record| match tag {
                None => true,
                Some((key, value)) => record
                    .tags
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual == value),
            })
            .cloned()
            .collect();
        records.sort_by(|left, right| {
            right
                .created_at
                .partial_cmp(&left.created_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        records.into_iter().take(limit.max(1)).collect()
    }

    /// 入队位置（1 起）；已在运行或已终态时返回 `None`。
    pub fn queue_position(&self, trigger_id: &str) -> Option<usize> {
        self.lock()
            .queue
            .iter()
            .position(|id| id == trigger_id)
            .map(|index| index + 1)
    }

    /// 取下一个可运行的触发：队列非空且有空闲 slot。
    ///
    /// 返回 `None` 表示此刻没有可跑的任务；调用方应当阻塞等待唤醒信号而不是轮询。
    ///
    /// `keyed` 模式下同一个 `session_key` 必须串行：否则两个 slot 会同时往同一份会话转录里写。
    /// 因此跳过「同 key 已在跑」的条目，让它留在队列里等前一个结束。
    pub fn take_next(&self, max_concurrency: usize) -> Option<TriggerRecord> {
        let mut inner = self.lock();
        if inner.running.len() >= max_concurrency.max(1) {
            return None;
        }
        // 已在运行的 session_key 集合：`per_trigger` 每次空 key，因此不参与判断。
        let busy_keys: Vec<String> = {
            let running = inner.running.clone();
            inner
                .records
                .iter()
                .filter(|record| {
                    !record.session_key.is_empty()
                        && running.iter().any(|id| id == &record.trigger_id)
                })
                .map(|record| record.session_key.clone())
                .collect()
        };
        // 先按队列顺序算出一个可取的候选（不在这里同时借 `inner.queue` 与 `inner.records`）。
        let candidates: Vec<String> = inner.queue.iter().cloned().collect();
        let candidate = candidates.into_iter().find(|id| {
            inner
                .records
                .iter()
                .find(|record| &record.trigger_id == id)
                .is_some_and(|record| {
                    record.session_key.is_empty() || !busy_keys.contains(&record.session_key)
                })
        })?;
        let trigger_id = {
            let index = inner.queue.iter().position(|id| *id == candidate)?;
            inner.queue.remove(index)?
        };
        let record = inner
            .records
            .iter_mut()
            .find(|record| record.trigger_id == trigger_id)?;
        record.state = TriggerState::Running;
        record.started_at = now_seconds();
        // 出队即占 slot：状态与 running 名单必须一起改，否则并发上限会短暂失效。
        record.cancel_requested = false;
        let snapshot = record.clone();
        inner.running.push(trigger_id);
        Some(snapshot)
    }

    /// 等到队列有新任务或有 slot 空出；`timeout` 到期也返回。
    pub fn wait_for_work(&self, timeout: std::time::Duration) -> bool {
        let inner = self.lock();
        if !inner.queue.is_empty() {
            return true;
        }
        let (_guard, _) = self
            .wake
            .wait_timeout(inner, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        true
    }

    /// 由工作线程回填会话 ID：闭环排查要靠它读 transcript。
    pub fn attach_session(&self, trigger_id: &str, session_id: &str) -> Result<(), RecordError> {
        self.update(trigger_id, |record| {
            record.session_id = session_id.to_string();
        })
    }

    /// 放回 slot 并进入终态。
    pub fn finish(
        &self,
        trigger_id: &str,
        state: TriggerState,
        result: &str,
        error: &str,
    ) -> Result<(), RecordError> {
        let mut inner = self.lock();
        inner.running.retain(|id| id != trigger_id);
        let record = inner
            .records
            .iter_mut()
            .find(|record| record.trigger_id == trigger_id)
            .ok_or_else(|| RecordError::NotFound(trigger_id.to_string()))?;
        record.state = state;
        record.result = result.to_string();
        record.error = error.to_string();
        record.finished_at = now_seconds();
        drop(inner);
        self.wake.notify_all();
        Ok(())
    }

    /// 记录投递结果。投递失败**不改** `state`——那是两个事实。
    pub fn record_notify(
        &self,
        trigger_id: &str,
        outcome: NotifyOutcome,
    ) -> Result<(), RecordError> {
        self.update(trigger_id, |record| record.notify_outcome = Some(outcome))
    }

    pub fn record_usage(
        &self,
        trigger_id: &str,
        model_turns: u64,
        tool_calls: u64,
    ) -> Result<(), RecordError> {
        self.update(trigger_id, |record| {
            record.model_turns = model_turns;
            record.tool_calls = tool_calls;
        })
    }

    /// 请求取消：置标记并唤醒池。已终态时是空操作（幂等）。
    pub fn request_cancel(&self, trigger_id: &str) -> Result<TriggerRecord, RecordError> {
        let mut inner = self.lock();
        inner.queue.retain(|id| id != trigger_id);
        let record = inner
            .records
            .iter_mut()
            .find(|record| record.trigger_id == trigger_id)
            .ok_or_else(|| RecordError::NotFound(trigger_id.to_string()))?;
        if record.state.is_terminal() {
            return Ok(record.clone());
        }
        record.cancel_requested = true;
        if record.state == TriggerState::Queued {
            record.state = TriggerState::Cancelled;
            record.error = "取消：尚未开始执行。".to_string();
            record.finished_at = now_seconds();
        }
        let snapshot = record.clone();
        drop(inner);
        self.wake.notify_all();
        Ok(snapshot)
    }

    pub fn cancel_requested(&self, trigger_id: &str) -> Result<bool, RecordError> {
        Ok(self.get(trigger_id)?.cancel_requested)
    }

    /// 追加一个事件并分配递增 ID；超出窗口时丢最旧的。
    pub fn append_event(
        &self,
        trigger_id: &str,
        event: &str,
        data: Value,
    ) -> Result<TriggerEvent, RecordError> {
        let mut inner = self.lock();
        let record = inner
            .records
            .iter_mut()
            .find(|record| record.trigger_id == trigger_id)
            .ok_or_else(|| RecordError::NotFound(trigger_id.to_string()))?;
        let recorded = TriggerEvent {
            id: record.next_event_id,
            event: event.to_string(),
            data,
        };
        record.next_event_id += 1;
        record.events.push(recorded.clone());
        if record.events.len() > MAX_EVENTS_PER_TRIGGER {
            let excess = record.events.len() - MAX_EVENTS_PER_TRIGGER;
            record.events.drain(..excess);
        }
        drop(inner);
        self.wake.notify_all();
        Ok(recorded)
    }

    /// 读取游标之后的事件；游标超出已记录范围时按契约报错。
    pub fn events_after(
        &self,
        trigger_id: &str,
        cursor: u64,
    ) -> Result<Vec<TriggerEvent>, RecordError> {
        let inner = self.lock();
        let record = inner
            .records
            .iter()
            .find(|record| record.trigger_id == trigger_id)
            .ok_or_else(|| RecordError::NotFound(trigger_id.to_string()))?;
        // 只有「游标比已产生的最大 ID 还大」才算过期：那说明客户端拿的是另一条流或旧记录。
        // 游标落在保留窗口之前（早期的增量已被挤出）**不是**错误——终态事件永远是最新一条。
        let latest = record.next_event_id.saturating_sub(1);
        if cursor > latest {
            return Err(RecordError::CursorExpired(latest.saturating_add(1)));
        }
        Ok(record
            .events
            .iter()
            .filter(|event| event.id > cursor)
            .cloned()
            .collect())
    }

    /// 等到该触发有新事件、进入终态或超时。
    pub fn wait_for_events(
        &self,
        trigger_id: &str,
        cursor: u64,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            let changed = inner
                .records
                .iter()
                .find(|record| record.trigger_id == trigger_id)
                .is_some_and(|record| {
                    record.state.is_terminal()
                        || record.events.iter().any(|event| event.id > cursor)
                });
            if changed {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = self
                .wake
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
        }
    }

    /// 服务关闭：排队中的全部置 `cancelled`（它们的 slot 从未被占用）。
    pub fn cancel_queued(&self) -> usize {
        let mut inner = self.lock();
        let queued: Vec<String> = inner.queue.drain(..).collect();
        let count = queued.len();
        let now = now_seconds();
        for record in inner
            .records
            .iter_mut()
            .filter(|record| queued.iter().any(|id| id == &record.trigger_id))
        {
            record.state = TriggerState::Cancelled;
            record.error = "服务关闭：排队中的触发已取消。".to_string();
            record.finished_at = now;
        }
        count
    }

    /// 当前运行中的 `trigger_id` 快照（关闭时逐个请求取消）。
    pub fn running_ids(&self) -> Vec<String> {
        self.lock().running.clone()
    }

    fn update(
        &self,
        trigger_id: &str,
        change: impl FnOnce(&mut TriggerRecord),
    ) -> Result<(), RecordError> {
        let mut inner = self.lock();
        let record = inner
            .records
            .iter_mut()
            .find(|record| record.trigger_id == trigger_id)
            .ok_or_else(|| RecordError::NotFound(trigger_id.to_string()))?;
        change(record);
        drop(inner);
        self.wake.notify_all();
        Ok(())
    }
}

/// 回收过期记录：只清终态且超过保留期的，活动任务永不回收。
fn purge_expired(inner: &mut Inner, retention_seconds: f64) {
    let now = now_seconds();
    inner.records.retain(|record| {
        if !record.state.is_terminal() {
            return true;
        }
        now - record.finished_at <= retention_seconds
    });
}

/// 当前时间（秒，带小数），与本地 API 的 `created_at` 同形。
pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// 按字符数截断，并标注被截掉的事实。
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n\n[已截断：原文超过 {limit} 字符]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(tag: &str) -> NewTrigger {
        NewTrigger {
            trigger_id: format!("trg_{tag}"),
            title: format!("任务 {tag}"),
            prompt: "跑一轮".to_string(),
            raw_message: "跑一轮".to_string(),
            workspace: "D:/Proj".to_string(),
            timeout_seconds: 600,
            session_key: String::new(),
            idempotency_key: String::new(),
            tags: Map::new(),
            notify: None,
        }
    }

    #[test]
    fn submit_queues_and_purges_on_take() {
        let store = TriggerStore::new(4, 1);
        let (record, duplicate) = store.submit(spec("a")).expect("应当受理");
        assert!(!duplicate);
        assert_eq!(record.state, TriggerState::Queued);
        assert_eq!(store.queue_position("trg_a"), Some(1));

        let taken = store.take_next(1).expect("应当取出");
        assert_eq!(taken.state, TriggerState::Running);
        assert_eq!(store.queue_position("trg_a"), None);
        // 单 slot 已占满：第二条取不出来。
        store.submit(spec("b")).expect("应当受理");
        assert!(store.take_next(1).is_none());

        store
            .finish("trg_a", TriggerState::Completed, "done", "")
            .expect("应当收尾");
        assert_eq!(store.take_next(1).expect("应当取出 b").trigger_id, "trg_b");
    }

    #[test]
    fn idempotency_key_returns_original_record() {
        let store = TriggerStore::new(4, 1);
        let mut first = spec("a");
        first.idempotency_key = "soil-1".to_string();
        let (record, duplicate) = store.submit(first).expect("应当受理");
        assert!(!duplicate);

        let mut second = spec("b");
        second.idempotency_key = "soil-1".to_string();
        let (again, duplicate) = store.submit(second).expect("应当受理");
        assert!(duplicate);
        assert_eq!(again.trigger_id, record.trigger_id);
        assert_eq!(store.list(None, None, 10).len(), 1);
    }

    #[test]
    fn queue_full_is_reported_not_dropped() {
        let store = TriggerStore::new(1, 1);
        store.submit(spec("a")).expect("应当受理");
        assert_eq!(
            store.submit(spec("b")).expect_err("应当拒绝"),
            RecordError::QueueFull(1)
        );
    }

    #[test]
    fn cancel_queued_trigger_finishes_immediately() {
        let store = TriggerStore::new(4, 1);
        store.submit(spec("a")).expect("应当受理");
        let record = store.request_cancel("trg_a").expect("应当取消");
        assert_eq!(record.state, TriggerState::Cancelled);
        assert_eq!(store.queue_position("trg_a"), None);
        // 幂等：再取消一次还是那个终态。
        assert_eq!(
            store.request_cancel("trg_a").expect("应当幂等").state,
            TriggerState::Cancelled
        );
    }

    #[test]
    fn events_cursor_is_only_invalid_when_ahead_of_the_stream() {
        let store = TriggerStore::new(4, 1);
        store.submit(spec("a")).expect("应当受理");
        store
            .append_event("trg_a", "run.started", json!({}))
            .expect("应当记录");
        let events = store.events_after("trg_a", 0).expect("应当可读");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 1);
        // 游标在任何已产生的事件之后（含等于最新 ID）：不是错误，只是没有新事件。
        assert!(store.events_after("trg_a", 1).expect("应当为空").is_empty());
        // 游标超前于流尾：说明客户端拿的是另一条流。
        assert_eq!(
            store.events_after("trg_a", 5).expect_err("游标超前"),
            RecordError::CursorExpired(2)
        );
    }

    #[test]
    fn terminal_records_are_reclaimed_but_active_ones_are_not() {
        let store = TriggerStore::new(4, 1);
        store.submit(spec("a")).expect("应当受理");
        store.take_next(1).expect("应当取出");
        // 把 finished_at 拨到保留期之外。
        {
            let mut inner = store.lock();
            let record = inner.records.first_mut().expect("有记录");
            record.state = TriggerState::Completed;
            record.finished_at = now_seconds() - 7200.0;
        }
        store.submit(spec("b")).expect("应当受理");
        assert_eq!(store.list(None, None, 10).len(), 1);
        assert_eq!(store.list(None, None, 10)[0].trigger_id, "trg_b");
    }

    #[test]
    fn keyed_session_serializes_same_key_across_slots() {
        let store = TriggerStore::new(8, 1);
        let mut first = spec("a");
        first.session_key = "daily".to_string();
        let mut second = spec("b");
        second.session_key = "daily".to_string();
        let mut other = spec("c");
        other.session_key = "news".to_string();
        store.submit(first).expect("应当受理");
        store.submit(second).expect("应当受理");
        store.submit(other).expect("应当受理");

        // 两个 slot 可用，但 "daily" 已在跑：第二个 slot 只能取到 "news"。
        assert_eq!(store.take_next(2).expect("应当取出").trigger_id, "trg_a");
        assert_eq!(store.take_next(2).expect("应当取出").trigger_id, "trg_c");
        // 两个 slot 都占满，"daily" 仍在排队。
        assert!(store.take_next(2).is_none());
        assert_eq!(store.queue_position("trg_b"), Some(1));

        store
            .finish("trg_a", TriggerState::Completed, "done", "")
            .expect("应当收尾");
        assert_eq!(store.take_next(2).expect("应当取出").trigger_id, "trg_b");
    }

    #[test]
    fn notify_matching_follows_on_field() {
        let spec = |on: &str| NotifySpec {
            channel: "telegram".to_string(),
            target: "1".to_string(),
            on: on.to_string(),
            max_chars: 4000,
        };
        assert!(spec("completed").matches(TriggerState::Completed));
        assert!(!spec("completed").matches(TriggerState::Failed));
        assert!(spec("failed").matches(TriggerState::Failed));
        assert!(spec("always").matches(TriggerState::Failed));
        assert!(
            !NotifySpec {
                channel: "none".to_string(),
                ..spec("always")
            }
            .matches(TriggerState::Completed)
        );
    }

    #[test]
    fn list_filters_by_state_and_tag() {
        let store = TriggerStore::new(4, 10);
        let mut first = spec("a");
        first.tags.insert("source".to_string(), json!("planter"));
        store.submit(first).expect("应当受理");
        store.submit(spec("b")).expect("应当受理");
        store.take_next(2).expect("应当取出");
        store
            .finish("trg_a", TriggerState::Completed, "", "")
            .expect("收尾");

        assert_eq!(store.list(None, None, 10).len(), 2);
        assert_eq!(store.list(Some(TriggerState::Completed), None, 10).len(), 1);
        assert_eq!(store.list(None, Some(("source", "planter")), 10).len(), 1);
    }
}
