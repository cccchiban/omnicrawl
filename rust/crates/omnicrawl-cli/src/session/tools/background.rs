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
        let runtime = self.runtime.as_ref().expect("运行时刚被构建");
        let options = parse_options(&self.config)?;
        // 与主回合同一套线上名收敛（子 Agent 也能拿到 MCP 工具）。
        let (tools, name_map) = conform_tool_names(&parse_tools(&self.config));
        let conversation = conversation_from_openai_messages(messages);
        let identity = self.config.prompt_cache_identity.clone();
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
        match runtime.run_turn(&input, &mut sink) {
            Ok(reply) => {
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
                Ok(reply)
            }
            Err(error) => {
                if error.kind == RuntimeErrorKind::Cancelled {
                    return Err(LoopError::Cancelled(error.message));
                }
                self.notify_model_error(&error.message);
                Err(LoopError::ReplySource(error.message))
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

