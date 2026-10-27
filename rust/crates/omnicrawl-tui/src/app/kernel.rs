//! 内核通道：握手、帧泵与命令发送。
//!
//! 与内核进程之间的唯一出入口；协议帧到这里被翻译成状态机可用的动作。

use super::*;

impl App {


    /// 握手：`initialize` 带上模型配置（内核自己发请求）、工具声明与可选的会话块。
    ///
    /// 取值规则两条：
    /// - **没显式给基地址**（`--base-url` / `OPENAI_BASE_URL` 为空）时按配置走：Provider、协议、
    ///   基地址、凭据变量名、生成选项（推理强度/温度/最大输出/超时/重试/provider_options）与
    ///   上下文窗口都来自 config.toml + models.toml 的解析结果——独立运行时不依赖命令行参数。
    /// - **显式给了基地址**时视为「外部渠道」：Provider/协议/生成选项/超时/重试/凭据变量名
    ///   一律用命令行给的值（内核按运行时默认语义发请求），不叠加配置里那条渠道的设置，
    ///   免得把别的协议塞给这个端点。
    ///
    /// `--model` 始终优先。提示词缓存身份在这里按稳定前缀算出来交给内核（它据此
    /// 派生 `prompt_cache_key`），与 Python `build_prompt_cache_identity` 逐字节对齐。
    pub fn handshake(&mut self) -> Result<(), String> {
        let external_channel = uses_external_channel(&self.options);
        // 稳定前缀的三样来源：system prompt、工具声明、项目规范。
        // 先算好再进 `KernelModelConfig` 字面量，避免同一份内容算两遍（模板可能很大）。
        let system_prompt = self.prompt.system_prompt();
        let tool_declarations = self.registry.declarations();
        // 活动 Skill 取决于用户本轮输入，握手时还没有，因此传空列表；
        // 此时 `active_skill_context_hash` 就是空数组的哈希，与 Python 无命中时同值。
        let prompt_cache_identity = build_prompt_cache_identity(
            &system_prompt,
            self.workspace.as_path(),
            &self.prompt.project_instructions().unwrap_or_default(),
            &self.prompt.skill_metas(),
            &[],
            &tool_declarations,
        );
        let model = KernelModelConfig {
            model: self.options.model.clone(),
            provider: if external_channel {
                String::new()
            } else {
                self.llm.provider.clone()
            },
            protocol: if external_channel {
                String::new()
            } else {
                self.llm.protocol.clone()
            },
            base_url: if external_channel {
                self.options.base_url.clone()
            } else {
                self.llm.base_url.clone()
            },
            api_key: if external_channel {
                self.options.api_key.clone()
            } else {
                self.llm.api_key.clone()
            },
            user_agent: USER_AGENT.to_string(),
            system_prompt,
            context_messages: match self.prompt.context_messages_with_plugins(
                true,
                Some(self.plugins.as_ref()),
                None,
                None,
            ) {
                Ok(messages) => messages,
                Err(error) => {
                    // `unwrap_or_default` 会让模型静默地收不到项目规范 / Skill / 运行环境，
                    // 看起来就像「系统提示词没生效」；装不出来就明说。
                    self.state.notice(format!(
                        "上下文装配失败：{error}（本轮只发系统提示词与历史）"
                    ));
                    Vec::new()
                }
            },
            tools: tool_declarations,
            options: if external_channel {
                json!({})
            } else {
                generation_options(&self.llm)
            },
            request_timeout_seconds: if external_channel {
                None
            } else if self.llm.request_timeout_seconds > 0 {
                Some(self.llm.request_timeout_seconds as f64)
            } else {
                None
            },
            // 上下文窗口与渠道无关：命令行显式值优先，其次配置。
            context_window_tokens: effective_context_window(
                self.options.context_window_tokens,
                &self.llm,
            ),
            // Provider 能力声明来自自定义模型条目的 `capabilities.prompt_cache`；
            // 未声明时内核 `should_send_prompt_cache_key` 对 GPT 系列有回退分支，
            // 与 Python 在 `capabilities.prompt_cache` 未声明时的行为一致。
            prompt_cache_capable: self.llm.prompt_cache.unwrap_or(false),
            prompt_cache_identity: prompt_cache_identity.to_identity_map(),
            // 交给内核做优先级判定：为真时带图观察直送主模型，不再走 `[vision]` 代理。
            native_vision: self.native_vision_enabled(),
            request_retry_count: if external_channel {
                1
            } else {
                self.llm.request_retry_count.max(1) as u32
            },
        };
        let context_window =
            effective_context_window(self.options.context_window_tokens, &self.llm);
        let environment = ConfigEnvironment::from_process();
        // 压缩策略整段下发：读不到配置时降级为「内核默认」，但要明说一声，
        // 否则用户只会看到「模型摘要未通过校验」而不知道策略根本没生效。
        let compaction = match load_context_compaction_config(&environment, None) {
            Ok(config) => Some(kernel_compaction_settings(context_window, &config)),
            Err(error) => {
                self.state.notice(format!(
                    "上下文压缩配置读取失败：{}（本会话按内核默认压缩策略运行）",
                    error.message()
                ));
                None
            }
        };
        // 记忆根同理：内核不知道用户把记忆放在哪儿，不给就等于关掉压缩回写与自动召回；
        // 开关与工具表同源（`[memory] enabled`，缺省开启）。
        let memory_root = load_feature_enabled(&environment, "memory", true, None, None)
            .unwrap_or(true)
            .then(|| {
                user_data_root(environment.home())
                    .to_string_lossy()
                    .to_string()
            });
        let session = self.options.session_root.as_ref().map(|root| {
            Box::new(KernelSessionConfig {
                root: root.to_string_lossy().to_string(),
                session_id: String::new(),
                memory_root: memory_root.clone(),
                workspace_root: Some(self.workspace.to_string_lossy().to_string()),
                compaction: compaction.clone(),
            })
        });
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            client: json!({
                "name": "omnicrawl-tui",
                "version": env!("CARGO_PKG_VERSION"),
            }),
            model: Some(Box::new(model)),
            session,
            // TUI 总是持有插件运行期：声明能力，内核才会在模型请求前发 `model.hook`。
            plugin_model_hooks: true,
            // 淘汰运行期已装配就声明能力：内核才会把「刚变老」的那一批交回来裁决。
            tool_call_prune: self
                .prune
                .as_ref()
                .map(|prune| prune.enabled)
                .unwrap_or(false),
        };
        let id = self.kernel.next_id();
        let frame = Command::Initialize(params).to_frame(id.clone());
        self.kernel
            .send_frame(&frame)
            .map_err(|error| format!("发送握手帧失败：{error}"))?;

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500));
            let Some(frame) = self.kernel.recv_timeout(wait) else {
                continue;
            };
            if frame.is_response() && frame.id() == Some(&id) {
                if let Some(error) = frame.error.as_ref() {
                    return Err(format!("内核拒绝握手：{}", error.message));
                }
                // 会话 id 只在内核侧产生（`initialize.session` 没给 id 时内核新建一条），
                // 握手回包是宿主知道「现在在哪个会话」的唯一途径。
                if let Some(session_id) = frame
                    .result
                    .as_ref()
                    .and_then(|result| result.get("session_id"))
                    .and_then(Value::as_str)
                {
                    self.session_id = session_id.to_string();
                }
                let declared = self.registry.declarations().len();
                report_startup_log(LogLevel::Info, &format!("已向内核声明 {declared} 个工具。"));
                return Ok(());
            }
            self.handle_frame(frame);
        }
        Err("等待内核握手响应超时。".to_string())
    }


    /// 收干内核帧与工具执行结果；内核退出时收尾退出。
    pub fn drain_frames(&mut self) {
        // 先消化同步往返期间让路的帧，再读内核通道：两边都按到达顺序处理。
        while let Some(frame) = self.deferred_frames.pop_front() {
            self.handle_frame(frame);
        }
        while let Some(frame) = self.kernel.try_recv() {
            self.handle_frame(frame);
        }
        self.drain_completions();
        self.drain_prune_results();
        self.drain_suggest_results();
        self.drain_kernel_logs();
        self.drain_diagnostics();
        self.drain_channel_models();
        self.enforce_tool_deadline();
        // 内核退出只收尾一次（本函数每 50ms 跑一遍），且**不**顺手退出界面：
        // 退出前用户必须有机会看清原因与会话现场。
        if self.kernel.is_closed() && !self.quit && !self.kernel_lost {
            self.kernel_lost = true;
            // 先关卡：内核没了，工具批次与排队中的提交都不可能再落地，留着只会产生
            // 「回工具批次失败」「工具超时」这类二次噪音。
            self.tool_deadline = None;
            self.state.cancel_batch();
            // 模态页先收起来：设置页要靠内核落地，留着只会盖住报错。
            self.settings = None;
            self.state.fail_turn(KERNEL_LOST_MESSAGE.to_string());
            self.plugins
                .turn_error(KERNEL_LOST_MESSAGE, None, self.current_turn_id().as_deref());
            // 现场仍在（消息流、工具卡、报错都在原位），再补一条「怎么退出、怎么恢复」。
            self.state.notice(self.kernel_lost_hint());
            self.state.set_status(Some(KERNEL_LOST_STATUS.to_string()));
        }
    }


    /// 内核退出后写在会话流里的自救指引：现场还在、怎么退出、怎么恢复，一次说清。
    ///
    /// 会话 id 为空（还没握手出会话）时退回占位写法：不能因为拿不到 id 就把指引省了。
    pub(super) fn kernel_lost_hint(&self) -> String {
        let session = self.current_session_id();
        let resume = if session.is_empty() {
            "omnicrawl --resume <会话 id>".to_string()
        } else {
            format!("omnicrawl --resume {session}")
        };
        format!(
            "界面停在原处以保留现场（消息、工具卡与报错都还在）。按 Enter / Esc / Ctrl+Q 退出；\
             本会话已保存在本地，重启后用 `{resume}` 恢复。"
        )
    }


    /// 把内核 stderr 里的运行期报错转成会话区提示。
    ///
    /// 内核的 stderr 以前直接继承到终端，报错会写在光标处、盖住底部输入框；
    /// 现在逐行收进通道，只有错误行（失败 / 错误 / 异常 / 无法）作为提示进会话流，
    /// 常规日志丢弃，避免刷屏。
    pub(super) fn drain_kernel_logs(&mut self) {
        let Some(logs) = self.kernel_logs.take() else {
            return;
        };
        let mut lines: Vec<String> = Vec::new();
        let mut disconnected = false;
        loop {
            match logs.try_recv() {
                Ok(line) => lines.push(line),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        for line in lines {
            if let Some(message) = kernel_error_notice(&line) {
                self.state.notice(message);
            }
        }
        // 内核退出时通道断开：保留 `None`，不再每帧空转。
        if !disconnected {
            self.kernel_logs = Some(logs);
        }
    }


    pub(super) fn send(&mut self, command: Command) -> Id {
        let id = self.kernel.next_id();
        let frame = command.to_frame(id.clone());
        if let Err(error) = self.kernel.send_frame(&frame) {
            self.state.notice(format!("发送内核命令失败：{error}"));
        }
        id
    }


    /// 发一条命令并等它的响应帧（只用于毫秒级的只读查询）。
    ///
    /// 命令层的 `CommandAgent` 是同步接口，而内核链路是异步的帧，因此这里在等待期间
    /// 把「先到的其他帧」原样收进 [`App::deferred_frames`]，交给下一次 [`App::drain_frames`]
    /// 按原顺序处理：通知不丢，也不会在命令处理器里重入渲染或重入工具批次。
    pub(super) fn request_kernel(&mut self, command: Command, timeout: Duration) -> Result<Frame, String> {
        if self.kernel.is_closed() {
            return Err("内核进程已退出。".to_string());
        }
        let id = self.send(command);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(KERNEL_POLL_INTERVAL);
            match self.kernel.recv_timeout(wait) {
                Some(frame) if frame.is_response() && frame.id() == Some(&id) => {
                    return match frame.error.as_ref() {
                        Some(error) => Err(format!("内核拒绝请求：{}", error.message)),
                        None => Ok(frame),
                    };
                }
                Some(frame) => self.deferred_frames.push_back(frame),
                None if self.kernel.is_closed() => {
                    return Err("内核进程在等待响应期间退出。".to_string())
                }
                None => {}
            }
        }
        Err(format!("等待内核响应超时（{} 秒）。", timeout.as_secs()))
    }


    /// 收集已完成的工具执行；整批就绪时回内核。
    pub(super) fn drain_completions(&mut self) {
        let mut finished: Vec<ToolCompletion> = Vec::new();
        while let Ok(completion) = self.completions.try_recv() {
            finished.push(completion);
        }
        for completion in finished {
            let now = Instant::now();
            self.state
                .finish_tool_run(&completion.call, &completion.result, now);
            self.state
                .record_tool_result(completion.index, completion.result, completion.vision);
        }
        self.flush_ready_batch();
    }


    /// 整批就绪就回观察；批次 id 必须在取观察前记下（取完即卸下批次）。
    pub(super) fn flush_ready_batch(&mut self) {
        let Some(request_id) = self.state.batch_request_id() else {
            return;
        };
        let attach_images = self.attach_vision_images();
        if let Some(observations) = self.state.take_observations(attach_images) {
            self.respond_batch(&request_id, observations);
        }
    }


    pub(super) fn cancel_turn(&mut self) {
        let Some(turn_id) = self.state.turn.turn_id().map(|value| value.to_string()) else {
            return;
        };
        self.send(Command::TurnCancel(omnicrawl_ipc::TurnCancelParams {
            turn_id: turn_id.clone(),
        }));
        // 取消也是回合结束：插件侧发 `turn.cancelled`（通知类，失败不阻断收尾）。
        self.plugins.turn_cancelled(None, Some(turn_id.as_str()));
        // 先回收正在跑的进程树与这个回合的后台任务，再卸下批次：迟到的结果会被忽略。
        self.registry.cancel_token().cancel();
        self.registry
            .stop_monitors_in_scope(&turn_id, crate::tools::monitor::CANCEL_REASON);
        self.tool_deadline = None;
        self.state.cancel_batch();
        self.state.fail_turn("已取消当前回合。".to_string());
        // 取消也是回合结束：排队消息接着按 FIFO 提交。
        self.drain_pending_inputs();
    }
}
