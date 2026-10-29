//! 定时泵：活动动画、审查与监控结果的周期性收敛。
//!
//! 每帧只做「该到期的事」，其余状态推进留给事件。

use super::*;

impl App {


    /// 首屏挂载后启动欢迎 Logo 的入场动画（对映 Python 的
    /// `_start_welcome_logo_animation`：只在空会话首屏播放一次，重复调用无效）。
    ///
    /// 块字下方那行会话徽标（模型 * THINKING 推理档 APPROVAL 审批模式 MCP 数量）取当前配置，
    /// 与块字同一段动画一起入场。
    pub fn start_welcome_logo_animation(&mut self, now: Instant) {
        self.refresh_welcome_badge();
        self.state.logo.start(now);
    }

    /// 把欢迎页徽标对齐到当前配置（模型 / 推理档 / 审批模式 / MCP 数量）。
    ///
    /// 徽标画在 Logo 下方，属于会话区内容：值变了才置脏，避免每帧无谓重画。
    fn refresh_welcome_badge(&mut self) {
        let badge = crate::ui::fullscreen::rendering::welcome_logo::session_badge_text(
            &self.state.model,
            &self.llm.reasoning_effort,
            self.state.approval.label(),
            self.state.mcp_servers as i64,
        );
        if self.state.logo.set_subtitle(badge) {
            self.state.touch_conversation();
        }
    }


    /// 推进一帧欢迎 Logo 动画（对映 Textual 的 0.05s 定时器）。
    ///
    /// 事件循环的节拍跟随按键与内核帧，不能按计次推进，因此把当前时刻交给动画
    /// 游标按经过时间换算帧号；动画落定后这里不再做任何事。
    pub fn tick_welcome_logo_animation(&mut self, now: Instant) {
        if self.state.logo.is_playing() {
            // 空会话首屏那幅块字是按帧拼接的：动画没停就都是会话区的内容变化。
            self.state.touch_conversation();
            self.state.logo.tick(now);
        }
    }


    /// 按最新遥测刷新底部单行 HUD（上下文用量 / 吞吐 / 速率 + 工作区）。
    ///
    /// 底部不再轮播：模型与各开关已移到欢迎 Logo 下方的会话徽标，这里只跟着遥测走。
    pub fn tick_hud(&mut self) {
        self.state.refresh_hud();
        // 徽标跟着配置走：设置里换模型 / 推理档 / 审批模式或 MCP 后下一帧就对齐。
        self.refresh_welcome_badge();
    }


    /// 推进状态行 spinner 与运行中工具卡的耗时。
    ///
    /// 子代理的耗时由独立窗口自己推进，主会话这里没有需要刷新的东西；空闲时两者都不动，
    /// 三块版本号不变，事件循环因此能整帧跳过绘制。
    pub fn tick_subagent_trees(&mut self) {
        self.state.tick_activity();
    }


