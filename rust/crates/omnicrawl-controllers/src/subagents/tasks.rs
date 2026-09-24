//! `omnicrawl/agent/subagents/tasks.py` 的移植：进程内 SubAgent 后台任务状态、取消、
//! TTL 与一次性通知。
//!
//! 本模块不持有模型、Session 或工具对象；上层通过 `runner` 注入单任务执行。这样后台线程
//! 不会复制 Agent Loop，也不会把 prompt、推理或凭据写入任务快照。所有公开 payload 都只来自
//! 上游已完成安全投影的元数据。
//!
//! 与 Python 的线程模型差异只有一处：`ThreadPoolExecutor` 换成「维护线程 + 任务线程」，
//! 但对外可见的顺序（`queued` 先于 `running`）、并发上限、合作式取消与 TTL 语义保持一致。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use omnicrawl_session::redaction::redact_sensitive_text;

use crate::error::AgentError;
use crate::shared::{python_str, python_truthy};

const TERMINAL: [&str; 3] = ["completed", "failed", "cancelled"];
const RESULT_FIELDS: [&str; 10] = [
    "status",
    "summary",
    "artifacts",
    "usage",
    "error",
    "task_id",
    "description",
    "agent_type",
    "definition_source",
    "recovered",
];

pub fn is_terminal(status: &str) -> bool {
    TERMINAL.contains(&status)
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// 合作式取消令牌；runner 负责轮询 `is_set`。
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

/// 后台任务的安全元数据；不得把原始 prompt 放进 `metadata`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubAgentTaskSpec {
    pub task_id: String,
    pub description: String,
    pub agent_type: String,
    pub batch_id: String,
    pub metadata: Map<String, Value>,
}

impl SubAgentTaskSpec {
    pub fn new(
        task_id: impl Into<String>,
        description: impl Into<String>,
        agent_type: impl Into<String>,
        batch_id: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            description: description.into(),
            agent_type: agent_type.into(),
            batch_id: batch_id.into(),
            metadata: Map::new(),
        }
    }
}

/// 面向 list/get 的有界任务快照。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubAgentTaskSnapshot {
    pub task_id: String,
    pub batch_id: String,
    pub owner_id: String,
    pub session_id: String,
    pub description: String,
    pub agent_type: String,
    pub status: String,
    pub result: Option<Map<String, Value>>,
    pub error: Option<Map<String, Value>>,
    pub created_at: f64,
    pub updated_at: f64,
}

