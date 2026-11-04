//! 由 `tools.rs` 按职责拆出，语义未变。

use super::*;
use super::subagent_tools::{emit_subagent_event};

/// 子代理的子回合：模型请求直连，工具批次就地借连接执行。
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_background_task(
    runtime: &SubAgentRuntime,
    model_config: &KernelModelConfig,
    task: &PreparedTask,
    fork_messages: &[Value],
    conn: &Rc<RefCell<Conn>>,
    cancel: &CancelToken,
    workspace_root: Option<&str>,
    stream_conversation: bool,
) -> Result<SubAgentExecution, (String, String)> {
    let mut messages = runtime.task_messages(task, fork_messages);
    let mut model = BackgroundModelPort {
        config: runtime.child_model_config(model_config, task),
        conn: Rc::clone(conn),
        cancel: cancel.clone(),
        task_id: task.task_id.clone(),
        batch_id: task.batch_id.clone(),
        agent_type: task.agent_type.clone(),
        stream: stream_conversation,
        runtime: None,
        model_turns: 0,
    };
    let mut tools = BackgroundTools {
        conn: Rc::clone(conn),
        turn_id: task.task_id.clone(),
        workspace_root: workspace_root.map(str::to_string),
        batch_id: task.batch_id.clone(),
        agent_type: task.agent_type.clone(),
        stream: stream_conversation,
        tool_calls: 0,
    };
    let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));
    let cancel_for_check = cancel.clone();
    let mut cancel_check = move || {
        if cancel_for_check.is_set() {
            Err(LoopError::Cancelled("子任务已取消。".to_string()))
        } else {
            Ok(())
        }
    };
    let guards = LoopGuards {
        cancel_check: Some(&mut cancel_check),
        stop_check: None,
    };
    let limits = AgentLoopLimits {
        max_model_turns: None,
        max_tool_calls: None,
        timeout_seconds: Some(runtime.config.default_timeout_seconds),
    };
    match runner.run(&mut messages, &mut model, &mut tools, limits, guards) {
        Ok(result) => Ok(SubAgentExecution {
            final_text: result.final_text,
            model_turns: result.model_turns,
            tool_calls: result.tool_calls,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
        }),
        Err(LoopError::Cancelled(message)) => Err(("SUBAGENT_CANCELLED".to_string(), message)),
        Err(error) => Err((
            "SUBAGENT_MODEL_ERROR".to_string(),
            format!("子任务模型请求失败：{}", error.tag()),
        )),
    }
}

/// 子代理的工具端口：直接在主循环线程里执行批次。
///
/// 批次不再经通道递回主循环——子回合与主回合同在一条线程，端口自己就能构造
/// `RemoteTools` 执行，不必等自己。
pub(crate) struct BackgroundTools {
    /// 就地执行用的连接：子回合与主回合同在一条线程，直接借它。
    pub(super) conn: Rc<RefCell<Conn>>,
    pub(super) turn_id: String,
    /// 隔离根：worktree 子任务的工具批次要在自己的工作树里执行。
    pub(super) workspace_root: Option<String>,
    /// 子代理对话流：批次号、角色名（与 `subagent.tool.*` 事件同一身份）。
    pub(super) batch_id: String,
    pub(super) agent_type: String,
    /// 是否流式上报子代理对话（只有 `/review` 这类宿主入口开启）。
    pub(super) stream: bool,
    /// 已经执行完的工具调用数：按批次的观察条数累加，覆盖一个批次多个调用的情况。
    pub tool_calls: usize,
}

impl BackgroundTools {
    /// 子代理工具事件的身份字段（与 Python 的 `subagent.tool.*` 载荷同形）。
    pub(crate) fn conversation_payload(&self, tool: &str, call_id: &str) -> Value {
        json!({
            "task_id": self.turn_id,
            "batch_id": self.batch_id,
            "agent_type": self.agent_type,
            "tool": tool,
            "call_id": call_id,
        })
    }

    /// 子代理事件直接外发：子回合已在主循环线程里，不需要经通道中转。
    pub(crate) fn emit(&self, name: &str, payload: Value) {
        emit_subagent_event(&self.conn, name, payload);
    }
}

