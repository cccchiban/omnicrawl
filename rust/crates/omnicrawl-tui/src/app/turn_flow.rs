//! 回合流转与命令派发：提交输入、发往内核、斜杠命令与随之而来的回执处理。
//!
//! 与 [`super::kernel_flow`] 分工：那里是「内核帧进来怎么处理」，这里是「用户输入怎么
//! 变成内核命令」，以及回执到达后如何收尾当轮。

use super::*;

impl App {
    /// 提交输入框：生成期间排队等待，空闲时直接发给内核。
    ///
    /// 「立即命令」在生成期间也当场处理、不入队（对映 Python 的
    /// `_command_dispatcher.is_immediate`）：本批只有 `/settings`，它是纯界面
    /// 模态页、不产生模型回合，排队等反而让用户按了没反应。`/undo` 需要回合
    /// 空闲才能执行，因此照常排队，轮到时由 [`Self::request_undo`] 给出提示。
    pub(super) fn submit_or_queue(&mut self) {
        let Some(submission) = self.state.submit() else {
            return;
        };
        let text = submission.text;
        // `/sessions` 特殊处理（对映 Python `_handle_command`）：不把列表追加进对话区，
        // 而是在输入框上方弹可导航的会话菜单。放在这里而不是命令派发层，是为了让它
        // 在生成中（立即命令分支）与空闲时走同一条路径。
        if text.trim() == "/sessions" {
            self.open_sessions_menu();
            return;
        }
        if self.state.turn.is_running() {
            // 生成期间的命令调度按声明类型走（对映 Python 的 `is_immediate`：
            // 纯界面与只读查询当场执行，状态变更类排队等回合结束）。
            if let Some(parsed) = command_registry().parse(&text) {
                if parsed.command.command_type.immediate() {
                    self.dispatch_command(text, parsed);
                } else {
                    self.state.queue_pending(text, submission.images);
                }
                return;
            }
            self.state.queue_pending(text, submission.images);
            return;
        }
        self.dispatch_submission(text, submission.images);
    }

    /// 把一次提交派发到斜杠命令或内核。
    ///
    /// 命中注册表即交给命令层（未命中的输入照旧当成一轮对话）；需要内核往返的两条
    /// （`/undo`、`/compact`）先在宿主侧拦下来异步下发。
    ///
    /// `images` 是随这次提问粘贴的图片：斜杠命令不走模型，因此命中命令时按「没地方去」
    /// 处理——提醒一句，不让图片悄悄消失。
    pub(super) fn dispatch_submission(&mut self, text: String, images: Vec<PendingImage>) {
        // 内核没了就什么都不发：界面已常显退出指引，往死连接里写只会刷出「发送失败」噪音。
        // 键盘路径到不了这里（Enter 在该状态下是退出），这是给鼠标等旁路的兜底。
        if self.kernel_lost {
            return;
        }
        if let Some(parsed) = command_registry().parse(&text) {
            if !images.is_empty() {
                self.state
                    .show_notice_line("斜杠命令不接受图片，已忽略粘贴的图片。", Instant::now());
            }
            self.dispatch_command(text, parsed);
            return;
        }
        // `/skill:<名> [任务]` 是运行期命令（Skill 不在注册表里）：命中就换成活动技能
        // 正文块，未命中就回一份可用列表，两种情况都当作本轮提交。
        if let Some((name, rest)) = parse_skill_command(&text) {
            self.dispatch_skill_command(&text, &name, rest);
            return;
        }
        // 非斜杠命令的提交记入输入框历史，供上下键回看（对映 Python `_submit` 中的
        // `history_record`：命令不记录）。
        self.state.composer.history_record(&text);
        // 建议还在后台跑：这一条排队等它——两条同时提交会被内核按 `TURN_BUSY` 拒掉一条。
        if self.suggest_in_flight > 0 {
            self.state.queue_pending(text, images);
            return;
        }
        // 建议：先问一次决策模型「本次该用哪些工具／哪些子代理」（网络请求，超时 20s，不能占
        // UI 线程），结果回来再提交；不可用或没有建议时立即提交，原文照发（fail-open）。
        // 两类同时开启时由后台线程并发发起，总耗时取较大者而不是相加。
        if let Some((text, images)) = self.start_suggestion(text, images) {
            self.submit_to_kernel(text, images);
        }
    }