    /// 本次等待终端事件的时长预算：按「下一件要做的事」定，而不是固定节拍。
    ///
    /// 固定 50ms 节拍在「回车 → 首字」这段最要紧的区间里最多白等一整个节拍，
    /// 空闲时又白醒一次做整帧判断。这里：
    ///
    /// - 有回合在跑、有在途后台结果 → 按活动帧率短睡；
    /// - 欢迎 Logo 动画在播 → 精确睡到下一帧到期时刻；
    /// - 其余（真正空闲）→ 睡到上限，靠终端事件唤醒。
    pub fn poll_budget(&self, now: Instant) -> Duration {
        let active = self.state.turn.is_running()
            || self.slow_task.is_some()
            || self.onejev_task.is_some()
            || self.channel_models_task.is_some()
            // 淘汰裁决还在后台跑：结果一到就要应答内核（内核正等这条响应）。
            || self.prune_in_flight > 0
            // 工具建议还在后台跑：结果一到就要把提问发出去。
            || self.suggest_in_flight > 0
            || self.state.has_running_activity()
            // 思考段还在逐帧铺开：必须按活动帧率短睡，否则显现会一顿一顿。
            || self.state.is_reasoning_revealing()
            // 图片还在后台解码：睡太久会让缩略图迟迟不出现。
            || self.state.has_pending_image_previews();
        let mut budget = if active {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        if let Some(due) = self.state.next_animation_frame(now) {
            budget = budget.min(due);
        }
        budget
    }


    /// 审查配置的热应用：外部改 `config.toml` / `decision_models.toml` 后无需重启。
    ///
    /// 主循环每帧调一次；探测器自己按文件修改时间与最小间隔节流。命中变更时重建审查运行期
    /// （审查模型 / 决策通道 / 脱敏旁路），并按需跟随配置里的审批模式——命令行 `--approval`
    /// 显式给过模式时，只有 `[approval] mode` 这一项本身被改写才跟随。
    ///
    /// 与设置面板保存不同，这里只提示一行，不碰设置面板状态：用户没打开面板时也没有可同步的视图。
    pub fn tick_review_config(&mut self) {
        let environment = ConfigEnvironment::from_process();
        let Some(change) = self.review_watch.poll(&environment) else {
            return;
        };
        let mut notes: Vec<String> = Vec::new();
        if change.rebuild_runtime {
            self.rebuild_review(&environment);
            let decision = self.review.as_ref().is_some_and(|review| {
                matches!(
                    &review.channel,
                    omnicrawl_host::review::ReviewChannel::Decision(_)
                )
            });
            // `active()` 只表示「已配对话模型」；决策通道不需要它，别据此误报不可用。
            let usable = self.review.as_ref().is_some_and(|review| review.active()) || decision;
            if usable {
                notes.push("审查配置已重新加载".to_string());
            } else {
                // 与启动路径同一语义：审查运行期不可用时，需审查的调用一律 fail-closed 拒绝。
                notes
                    .push("审查配置已重新加载，但审查模型不可用，需审查的调用将被拒绝".to_string());
            }
        }
        if let Some(mode) = change.approval_mode.as_deref() {
            match ApprovalMode::parse(mode) {
                // 配置里的模式与本会话当前模式一致（例如 `--approval` 给的就是这一档）：不提示、不改。
                Ok(parsed) if parsed == self.options.approval => {}
                Ok(parsed) if self.approval_from_cli => {
                    notes.push(format!(
                        "配置文件里的审批模式已改为 {}，但本会话以 --approval 为优先，未跟随",
                        parsed.label()
                    ));
                }
                Ok(parsed) => {
                    self.state.approval = parsed;
                    self.options.approval = parsed;
                    notes.push(format!("审批模式已跟随配置切换为 {}", parsed.label()));
                }
                Err(error) => notes.push(format!("配置里的审批模式无法解析：{error}")),
            }
        }
        self.review_watch.accept();
        if !notes.is_empty() {
            self.state.notice(notes.join("；"));
        }
    }


    /// 按 `MONITOR_POLL_INTERVAL` 节流轮询后台任务日志，把增量归并进工具卡。
    ///
    /// 对映 Python `ConversationViewMixin._refresh_monitor_events`：只往消息流追加，
    /// 不改动回合状态，不干扰正在跑的模型回合或其他后台任务。游标只在本适配器里，
    /// 本地 API 的 `/monitors` 与模型侧的 `monitor` 工具各有自己的消费位置。
    /// 与 Python 的差异：同一个任务的后续批次归并到同一张卡，不逐批新开卡片。
    pub fn tick_monitor_events(&mut self, now: Instant) {
        if now.duration_since(self.monitor_polled_at) < crate::monitor::MONITOR_POLL_INTERVAL {
            return;
        }
        self.monitor_polled_at = now;
        for batch in self.monitor_state.refresh(self.registry.monitors()) {
            let text = crate::monitor::format_monitor_display_batch(&batch);
            self.state
                .push_monitor_batch(&batch.monitor_id, &batch.status, text);
        }
    }


    /// 把库里的运行期诊断写进会话流：不再落在终端光标处盖住画布（用户要求）。
    ///
    /// 与内核 stderr 那条不同，这里**不做任何过滤**（用户选择「原样入流」）：诊断文案
    /// 本身就是结论，漏一条就只能靠猜。样式与内核报错完全一致——`Record::Notice`，
    /// 渲染成 `· ` 开头的暗灰提示行，跟着会话流一起滚动、可回看。
    pub(super) fn drain_diagnostics(&mut self) {
        let Some(lines) = self.diagnostic_lines.take() else {
            return;
        };
        let mut messages: Vec<String> = Vec::new();
        let mut disconnected = false;
        loop {
            match lines.try_recv() {
                // 级别暂不参与展示（用户选择与内核报错同一种提示行）；
                // 级别已经落在 `logs/tui.log` 里，需要分级排查时看那里。
                Ok((_level, message)) => messages.push(message),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        for message in messages {
            self.state.notice(message);
        }
        // 接收端已摘（退出收尾）时保留 `None`，不再每帧空转。
        if !disconnected {
            self.diagnostic_lines = Some(lines);
        }
    }
}