impl SubAgentTaskSnapshot {
    /// 只公开任务元数据；owner/session 仅用于 Host 内部隔离。
    pub fn as_dict(&self) -> Value {
        json!({
            "task_id": self.task_id,
            "batch_id": self.batch_id,
            "description": self.description,
            "agent_type": self.agent_type,
            "status": self.status,
            "result": self.result.clone().map(Value::Object).unwrap_or(Value::Null),
            "error": self.error.clone().map(Value::Object).unwrap_or(Value::Null),
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }
}

pub type TaskRunner =
    Arc<dyn Fn(&SubAgentTaskSpec, &CancelToken) -> Map<String, Value> + Send + Sync>;
pub type TaskObserver = Arc<dyn Fn(&str, Value) + Send + Sync>;
pub type IdleCallback = Box<dyn FnOnce() + Send>;

/// 管理一个 Agent 范围内的后台任务。
pub struct SubAgentTaskManager {
    inner: Arc<Inner>,
    maintenance: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    retention_seconds: f64,
    state: Mutex<State>,
    signal: Condvar,
}

struct State {
    tasks: BTreeMap<String, Task>,
    batch_tasks: BTreeMap<String, Vec<String>>,
    notifications: BTreeMap<String, Vec<Value>>,
    idle_callbacks: Vec<IdleEntry>,
    queue: VecDeque<String>,
    running: usize,
    max_workers: usize,
    closed: bool,
    shutdown: bool,
}

struct Task {
    spec: SubAgentTaskSpec,
    owner_id: String,
    session_id: String,
    observer: Option<TaskObserver>,
    runner: TaskRunner,
    cancel: CancelToken,
    status: String,
    result: Option<Map<String, Value>>,
    error: Option<Map<String, Value>>,
    created_at: f64,
    updated_at: f64,
    /// 是否已被 worker 取走（对应 Python `task.future` 已开始执行）。
    taken: bool,
}

struct IdleEntry {
    owner_id: String,
    session_id: Option<String>,
    callback: IdleCallback,
}

fn notification_key(owner_id: &str, session_id: &str) -> String {
    format!("{owner_id}\u{1f}{session_id}")
}

impl State {
    fn has_active(&self, owner_id: &str, session_id: Option<&str>) -> bool {
        self.tasks.values().any(|task| {
            task.owner_id == owner_id
                && (session_id.is_none() || session_id == Some(task.session_id.as_str()))
                && !is_terminal(&task.status)
        })
    }

    fn take_idle_callbacks(&mut self) -> Vec<IdleCallback> {
        let entries = std::mem::take(&mut self.idle_callbacks);
        let mut ready = Vec::new();
        let mut waiting = Vec::new();
        for entry in entries {
            if self.has_active(&entry.owner_id, entry.session_id.as_deref()) {
                waiting.push(entry);
            } else {
                ready.push(entry.callback);
            }
        }
        self.idle_callbacks = waiting;
        ready
    }

    /// 在已持有状态锁时构造事件 payload；终态时同时进入通知队列。
    fn publish_payload(
        &mut self,
        task_id: &str,
        status: &str,
    ) -> Option<(Value, Option<TaskObserver>)> {
        let (
            observer,
            owner_id,
            session_id,
            batch_id,
            spec_task_id,
            description,
            agent_type,
            result,
            error,
        ) = {
            let task = self.tasks.get(task_id)?;
            (
                task.observer.clone(),
                task.owner_id.clone(),
                task.session_id.clone(),
                task.spec.batch_id.clone(),
                task.spec.task_id.clone(),
                take_chars(&task.spec.description, 120),
                take_chars(&task.spec.agent_type, 80),
                task.result.clone(),
                task.error.clone(),
            )
        };
        let mut payload = Map::new();
        payload.insert("batch_id".to_string(), Value::String(batch_id));
        payload.insert("task_id".to_string(), Value::String(spec_task_id));
        payload.insert("description".to_string(), Value::String(description));
        payload.insert("agent_type".to_string(), Value::String(agent_type));
        payload.insert("status".to_string(), Value::String(status.to_string()));
        payload.insert("timestamp".to_string(), json!(now_seconds()));
        if let Some(result) = result {
            payload.insert("result".to_string(), Value::Object(result));
        }
        if let Some(error) = error {
            payload.insert("error".to_string(), Value::Object(error));
        }
        if is_terminal(status) {
            self.notifications
                .entry(notification_key(&owner_id, &session_id))
                .or_default()
                .push(Value::Object(payload.clone()));
        }
        Some((Value::Object(payload), observer))
    }

    fn cleanup_expired(&mut self, now: f64, retention_seconds: f64) {
        let cutoff = now - retention_seconds;
        let expired: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, task)| is_terminal(&task.status) && task.updated_at <= cutoff)
            .map(|(task_id, _)| task_id.clone())
            .collect();
        for task_id in expired {
            if let Some(task) = self.tasks.remove(&task_id) {
                if let Some(ids) = self.batch_tasks.get_mut(&task.spec.batch_id) {
                    ids.retain(|item| item != &task_id);
                    if ids.is_empty() {
                        self.batch_tasks.remove(&task.spec.batch_id);
                    }
                }
            }
        }
        let keys: Vec<String> = self.notifications.keys().cloned().collect();
        for key in keys {
            let events = self.notifications.get(&key).cloned().unwrap_or_default();
            let fresh: Vec<Value> = events
                .into_iter()
                .filter(|event| {
                    event
                        .get("timestamp")
                        .and_then(Value::as_f64)
                        .unwrap_or(now)
                        > cutoff
                })
                .collect();
            if fresh.is_empty() {
                self.notifications.remove(&key);
            } else {
                self.notifications.insert(key, fresh);
            }
        }
    }

