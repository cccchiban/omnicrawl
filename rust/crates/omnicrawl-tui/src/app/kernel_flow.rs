//! 内核帧与工具批次编排：`handle_frame` 分发、批次步骤推进、审查闸与截止时间。
//!
//! 这一块决定「一条内核帧变成什么界面动作」，也决定一批工具怎么发出去、怎么收回来。
//! 与输入处理分开：改批次编排不该动键盘派发，反之亦然。

use super::*;

impl App {
    /// 是否把图片交给内核：原生视觉或 `[vision]` 代理任一可用即可（与 Python
    /// `route_image_result` 同义）。两者都没有时图片不会进请求——主模型看不懂图，
    /// 交出去只会白跑一趟 base64 过管道。
    pub(super) fn attach_vision_images(&self) -> bool {
        self.native_vision_enabled() || vision_proxy_configured(&ConfigEnvironment::from_process())
    }

    /// 当前生效的原生视觉开关（握手与图片注入共用同一份判定）。
    pub(super) fn native_vision_enabled(&self) -> bool {
        effective_native_vision(
            uses_external_channel(&self.options),
            self.options.native_vision,
            &self.llm,
        )
    }

    pub(super) fn handle_frame(&mut self, frame: Frame) {
        if frame.is_notification() {
            match HostEvent::from_frame(&frame) {
                Ok(event) => {
                    // 回合落地后按 FIFO 排空排队消息（对映 Python `_finish` 里的排空调用）。
                    let finished = matches!(event, HostEvent::TurnFinished(_));
                    // 回合 id 要在 `apply` 之前取：落地处理会把当前回合卸下。
                    let turn_id = self.current_turn_id();
                    // 内核发来的插件 Hook 触发点：宿主在此分发，对映 Python 在 agent runtime
                    // 内的同名派发（压缩计量、模型请求前后）。
                    let session_id = self.current_session_id();
                    let session_id = (!session_id.is_empty()).then_some(session_id);
                    match &event {
                        HostEvent::ContextCompaction(payload) => {
                            self.plugins.compaction_after_turn(
                                payload.post_turn_context_tokens,
                                payload.trigger_context_tokens,
                                session_id.as_deref(),
                                (!payload.turn_id.is_empty()).then_some(payload.turn_id.as_str()),
                            )
                        }
                        HostEvent::ModelResponseAfter(payload) => {
                            self.plugins.model_response_after(
                                &payload.model,
                                &payload.content,
                                payload.tool_call_count,
                                session_id.as_deref(),
                            )
                        }
                        HostEvent::ModelRequestError(payload) => self.plugins.model_request_error(
                            &payload.error,
                            &payload.model,
                            session_id.as_deref(),
                        ),
                        _ => {}
                    }
                    self.state.apply(&event, Instant::now());
                    if finished {
                        // `/skill:` 的上下文消息只覆盖本轮：落地后换回常规装配。
                        self.restore_skill_context();
                        self.plugins.turn_end(None, turn_id.as_deref());
                        if let HostEvent::TurnFinished(payload) = &event {
                            self.speak_reply(&payload.final_text);
                        }
                        self.drain_pending_inputs();
                    }
                }
                Err(error) => diagnostics::warn(format!("[tui] 未识别的内核通知：{error}")),
            }
            return;
        }
        let Some(id) = frame.id().cloned() else {
            diagnostics::warn("[tui] 内核发来没有 id 的帧，已忽略。");
            return;
        };
        match frame.method() {
            Some(method) if method == omnicrawl_ipc::method::TOOL_BATCH => {
                self.handle_tool_batch(id, &frame);
            }
            Some(method) if method == omnicrawl_ipc::method::MODEL_REPLY => {
                // 内核自带 provider runtime 发模型请求，宿主代答路径不再需要。
                let _ = self.kernel.respond_unsupported(&id, method);
            }
            Some(method) if method == omnicrawl_ipc::method::MODEL_HOOK => {
                self.handle_model_hook(id, &frame);
            }
            Some(method) if method == omnicrawl_ipc::method::CONTEXT_PRUNE => {
                self.handle_context_prune(id, &frame);
            }
            Some(method) => {
                let _ = self.kernel.respond_unsupported(&id, method);
            }
            // 响应帧：宿主命令（`turn.undo`、`session.compact`）与 `session.settings`
            // 的结果要回填界面，其余（如 `turn.submit` 的确认）由通知驱动。
            None => {
                if let Some(command) = self.pending_commands.remove(&id) {
                    self.finish_kernel_command(command, &frame);
                } else if self.pending_turn_submit.as_ref() == Some(&id) {
                    // `turn.submit` 的确认帧：成功不带错误，失败带回合终止原因。
                    self.pending_turn_submit = None;
                    if let Some(error) = frame.error.as_ref() {
                        self.fail_submitted_turn(&error.message);
                    }
                } else if self.prompt_request.as_ref() == Some(&id) {
                    self.prompt_request = None;
                    if let Some(error) = frame.error.as_ref() {
                        self.state.notice(format!(
                            "内核未接受提示词更新（{}），模式提示词只在本宿主生效。",
                            error.message
                        ));
                    }
                } else if let Some((pending, change)) = self.settings_request.clone() {
                    if pending == id {
                        self.settings_request = None;
                        self.apply_settings_response(&change, &frame);
                    }
                }
            }
        }
    }