    /// 起一次建议：需要建议时把提问交给后台线程并返回 `None`；不需要时原样返回提问。
    pub(super) fn start_suggestion(
        &mut self,
        text: String,
        images: Vec<PendingImage>,
    ) -> Option<(String, Vec<PendingImage>)> {
        let suggest = self.suggest.clone().unwrap_or_default();
        let subagent_suggest = self.subagent_suggest.clone().unwrap_or_default();
        // 两个开关都关着或都没有可用渠道：立即提交，原文照发，连候选清单都不必收集。
        if !suggest.active() && !subagent_suggest.active() {
            return Some((text, images));
        }
        let inventory = self.registry.tool_inventory();
        let subagent_roles = self
            .suggest_roles
            .clone()
            .unwrap_or_else(|| self.registry.subagent_roles().to_vec());
        // 两侧都没有可送审的候选：不必白跑一次网络。
        if !(suggest.active() && !inventory.is_empty())
            && !(subagent_suggest.active() && !subagent_roles.is_empty())
        {
            return Some((text, images));
        }
        let sender = self.suggest_sender.clone();
        let prompt = text.clone();
        self.suggest_in_flight += 1;
        thread::spawn(move || {
            let lines =
                omnicrawl_host::tools::suggestion_lines(&omnicrawl_host::tools::SuggestionInputs {
                    tools: (&suggest, &inventory),
                    subagents: (&subagent_suggest, &subagent_roles),
                    user_prompt: &prompt,
                });
            let text = omnicrawl_host::tools::insert_suggestions(&prompt, &lines);
            let _ = sender.send(SuggestTaskResult { text, images });
        });
        None
    }

    /// 取回后台建议结果：拼好的提问在这里提交（提交必须在 UI 线程做）。
    pub(super) fn drain_suggest_results(&mut self) {
        while let Ok(result) = self.suggest_results.try_recv() {
            self.suggest_in_flight = self.suggest_in_flight.saturating_sub(1);
            self.submit_to_kernel(result.text, result.images);
        }
        // 建议期间排队的输入接着按 FIFO 提交（对映回合结束后的排空）。
        if self.suggest_in_flight == 0 {
            self.drain_pending_inputs();
        }
    }

    /// 把一次提交发给内核：`turn.start` 在进内核之前分发，界面显示最终文本。
    pub(super) fn submit_to_kernel(&mut self, text: String, images: Vec<PendingImage>) {
        if self.kernel_lost {
            // 内核对不上就不发；顺手丢掉本轮的活动 Skill 上下文，免得落到下一轮。
            self.pending_skill_context = None;
            return;
        }
        let turn_id = format!("turn-{}", self.next_turn);
        self.next_turn += 1;
        // `turn.start` 在进内核之前分发：transform 类 Handler 可改写 `userText`，
        // 守卫类 Handler 拒绝时这一轮根本不发出去（与 Python 的 `turn_payload` 同位）。
        let text = match self.plugins.turn_start(&text, None, Some(&turn_id)) {
            Ok(rewritten) => rewritten,
            Err(error) => {
                // 本轮没发出去，活动 Skill 上下文也不能留到下一轮。
                self.pending_skill_context = None;
                self.state.notice(error.to_string());
                return;
            }
        };
        // 归档只在 `turn.start` 通过后做：本轮真发得出去，才不至于为一次被拦下的提交留废文件。
        // 复制失败不阻断发送（见 `image_archive`），正文里只列真正落盘成功的那几张。
        let text = match crate::image_archive::archive_pasted_images(&self.workspace, &images) {
            Some(note) => format!("{text}\n\n{note}"),
            None => text,
        };
        let images = images
            .into_iter()
            .map(|image| omnicrawl_ipc::bridge::TurnImageAttachment {
                media_type: image.media_type,
                data_base64: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
                detail: String::new(),
            })
            .collect::<Vec<_>>();
        self.state
            .begin_turn(turn_id.clone(), text.clone(), images.clone());
        // 用户提交的文本是审查模型判断授权边界的最新事实（与 host 侧 `record_user_text` 同位）。
        self.review_context.record_user_text(&text);
        // 新回合开始：清掉上一回合 Esc 留下的取消标记。工具表的取消令牌跨回合沿用
        // （重建工具表 / 切工作区都不换），不复位会让本回合每个 `bash` 一启动就被判为已取消。
        self.registry.cancel_token().reset();
        // 活动 Skill 的上下文消息必须赶在 `turn.submit` 之前下发（同一连接按序处理，
        // 因此本轮的请求就带技能正文）；回合收尾时再推回常规装配。
        if let Some(messages) = self.pending_skill_context.take() {
            let _ = self.send(Command::SessionSettings(Box::new(SessionSettingsParams {
                model: Some(Box::new(SessionModelSettings {
                    context_messages: Some(messages),
                    ..SessionModelSettings::default()
                })),
                compaction: None,
            })));
            self.restore_skill_context_after_turn = true;
        }
        let id = self.send(Command::TurnSubmit(omnicrawl_ipc::TurnSubmitParams {
            turn_id,
            user_text: text,
            images,
        }));
        self.pending_turn_submit = Some(id);
    }