impl ToolBatchHost for BackgroundTools {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        _first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
        // 开始事件先发：宿主据此把工具卡挂进子代理会话面板。
        if self.stream {
            for call in calls {
                let mut payload = self.conversation_payload(&call.name, &call.id);
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "arguments".to_string(),
                        public_tool_arguments(&call.name, &call.arguments),
                    );
                }
                self.emit("subagent.tool.started", payload);
            }
        }
        let started = std::time::Instant::now();
        // 就地执行：子回合与主回合同在一条线程，直接借连接里的工具端口，不经通道。
        // 子任务的工具调用不落父会话、不参与淘汰、不记 undo，与后台批次同一口径。
        let observations = {
            let mut tools = RemoteTools {
                conn: Rc::clone(&self.conn),
                turn_id: self.turn_id.clone(),
                workspace_root: self.workspace_root.clone(),
                vision_model: self.conn.borrow().model.clone(),
                undo: None,
                todos: None,
                active_assistant: None,
                task_hint: String::new(),
                previous_batch: Vec::new(),
                previous_batch_event_ids: Vec::new(),
            };
            tools
                .execute_tool_batch(calls, 0)
                .map_err(|error| LoopError::ToolBatch(error.tag().to_string()))?
        };
        let elapsed = started.elapsed().as_secs_f64();
        // 工具调用数在这里累加：批次的观察条数就是真正执行过的调用数。
        self.tool_calls += observations.len();
        if self.stream {
            for observation in &observations {
                let mut payload = self
                    .conversation_payload(&observation.tool_call.name, &observation.tool_call.id);
                if let Some(object) = payload.as_object_mut() {
                    let output = if observation.result.full_output.is_empty() {
                        observation.result.output.clone()
                    } else {
                        observation.result.full_output.clone()
                    };
                    object.insert("ok".to_string(), Value::Bool(observation.result.ok));
                    object.insert("output".to_string(), Value::String(output));
                    object.insert("duration_seconds".to_string(), json!(elapsed));
                }
                self.emit("subagent.tool.completed", payload);
            }
        }
        Ok(observations)
    }
}

/// 子代理的模型端口：直接发请求，事件就地外发。
pub(crate) struct BackgroundModelPort {
    pub(super) config: KernelModelConfig,
    /// 就地外发事件用的连接：子回合与主回合同在一条线程。
    pub(super) conn: Rc<RefCell<Conn>>,
    pub(super) cancel: CancelToken,
    /// 子代理对话流：任务号、批次号与角色名。
    pub(super) task_id: String,
    pub(super) batch_id: String,
    pub(super) agent_type: String,
    /// 是否流式上报子代理对话（与工具端口同一开关）。
    pub(super) stream: bool,
    /// 本任务内的模型运行时：子代理回合同样会「首次请求 + 每次工具观察后再来一次」，
    /// 复用同一实例才能保住出网脱敏的逐消息缓存（每次重建都会把 memo 清空重扫）。
    pub(super) runtime: Option<Box<dyn ModelRuntime>>,
    /// 已经发出的模型请求数：预算超限与取消路径也要如实上报，因此在这里自己数。
    pub model_turns: usize,
}