    /// 服务内核的 `model.hook`：跑 `model.request.before`，回改写后的消息或拒绝文案。
    pub(super) fn handle_model_hook(&mut self, id: Id, frame: &Frame) {
        let request = match ModelHookRequest::from_frame(frame) {
            Ok(request) => request,
            Err(error) => {
                let _ = self.kernel.respond_error(
                    &id,
                    error_code::INVALID_PARAMS,
                    &format!("model.hook 负载不符：{error}"),
                );
                return;
            }
        };
        let ModelHookRequest {
            mut messages,
            model,
        } = request;
        let session_id = self.current_session_id();
        let session_id = (!session_id.is_empty()).then_some(session_id);
        match self
            .plugins
            .model_request_before(&mut messages, &model, session_id.as_deref())
        {
            Ok(()) => {
                let result = ModelHookResult { messages }.to_result();
                let _ = self.kernel.respond(&id, result);
            }
            // 插件拒绝：错误响应带上拒绝文案，内核据此中止本轮。
            Err(error) => {
                let _ =
                    self.kernel
                        .respond_error(&id, error_code::INVALID_REQUEST, error.message());
            }
        }
    }

    /// 服务内核的 `context.prune`：把「刚变老」的那批调用交决策模型裁决，回可以移除的调用 ID。
    ///
    /// 与 `omnicrawl-host::turn` 的 `run_context_prune` 同一口径：开关没开、没有可用渠道、
    /// 缺凭据、请求失败、响应不可解析都回**空列表**（等于「都留着」）——淘汰是 fail-open 的
    /// 省上下文手段，不值得让回合失败，也绝不能在裁决不可用时误删内容。
    ///
    /// 任务背景直接用内核随请求带来的 `task`（内核侧就是本轮提交的原文），宿主不必另存一份。
    ///
    /// 裁决是网络请求（`PRUNE_TIMEOUT_SECONDS` 20s），与审查同档：**不能放在 UI 线程上**，
    /// 否则每批工具后的这次送审都会冻住整个界面（动画停、按键无响应）。这里只解析请求，
    /// 真正的裁决交给 [`Self::spawn_prune_job`]，应答在 [`Self::drain_prune_results`] 里发。
    pub(super) fn handle_context_prune(&mut self, id: Id, frame: &Frame) {
        let request = match ContextPruneRequest::from_frame(frame) {
            Ok(request) => request,
            Err(error) => {
                let _ = self.kernel.respond_error(
                    &id,
                    error_code::INVALID_PARAMS,
                    &format!("context.prune 负载不符：{error}"),
                );
                return;
            }
        };
        let groups = request
            .groups
            .iter()
            .map(|group| {
                PruneCandidate::bounded(
                    group.call_id.clone(),
                    group.tool.clone(),
                    group.arguments.clone(),
                    group.ok,
                    group.output.clone(),
                )
            })
            .collect::<Vec<_>>();
        self.spawn_prune_job(id, request.task, groups);
    }

