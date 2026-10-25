//! 由 `turn.rs` 按职责拆出，语义未变。

use super::*;

/// 压缩后的上下文 Token 估算：稳定上下文 + 运行期历史。
///
/// 稳定上下文 = 系统提示词 + 宿主给的上下文消息（项目规范、Skill、运行环境）+ 工具声明。
/// 拼装与计价都在 `omnicrawl_controllers::context_compaction::meter::conversation_tokens`，
/// 与压缩驱动自己的预算测量共用同一份实现，因此宿主显示的「压缩后大小」与实际进入下一次
/// 请求的内容一致；多模态消息里的图片按句柄计价，不会把 Base64 算进来。
///
/// 取不到模型配置时返回 `None`：没有系统提示词与工具面就算不出可比的整体大小，此时宁可
/// 不报，也不给一个只含历史的偏小数。
pub(crate) fn post_compaction_context_tokens(conn: &Rc<RefCell<Conn>>, history: &[Value]) -> Option<i64> {
    let model = conn.borrow().model.clone()?;
    let tokens = omnicrawl_controllers::context_compaction::meter::conversation_tokens(
        &model.system_prompt,
        &model.context_messages,
        &model.tools,
        history,
        &omnicrawl_controllers::context_compaction::meter::text_tokens,
    );
    Some(tokens.max(0))
}

/// 内核侧的对话连接：帧的收发、请求应答配对与回合状态。
pub(crate) struct Conn {
    pub(crate) inbound: Receiver<Inbound>,
    pub(crate) out: Box<dyn Write>,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) handshaken: bool,
    pub(crate) exit_requested: bool,
    pub(crate) next_id: i64,
    /// 宿主交来的模型配置：有它内核自己发模型请求，没有则退回 `model.reply` 代答。
    pub(crate) model: Option<KernelModelConfig>,
    /// 宿主交来的会话配置：有它内核自己持有会话（多轮历史、转录落盘与回合结束后的压缩）。
    pub(crate) session: Option<KernelSession>,
    /// 宿主是否支持插件模型 Hook：声明后才发 `model.hook` 请求（否则不阻住回合）。
    pub(crate) plugin_model_hooks: bool,
    /// 宿主是否支持工具调用淘汰裁决：声明后才发 `context.prune`（否则不阻住回合）。
    pub(crate) tool_call_prune: bool,
    /// 本回合模型请求的用量累计与最近一次请求消息（压缩判定与前缀复用都要用）。
    pub(crate) usage: Rc<RefCell<TurnUsage>>,
    /// 本轮「淘汰标注」账本：裁决判为无用的调用 ID。
    ///
    /// 与 `usage` 一样是**每回合清零**的运行期状态，不进转录：裁决当刻只落
    /// `tool_call_evicted` 事件（=标注），真正的剔除留到回合收尾一次性做
    /// （见 [`run_session_tail`]）——这就是用户要的「先标注-后压缩」。
    pub(crate) evict_marks: Rc<RefCell<Vec<String>>>,
    /// 后台任务管理器：任务表供 `subagent.query` 与工作区切换拦阻查询。
    pub(crate) tasks: Option<SubAgentTaskManager>,
    /// 跨回合复用的模型运行时：按构建键缓存最后一个。
    ///
    /// 回合边界原本都会重建运行时，于是出网脱敏的逐消息缓存、屏蔽计划缓存与 ureq
    /// 连接池全部清零，模型请求要重做 TCP + TLS 握手——这些都落在「回车 → 首字」区间。
    /// 只保留一份：同一时刻只有一条模型配置在生效，换渠道/换 Key 时构建键变化即重建。
    pub(crate) runtime_cache: Option<Rc<KernelModelRuntime>>,
}