impl ReplySource for BackgroundModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        self.model_turns += 1;
        if self.runtime.is_none() {
            self.runtime = Some(build_model_runtime(&self.config)?);
        }
        let options = parse_options(&self.config)?;
        // 与主回合同一套线上名收敛（子 Agent 也能拿到 MCP 工具）。
        let (tools, name_map) = conform_tool_names(&parse_tools(&self.config));
        let identity = self.config.prompt_cache_identity.clone();
        let limit = self.config.request_retry_count.max(1);
        let mut attempt = 0;
        // 重试前的 call_id 轮换开关：只有网关明确判过「重复提交相同 call_id」才打开。
        let mut rotate_ids = false;

        loop {
            // 轮换只发生在重试路径：网关的重复提交判定按 id 认，原样再发等于再犯一次。
            let mut attempt_messages = if rotate_ids {
                rotate_ids = false;
                let mut list = messages.clone();
                let rotated = rotate_tail_call_ids(&mut list, &format!("r{attempt}"));
                self.notify_retry(format!(
                    "网关判定工具调用重复提交，重试前已轮换 {rotated} 个 call_id"
                ));
                list
            } else {
                messages.clone()
            };
            // 与主回合同一套出站收尾：配对缺口补齐/摘除，内部名改回线上名，
            // 只落在发出去的这份副本上。
            let pairing = repair_tool_pairing(&mut attempt_messages);
            if pairing.has_violation() {
                eprintln!("[kernel] 子代理工具调用配对体检：{}", pairing.describe());
            }
            name_map.restore_wire_names(&mut attempt_messages);
            let conversation = conversation_from_openai_messages(&attempt_messages);
            let input = ChatRequestInput {
                model: &self.config.model,
                system_prompt: &self.config.system_prompt,
                messages: &conversation,
                tools: &tools,
                options: &options,
                profile_request_timeout_seconds: self
                    .config
                    .request_timeout_seconds
                    .filter(|seconds| *seconds > 0.0)
                    .unwrap_or_default(),
                prompt_cache_capable: self.config.prompt_cache_capable,
                prompt_cache_identity: &identity,
            };
            let mut sink = BackgroundSink {
                conn: Rc::clone(&self.conn),
                cancel: self.cancel.clone(),
                task_id: self.task_id.clone(),
                batch_id: self.batch_id.clone(),
                agent_type: self.agent_type.clone(),
                stream: self.stream,
            };
            // 运行时在本段循环里只构建一次；只有中继账号池类拒绝会把 `self.runtime`
            // 置空，下一次重试重新建连接池。借用必须止于本行，否则拿不到可变借用。
            let result = {
                let runtime = self.runtime.as_ref().expect("运行时刚被构建");
                runtime.run_turn(&input, &mut sink)
            };
            match result {
                Ok(reply) => {
                    if is_empty_reply(&reply) {
                        attempt += 1;
                        if attempt < limit {
                            self.notify_retry(format!("正在重试(第{attempt}次)"));
                            continue;
                        }
                        return Err(self.model_error(format!(
                            "子任务连续 {limit} 次返回空响应，已停止本次请求。"
                        )));
                    }
                    let mut reply = reply;
                    name_map.restore_reply(&mut reply);
                    let reply = to_agent_reply(reply)?;
                    // 正文已经随流逐片进过窗口；整段返回的通道（无流式增量）补发一次，
                    // 否则窗口里这一段回复会完全缺失。
                    if self.stream && !reply.content.is_empty() && !reply.content_streamed {
                        emit_subagent_event(
                            &self.conn,
                            "subagent.turn.text",
                            json!({
                                "task_id": self.task_id,
                                "batch_id": self.batch_id,
                                "agent_type": self.agent_type,
                                "text": reply.content,
                            }),
                        );
                    }
                    self.notify_model_response(&reply);
                    return Ok(reply);
                }
                Err(error) => {
                    if error.kind == RuntimeErrorKind::Cancelled {
                        return Err(LoopError::Cancelled(error.message));
                    }
                    attempt += 1;
                    // 与主代理同一条策略：任何非取消的请求失败都先退避重试；上下文超限例外，
                    // 它要走压缩恢复路径，不能在这里耗重试次数。
                    let overflow = error.message.contains(CONTEXT_LENGTH_EXCEEDED_MESSAGE);
                    let retryable = !overflow;
                    // 与主回合同口径：中继账号池/工具定义类拒绝换连接重试，退避拉长。
                    let relay_rejected =
                        omnicrawl_llm::is_relay_account_or_tool_definition_error(&error.message);
                    if relay_rejected {
                        self.runtime = None;
                    }
                    if retryable && attempt < limit {
                        // 已经推进子代理窗口的半截流要先撤销，否则重试会叠在旧文本上。
                        if error.kind == RuntimeErrorKind::StreamInterrupted && self.stream {
                            self.emit_stream_rollback();
                        }
                        // 退避期间也要响应取消：子任务被取消后不该再多睡几秒。
                        if self.cancel.is_set() {
                            return Err(LoopError::Cancelled("子任务已取消。".to_string()));
                        }
                        self.notify_retry(format!("请求失败，正在自动重试（第{attempt}次）"));
                        // 这次失败是网关在说「同一个 call_id 又提交了一次」：原样重发只会
                        // 再被拒，下一轮用轮换过的尾部 id 发。
                        rotate_ids = is_tool_state_rejection(&error.message);
                        let backoff_ms = (400u64 << (attempt - 1).min(3)).min(2_000);
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                        continue;
                    }
                    if retryable {
                        let hint = if relay_rejected {
                            "（模型中继暂无可用账号：中继的上游账号被判定不可用，稍后重试或更换渠道。）"
                        } else {
                            ""
                        };
                        return Err(self.model_error(format!(
                            "上游错误已达到自动重试上限，已停止本次请求。{hint}{}",
                            error.message
                        )));
                    }
                    return Err(self.model_error(error.message));
                }
            }
        }
    }
}