    /// 后台线程跑淘汰裁决，结果经 [`App::prune_results`] 回填。
    ///
    /// 与 [`Self::spawn_review_job`] 同一手法：一个请求一个线程，UI 线程不等它。
    /// 运行期缺失（开关关、无渠道）时不值得起线程，直接回空列表。
    pub(super) fn spawn_prune_job(&mut self, id: Id, task: String, groups: Vec<PruneCandidate>) {
        let Some(prune) = self.prune.clone() else {
            let _ = self.kernel.respond(
                &id,
                ContextPruneResult {
                    evicted_call_ids: Vec::new(),
                }
                .to_result(),
            );
            return;
        };
        let sender = self.prune_sender.clone();
        self.prune_in_flight += 1;
        thread::spawn(move || {
            let evicted_call_ids = evicted_call_ids_for(&prune, &task, &groups).unwrap_or_default();
            let _ = sender.send(PruneTaskResult {
                id,
                evicted_call_ids,
            });
        });
    }

    /// 取回后台裁决结果并应答内核。
    pub(super) fn drain_prune_results(&mut self) {
        while let Ok(result) = self.prune_results.try_recv() {
            self.prune_in_flight = self.prune_in_flight.saturating_sub(1);
            let payload = ContextPruneResult {
                evicted_call_ids: result.evicted_call_ids,
            };
            let _ = self.kernel.respond(&result.id, payload.to_result());
        }
    }

    pub(super) fn handle_tool_batch(&mut self, id: Id, frame: &Frame) {
        // 协议上内核会等前一批的响应再发下一批；真的撞上时明确拒绝，
        // 而不是让新批次覆盖旧批次（那会让前一批永远等不到响应）。
        if self.state.batch_request_id().is_some() {
            let _ = self.kernel.respond_error(
                &id,
                error_code::INTERNAL_ERROR,
                "宿主仍有未完成的工具批次，拒绝并发批次。",
            );
            return;
        }
        let batch = match ToolBatch::from_frame(frame) {
            Ok(batch) => batch,
            Err(error) => {
                let _ = self.kernel.respond_error(
                    &id,
                    error_code::INVALID_PARAMS,
                    &format!("tool.batch 负载不符：{error}"),
                );
                return;
            }
        };
        // 子任务的工具批次带着隔离根：本批改用「根在该目录」的临时工具表，
        // 路径保护与命令工作目录因此都落在隔离区里；没有根的批次照旧用共享表。
        self.batch_registry = match batch.workspace_root.as_deref() {
            Some(root) if !root.trim().is_empty() => {
                match ToolRegistry::new(
                    root,
                    &self.registry_options,
                    self.options.command_timeout_seconds,
                ) {
                    Ok(registry) => Some(Arc::new(registry)),
                    Err(_) => None,
                }
            }
            _ => None,
        };
        // 子代理回合的调用不算主代理的动作：照旧执行，但不往主会话挂卡片，否则子代理的
        // 过程会混进主代理的消息区（它们由子代理独立窗口渲染）。
        let quiet = host::is_subagent_turn(&batch.turn_id);
        let step = if quiet {
            self.state.start_subagent_batch(id.clone(), batch.calls)
        } else {
            self.state.start_batch(id.clone(), batch.calls)
        };
        self.handle_batch_step(id, step);
    }

