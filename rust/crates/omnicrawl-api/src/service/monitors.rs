//! 由 `service.rs` 按职责拆出，语义未变。

use super::*;

impl AgentService {
        /// 冻结当前 API 可见的 Session，用于后台事件流（对应 Python `current_subagent_session_id`）。
        ///
        /// 多 worker 下不能取本进程内核的会话：产生事件的 Run 可能由别的 worker 持有，
        /// 取本地会话会让 SSE 订到错误的 Session。改取最近一次 Run 的 Session；尚未跑过
        /// Run 的 worker 退回本进程会话。
        pub fn current_subagent_session_id(&self) -> String {
            if let Some(store) = self.store.shared() {
                if let Ok(Some(record)) = store.latest_run() {
                    return record.session_id;
                }
            }
            self.current_session_id()
        }

        /// 指定会话的后台任务事件（游标之后）；`/subagents/events` 的每次轮询都走这里。
        ///
        /// 共享存储是唯一事实来源：所有者与旁观 worker 必须读到同一份事件。
        pub fn subagent_events_after(&self, session_id: &str, cursor: u64) -> Vec<RunEvent> {
            if let Some(store) = self.store.shared() {
                return store
                    .subagent_events_after(session_id, cursor)
                    .unwrap_or_default();
            }
            self.subagent_feed.events_after(session_id, cursor)
        }

        /// 等到指定会话的事件流有新事件或超时。
        ///
        /// 跨进程没有可用的条件变量（所有者与旁观 worker 不在同一进程），改用短轮询；
        /// 本地单次存在性查询在微秒级，对单机 API 足够。
        pub fn wait_for_subagent_events(&self, session_id: &str, cursor: u64, timeout: Duration) {
            if let Some(store) = self.store.shared() {
                let deadline = Instant::now() + timeout;
                loop {
                    if store.has_subagent_events_after(session_id, cursor) {
                        return;
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return;
                    }
                    thread::sleep(SHARED_WAIT_POLL.min(deadline - now));
                }
            }
            self.subagent_feed
                .wait_for_events(session_id, cursor, timeout);
        }

        /// `GET /subagents`：当前会话可见的后台任务。
        pub fn list_subagent_tasks(&self) -> Result<Vec<Value>, ApiError> {
            let value = self.subagent_query("list", "")?;
            if subagent_unavailable_in(&value) {
                return Err(subagent_unavailable());
            }
            Ok(value
                .get("tasks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default())
        }

        /// `GET /subagents/{task_id}`：任务不可见时返回 `None`（路由按 404 回答）。
        pub fn get_subagent_task(&self, task_id: &str) -> Result<Option<Value>, ApiError> {
            let value = self.subagent_query("get", task_id)?;
            if subagent_unavailable_in(&value) {
                return Err(subagent_unavailable());
            }
            Ok(value.get("task").cloned().filter(|task| !task.is_null()))
        }

        /// `POST /subagents/{task_id}/cancel`：取消未被受理时返回 `None`（路由按 404 回答）。
        pub fn cancel_subagent_task(&self, task_id: &str) -> Result<Option<Value>, ApiError> {
            let value = self.subagent_query("cancel", task_id)?;
            if subagent_unavailable_in(&value) {
                return Err(subagent_unavailable());
            }
            let result = value.get("result").cloned().unwrap_or(Value::Null);
            let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
            Ok(ok.then_some(result))
        }

        /// 一次内核侧后台任务查询；回合在途时快速失败（内核主循环被回合占用）。
        pub(super) fn subagent_query(&self, action: &str, task_id: &str) -> Result<Value, ApiError> {
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能查询后台任务。"))?;
            match guard.as_mut() {
                Some(runner) => runner
                    .manage_subagents(action, task_id)
                    .map_err(kernel_command_failed),
                None => Err(ApiError::service_unavailable()),
            }
        }

        /// `GET /monitors`：当前受管的后台任务快照。
        pub fn list_monitors(&self) -> Result<Vec<Value>, ApiError> {
            Ok(self
                .monitors()?
                .tasks()
                .iter()
                .map(MonitorTaskView::to_value)
                .collect())
        }

        /// `GET /monitors/{id}`：未知任务回 404。
        pub fn monitor_task(&self, monitor_id: &str) -> Result<Value, ApiError> {
            self.monitors()?
                .task(monitor_id)
                .map(|task| task.to_value())
                .ok_or_else(monitor_not_found)
        }

        /// 后台任务日志的一轮轮询（SSE 生产者每次循环调用一次）。
        pub fn poll_monitor_events(
            &self,
            monitor_id: &str,
            cursor: u64,
            max_events: usize,
        ) -> Result<MonitorPollView, ApiError> {
            self.monitors()?
                .poll_view(monitor_id, cursor, max_events)
                .ok_or_else(monitor_not_found)
        }

        /// 等到有新日志或任务结束；任务消失时立即返回（SSE 侧据此收尾）。
        pub fn wait_for_monitor_events(&self, monitor_id: &str, cursor: u64, timeout: Duration) {
            if let Ok(monitors) = self.monitors() {
                monitors.wait_for_events(monitor_id, cursor, timeout);
            }
        }

        pub(super) fn monitors(&self) -> Result<&MonitorManager, ApiError> {
            self.options
                .monitors
                .as_ref()
                .ok_or_else(monitor_unavailable)
        }

}
