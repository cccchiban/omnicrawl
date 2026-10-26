//! 由 `service.rs` 按职责拆出，语义未变。

use super::*;

impl AgentService {
        /// 审查配置的热应用：外部改 `config.toml` / `decision_models.toml` 后即时换掉运行期。
        ///
        /// 覆盖 `[approval] mode`、`review_model` 与 `[decision_models.features] tool_call_review`
        /// 及默认决策渠道；与设置端点写盘后的效果同源，区别只是配置来自文件而不是请求。
        /// 读取失败时保留现有运行期并留待下一次轮询重试（不 accept），避免一次瞬时读失败把审查降级。
        pub(super) fn apply_review_config(&self, runner: &mut TurnRunner) {
            let env = &self.options.env;
            let Ok(mut watch) = self.review_watch.lock() else {
                return;
            };
            let Some(change) = watch.poll(env) else {
                return;
            };
            let review = if change.rebuild_runtime {
                match load_llm_config(env) {
                    Ok(llm) => Some(build_review_options(&llm, env)),
                    Err(error) => {
                        eprintln!("[api] 审查配置读取失败，沿用当前审查运行期：{error}");
                        return;
                    }
                }
            } else {
                None
            };
            let approval = change
                .approval_mode
                .as_deref()
                .and_then(runtime_approval_mode);
            let rebuild_runtime = change.rebuild_runtime;
            let approval_from_file = change.approval_mode.is_some();
            if let Some(options) = review.as_ref() {
                runner.set_review(options.clone());
            }
            if let Some(mode) = approval {
                runner.set_approval_mode(mode);
            }
            watch.accept();
            // 先放掉探测器锁再同步重起模板：锁顺序与 `set_runtime_approval_mode` 保持一致
            // （那个入口的顺序是 runner → spawner → watch）。
            drop(watch);
            // 重起内核（切换会话 / 工作区）会按 `spawner.runner` 重建运行器：不同步就会退回旧配置。
            if let Ok(mut spawner) = self.spawner.lock() {
                if let Some(spawner) = spawner.as_mut() {
                    if let Some(options) = review {
                        spawner.runner.review = options;
                    }
                    if let Some(mode) = approval {
                        spawner.runner.approval = mode;
                    }
                }
            }
            let mut applied: Vec<&str> = Vec::new();
            if rebuild_runtime {
                applied.push("审查运行期");
            }
            if approval_from_file {
                applied.push("审批模式");
            }
            if !applied.is_empty() {
                eprintln!("[api] 审查配置已热应用：{}。", applied.join("、"));
            }
        }

        /// 当前模型：启动值，或最近一次运行期切换后的值。
        pub fn current_model(&self) -> String {
            self.model
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }

        /// 运行期切换成功后同步当前模型。
        pub(super) fn set_current_model(&self, model: &str) {
            if let Ok(mut guard) = self.model.lock() {
                *guard = model.to_string();
            }
        }

        /// 审批模式取 `config.toml` 当前值；读不到时退到启动时解析的结果。
        pub fn approval_mode(&self) -> String {
            match load_approval_mode(&self.options.env, None) {
                Ok(mode) => mode,
                Err(_) => match self.options.approval {
                    ApprovalMode::Auto => "auto".to_string(),
                    ApprovalMode::Review => "review".to_string(),
                    ApprovalMode::Manual => "manual".to_string(),
                },
            }
        }

        /// 推理强度取 `config.toml` 当前值；空值与缺失统一报 `none`。
        pub fn reasoning_effort(&self) -> String {
            let effort = load_llm_config(&self.options.env)
                .map(|config| config.reasoning_effort)
                .unwrap_or_default();
            let effort = effort.trim();
            if effort.is_empty() {
                "none".to_string()
            } else {
                effort.to_string()
            }
        }

        /// `GET /runtime` 的响应体。
        pub fn runtime_snapshot(&self) -> Value {
            json!({
                "workspace_root": self.workspace_root().to_string_lossy(),
                "session_id": self.current_session_id(),
                "model": self.current_model(),
                "reasoning_effort": self.reasoning_effort(),
                "approval_mode": self.approval_mode(),
                "active_run_id": self.store.active_run_id(),
            })
        }