impl BackgroundModelPort {
    /// `model.request.error`：一次子代理模型请求以错误终结时的通知。
    pub(crate) fn notify_model_error(&self, message: &str) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::ModelRequestError(ModelRequestErrorPayload {
                error: message.to_string(),
                model: self.config.model.clone(),
            }));
    }

    /// 记一条错误通知并把它原样折成循环错误。
    fn model_error(&self, message: String) -> LoopError {
        self.notify_model_error(&message);
        LoopError::ReplySource(message)
    }

    /// 重试进度：走子代理事件而不是 `HostEvent::RetryStatus`。
    ///
    /// `RetryStatus` 会顶掉主会话输入框上的状态行；子回合与主回合共用一条连接，
    /// 用它就等于把「子任务在重试」写成主代理的状态。
    fn notify_retry(&self, message: String) {
        emit_subagent_event(
            &self.conn,
            "subagent.retry",
            json!({
                "task_id": self.task_id,
                "batch_id": self.batch_id,
                "agent_type": self.agent_type,
                "message": message,
            }),
        );
    }

    /// 半截流作废：子代理窗口据此撤掉已经显示的那段正文，再接收重试后的新正文。
    ///
    /// 不能发 `HostEvent::StreamRollback`——那是主会话的撤销信号，会连带删掉主代理
    /// 正在显示的最后一条正文记录。
    fn emit_stream_rollback(&self) {
        emit_subagent_event(
            &self.conn,
            "subagent.turn.rollback",
            json!({
                "task_id": self.task_id,
                "batch_id": self.batch_id,
                "agent_type": self.agent_type,
            }),
        );
    }

    /// `model.response.after`：一次子代理模型请求成功返回后的观察通知。
    pub(crate) fn notify_model_response(&self, reply: &AgentModelReply) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::ModelResponseAfter(ModelResponseAfterPayload {
                model: self.config.model.clone(),
                content: reply.content.clone(),
                tool_call_count: reply.tool_calls.len(),
            }));
    }
}

/// 子代理的流事件出口：正文与思考增量转成 `subagent.turn.*` 事件，不混进主会话。
///
/// 子回合和主回合同在一条连接上，`HostEvent::Delta` / `ReasoningDelta` 会被宿主直接
/// 追加到主会话的正文与思考段里。因此这里既不发 `turn.delta`，也不把增量丢掉——
/// 改成带着批次身份的子代理事件，由宿主送进独立窗口；窗口按批次渲染，主代理的回复
/// 不受影响。`stream` 为假（不需要过程流的入口）时只外发 Provider 告警。
pub(crate) struct BackgroundSink {
    pub(super) conn: Rc<RefCell<Conn>>,
    pub(super) cancel: CancelToken,
    pub(super) task_id: String,
    pub(super) batch_id: String,
    pub(super) agent_type: String,
    pub(super) stream: bool,
}

impl BackgroundSink {
    pub(crate) fn emit_text(&self, name: &str, text: &str) {
        if !self.stream || text.is_empty() {
            return;
        }
        emit_subagent_event(
            &self.conn,
            name,
            json!({
                "task_id": self.task_id,
                "batch_id": self.batch_id,
                "agent_type": self.agent_type,
                "text": text,
            }),
        );
    }
}

impl TurnSink for BackgroundSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        match event {
            ModelStreamEvent::ProviderWarning(ref warning) => {
                let event = provider_warning_event(warning);
                self.conn.borrow_mut().notify(event);
            }
            // 文本与思考的增量按片外发：窗口里能逐字出现，不必等整段回复落地。
            ModelStreamEvent::TextDelta(delta) => self.emit_text("subagent.turn.text", &delta.text),
            ModelStreamEvent::ReasoningDelta(delta) => {
                self.emit_text("subagent.turn.reasoning", &delta.text)
            }
            _ => {}
        }
        if self.cancel.is_set() {
            SinkFlow::Cancel
        } else {
            SinkFlow::Continue
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_set()
    }
}