    pub(super) fn handle_batch_step(&mut self, request_id: Id, step: BatchStep) {
        // 批次推进可能已经在宿主内定调了某些调用（update_todos / pause_work / ask_user）：
        // 它们不经过执行层，卡片要在这里补上终态，否则会一直停在「调用中」并继续计时。
        self.state.settle_internal_batch_calls();
        match step {
            BatchStep::Awaiting => {
                // 插件守卫先于人工审批：被拒的调用不再弹审批面板。
                let plugins = Arc::clone(&self.plugins);
                let mode = self.options.approval.as_str();
                if let Some(next) = self
                    .state
                    .guard_pending_approval(|call| plugin_tool_guards(&plugins, call, true, mode))
                {
                    self.handle_batch_step(request_id, next);
                    return;
                }
                // 提问托管：开关打开且提问带选项时，先让决策模型替你选一项，
                // 选出来就跳过面板（并在会话流里留一条提示）。任何失败都退回人工提问。
                if let Some(next) = self.try_custody_pending_question() {
                    self.handle_batch_step(request_id, next);
                }
            }
            BatchStep::Execute(jobs) => {
                self.tool_deadline = Some(
                    Instant::now()
                        + Duration::from_secs(self.options.tool_timeout_seconds.max(1) as u64),
                );
                // 本批的工具属于当前回合：回合取消时只回收它自己的后台任务。
                self.refresh_advisor_context();
                self.batch().set_monitor_scope(self.state.turn.turn_id());
                let mode = self.options.approval.as_str();
                for (index, mut call) in jobs {
                    // 插件守卫先于执行：被挡下的调用不进执行层，按拒绝结果回填。
                    match plugin_tool_guards(&self.plugins, &call, false, mode) {
                        Ok(Some(arguments)) => {
                            self.state.rewrite_call_arguments(index, arguments.clone());
                            call.arguments = arguments;
                        }
                        Ok(None) => {}
                        Err(reason) => {
                            self.state.begin_tool_run(&call, Instant::now());
                            self.plugin_approval_after(&call.name, false, &reason);
                            let _ = self.completion_sender.send(ToolCompletion {
                                index,
                                call,
                                result: host::denied_with_reason(&reason),
                                vision: None,
                            });
                            continue;
                        }
                    }
                    // 审查闸：`review` 模式下判定为 Review 的调用交给审查模型（与
                    // `omnicrawl-host::turn` 同序）。审查是一次网络请求，同步跑会卡界面，
                    // 因此这里只做判定：需要审查的调用单独起一个后台线程跑模型，
                    // 结果经 [`ToolCompletion`] 通道回填（拒绝时写拒绝结果）。
                    if self.needs_review_gate(&call) {
                        self.plugin_approval_after(&call.name, true, "");
                        self.state.begin_tool_run(&call, Instant::now());
                        self.spawn_review_job(index, call);
                        continue;
                    }
                    // 走到执行层的调用都已被放行（需人工审批的调用已在 `Awaiting` 分支消费）。
                    self.plugin_approval_after(&call.name, true, "");
                    self.state.begin_tool_run(&call, Instant::now());
                    self.spawn_tool_job(index, call);
                }
            }
            BatchStep::Complete => {
                self.tool_deadline = None;
                self.batch_registry = None;
                let attach_images = self.attach_vision_images();
                if let Some(observations) = self.state.take_observations(attach_images) {
                    self.respond_batch(&request_id, observations);
                }
            }
        }
    }

    /// 提问托管：待决提问带选项且托管可用时自动作答，返回推进后的步骤。
    ///
    /// `None` 表示本次不托管（开关关、没有选项、决策服务不可用或这一趟失败），
    /// 面板照旧留在界面上等用户——托管的目的是省一次人工往返，不值当替用户瞎猜。
    /// 语义与 `omnicrawl-host::turn` 的 `custody_answer` 同源。
    pub(super) fn try_custody_pending_question(&mut self) -> Option<host::BatchStep> {
        let Some(host::Waiting::Question(panel)) = self.state.waiting() else {
            return None;
        };
        if !panel.is_select() {
            return None;
        }
        let panel = panel.clone();
        // 托管请求的 state 里带上本回合的提问正文与用户请求。
        self.custody_context.question = panel.prompt.clone();
        let custody = self.custody.as_ref()?;
        let index =
            omnicrawl_host::tools::chosen_option(custody, &self.custody_context, &panel.options)?;
        let answer = panel.options.get(index)?.clone();
        self.state
            .notice(omnicrawl_host::turn::custody_notice(&panel.prompt, &answer));
        self.answer_pending_question(answer)
    }

