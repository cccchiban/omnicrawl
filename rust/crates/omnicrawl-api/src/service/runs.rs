//! 由 `service.rs` 按职责拆出，语义未变。

use super::*;

impl AgentService {
        /// 受理一个生成任务：登记运行、起回合线程，立刻返回待办视图。
        pub fn start_run(self: &Arc<Self>, message: &str) -> Result<RunRecord, ApiError> {
            let text = message.trim();
            if text.is_empty() {
                return Err(ApiError::bad_request(
                    "INVALID_MESSAGE",
                    "message 不能为空。",
                ));
            }
            let record = self
                .store
                .create(&random_id(), text, &self.current_session_id())
                .map_err(|error| error.to_api_error())?;
            let run_id = record.run_id.clone();
            let service = Arc::clone(self);
            let message = text.to_string();
            thread::spawn(move || service.execute_run(&run_id, &message));
            Ok(record)
        }

        pub fn get_run(&self, run_id: &str) -> Result<RunRecord, ApiError> {
            self.store.get(run_id).map_err(|error| error.to_api_error())
        }

        pub fn summary(&self, run_id: &str) -> Result<Value, ApiError> {
            Ok(self.get_run(run_id)?.summary())
        }

        /// 读取事件流（`GET /runs/{id}/events` 的每次轮询都走这里）。
        pub fn events_after(&self, run_id: &str, cursor: u64) -> Result<Vec<RunEvent>, ApiError> {
            self.store
                .events_after(run_id, cursor)
                .map_err(|error| error.to_api_error())
        }

        /// 请求取消：置位取消标记并让待决决策得到 409。
        pub fn cancel_run(&self, run_id: &str) -> Result<Value, ApiError> {
            let run = self.get_run(run_id)?;
            if run.status.is_terminal() {
                return Ok(run.summary());
            }
            self.store
                .request_cancel(run_id)
                .map_err(|error| error.to_api_error())?;
            // 多 worker：把取消投递给持有该 Run 的所有者进程（对应 Python `request_cancel`）。
            self.push_shared_decision(run_id, DECISION_CANCEL, "", json!({}));
            if let Some(control) = self.control.lock().ok().and_then(|guard| guard.clone()) {
                control.request_cancel();
            }
            self.summary(run_id)
        }

        /// 提交审批决议；重复决议得到 409，不认识的 id 得到 404。
        pub fn decide_confirmation(
            &self,
            run_id: &str,
            confirmation_id: &str,
            approved: bool,
        ) -> Result<Value, ApiError> {
            self.get_run(run_id)?;
            let confirmation = self
                .store
                .resolve_confirmation(run_id, confirmation_id, approved)
                .map_err(|error| error.to_api_error())?;
            self.push_shared_decision(
                run_id,
                DECISION_CONFIRM,
                confirmation_id,
                json!({"approved": approved}),
            );
            Ok(json!({
                "confirmation_id": confirmation.confirmation_id,
                "approved": approved,
            }))
        }

        /// 提交提问答案；`select` 只能提交已声明的选项。
        pub fn decide_question(
            &self,
            run_id: &str,
            question_id: &str,
            answer: &str,
        ) -> Result<Value, ApiError> {
            self.get_run(run_id)?;
            let question = self
                .store
                .resolve_question(run_id, question_id, answer)
                .map_err(|error| error.to_api_error())?;
            self.push_shared_decision(
                run_id,
                DECISION_ANSWER,
                question_id,
                json!({"answer": answer}),
            );
            Ok(json!({
                "question_id": question.question_id,
                "answer": question.answer,
            }))
        }

        /// 把一条跨进程决策投进共享存储；单进程内存后端上是空操作。
        pub(super) fn push_shared_decision(&self, run_id: &str, kind: &str, target_id: &str, payload: Value) {
            if let Some(store) = self.store.shared() {
                let _ = store.push_decision(run_id, kind, target_id, payload);
            }
        }