    /// 回合收尾：把 `/skill:` 用过的活动技能正文换回常规上下文装配。
    pub(super) fn restore_skill_context(&mut self) {
        if !self.restore_skill_context_after_turn {
            return;
        }
        self.restore_skill_context_after_turn = false;
        let has_tools = !self.registry.declarations().is_empty();
        let session_id = self.current_session_id();
        let messages = match self.prompt.context_messages_with_plugins(
            has_tools,
            Some(self.plugins.as_ref()),
            (!session_id.is_empty()).then_some(session_id.as_str()),
            None,
        ) {
            Ok(messages) => messages,
            Err(_) => return,
        };
        self.push_session_settings_raw(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                context_messages: Some(messages),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        });
    }

    /// 执行一条已解析的斜杠命令。
    ///
    /// `text` 是原始输入（命令层按它解析参数），`parsed` 只用来判断调度方式：
    /// 需要内核往返的命令在这里分叉到 [`Self::start_kernel_command`]。
    pub(super) fn dispatch_command(&mut self, text: String, parsed: ParsedCommand) {
        if let Some(kind) = KernelCommand::from_parsed(&parsed) {
            self.start_kernel_command(text, kind);
            return;
        }
        // 慢命令在宿主侧接管（对映 Python `CommandOutcome.execution == "slow"`）：
        // 延迟执行体是同步接口，内联跑会冻结界面，因此只接 `/workspace` 与 `/mcp`
        // 两条真正慢的，剩下的照旧交命令层。
        match parsed.command.name.as_str() {
            "workspace" if !parsed.args.trim().is_empty() => {
                let path = parsed.args.trim().to_string();
                self.start_workspace_switch(&path);
                return;
            }
            "mcp" => {
                self.start_mcp_status();
                return;
            }
            _ => {}
        }
        let result = {
            let agent = TuiHostAgent::new(self);
            command_registry().dispatch(&text, &agent, Channel::Tui, None)
        };
        self.apply_command_result(result);
    }

    /// 处理 `/skill:<名> [余下任务]`：命中就加载技能正文并提交余下任务，未命中回可用列表。
    ///
    /// 与 Python `_apply_skill_command` 同序：命中时状态行提示已加载、余下文本当任务
    /// （没写任务就用默认提示），未命中时把带可用列表的说明当作本轮文本；两种情况都继续
    /// 走一次回合，因此输入框历史记的是原始输入。
    pub(super) fn dispatch_skill_command(&mut self, text: &str, name: &str, rest: Option<String>) {
        if self.kernel_lost {
            return;
        }
        self.state.composer.history_record(text);
        let name = name.trim().to_string();
        let task = match self.prompt.load_skill(&name) {
            Some(skill) => {
                let context = omnicrawl_host::prompt::active_skill_context_for(&skill);
                let has_tools = !self.registry.declarations().is_empty();
                match self.prompt.active_skill_messages(has_tools, &context) {
                    Ok(messages) => {
                        self.pending_skill_context = Some(messages);
                        self.state.notice(skill_loaded_status(&name));
                        rest.unwrap_or_else(|| skill_default_prompt(&name))
                    }
                    Err(error) => {
                        self.state
                            .notice(format!("活动 Skill 上下文装配失败：{error}"));
                        return;
                    }
                }
            }
            None => {
                self.state.notice(skill_not_found_status(&name));
                skill_not_found_message(&name, &self.skill_names())
            }
        };
        self.submit_to_kernel(task, Vec::new());
    }

    /// `/skill:` 未命中时列出的可用名称（与 `/skills` 展示同源）。
    pub(super) fn skill_names(&self) -> Vec<String> {
        self.prompt
            .skill_metas()
            .iter()
            .map(|meta| meta.name.clone())
            .collect()
    }

    /// 把命令结果落到界面上：消息进消息流，调度标记交给对应的界面动作。
    ///
    /// 延迟执行体（`/workspace` 一类慢命令）目前在本线程内联跑完：宿主还没有慢命令
    /// worker，而本批能真正执行的命令都不产生延迟体（见 README「斜杠命令接线」）。
    pub(super) fn apply_command_result(&mut self, mut result: CommandResult) {
        // 命令层要求重放会话视图（如 `/undo`）：先按消息投影立即对齐，再请内核给
        // 事件流做完整重建（工具卡与子 Agent 进度树只有事件流里才有）。
        if result.replay_conversation {
            self.request_session_replay(Vec::new(), None);
        }
        if let Some(run) = result.deferred.take() {
            let deferred = {
                let agent = TuiHostAgent::new(self);
                run(&agent)
            };
            if let Some(message) = deferred.message {
                self.state.notice(message);
            }
            if let Some(error) = deferred.error {
                self.state.notice(error);
            }
        }
        // `/approval:*` 一类的命令会把改动写到 config.toml 并当场生效：重取基线，
        // 免得下一帧的热应用把它当外部改动再报一次。
        self.review_watch
            .baseline(&ConfigEnvironment::from_process());
        if let Some(status) = result.working_status.take() {
            self.state.set_status(Some(status));
        }
        // 打开类命令（`/settings`）不追加提示：Python 在 `open_settings` 分支里直接
        // `return`，`message`（如「打开设置面板」）从来不会进消息流；这里同样先处理再
        // `return`，否则会多出一条「· 打开设置面板」这类纯提醒。
        if result.clear_conversation {
            self.state.clear_conversation();
            self.state.scroll_to_bottom();
        }
        if result.open_settings {
            self.open_settings();
            return;
        }
        if let Some(message) = result.message.take() {
            self.state.notice(message);
        }
        if let Some(error) = result.error.take() {
            self.state.notice(error);
        }
        // `refresh_context` 对内核侧上下文没有可刷新的东西：模型上下文由内核持有，
        // 宿主这里没有缓存可失效。
        if result.exit_requested {
            self.start_shutdown();
        }
    }

    /// 需要内核往返的宿主命令：命令层是同步接口、内核链路是异步帧，因此由宿主
    /// 先拦下并异步下发（响应在 `handle_frame` 里回填）。
    pub(super) fn start_kernel_command(&mut self, text: String, kind: KernelCommand) {
        // 状态变更类命令在回合进行中等回合结束（与排队语义一致）。
        // 斜杠命令不接受图片（`dispatch_submission` 已拦下并提示），排队项里不带图。
        if self.state.turn.is_running() {
            self.state.queue_pending(text, Vec::new());
            return;
        }
        // 同类命令不并发：`/review HEAD~3` 与 `/review` 算同类（label 相同）。
        if self
            .pending_commands
            .values()
            .any(|pending| pending.label() == kind.label())
        {
            self.state
                .notice(format!("上一条 {} 还在执行中，稍候再试。", kind.label()));
            return;
        }
        let command = match kind.to_command(&text) {
            Ok(Some(command)) => command,
            // 内部后继命令（如评审报告注入）不走这个入口。
            Ok(None) => return,
            Err(message) => {
                self.state.notice(message);
                return;
            }
        };
        // `/review` 是唯一需要在进内核之前先做本地预检的：没有 git 或没有可评审改动时
        // 连子 Agent 都不必拉起来（预检用与命令层同一个函数，文案不会两份）。
        if let KernelCommand::Review { scope } = &kind {
            let workspace = self.workspace.clone();
            let scope = scope.clone();
            let precheck = {
                let agent = TuiHostAgent::new(self);
                check_review_preconditions(&agent, &workspace, &scope)
            };
            if let Some(message) = precheck {
                self.state.notice(message);
                return;
            }
        }
        self.state
            .set_status(Some(kind.working_status().to_string()));
        let id = self.send(command);
        self.pending_commands.insert(id, kind);
    }

    /// 当前回合 id（无在途回合时为空）。
    pub(super) fn current_turn_id(&self) -> Option<String> {
        self.state.turn.turn_id().map(|value| value.to_string())
    }

    /// 回合结束后按 FIFO 排空排队消息（对映 Python `_drain_pending_inputs`）。
    ///
    /// 模态页（设置面板）打开时不排空，与 Python 的 `len(self.screen_stack) == 1`
    /// 守卫同义。每次成功派发都会开启新回合，循环条件随之结束，因此不会出现
    /// 重叠的回合 worker。
    ///
    /// 只在这一帧里派发**一条**：`dispatch_submission` 有几条路径会把消息原样排回队尾
    /// （建议在途、回合正在运行），那几条路径不会开启新回合，`turn.is_running()` 也就
    /// 一直为假。无界 `loop` 因此会在 UI 线程里空转：队首取出又排回，队列永不为空，
    /// 建议结果永远取不回来、`suggest_in_flight` 永远不归零，每圈还写一次输入框历史，
    /// 直到 `Vec<String>` 撑爆内存。派发一条就返回，下一帧的 `drain_frames` 会重新
    /// 取回建议结果并再次调用本函数，排空照旧按 FIFO 推进。
    pub(super) fn drain_pending_inputs(&mut self) {
        loop {
            if self.settings.is_some() || self.state.turn.is_running() {
                return;
            }
            let queued = self.state.pending_len();
            if queued == 0 {
                return;
            }
            let Some(submission) = self.state.take_next_pending() else {
                return;
            };
            self.dispatch_submission(submission.text, submission.images);
            // 消息又被排回队尾（没有开启新回合）：本帧到此为止，避免原地空转。
            if !self.state.turn.is_running() && self.state.pending_len() >= queued {
                return;
            }
        }
    }

    /// 内核对宿主命令的响应 → 界面消息；失败时原样透出内核的中文原因。
    pub(super) fn finish_kernel_command(&mut self, command: KernelCommand, frame: &Frame) {
        // 回执到了就收起状态行：命令已经不在执行中。
        self.state.set_status(None);
        match (&frame.error, &frame.result) {
            (Some(error), _) => {
                // 回放读不到事件时不把错误摆到消息流：对话视图已经有消息投影可看
                // （Python `_replay_session_conversation` 同样静默跳过读取失败）。
                if let KernelCommand::Replay { fallback, notice } = &command {
                    if !fallback.is_empty() {
                        self.state.replay_history(fallback);
                    }
                    if let Some(notice) = notice {
                        self.state.notice(notice.clone());
                    }
                    return;
                }
                let label = command.label();
                self.state.notice(format!("{label}失败：{}", error.message));
            }
            (None, Some(result)) => {
                // `/undo` 的回执带重建后的历史：该提示要等回放完成后才进消息流，
                // 否则回放的清空会把刚追加的提示一并抹掉。
                if let Some(history) = command.replay_history(result) {
                    self.request_session_replay(history, Some(command.success_message(result)));
                    // 回放请求已经接过这条命令的提示，别再重复追加。
                    return;
                }
                // 回放回执的结果就是对话视图，不另发提示。
                if let Some(events) = command.replay_events(result) {
                    self.state.replay_events(&events);
                    if let KernelCommand::Replay { notice, .. } = &command {
                        if let Some(notice) = notice {
                            self.state.notice(notice.clone());
                        }
                    }
                } else {
                    let message = command.success_message(result);
                    if !message.trim().is_empty() {
                        self.state.notice(message);
                    }
                }
                // 显式压缩（`/compact`）不会再有后续模型请求来修正遥测：回执里的压缩后大小
                // 在这里落到遥测上（与自动压缩走 `turn.context_compaction` 同一口径）。
                if let KernelCommand::Compact = &command {
                    if let Some(tokens) = result
                        .get("post_compaction_context_tokens")
                        .and_then(Value::as_i64)
                    {
                        self.state.refresh_after_compaction(tokens);
                    }
                }
                // `/review` 的第二段：报告先展示，再注入内核上下文供下一轮请求使用。
                if let Some(report) = command.review_report(result) {
                    let id = self.send(Command::SessionAppend(SessionAppendParams {
                        role: "assistant".to_string(),
                        content: report,
                    }));
                    self.pending_commands
                        .insert(id, KernelCommand::ReviewInject);
                }
            }
            (None, None) => {
                self.state
                    .notice(format!("{}失败：内核没有返回结果。", command.label()));
            }
        }
    }


}