    /// 回答待决提问：先把问答记进审查上下文（授权边界的最新事实），再推进批次。
    ///
    /// 与 `omnicrawl-host::turn` 的 `record_ask_user` 同位：问答是审查模型判断授权
    /// 范围的最新依据，先记下来再继续推进。
    pub(super) fn answer_pending_question(&mut self, answer: String) -> Option<host::BatchStep> {
        if let Some(Waiting::Question(panel)) = self.state.waiting() {
            self.review_context.record_ask_user(&panel.prompt, &answer);
        }
        self.state.answer_question(answer)
    }

    /// 审查闸判定：`review` 模式下删除类 / 下载并执行类 / 高风险 Git 调用需要审查。
    ///
    /// 判定复用 `omnicrawl-host::review::needs_review`（语义基准是 Python
    /// `_approve_tool_call` 的 `Review` 分支）；工具说明与参数 schema 取自本批工具表。
    pub(super) fn needs_review_gate(&self, call: &ToolCall) -> bool {
        if self.options.approval != ApprovalMode::Review {
            return false;
        }
        let (description, schema) = self.batch().approval_facts(&call.name).unwrap_or_default();
        omnicrawl_host::review::needs_review(
            self.options.approval,
            &call.name,
            &description,
            &schema,
            &call.arguments,
        )
    }