        /// 隔离工作区退出收尾：按 `[agent_workspace]` 的 `apply_on_exit` / `cleanup_on_exit`
        /// 处理主隔离区，再按 auto 策略收尾 SubAgent worktree（只清理无变更的）。
        ///
        /// 对映 Python `LocalToolAgent._finalize_attached_isolation`：与 TUI 共用同一收尾路径，
        /// 避免 API / 连接器入口的改动滞留在隔离区。
        pub(super) fn finalize_isolation(&self) {
            let session = self
                .options
                .isolation
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            let mut summaries: Vec<String> = Vec::new();
            if let Some(session) = session {
                let config = load_agent_workspace_config(&self.options.env, None).unwrap_or_default();
                let summary = finalize_isolation_session(
                    &session,
                    config.apply_on_exit,
                    &config.cleanup_on_exit,
                    None,
                    None,
                );
                if !summary.is_empty() {
                    summaries.push(summary);
                }
            }
            let sub_summary = finalize_subagent_worktrees(None, "origin");
            if !sub_summary.is_empty() {
                summaries.push(sub_summary);
            }
            if !summaries.is_empty() {
                eprintln!("[api] [isolation] {}", summaries.join("；"));
            }
        }

        /// 启动回合外事件泵：周期性排空内核通知并投影到会话级事件流。
        ///
        /// 回合内的事件由回合线程的 sink 直接喂；回合外内核仍会推后台任务/审批事件，没有这条泵，
        /// 订阅 `/subagents/events` 的客户端只能等到下一次回合开始才看见它们。
        pub fn start_event_pump(self: &Arc<Self>) {
            let service = Arc::clone(self);
            thread::spawn(move || loop {
                thread::sleep(EVENT_PUMP_INTERVAL);
                if service.closed.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut guard) = service.runner.try_lock() else {
                    // 回合在途：事件此刻由回合线程消费，泵不争锁。
                    continue;
                };
                let Some(runner) = guard.as_mut() else {
                    continue;
                };
                for event in runner.drain_notifications() {
                    if let HostEvent::SubagentEvent(payload) = event {
                        service.record_subagent_event(&payload);
                    }
                }
                // 审查配置的外部改动：与回合无关，泵顺手在持锁时应用，不必等下一次请求或重启。
                service.apply_review_config(runner);
            });
        }

        /// 把一条回合外的子代理通知投影到会话级事件流。
        pub(super) fn record_subagent_event(&self, payload: &SubagentEventPayload) {
            let _ = self.emit_subagent_event("", payload);
        }

        /// 子代理通知 → 会话级事件流，返回公开的 `(事件名, 载荷)`。
        ///
        /// 单进程用进程内会话事件流；多 worker 以共享存储为唯一事实来源，因为产生事件的
        /// Run 可能由别的 worker 持有（对应 Python `_on_subagent_event` 的两条分支）。
        pub(super) fn emit_subagent_event(&self, run_id: &str, payload: &SubagentEventPayload) -> (String, Value) {
            let (name, data) = self.subagent_event_payload(run_id, payload);
            if let Some(store) = self.store.shared() {
                self.emit_shared_subagent_event(store, &name, &data);
                return (name, data);
            }
            let session_id = self.current_session_id();
            self.subagent_feed.push(
                &session_id,
                &name,
                data.clone(),
                self.options.max_events_per_run,
            );
            (name, data)
        }

        /// 多 worker：后台任务事件写进共享存储的会话级事件流。
        ///
        /// 事件 id 由会话级计数器分配；任务来源优先查 `task_sources`，只有活动运行才能
        /// 建立新映射（与 Python `_on_shared_subagent_event` 同规则）。
        pub(super) fn emit_shared_subagent_event(&self, store: &SharedRunStore, name: &str, data: &Value) {
            let task_id = data
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let known = if task_id.is_empty() {
                None
            } else {
                store.task_source(&task_id).ok().flatten()
            };
            let (parent_run_id, session_id) = match known {
                Some(source) => source,
                None => {
                    let Some(active_id) = self.store.active_run_id() else {
                        return;
                    };
                    let Ok(record) = self.store.get(&active_id) else {
                        return;
                    };
                    if !record.status.is_active() {
                        return;
                    }
                    if !task_id.is_empty() {
                        let _ = store.record_task_source(&task_id, &record.run_id, &record.session_id);
                    }
                    (record.run_id, record.session_id)
                }
            };
            let Ok(event_id) = store.allocate_subagent_event_id(&session_id) else {
                return;
            };
            let mut payload = data.clone();
            if let Some(map) = payload.as_object_mut() {
                map.insert("session_id".to_string(), json!(session_id));
                map.insert("parent_run_id".to_string(), json!(parent_run_id));
            }
            if store
                .append_subagent_event(&session_id, event_id, name, payload)
                .is_err()
            {
                return;
            }
            if TERMINAL_SUBAGENT_EVENTS.contains(&name) && !task_id.is_empty() {
                let _ = store.drop_task_source(&task_id);
            }
        }

        /// 子代理通知 → 公开事件载荷：值级脱敏 + 补来源字段。
        ///
        /// `run_id` 为空表示这条通知发生在回合之外（后台任务），此时只带 `session_id`。
        pub(super) fn subagent_event_payload(
            &self,
            run_id: &str,
            payload: &SubagentEventPayload,
        ) -> (String, Value) {
            let session_id = self.current_session_id();
            let mut data = match redact_sensitive_values(&payload.payload) {
                Value::Object(map) => Value::Object(map),
                other => json!({"payload": other}),
            };
            if let Some(map) = data.as_object_mut() {
                map.insert("session_id".to_string(), json!(session_id));
                if !run_id.is_empty() {
                    map.insert("run_id".to_string(), json!(run_id));
                }
            }
            (payload.name.clone(), data)
        }

        pub(super) fn execute_run(self: Arc<Self>, run_id: &str, message: &str) {
            let _ = self.store.set_status(run_id, RunStatus::Running);
            let session_id = self.current_session_id();
            let _ = self.store.append_event(
                run_id,
                "run.started",
                json!({"run_id": run_id, "session_id": session_id}),
            );

            let control = TurnControl::new();
            if let Ok(mut guard) = self.control.lock() {
                *guard = Some(control.clone());
            }
            // 受理与取消之间有窗口：连接器的 `/cancel` 可能在本回合装上控制面之前就
            // 置好了 store 里的取消标记（`cancel_run` 只对已存在的控制面生效）。这里补一次
            // 对齐，否则标记留在 store 里，回合照常跑到底，用户看到的是「取消了却仍在执行」。
            if self.store.cancel_requested(run_id).unwrap_or(false) {
                control.request_cancel();
            }
            let mut interactor = ServiceInteractor {
                service: Arc::clone(&self),
                run_id: run_id.to_string(),
                timeout_seconds: self.options.confirmation_timeout_seconds,
            };
            let outcome = {
                let mut guard = match self.runner.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                match guard.as_mut() {
                    Some(runner) => {
                        let mut sink = |event: HostEvent| self.publish(run_id, event);
                        runner.submit(message, &control, &mut interactor, &mut sink)
                    }
                    None => Err(TurnError::KernelExited),
                }
            };
            if let Ok(mut guard) = self.control.lock() {
                *guard = None;
            }
            // 回合结束后清掉可能残留的跨进程决策（例如非等待路径上的取消）。
            if let Some(store) = self.store.shared() {
                let _ = store.take_decisions(run_id);
            }

            match outcome {
                Ok(result) => {
                    let _ = self
                        .store
                        .finish(run_id, RunStatus::Completed, &result.final_text, "");
                    let _ = self.store.append_event(
                        run_id,
                        "run.completed",
                        json!({"run_id": run_id, "result": result.final_text}),
                    );
                }
                Err(TurnError::Cancelled) => {
                    let _ = self
                        .store
                        .finish(run_id, RunStatus::Cancelled, "", RUN_CANCELLED_MESSAGE);
                    let _ = self.store.append_event(
                        run_id,
                        "run.cancelled",
                        json!({"run_id": run_id, "message": RUN_CANCELLED_MESSAGE}),
                    );
                }
                Err(error) => {
                    // 第三方异常正文不可信：只记日志，对外统一文案。
                    eprintln!("[api] run {run_id} 失败：{error}");
                    let _ = self
                        .store
                        .finish(run_id, RunStatus::Failed, "", RUN_FAILED_MESSAGE);
                    let _ = self.store.append_event(
                        run_id,
                        "run.failed",
                        json!({"run_id": run_id, "message": RUN_FAILED_MESSAGE}),
                    );
                }
            }
        }

        /// 内核通知 → 公开 SSE 事件。
        ///
        /// 隐藏推理（`turn.reasoning_delta`）与工具输出增量不进事件流；子代理与清单事件
        /// 带来源（`run_id` / `session_id`），参数一律走安全投影。
        pub(super) fn publish(&self, run_id: &str, event: HostEvent) {
            let session_id = self.current_session_id();
            let (name, data): (String, Value) = match event {
                HostEvent::Delta(payload) => (
                    "assistant.delta".to_string(),
                    json!({"delta": payload.text}),
                ),
                HostEvent::Status(payload) => (
                    "status.changed".to_string(),
                    json!({"message": payload.message}),
                ),
                // 会话区提示（脱敏占位符还原告警等）：与 `turn.status` 的差别只在宿主的落点
                // （对话流 vs 运行状态行），对外仍是同一个「消息变更」事件——公开事件清单不变。
                HostEvent::Notice(payload) => (
                    "status.changed".to_string(),
                    json!({"message": payload.message}),
                ),
                HostEvent::RetryStatus(payload) => (
                    "status.changed".to_string(),
                    json!({"message": payload.message}),
                ),
                HostEvent::ProtocolWait => (
                    "status.changed".to_string(),
                    json!({"message": PROTOCOL_WAIT_STATUS}),
                ),
                HostEvent::StreamRollback => (
                    crate::connector::ROLLBACK_EVENT.to_string(),
                    json!({"message": STREAM_ROLLBACK_STATUS}),
                ),
                HostEvent::TokenUsage(payload) => (
                    "usage.updated".to_string(),
                    json!({
                        "input_tokens": payload.input_tokens,
                        "output_tokens": payload.output_tokens,
                        "cached_input_tokens": payload.cached_input_tokens,
                    }),
                ),
                HostEvent::ToolStarted(payload) => (
                    "tool.started".to_string(),
                    json!({
                        "step": payload.step,
                        "tool_call_id": payload.call.id,
                        "tool": payload.call.name,
                        "arguments": public_tool_arguments(&payload.call.name, &payload.call.arguments),
                    }),
                ),
                HostEvent::ToolFinished(payload) => (
                    "tool.completed".to_string(),
                    json!({
                        "tool_call_id": payload.call.id,
                        "tool": payload.call.name,
                        "ok": payload.result.ok,
                        "output": payload.result.output,
                        // HTML artifact 正文只留在受会话保护的存储里；Rust 工具结果暂无 artifact。
                        "artifact": {},
                    }),
                ),
                HostEvent::TodoUpdate(payload) => {
                    let todos = todo_snapshots(&payload.todos);
                    let _ = self.store.record_todos(run_id, todos.clone());
                    (
                        "todo.updated".to_string(),
                        json!({
                            "run_id": run_id,
                            "session_id": session_id,
                            "todos": todos.iter().map(TodoSnapshot::to_value).collect::<Vec<Value>>(),
                        }),
                    )
                }
                HostEvent::SubagentEvent(payload) => {
                    // 会话级控制流与回合内事件流看到同一份载荷（参数已脱敏）。
                    self.emit_subagent_event(run_id, &payload)
                }
                // 回合收尾由 `submit` 的返回值负责，通知本身不重复成事件。
                HostEvent::TurnFinished(_) => return,
                // 压缩计量与模型请求的两个观测点不进 SSE：它们只用于宿主侧计量，
                // 不构成对外事件契约。
                HostEvent::ContextCompaction(_)
                | HostEvent::ModelResponseAfter(_)
                | HostEvent::ModelRequestError(_) => return,
                // 界面增量渲染专用的事件都不进 SSE：隐藏推理、工具输出增量，以及流式工具卡的
                // 三段（`tool_call_started` / `tool_call_arguments` / `tool_output_compression`）。
                // 后者是内核为了让 TUI 在模型还在写参数时就把卡片立起来而新增的协议通知，
                // Python 侧 API 只订阅 `run_stream` 的固定回调集（不含这三个），公开事件清单
                // 里也没有对应事件名；工具的公开形态仍是 `tool.started`（批次执行时）与
                // `tool.completed`（整批回填后），批量增长中的半截参数不属于对外契约。
                HostEvent::ReasoningDelta(_)
                | HostEvent::ToolOutputUpdate(_)
                | HostEvent::ToolCallStarted(_)
                | HostEvent::ToolCallArguments(_)
                | HostEvent::ToolOutputCompression(_) => return,
            };
            let _ = self.store.append_event(run_id, &name, data);
        }

}