        /// 替换顾问运行期配置并重建工具表（连接器 `/advisor`）。
        ///
        /// 与 TUI 的 `command_apply_advisor_config` 同义：写盘由命令处理器负责，这里只把新配置
        /// 映射成运行期选项再重建工具表，让 advisor 工具即时出现/消失。
        pub fn apply_advisor_config(
            &self,
            config: &omnicrawl_config::features::advisor::AdvisorConfig,
        ) -> Result<(), ApiError> {
            let environment = self.options.env.clone();
            let mut registry_options = self.current_registry_options();
            let previous = registry_options.advisor.clone();
            let mut advisor = previous.clone();
            advisor.enabled = config.enabled && !config.model_key.trim().is_empty();
            advisor.effort = config.effort.clone();
            advisor.disabled_for_models = config.disabled_for_models.clone();
            match omnicrawl_config::models::channels::load_channel_configuration(
                &environment,
                None,
                None,
            )
            .ok()
            .and_then(|channels| {
                channels
                    .channels
                    .into_iter()
                    .find(|channel| channel.key == config.model_key)
            }) {
                Some(channel) => {
                    advisor.model = channel.model_id.clone();
                    advisor.base_url = if channel.base_url.trim().is_empty() {
                        self.options.model.base_url.clone()
                    } else {
                        channel.base_url.clone()
                    };
                    // 顾问凭据只认渠道里的明文密钥（与运行期 `resolve_api_key` 同口径）。
                    advisor.api_key = channel.api_key.clone();
                }
                None => {
                    advisor.model = config.model_key.clone();
                    advisor.base_url = self.options.model.base_url.clone();
                    advisor.api_key = String::new();
                }
            }
            registry_options.advisor = advisor;
            if let Err(error) = self.reload_registry_options(&registry_options, "顾问设置") {
                // 回滚：不能让磁盘是新值、运行期是旧值这种看不见的错配留在进程里。
                let mut rollback = self.current_registry_options();
                rollback.advisor = previous;
                let _ = self.reload_registry_options(&rollback, "顾问回滚");
                return Err(error);
            }
            Ok(())
        }

        /// 运行期下发一次 `session.settings`。
        ///
        /// 内核主循环在跑回合时被占用，`session.settings` 要到回合结束才会被处理，因此这里用
        /// `try_lock` 快速失败而不是排队等待——否则 HTTP 请求会挂住整段生成时间。
        pub(super) fn apply_session_settings(&self, params: SessionSettingsParams) -> Result<Value, ApiError> {
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能下发运行期设置。"))?;
            match guard.as_mut() {
                Some(runner) => runner
                    .apply_session_settings(params, &mut |_| {})
                    .map_err(|message| {
                        ApiError::new(
                            "KERNEL_SETTINGS_FAILED",
                            message,
                            StatusCode::BAD_GATEWAY,
                            None,
                        )
                    }),
                None => Err(ApiError::service_unavailable()),
            }
        }

        /// 切换模型：把新模型视图整理成 `session.settings` 下发，成功后同步当前模型。
        ///
        /// 工具声明不下发（切换模型不动工具表）；空字符串按「不改」处理，与协议语义一致。
        pub fn apply_model_switch(&self, llm: &LlmConfig) -> Result<(), ApiError> {
            let settings = SessionModelSettings {
                model: Some(llm.model.clone()),
                options: Some(generation_options(llm)),
                reasoning_effort: non_empty(&llm.reasoning_effort),
                system_prompt: None,
                context_messages: None,
                tools: None,
                context_window_tokens: Some(llm.context_window_tokens),
                provider: non_empty(&llm.provider),
                protocol: non_empty(&llm.protocol),
                base_url: non_empty(&llm.base_url),
                api_key: non_empty(&llm.api_key),
                // 原生视觉随模型走：换模型后带图观察是该直送主模型还是交给 `[vision]` 代理，
                // 取决于新模型的声明，所以每次切换都要重新下发。
                native_vision: Some(self.options.native_vision),
                // 淘汰能力不是模型配置的一部分：它是宿主运行期的开关，由 `rebuild_prune` 单独下发。
                ..SessionModelSettings::default()
            };
            // 窗口变了要同步触发阈值：「窗口的百分之几时压缩」是一个配置关系，
            // 只改窗口会留下「新窗口 + 旧阈值」的自相矛盾组合（TUI 侧同样成对下发）。
            let compaction = window_compaction_settings(&self.options, llm.context_window_tokens);
            self.apply_session_settings(SessionSettingsParams {
                model: Some(Box::new(settings)),
                compaction,
            })?;
            self.set_current_model(&llm.model);
            Ok(())
        }

        /// 运行期切换推理强度：只写内核生成选项里的这一个键。
        pub fn apply_reasoning_effort(&self, effort: &str) -> Result<(), ApiError> {
            let settings = SessionModelSettings {
                reasoning_effort: Some(effort.to_string()),
                ..SessionModelSettings::default()
            };
            self.apply_session_settings(SessionSettingsParams {
                model: Some(Box::new(settings)),
                compaction: None,
            })?;
            Ok(())
        }

        /// 运行期切换审批模式（下一次工具批次生效；审批由宿主定调）。
        pub fn set_runtime_approval_mode(&self, mode: ApprovalMode) -> Result<(), ApiError> {
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能切换审批模式。"))?;
            match guard.as_mut() {
                Some(runner) => {
                    runner.set_approval_mode(mode);
                }
                None => return Err(ApiError::service_unavailable()),
            }
            // 重起内核会按 `spawner.runner` 重建运行器；不同步就会退回旧模式。
            if let Ok(mut spawner) = self.spawner.lock() {
                if let Some(spawner) = spawner.as_mut() {
                    spawner.runner.approval = mode;
                }
            }
            // 值已由请求写入磁盘：重取基线，免得下一次轮询把这次改动当成外部变更再应用一遍。
            if let Ok(mut watch) = self.review_watch.lock() {
                watch.baseline(&self.options.env);
            }
            Ok(())
        }

}