    /// 后台线程跑审查模型，结果经 [`ToolCompletion`] 回填：批准则转正常执行，
    /// 拒绝（含审查运行期不可用）则写拒绝结果。
    ///
    /// 审查模型调用是网络请求（与主回合的模型请求同一档超时），不能放在 UI 线程上；
    /// 与 [`Self::spawn_tool_job`] 一样，一个调用一个线程。
    pub(super) fn spawn_review_job(&self, index: usize, call: ToolCall) {
        let review = self.review.clone();
        let context = self.review_context.clone();
        let registry = Arc::clone(self.batch());
        let plugins = Arc::clone(&self.plugins);
        let sender = self.completion_sender.clone();
        let workspace_root = self.workspace.to_string_lossy().to_string();
        thread::spawn(move || {
            let verdict = match review.as_ref() {
                Some(options) => {
                    let (description, schema) =
                        registry.approval_facts(&call.name).unwrap_or_default();
                    let _ = schema;
                    omnicrawl_host::review::review_tool_call(
                        options,
                        &omnicrawl_host::review::ReviewRequest {
                            tool_name: &call.name,
                            description: &description,
                            arguments: &call.arguments,
                            workspace_root: &workspace_root,
                            context: &context,
                        },
                    )
                }
                // 审查运行期缺失：与 host 侧同一 fail-closed 文案。
                None => Err(
                    omnicrawl_controllers::approval::review_request_failed_reason(
                        "审查模型不可用：宿主未装配审查运行期。",
                    ),
                ),
            };
            match verdict {
                Ok(()) => {
                    // 批准：接着走正常执行（插件执行前守卫与工具执行体）。
                    plugins.tool_approval_after(&call.name, true, "", "review");
                    let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        registry.execute_with_vision(&call)
                    }));
                    let (mut result, vision) = match executed {
                        Ok(Some(execution)) => {
                            let vision = if execution.images.is_empty() {
                                None
                            } else {
                                Some(host::VisionPayload {
                                    prompt: execution.vision_prompt,
                                    images: execution.images,
                                })
                            };
                            (execution.result, vision)
                        }
                        Ok(None) => (host::unavailable_result(&call.name), None),
                        Err(_) => {
                            plugins.tool_execute_error(&call.name, "工具执行线程 panic");
                            (host::panicked_result(&call.name), None)
                        }
                    };
                    let base = if result.full_output.is_empty() {
                        result.output.clone()
                    } else {
                        result.full_output.clone()
                    };
                    result.full_output = plugins.tool_execute_after(&call.name, result.ok, &base);
                    let _ = sender.send(ToolCompletion {
                        index,
                        call,
                        result,
                        vision,
                    });
                }
                Err(reason) => {
                    // 审查拒绝也属「未批准」：MCP 调用落一条拒绝审计。
                    host::record_mcp_denial(&registry, &call.name, &call.arguments, &reason);
                    plugins.tool_approval_after(&call.name, false, &reason, "review");
                    let _ = sender.send(ToolCompletion {
                        index,
                        call,
                        result: host::denied_with_reason(&reason),
                        vision: None,
                    });
                }
            }
        });
    }

    /// 一个调用一个线程：同批工具并发执行（与 Python 宿主一致）。
    ///
    /// 执行体的 panic 必须转成一条失败观察：线程静默消失会让整批永远凑不齐，
    /// 内核就会一直等这个 `tool.batch` 的响应。
    pub(super) fn spawn_tool_job(&self, index: usize, call: ToolCall) {
        let registry = Arc::clone(self.batch());
        let plugins = Arc::clone(&self.plugins);
        let sender = self.completion_sender.clone();
        thread::spawn(move || {
            let tool_name = call.name.clone();
            let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.execute_with_vision(&call)
            }));
            let (mut result, vision) = match executed {
                Ok(Some(execution)) => {
                    let vision = if execution.images.is_empty() {
                        None
                    } else {
                        Some(host::VisionPayload {
                            prompt: execution.vision_prompt,
                            images: execution.images,
                        })
                    };
                    (execution.result, vision)
                }
                Ok(None) => (host::unavailable_result(&tool_name), None),
                Err(_) => {
                    plugins.tool_execute_error(&tool_name, "工具执行线程 panic");
                    (host::panicked_result(&tool_name), None)
                }
            };
            // `tool.execute.after` 可改写展示文本（`displayText`）；模型看到的输出不变。
            let base = if result.full_output.is_empty() {
                result.output.clone()
            } else {
                result.full_output.clone()
            };
            result.full_output = plugins.tool_execute_after(&tool_name, result.ok, &base);
            let _ = sender.send(ToolCompletion {
                index,
                call,
                result,
                vision,
            });
        });
    }

    /// `tool.approval.after`：审批结论通知。
    pub(super) fn plugin_approval_after(&self, tool: &str, approved: bool, reason: &str) {
        self.plugins
            .tool_approval_after(tool, approved, reason, self.options.approval.as_str());
    }

    /// 批次执行超过 `tool_timeout_seconds` 时按超时收口：未回填的调用写成超时结果，
    /// 后台线程继续运行但结果被丢弃（与 Python 的批次截止时间语义一致）。
    pub(super) fn enforce_tool_deadline(&mut self) {
        let Some(deadline) = self.tool_deadline else {
            return;
        };
        if self.state.batch_request_id().is_none() {
            self.tool_deadline = None;
            return;
        }
        if Instant::now() < deadline {
            return;
        }
        let seconds = self.options.tool_timeout_seconds;
        let ready = self.state.fill_tool_timeout(seconds);
        // 不再直接写终端（会盖住输入框）：作为会话区提示显示。
        self.state.notice(format!(
            "工具执行超过 {seconds} 秒仍未完成，已按超时回收等待（后台线程的结果会被丢弃）。"
        ));
        self.tool_deadline = None;
        if ready {
            self.flush_ready_batch();
        }
    }

    pub(super) fn respond_batch(&mut self, id: &Id, observations: Vec<omnicrawl_core::AgentLoopObservation>) {
        let result = omnicrawl_ipc::ToolBatchResult { observations }.to_result();
        if let Err(error) = self.kernel.respond(id, result) {
            self.state.notice(format!("回工具批次失败：{error}"));
        }
    }

}