    /// 返回下一项终态记录的到期等待时间；没有记录时 `None`。
    fn wait_seconds(&self, retention_seconds: f64, now: f64) -> Option<f64> {
        if !self.queue.is_empty() && self.running < self.max_workers {
            return Some(0.0);
        }
        let mut expiry: Option<f64> = None;
        for task in self.tasks.values() {
            if is_terminal(&task.status) {
                let value = task.updated_at + retention_seconds;
                expiry = Some(expiry.map_or(value, |current: f64| current.min(value)));
            }
        }
        for events in self.notifications.values() {
            for event in events {
                let timestamp = event
                    .get("timestamp")
                    .and_then(Value::as_f64)
                    .unwrap_or(now);
                let value = timestamp + retention_seconds;
                expiry = Some(expiry.map_or(value, |current: f64| current.min(value)));
            }
        }
        expiry.map(|value| (value - now).max(0.0))
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

enum Outcome {
    Result(Map<String, Value>),
    Cancelled,
    Panicked,
}

impl SubAgentTaskManager {
    pub fn new(retention_seconds: f64, max_workers: usize) -> Self {
        let inner = Arc::new(Inner {
            retention_seconds: retention_seconds.max(1.0),
            state: Mutex::new(State {
                tasks: BTreeMap::new(),
                batch_tasks: BTreeMap::new(),
                notifications: BTreeMap::new(),
                idle_callbacks: Vec::new(),
                queue: VecDeque::new(),
                running: 0,
                max_workers: max_workers.max(1),
                closed: false,
                shutdown: false,
            }),
            signal: Condvar::new(),
        });
        let handle = {
            let inner = inner.clone();
            std::thread::spawn(move || maintenance_loop(inner))
        };
        Self {
            inner,
            maintenance: Mutex::new(Some(handle)),
        }
    }

    pub fn retention_seconds(&self) -> f64 {
        self.inner.retention_seconds
    }

    /// 调整后续后台任务可使用的并发上限。
    pub fn set_max_workers(&self, max_workers: usize) -> Result<(), AgentError> {
        if max_workers < 1 {
            return Err(AgentError::new("max_workers 必须是正整数。"));
        }
        let mut state = self.inner.lock();
        if state.closed {
            return Ok(());
        }
        state.max_workers = max_workers;
        self.inner.signal.notify_all();
        Ok(())
    }

    /// 登记并后台执行任务，返回安全的 batch/task ID 投影。
    pub fn spawn(
        &self,
        owner_id: &str,
        session_id: &str,
        specs: &[SubAgentTaskSpec],
        runner: TaskRunner,
        observer: Option<TaskObserver>,
    ) -> Result<Value, AgentError> {
        if specs.is_empty() {
            return Err(AgentError::new("后台任务不能为空。"));
        }
        let mut queued_ids: Vec<String> = Vec::new();
        {
            let mut state = self.inner.lock();
            if state.closed {
                return Err(AgentError::new("SubAgentTaskManager 已关闭。"));
            }
            if specs
                .iter()
                .any(|spec| state.tasks.contains_key(&spec.task_id))
            {
                return Err(AgentError::new("后台任务 ID 重复。"));
            }
            let created_at = now_seconds();
            for spec in specs {
                state.tasks.insert(
                    spec.task_id.clone(),
                    Task {
                        spec: spec.clone(),
                        owner_id: owner_id.to_string(),
                        session_id: session_id.to_string(),
                        observer: observer.clone(),
                        runner: runner.clone(),
                        cancel: CancelToken::new(),
                        status: "queued".to_string(),
                        result: None,
                        error: None,
                        created_at,
                        updated_at: created_at,
                        taken: false,
                    },
                );
                state
                    .batch_tasks
                    .entry(spec.batch_id.clone())
                    .or_default()
                    .push(spec.task_id.clone());
                state.queue.push_back(spec.task_id.clone());
                queued_ids.push(spec.task_id.clone());
            }
        }
        // worker 已被唤醒，但 `queued` 必须先于 `running` 发布。
        for task_id in &queued_ids {
            self.publish(task_id, "queued");
        }
        self.inner.signal.notify_all();
        Ok(json!({
            "batch_id": specs[0].batch_id,
            "task_ids": specs.iter().map(|spec| spec.task_id.clone()).collect::<Vec<_>>(),
            "status": "queued",
        }))
    }

    pub fn get(&self, task_id: &str, owner_id: &str, session_id: Option<&str>) -> Option<Value> {
        self.cleanup_now();
        let state = self.inner.lock();
        let task = state.tasks.get(task_id)?;
        if task.owner_id != owner_id
            || (session_id.is_some() && session_id != Some(task.session_id.as_str()))
        {
            return None;
        }
        Some(snapshot(task).as_dict())
    }

    pub fn list(&self, owner_id: &str, session_id: Option<&str>) -> Vec<Value> {
        self.cleanup_now();
        let state = self.inner.lock();
        let mut items: Vec<&Task> = state
            .tasks
            .values()
            .filter(|task| {
                task.owner_id == owner_id
                    && (session_id.is_none() || session_id == Some(task.session_id.as_str()))
            })
            .collect();
        items.sort_by(|left, right| {
            (left.created_at, left.spec.task_id.clone())
                .partial_cmp(&(right.created_at, right.spec.task_id.clone()))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        items.iter().map(|task| snapshot(task).as_dict()).collect()
    }

    pub fn cancel(
        &self,
        owner_id: &str,
        session_id: Option<&str>,
        task_id: Option<&str>,
        batch_id: Option<&str>,
    ) -> Value {
        let mut changed = 0usize;
        let mut active_count = 0usize;
        let mut immediate: Vec<String> = Vec::new();
        {
            let mut state = self.inner.lock();
            let mut ids: Vec<String> = match task_id {
                Some(value) => vec![value.to_string()],
                None => {
                    let mut values = state
                        .batch_tasks
                        .get(batch_id.unwrap_or(""))
                        .cloned()
                        .unwrap_or_default();
                    values.sort();
                    values
                }
            };
            ids.dedup();
            let selected: Vec<String> = ids
                .into_iter()
                .filter(|id| {
                    state.tasks.get(id).is_some_and(|task| {
                        task.owner_id == owner_id
                            && session_id.is_none_or(|value| task.session_id == value)
                    })
                })
                .collect();
            if selected.is_empty() {
                return json!({
                    "ok": false,
                    "code": "SUBAGENT_NOT_FOUND",
                    "message": "未找到任务或批次。",
                });
            }
            let now = now_seconds();
            for id in selected {
                let Some(task) = state.tasks.get_mut(&id) else {
                    continue;
                };
                if is_terminal(&task.status) {
                    continue;
                }
                task.cancel.set();
                active_count += 1;
                let was_queued = !task.taken;
                task.updated_at = now;
                if was_queued {
                    changed += 1;
                    immediate.push(id);
                }
            }
        }
        for id in &immediate {
            self.finish(id, "cancelled", None, Some(cancelled_error("任务已取消。")));
        }
        let status = if changed > 0 {
            "cancelled"
        } else if active_count > 0 {
            "cancelling"
        } else {
            "already_terminal"
        };
        json!({
            "ok": true,
            "batch_id": batch_id,
            "task_id": task_id,
            "cancelled_count": changed,
            "status": status,
        })
    }

    pub fn cancel_all(&self, owner_id: &str, session_id: Option<&str>) -> usize {
        let snapshots = self.list(owner_id, session_id);
        let mut count = 0usize;
        for item in snapshots {
            let Some(task_id) = item.get("task_id").and_then(Value::as_str) else {
                continue;
            };
            let result = self.cancel(owner_id, session_id, Some(task_id), None);
            count += result
                .get("cancelled_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
        }
        count
    }

    /// 消费未过期终态通知；消费后不会再次返回。
    pub fn drain_notifications(&self, owner_id: &str, session_id: Option<&str>) -> Vec<Value> {
        self.cleanup_now();
        let mut state = self.inner.lock();
        let keys: Vec<String> = state
            .notifications
            .keys()
            .filter(|key| {
                let mut parts = key.split('\u{1f}');
                let owner = parts.next().unwrap_or_default();
                let session = parts.next().unwrap_or_default();
                owner == owner_id && (session_id.is_none() || session_id == Some(session))
            })
            .cloned()
            .collect();
        let mut events = Vec::new();
        for key in keys {
            if let Some(items) = state.notifications.remove(&key) {
                events.extend(items);
            }
        }
        events
    }

    pub fn is_idle(&self, owner_id: &str, session_id: Option<&str>) -> bool {
        let state = self.inner.lock();
        !state.has_active(owner_id, session_id)
    }

    pub fn wait_for_idle(&self, owner_id: &str, session_id: Option<&str>, timeout: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
        loop {
            if self.is_idle(owner_id, session_id) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }

    /// 在 owner 范围真实空闲后调用一次，用于延迟释放共享资源。
    pub fn call_when_idle(&self, owner_id: &str, session_id: Option<&str>, callback: IdleCallback) {
        {
            let mut state = self.inner.lock();
            if state.has_active(owner_id, session_id) {
                state.idle_callbacks.push(IdleEntry {
                    owner_id: owner_id.to_string(),
                    session_id: session_id.map(str::to_string),
                    callback,
                });
                return;
            }
        }
        callback();
    }

    /// 关闭指定任务范围；永久关闭时同时回收维护线程。
    pub fn close(&self, owner_id: &str, session_id: Option<&str>) {
        let permanent = session_id.is_none();
        if permanent {
            let mut state = self.inner.lock();
            state.closed = true;
            state.shutdown = true;
            self.inner.signal.notify_all();
        }
        self.cancel_all(owner_id, session_id);
        if permanent {
            let handle = self
                .maintenance
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(handle) = handle {
                let _ = handle.join();
            }
        }
    }

    /// 导入跨进程恢复的终态任务快照；只用于 list/get 控制面可见性。
    pub fn import_recovered_snapshots(
        &self,
        owner_id: &str,
        session_id: &str,
        snapshots: &[Value],
    ) -> usize {
        if snapshots.is_empty() {
            return 0;
        }
        let mut imported = 0usize;
        let mut state = self.inner.lock();
        if state.closed {
            return 0;
        }
        for raw in snapshots {
            let Some(object) = raw.as_object() else {
                continue;
            };
            let task_id = string_field(object.get("task_id")).trim().to_string();
            if task_id.is_empty() || state.tasks.contains_key(&task_id) {
                continue;
            }
            let status = string_field(object.get("status")).trim().to_string();
            if !is_terminal(&status) {
                continue;
            }
            let snap_owner = {
                let value = string_field(object.get("owner_id")).trim().to_string();
                if value.is_empty() {
                    owner_id.to_string()
                } else {
                    value
                }
            };
            let snap_session = {
                let value = string_field(object.get("session_id")).trim().to_string();
                if value.is_empty() {
                    session_id.to_string()
                } else {
                    value
                }
            };
            if snap_owner != owner_id || snap_session != session_id {
                continue;
            }
            let mut batch_id = string_field(object.get("batch_id")).trim().to_string();
            if batch_id.is_empty() {
                let suffix = task_id.strip_prefix("task-").unwrap_or(task_id.as_str());
                batch_id = format!("batch-{}", take_chars(suffix, 12));
            }
            let description = take_chars(
                &non_empty(string_field(object.get("description")), "SubAgent 任务"),
                120,
            );
            let agent_type = take_chars(
                &non_empty(string_field(object.get("agent_type")), "unknown"),
                80,
            );
            let created_at = float_field(object.get("created_at")).unwrap_or_else(now_seconds);
            let updated_at = float_field(object.get("updated_at")).unwrap_or(created_at);
            let mut result = object
                .get("result")
                .and_then(Value::as_object)
                .and_then(|value| bound_result(Some(value)));
            let mut error = object
                .get("error")
                .and_then(Value::as_object)
                .and_then(|value| bound_error(Some(value)));
            if result.is_none() && status == "completed" {
                result = Some(default_completed_result());
            } else if let Some(values) = result.as_mut() {
                values.insert("recovered".to_string(), Value::Bool(true));
                values
                    .entry("status".to_string())
                    .or_insert_with(|| Value::String(status.clone()));
            }
            if error.is_none() && (status == "failed" || status == "cancelled") {
                error = Some(if status == "cancelled" {
                    cancelled_error("任务已取消。")
                } else {
                    error_map("SUBAGENT_ERROR", "子任务失败。")
                });
            }
            state.tasks.insert(
                task_id.clone(),
                Task {
                    spec: SubAgentTaskSpec {
                        task_id: task_id.clone(),
                        description,
                        agent_type,
                        batch_id: batch_id.clone(),
                        metadata: {
                            let mut map = Map::new();
                            map.insert("recovered".to_string(), Value::Bool(true));
                            map
                        },
                    },
                    owner_id: owner_id.to_string(),
                    session_id: session_id.to_string(),
                    observer: None,
                    runner: Arc::new(|_, _| Map::new()),
                    cancel: CancelToken::new(),
                    status,
                    result,
                    error,
                    created_at,
                    updated_at,
                    taken: true,
                },
            );
            state.batch_tasks.entry(batch_id).or_default().push(task_id);
            imported += 1;
        }
        if imported > 0 {
            let retention = self.inner.retention_seconds;
            let now = now_seconds();
            state.cleanup_expired(now, retention);
            self.inner.signal.notify_all();
        }
        imported
    }

    fn cleanup_now(&self) {
        let retention = self.inner.retention_seconds;
        let mut state = self.inner.lock();
        let now = now_seconds();
        state.cleanup_expired(now, retention);
    }

    fn publish(&self, task_id: &str, status: &str) {
        let published = {
            let mut state = self.inner.lock();
            state.publish_payload(task_id, status)
        };
        let Some((payload, observer)) = published else {
            return;
        };
        if let Some(observer) = observer {
            observer(&format!("subagent.task.{status}"), payload);
        }
    }

    fn finish(
        &self,
        task_id: &str,
        status: &str,
        result: Option<Map<String, Value>>,
        error: Option<Map<String, Value>>,
    ) {
        let callbacks = {
            let mut state = self.inner.lock();
            let Some(task) = state.tasks.get_mut(task_id) else {
                return;
            };
            if is_terminal(&task.status) {
                return;
            }
            task.status = status.to_string();
            task.result = bound_result(result.as_ref());
            task.error = bound_error(error.as_ref());
            task.updated_at = now_seconds();
            state.take_idle_callbacks()
        };
        self.publish(task_id, status);
        for callback in callbacks {
            callback();
        }
        self.inner.signal.notify_all();
    }
}

impl Drop for SubAgentTaskManager {
    fn drop(&mut self) {
        {
            let mut state = self.inner.lock();
            state.shutdown = true;
            state.closed = true;
            self.inner.signal.notify_all();
        }
        let handle = self
            .maintenance
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

fn maintenance_loop(inner: Arc<Inner>) {
    loop {
        let (to_start, wait_for) = {
            let mut state = inner.lock();
            if state.shutdown {
                return;
            }
            let now = now_seconds();
            state.cleanup_expired(now, inner.retention_seconds);
            let mut to_start = Vec::new();
            while state.running < state.max_workers {
                let Some(task_id) = state.queue.pop_front() else {
                    break;
                };
                let running = state.running;
                let Some(task) = state.tasks.get_mut(&task_id) else {
                    continue;
                };
                if task.status != "queued" {
                    continue;
                }
                task.status = "running".to_string();
                task.taken = true;
                task.updated_at = now;
                state.running = running + 1;
                to_start.push(task_id);
            }
            (to_start, state.wait_seconds(inner.retention_seconds, now))
        };
        for task_id in &to_start {
            publish_via_inner(&inner, task_id, "running");
            start_task(inner.clone(), task_id.clone());
        }
        let guard = inner.lock();
        if guard.shutdown {
            return;
        }
        let timeout = wait_for.unwrap_or(3600.0).clamp(0.0, 3600.0);
        let _ = inner
            .signal
            .wait_timeout(guard, Duration::from_secs_f64(timeout));
    }
}

fn publish_via_inner(inner: &Arc<Inner>, task_id: &str, status: &str) {
    let published = {
        let mut state = inner.lock();
        state.publish_payload(task_id, status)
    };
    let Some((payload, observer)) = published else {
        return;
    };
    if let Some(observer) = observer {
        observer(&format!("subagent.task.{status}"), payload);
    }
}

fn start_task(inner: Arc<Inner>, task_id: String) {
    std::thread::spawn(move || {
        let (spec, cancel, runner) = {
            let state = inner.lock();
            let Some(task) = state.tasks.get(&task_id) else {
                return;
            };
            (task.spec.clone(), task.cancel.clone(), task.runner.clone())
        };
        let outcome = if cancel.is_set() {
            Outcome::Cancelled
        } else {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runner(&spec, &cancel)))
            {
                Ok(values) => Outcome::Result(values),
                Err(_) => Outcome::Panicked,
            }
        };
        {
            let mut state = inner.lock();
            state.running = state.running.saturating_sub(1);
        }
        let still_active = {
            let state = inner.lock();
            state
                .tasks
                .get(&task_id)
                .is_some_and(|task| !is_terminal(&task.status))
        };
        if still_active {
            match outcome {
                Outcome::Result(values) => {
                    let mut status = values
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                        .to_string();
                    if !is_terminal(&status) {
                        status = "completed".to_string();
                    }
                    if cancel.is_set() && status == "completed" {
                        status = "cancelled".to_string();
                    }
                    match status.as_str() {
                        "cancelled" => finish_via_inner(
                            &inner,
                            &task_id,
                            "cancelled",
                            None,
                            Some(cancelled_error("任务已取消。")),
                        ),
                        "failed" => {
                            let error = values.get("error").and_then(Value::as_object).cloned();
                            finish_via_inner(&inner, &task_id, "failed", Some(values), error)
                        }
                        _ => finish_via_inner(&inner, &task_id, "completed", Some(values), None),
                    }
                }
                Outcome::Cancelled => finish_via_inner(
                    &inner,
                    &task_id,
                    "cancelled",
                    None,
                    Some(cancelled_error("任务已取消。")),
                ),
                Outcome::Panicked if cancel.is_set() => finish_via_inner(
                    &inner,
                    &task_id,
                    "cancelled",
                    None,
                    Some(cancelled_error("任务已取消。")),
                ),
                Outcome::Panicked => finish_via_inner(
                    &inner,
                    &task_id,
                    "failed",
                    None,
                    Some(error_map("SUBAGENT_MODEL_ERROR", "子任务执行失败。")),
                ),
            }
        }
        inner.signal.notify_all();
    });
}

fn finish_via_inner(
    inner: &Arc<Inner>,
    task_id: &str,
    status: &str,
    result: Option<Map<String, Value>>,
    error: Option<Map<String, Value>>,
) {
    let callbacks = {
        let mut state = inner.lock();
        let Some(task) = state.tasks.get_mut(task_id) else {
            return;
        };
        if is_terminal(&task.status) {
            return;
        }
        task.status = status.to_string();
        task.result = bound_result(result.as_ref());
        task.error = bound_error(error.as_ref());
        task.updated_at = now_seconds();
        state.take_idle_callbacks()
    };
    publish_via_inner(inner, task_id, status);
    for callback in callbacks {
        callback();
    }
    inner.signal.notify_all();
}

fn snapshot(task: &Task) -> SubAgentTaskSnapshot {
    SubAgentTaskSnapshot {
        task_id: task.spec.task_id.clone(),
        batch_id: task.spec.batch_id.clone(),
        owner_id: task.owner_id.clone(),
        session_id: task.session_id.clone(),
        description: take_chars(&task.spec.description, 120),
        agent_type: take_chars(&task.spec.agent_type, 80),
        status: task.status.clone(),
        result: task.result.clone(),
        error: task.error.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
    }
}

/// `_bound_result`：只复制允许的公开字段，并对摘要做脱敏与截断。
pub fn bound_result(result: Option<&Map<String, Value>>) -> Option<Map<String, Value>> {
    let result = result?;
    let mut bounded = Map::new();
    for (key, value) in result {
        if RESULT_FIELDS.contains(&key.as_str()) {
            bounded.insert(key.clone(), value.clone());
        }
    }
    if let Some(value) = bounded.get("summary").cloned() {
        bounded.insert(
            "summary".to_string(),
            Value::String(take_chars(
                &redact_sensitive_text(&python_str(&value)),
                6000,
            )),
        );
    }
    if let Some(value) = bounded.get("artifacts").cloned() {
        if let Some(items) = value.as_array() {
            let mut items = items.clone();
            items.truncate(16);
            bounded.insert("artifacts".to_string(), Value::Array(items));
        }
    }
    if let Some(value) = bounded.get("recovered").cloned() {
        bounded.insert("recovered".to_string(), Value::Bool(python_truthy(&value)));
    }
    Some(bounded)
}

/// `_bound_error`：编号与消息都有界，消息经过脱敏。
pub fn bound_error(error: Option<&Map<String, Value>>) -> Option<Map<String, Value>> {
    let error = error?;
    let code = error
        .get("code")
        .map(python_str)
        .unwrap_or_else(|| "SUBAGENT_ERROR".to_string());
    let message = error
        .get("message")
        .map(python_str)
        .unwrap_or_else(|| "子任务失败。".to_string());
    let mut bounded = Map::new();
    bounded.insert("code".to_string(), Value::String(take_chars(&code, 80)));
    bounded.insert(
        "message".to_string(),
        Value::String(take_chars(&redact_sensitive_text(&message), 500)),
    );
    Some(bounded)
}

fn error_map(code: &str, message: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("code".to_string(), Value::String(code.to_string()));
    map.insert("message".to_string(), Value::String(message.to_string()));
    map
}

fn cancelled_error(message: &str) -> Map<String, Value> {
    error_map("SUBAGENT_CANCELLED", message)
}

fn default_completed_result() -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("status".to_string(), Value::String("completed".to_string()));
    map.insert("summary".to_string(), Value::String(String::new()));
    map.insert("artifacts".to_string(), Value::Array(Vec::new()));
    map.insert("usage".to_string(), Value::Object(Map::new()));
    map.insert("recovered".to_string(), Value::Bool(true));
    map
}

fn string_field(value: Option<&Value>) -> String {
    match value {
        Some(item) if python_truthy(item) => python_str(item),
        _ => String::new(),
    }
}

fn non_empty(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn float_field(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => {
            let parsed = number.as_f64()?;
            if parsed == 0.0 {
                None
            } else {
                Some(parsed)
            }
        }
        Some(Value::String(text)) => text.parse::<f64>().ok(),
        _ => None,
    }
}

fn take_chars(text: &str, limit: usize) -> String {
    if text.chars().count() > limit {
        return text.chars().take(limit).collect();
    }
    text.to_string()
}