impl Conn {
    pub(crate) fn send(&mut self, frame: &Frame) -> Result<(), String> {
        let mut line = frame.to_line();
        line.push('\n');
        self.out
            .write_all(line.as_bytes())
            .and_then(|()| self.out.flush())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn respond(&mut self, id: Id, result: Value) {
        if let Err(detail) = self.send(&Frame::response(id, result)) {
            eprintln!("[kernel] 写响应失败：{detail}");
        }
    }

    pub(crate) fn respond_error(&mut self, id: Id, error: ErrorObject) {
        if let Err(detail) = self.send(&Frame::error_response(id, error)) {
            eprintln!("[kernel] 写错误响应失败：{detail}");
        }
    }

    pub(crate) fn notify(&mut self, event: HostEvent) {
        if let Err(detail) = self.send(&event.to_frame()) {
            eprintln!("[kernel] 写事件失败：{detail}");
        }
    }

    /// 应用一次 `session.settings`：模型配置与会话压缩配置，对后续回合生效。
    ///
    /// `tool_call_prune` 不在模型配置里：它是宿主运行期的能力开关，因此单独落到
    /// [`Conn::tool_call_prune`]（设置面板改开关后即时生效，不必重起内核）。
    pub(crate) fn apply_settings(
        &mut self,
        params: &SessionSettingsParams,
    ) -> Result<Vec<String>, crate::settings::SettingsRejection> {
        let prune = params
            .model
            .as_deref()
            .and_then(|settings| settings.tool_call_prune);
        // 只有这一项时按「能力更新」处理：它不属于模型配置，不能因为没有模型配置而被拒。
        let model_only_prune = prune.is_some()
            && params
                .model
                .as_deref()
                .is_some_and(|settings| settings_without_prune_is_empty(settings));
        let mut applied = if model_only_prune {
            Vec::new()
        } else {
            let config = self.session.as_mut().map(|session| &mut session.config);
            crate::settings::apply(&mut self.model, config, params)?
        };
        if let Some(enabled) = prune {
            self.tool_call_prune = enabled;
            applied.push("model.tool_call_prune".to_string());
        }
        Ok(applied)
    }

    /// 退出前的会话收尾：补写 `session_closed` 并丢弃空占位。
    ///
    /// 宿主在发 `shutdown` 前已发 `session.close.before`、收到进程退出后发
    /// `session.close.after`，事件补写正落在这两个钩子之间。判定与幂等性见
    /// [`KernelSession::close`]。没有自持会话时什么都不做。
    pub(crate) fn close_session(&self) {
        if let Some(session) = self.session.as_ref() {
            session.close();
        }
    }

    /// 运行中切换工作区：把新根记到自持会话并转录 `workspace_switched`。
    ///
    /// 宿主侧已经解析并校验过路径，内核不再重复解析。`Ok(None)` 表示当前会话
    /// 不受内核持有（与 `session.append` 回同一个错误）；`Err` 是转录失败。
    pub(crate) fn switch_workspace(&mut self, path: &str) -> Result<Option<(String, String)>, String> {
        match self.session.as_mut() {
            Some(session) => session.switch_workspace(path).map(Some),
            None => Ok(None),
        }
    }

    /// 取跨回合复用的模型运行时：构建键一致就复用，否则重建并替换缓存。
    ///
    /// `build` 只在未命中时调用一次；构建失败（缺凭据、协议不合法等）不写缓存，
    /// 下一次请求会再试，行为与逐回合重建时一致。
    pub(crate) fn model_runtime(
        &mut self,
        key: &RuntimeBuildKey,
        build: impl FnOnce() -> Result<Box<dyn ModelRuntime>, LoopError>,
    ) -> Result<Rc<KernelModelRuntime>, LoopError> {
        if let Some(cached) = self.runtime_cache.as_ref() {
            if cached.key == *key {
                return Ok(Rc::clone(cached));
            }
        }
        let runtime = Rc::new(KernelModelRuntime {
            key: key.clone(),
            runtime: build()?,
        });
        self.runtime_cache = Some(Rc::clone(&runtime));
        Ok(runtime)
    }

    /// 完成握手：版本不匹配回 `-32001`。
    ///
    /// 回包里带上当前会话 id（`initialize.session` 没给 id 而新建时，这是宿主唯一
    /// 能知道「现在在哪个会话」的途径；`/sessions`、`/rename` 都依赖它）。
    pub(crate) fn handshake(&mut self, id: Id, host_version: &str) {
        match negotiate_version(host_version) {
            Ok(_) => {
                self.handshaken = true;
                let session_id = self
                    .session
                    .as_ref()
                    .map(|session| session.session_id.clone())
                    .unwrap_or_default();
                let mut result = initialize_result();
                if let Some(object) = result.as_object_mut() {
                    object.insert("session_id".to_string(), Value::String(session_id));
                }
                self.respond(id, result);
            }
            Err(_) => self.respond_error(id, unsupported_version_error(host_version)),
        }
    }

    /// 发请求并等它的响应；等待期间照常处理入站帧。
    pub(crate) fn request(&mut self, method_name: &str, params: Value) -> Result<Value, PortFailure> {
        self.next_id += 1;
        let id = Id::Number(self.next_id);
        self.send(&Frame::request(id.clone(), method_name, params))
            .map_err(PortFailure::Io)?;
        self.await_response(&id)
    }

    pub(crate) fn await_response(&mut self, id: &Id) -> Result<Value, PortFailure> {
        loop {
            let frame = match self.inbound.recv() {
                Ok(Inbound::Frame(frame)) => *frame,
                Ok(Inbound::Closed) | Err(_) => return Err(PortFailure::Disconnected),
            };
            let is_response = frame.method().is_none() && frame.id().is_some();
            if is_response {
                if frame.id() == Some(id) {
                    return match &frame.error {
                        Some(error) => Err(PortFailure::Remote(error.clone())),
                        None => Ok(frame.result.clone().unwrap_or(Value::Null)),
                    };
                }
                eprintln!("[kernel] 忽略不在等待的响应：{:?}", frame.id());
                continue;
            }
            if let Some(failure) = self.handle_inbound(&frame) {
                return Err(failure);
            }
        }
    }

    /// 处理回合进行中的入站帧；返回 `Some` 表示当前回合立即中止。
    pub(crate) fn handle_inbound(&mut self, frame: &Frame) -> Option<PortFailure> {
        let id = frame.id().cloned();
        let method_name = frame.method().unwrap_or_default().to_string();
        match (id, method_name.as_str()) {
            (Some(id), method::TURN_CANCEL) => {
                self.cancel.store(true, Ordering::SeqCst);
                self.respond(id, json!({}));
                Some(PortFailure::Cancelled("回合已取消。".to_string()))
            }
            (Some(id), method::WORKSPACE_SWITCH) => {
                // 回合进行中切换：当前回合的工作区快照已拍，更新只对后续回合生效。
                match serde_json::from_value::<WorkspaceSwitchParams>(
                    frame.params.clone().unwrap_or_else(|| json!({})),
                ) {
                    Ok(params) => handle_workspace_switch(self, id, &params),
                    Err(error) => self.respond_error(
                        id,
                        ErrorObject::new(
                            error_code::INVALID_PARAMS,
                            format!("workspace.switch 负载不符：{error}"),
                        ),
                    ),
                }
                None
            }
            (Some(id), method::SHUTDOWN) => {
                self.cancel.store(true, Ordering::SeqCst);
                self.exit_requested = true;
                // 回合进行中也能收到 shutdown：先收尾会话再回包，保证 `session_closed`
                // 落在宿主的 before / after 钩子之间。
                self.close_session();
                self.respond(id, json!({}));
                Some(PortFailure::Shutdown)
            }
            (Some(id), method::TURN_SUBMIT) => {
                self.respond_error(
                    id,
                    ErrorObject::new(error_code::TURN_BUSY, "已有回合在执行。"),
                );
                None
            }
            (Some(id), method::INITIALIZE) => {
                let host_version = protocol_version_of(frame.params.as_ref());
                if let Some(config) = model_config_of(frame.params.as_ref()) {
                    self.model = Some(*config);
                }
                self.plugin_model_hooks = plugin_model_hooks_of(frame.params.as_ref());
                self.tool_call_prune = tool_call_prune_of(frame.params.as_ref());
                if let Some(settings) = session_config_of(frame.params.as_ref()) {
                    match KernelSession::open(*settings) {
                        Ok(opened) => self.session = Some(opened),
                        Err(detail) => {
                            self.respond_error(
                                id,
                                ErrorObject::new(
                                    error_code::INVALID_PARAMS,
                                    format!("会话初始化失败：{detail}"),
                                ),
                            );
                            return None;
                        }
                    }
                }
                self.handshake(id, &host_version);
                None
            }
            (Some(id), method::SESSION_LIST)
            | (Some(id), method::SESSION_HISTORY)
            | (Some(id), method::SESSION_EVENTS) => {
                // 只读会话查询在回合进行中也照常应答（见 [`SessionQuery`]）。
                match SessionQuery::from_frame(frame) {
                    Ok(query) => match query.respond(self) {
                        Ok(value) => self.respond(id, value),
                        Err(error) => self.respond_error(id, error),
                    },
                    Err(error) => self.respond_error(id, error),
                }
                None
            }
            (Some(id), method::SESSION_SETTINGS) => {
                // 回合进行中也照常应答：本次更新对后续回合生效，正在跑的回合
                // 在开始时已快照模型配置，不会被中途换掉。
                let params = frame.params.clone().unwrap_or_else(|| json!({}));
                match serde_json::from_value::<SessionSettingsParams>(params) {
                    Ok(params) => match self.apply_settings(&params) {
                        Ok(applied) => self.respond(id, crate::settings::applied_result(&applied)),
                        Err(rejection) => self.respond_error(id, rejection.to_error()),
                    },
                    Err(error) => self.respond_error(
                        id,
                        ErrorObject::new(
                            error_code::INVALID_PARAMS,
                            format!("session.settings 负载不符：{error}"),
                        ),
                    ),
                }
                None
            }
            (Some(id), other) => {
                self.respond_error(
                    id,
                    ErrorObject::new(error_code::METHOD_NOT_FOUND, format!("未知方法 {other}。")),
                );
                None
            }
            (None, other) => {
                eprintln!("[kernel] 忽略宿主的通知：{other}");
                None
            }
        }
    }
}

/// 内核流事件 → 协议通知。
///
/// 工具生命周期事件由宿主在执行 `tool.batch` 时自行产生，这里只发模型侧的信息，
/// 避免同一件事在协议上出现两份。
pub(crate) struct ProtocolSink {
    pub(crate) conn: Rc<RefCell<Conn>>,
    pub(crate) usage: Rc<RefCell<TurnUsage>>,
}

impl TurnSink for ProtocolSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        // 内核自己发请求时主循环正阻塞在这一层：宿主的 `turn.cancel` / `shutdown` 只能靠
        // 每个流事件里顺手把入站帧抽干才会被看见（否则 ESC 要等整段流跑完）。
        if drain_inbound_during_turn(&self.conn) {
            return SinkFlow::Cancel;
        }
        let mut connection = self.conn.borrow_mut();
        match event {
            ModelStreamEvent::TextDelta(delta) => {
                connection.notify(HostEvent::Delta(TextPayload { text: delta.text }));
            }
            ModelStreamEvent::ReasoningDelta(delta) => {
                connection.notify(HostEvent::ReasoningDelta(TextPayload { text: delta.text }));
            }
            ModelStreamEvent::UsageReported(usage) => {
                // 用量字段是有符号的：负值原样累加与透传，与 Python 同口径。
                {
                    let mut totals = self.usage.borrow_mut();
                    totals.last_request_input_tokens = usage.input_tokens;
                    totals.input_tokens += usage.input_tokens;
                    totals.output_tokens += usage.output_tokens;
                    totals.cached_input_tokens += usage.cached_input_tokens;
                }
                connection.notify(HostEvent::TokenUsage(TokenUsagePayload {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                }));
            }
            ModelStreamEvent::ProviderWarning(warning) => {
                connection.notify(provider_warning_event(&warning));
            }
            // 工具调用随模型流一起给宿主：宿主先把卡片立起来、逐段补参数
            // （Python 只在批次执行时才画卡片，这是 Rust 侧刻意的增量渲染）。
            ModelStreamEvent::ToolCallStarted(started) => {
                connection.notify(HostEvent::ToolCallStarted(ToolCallStartedPayload {
                    call_id: started.call_id,
                    tool: started.name,
                }));
            }
            ModelStreamEvent::ToolCallArgumentsDelta(delta) => {
                connection.notify(HostEvent::ToolCallArguments(ToolCallArgumentsPayload {
                    call_id: delta.call_id,
                    delta: delta.delta,
                }));
            }
            _ => {}
        }
        SinkFlow::Continue
    }

    fn cancelled(&self) -> bool {
        self.conn.borrow().cancel.load(Ordering::SeqCst)
    }
}

