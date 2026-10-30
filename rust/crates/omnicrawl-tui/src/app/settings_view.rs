//! 设置面板的视图数据与事件派发：把配置读成一页的初值，把界面事件翻成 `SettingsChange`。
//!
//! 这里只负责「读配置 → 填面板」与「面板事件 → 变更」；变更怎么落盘见
//! [`super::settings_apply`]，一页的运行期重建见 [`super::settings_pages`]。

use super::*;

impl App {
    // ---------- 设置面板 ----------

    /// 打开设置面板：初始值来自配置与当前工具表，读不出来就只提示、不打开。
    pub(super) fn open_settings(&mut self) {
        match self.settings_values() {
            Ok(values) => self.settings = Some(SettingsState::new(values)),
            Err(message) => self.state.notice(format!("设置面板打不开：{message}")),
        }
    }

    /// 打开设置面板并直接落到某个一级项（欢迎页徽标这类鼠标入口用）。
    ///
    /// `key` 是 `ROW_ORDER` 里的键名（`model_management` / `tools` / `mcp` …），
    /// 与点左栏那一行同义：右侧直接挂上该页、焦点进右栏。
    pub(super) fn open_settings_at(&mut self, key: &str) {
        // 面板铺满整屏，徽标的悬停态先收掉：关掉面板后不会留着一截黄色。
        self.state.set_welcome_badge_hover(None);
        match self.settings_values() {
            Ok(values) => {
                let mut settings = SettingsState::new(values);
                settings.select_key(key);
                self.settings = Some(settings);
                // 直接落到会话管理页时同样要读一次列表（否则是空列表 + 空状态行）。
                self.load_sessions_if_needed();
            }
            Err(message) => self.state.notice(format!("设置面板打不开：{message}")),
        }
    }

    /// 读设置面板的初始值：上下文两页、四个单选页与内置工具开关。
    ///
    /// 上下文取自「命令行/环境显式给定 > config.toml 的 llm 段」，压缩阈值优先用配置里记着的
    /// 百分比，没有就按阈值 Token 反推（与 Python 的 `_context_compaction_percent` 同口径）；
    /// 记忆开关取运行期现值（`registry_options.memory_enabled`）。
    pub(super) fn settings_values(&self) -> Result<SettingsValues, String> {
        let environment = ConfigEnvironment::from_process();
        let llm = load_llm_config(&environment)
            .map_err(|error| format!("读取模型配置失败：{}", error.message()))?;
        let context_window_tokens =
            effective_context_window(self.options.context_window_tokens, &llm);
        let compaction = load_context_compaction_config(&environment, None)
            .map_err(|error| format!("读取上下文压缩配置失败：{}", error.message()))?;
        let percent = match compaction.trigger_context_percent {
            Some(percent) => nearest_compaction_percent(percent),
            None => nearest_compaction_percent(
                compaction.trigger_context_tokens * 100 / context_window_tokens.max(1),
            ),
        };
        let show_thinking = load_show_thinking(&environment, None).unwrap_or(true);
        let model_key = self.active_model_key(&environment, &llm);
        let (channel_rows, default_channel_key) = self.channel_views(&environment);
        // 决策模型页的初值：渠道、功能开关与自部署分区（读不出来就用配置默认值）。
        let (decision_rows, default_decision_key) = self.decision_views(&environment);
        let decision_template = decision_row_from_config(&DecisionChannelConfig::default());
        let decision_switches = decision_switch_rows(&load_decision_switches(&environment, None));
        let decision_local = self.decision_local_values(&environment);
        // 向量模型页的初值：渠道、默认渠道、新建模板与总开关（读不出来就用配置默认值）。
        let embedding_config =
            load_embedding_model_configuration(&environment, None).unwrap_or_default();
        let embedding_rows: Vec<EmbeddingRow> = embedding_config
            .channels
            .iter()
            .map(embedding_row_from_config)
            .collect();
        let default_embedding_key = embedding_config.default_key.clone();
        let embedding_template = embedding_row_from_config(&EmbeddingChannelConfig::default());
        let embedding_enabled =
            load_feature_enabled(&environment, "memory_embedding", false, None, None)
                .unwrap_or(false);
        let channel_template = default_channel(provider_options()[0], None)
            .map(|channel| channel_row_from_config(&channel))
            .unwrap_or_default();
        // 表单页的初值：读不出来就退回配置默认值，保存时会再校验一次。
        let advisor = load_advisor_config(&environment, None).unwrap_or_default();
        let compression =
            load_tool_output_compression_config(&environment, None).unwrap_or_default();
        let desensitization = load_desensitization_config(&environment, None).unwrap_or_default();
        let run_guard = load_run_guard_config(&environment, None).unwrap_or_default();
        let workspace = load_agent_workspace_config(&environment, None).unwrap_or_default();
        let image_gen = load_image_gen_configuration(&environment, None).unwrap_or_default();
        let subagent_config = load_subagent_config(&environment, None).unwrap_or_default();
        // 视觉页：代理开关 + 故障转移列表 + 当前主模型的模型原生视觉三态值。
        let vision = load_vision_configuration(&environment, None).unwrap_or(VisionConfiguration {
            enabled: false,
            models: Vec::new(),
        });
        let vision_native =
            resolve_native_vision(&environment, &llm.catalog_key, &llm.profile_id, None, None)
                .map(|setting| setting.value)
                .unwrap_or(None);
        // TTS 页：通用键 + 两条云通道的配置（界面按 `[tts_cloud].provider` 显示其中一家）。
        let tts_values = tts_settings_values(&environment);
        let form_channel_rows = channel_rows.clone();
        Ok(SettingsValues::new(
            context_window_tokens,
            percent,
            tool_switch_rows(&self.registry),
        )
        .with_model(&model_key)
        .with_channels(channel_rows, &default_channel_key, channel_template)
        .with_decision_models(decision_rows, &default_decision_key, decision_template)
        .with_decision_switches(decision_switches)
        .with_embedding_models(
            embedding_rows,
            &default_embedding_key,
            embedding_template,
            embedding_enabled,
        )
        .with_decision_local(decision_local)
        .with_choices(
            &llm.reasoning_effort,
            show_thinking,
            self.registry_options.memory_enabled,
        )
        .with_form(
            FormKind::Advisor,
            advisor_form_values(&advisor, &form_channel_rows),
        )
        .with_form(
            FormKind::ToolOutputCompression,
            compression_form_values(&compression, &form_channel_rows),
        )
        .with_form(
            FormKind::Desensitization,
            desensitization_form_values(&desensitization),
        )
        .with_form(FormKind::RunGuard, run_guard_form_values(&run_guard))
        .with_form(
            FormKind::AgentWorkspace,
            agent_workspace_form_values(&workspace),
        )
        .with_form(FormKind::ImageGen, image_gen_form_values(&image_gen))
        .with_form(
            FormKind::ComputerUse,
            computer_use_form_values(
                &load_computer_use_config(&environment, None).unwrap_or_default(),
            ),
        )
        .with_subagents(subagent_rows(&subagent_config))
        .with_mcp(self.mcp_settings_values(&environment))
        .with_web_search(web_search_row(&environment))
        .with_vision(
            vision.enabled,
            vision.models.iter().map(vision_ref_from_config).collect(),
            vision_native,
        )
        .with_tts(tts_values))
    }

    /// 渠道页的初始列表与默认渠道（读不出来就留空，页面会提示按 N 新建）。
    pub(super) fn channel_views(&self, environment: &ConfigEnvironment) -> (Vec<ChannelRow>, String) {
        match load_channel_configuration(environment, None, None) {
            Ok(configuration) => (
                configuration
                    .channels
                    .iter()
                    .map(channel_row_from_config)
                    .collect(),
                configuration.default_key.clone(),
            ),
            Err(error) => {
                diagnostics::warn(format!("[tui] 渠道配置读取失败，渠道页留空：{error}"));
                (Vec::new(), String::new())
            }
        }
    }

    /// 决策模型页的渠道列表与默认渠道（读不出来就退回配置默认值）。
    pub(super) fn decision_views(&self, environment: &ConfigEnvironment) -> (Vec<DecisionRow>, String) {
        match load_decision_model_configuration(environment, None) {
            Ok(configuration) => (
                configuration
                    .channels
                    .iter()
                    .map(decision_row_from_config)
                    .collect(),
                configuration.default_key.clone(),
            ),
            Err(error) => {
                diagnostics::warn(format!(
                    "[tui] 决策模型配置读取失败，决策页用默认渠道：{error}"
                ));
                let fallback = DecisionModelConfiguration::default();
                (
                    fallback
                        .channels
                        .iter()
                        .map(decision_row_from_config)
                        .collect(),
                    fallback.default_key,
                )
            }
        }
    }

    /// 模型管理页的当前模型渠道 key。
    ///
    /// 取渠道配置的默认渠道；读不出渠道（单模型 legacy 配置）时回落当前模型的引用
    /// （`llm.catalog_key`，即 `load_channel_configuration` 里 models.toml 的 key）。
    pub(super) fn active_model_key(&self, environment: &ConfigEnvironment, llm: &LlmConfig) -> String {
        match load_channel_configuration(environment, None, None) {
            Ok(configuration) if !configuration.channels.is_empty() => {
                if configuration.default_key.trim().is_empty() {
                    configuration
                        .channels
                        .first()
                        .map(|channel| channel.key.clone())
                        .unwrap_or_default()
                } else {
                    configuration.default_key.clone()
                }
            }
            Ok(_) => llm.catalog_key.clone(),
            Err(error) => {
                diagnostics::warn(format!(
                    "[tui] 渠道配置读取失败，模型管理页只显示当前渠道：{error}"
                ));
                llm.catalog_key.clone()
            }
        }
    }

    /// 设置面板的按键：面板自己认的键位归它，`Ctrl+Q`/`Ctrl+C` 仍由宿主兜底。
    pub(super) fn handle_settings_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => {
                    self.start_shutdown();
                    return;
                }
                KeyCode::Char('c') => {
                    self.settings = None;
                    return;
                }
                _ => {}
            }
            // 其余 Ctrl 组合归设置面板自己（渠道表单的 Ctrl+S 保存）。
            let event = self
                .settings
                .as_mut()
                .and_then(|settings| settings.handle_ctrl_key(key.code));
            self.dispatch_settings_event(event);
            return;
        }
        let event = self
            .settings
            .as_mut()
            .and_then(|settings| settings.handle_key(key.code));
        self.dispatch_settings_event(event);
        // 右侧可能刚切到「会话管理」页：两条入口（按键与鼠标）都汇到这里，统一按需载入列表。
        self.load_sessions_if_needed();
    }

    /// 右侧挂上「会话管理」页时按需加载列表。
    ///
    /// 只在刚进入该页时读一次：每次方向键都读会让内核往返淹没按键反馈。
    /// `Enter` 进页与鼠标点页都经过这里，因此以「当前 pane 是会话页且列表还没载入」判定。
    pub(super) fn load_sessions_if_needed(&mut self) {
        let Some(settings) = self.settings.as_ref() else {
            return;
        };
        if !settings.is_sessions_pane() || !settings.session_rows().is_empty() {
            return;
        }
        self.reload_session_rows();
    }

    /// 面板事件 → 宿主动作：关闭，或执行一次设置变更并回填结果。
    pub(super) fn dispatch_settings_event(&mut self, event: Option<SettingsEvent>) {
        match event {
            None => {}
            Some(SettingsEvent::Close) => self.settings = None,
            Some(SettingsEvent::Apply(change)) => {
                let outcome = self.apply_settings_change(&change);
                if let Some(settings) = self.settings.as_mut() {
                    match outcome {
                        Ok(message) => {
                            settings.apply_succeeded(&change, message);
                            // 联网搜索的凭据只在磁盘上有明文：保存后用真实配置覆盖本页的投影，
                            // 顺带把草稿里刚录入的明文换成掩码。
                            if matches!(
                                change,
                                SettingsChange::Form {
                                    kind: FormKind::WebSearch,
                                    ..
                                }
                            ) {
                                let row = web_search_row(&ConfigEnvironment::from_process());
                                settings.set_web_search(row);
                            }
                            // 图像生成的归一化可能折算端点与模型（切到 Gemini 却仍是 OpenAI
                            // 默认值时）：草稿也要跟着对齐，否则界面与磁盘各显示一份。
                            if matches!(
                                change,
                                SettingsChange::Form {
                                    kind: FormKind::ImageGen,
                                    ..
                                }
                            ) {
                                let configuration = load_image_gen_configuration(
                                    &ConfigEnvironment::from_process(),
                                    None,
                                )
                                .unwrap_or_default();
                                settings.set_image_gen_provider_defaults(
                                    &configuration.provider,
                                    &configuration.base_url,
                                    &configuration.model,
                                );
                            }
                        }
                        Err(message) => settings.apply_failed(message),
                    }
                }
                // 面板保存已经把新值应用到运行期，这里重取基线：否则下一帧的热应用会把它
                // 当成「外部改动」再报一次，并在 --approval 优先时给出误导性提示。
                self.review_watch
                    .baseline(&ConfigEnvironment::from_process());
            }
            Some(SettingsEvent::DiscoverChannelModels {
                profile_id,
                provider,
                protocol,
                base_url,
                api_key,
                user_agent,
            }) => self.start_channel_model_discovery(
                profile_id,
                provider,
                protocol,
                base_url,
                api_key,
                user_agent,
            ),
            Some(SettingsEvent::DeleteSession { session_id }) => {
                self.delete_session_from_settings(&session_id)
            }
            Some(SettingsEvent::RefreshSessions) => self.reload_session_rows(),
        }
    }

    /// 设置面板「会话管理」页：从内核读会话列表并回填。
    ///
    /// 列表读取失败只落到该页的状态行，不弹全局提示——面板还开着，状态行就在眼前。
    pub(super) fn reload_session_rows(&mut self) {
        // 先取列表与当前 id 再借面板：`command_session_list` 要可变借用 `self`，
        // 不能与 `settings` 的借用重叠。
        let loaded = self.command_session_list(false, 100);
        let current = self.session_id.clone();
        let Some(settings) = self.settings.as_mut() else {
            return;
        };
        match loaded {
            Ok(entries) => {
                let rows: Vec<SessionRow> = entries
                    .into_iter()
                    .map(|entry| SessionRow {
                        is_current: entry.session_id == current,
                        title: entry.title,
                        message_count: entry.message_count as usize,
                        session_id: entry.session_id,
                    })
                    .collect();
                settings.set_sessions(rows);
            }
            Err(message) => settings.set_sessions_status(&format!("会话列表读取失败：{message}")),
        }
    }

    /// 删除一个会话并就地更新列表；失败原因写回该页状态行。
    pub(super) fn delete_session_from_settings(&mut self, session_id: &str) {
        match self.command_session_delete(session_id) {
            Ok(removed) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.remove_session_row(&removed);
                }
            }
            Err(message) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.set_sessions_status(&format!("删除失败：{message}"));
                }
            }
        }
    }

    /// 后台自动检测一个渠道的模型列表（网络 I/O 不能占用界面线程）。
    ///
    /// 凭据只取渠道里的明文密钥（渠道页新录入的那一行，或 config.toml 里已存的值）；
    /// 协议字符串先解析成内核枚举，无法识别或缺少凭据时直接把原因回填给面板。
    pub(super) fn start_channel_model_discovery(
        &mut self,
        profile_id: String,
        provider: String,
        protocol: String,
        base_url: String,
        api_key: String,
        user_agent: String,
    ) {
        // 与 config 层 `ProviderProfile.resolve_api_key` 同口径：只认明文密钥。
        let api_key = api_key.trim().to_string();
        let timeout = (self.options.tool_timeout_seconds as f64).clamp(1.0, 10.0);
        let (sender, receiver) = mpsc::channel::<ChannelModelsResult>();
        self.channel_models_task = Some(receiver);
        thread::spawn(move || {
            let Some(parsed) = omnicrawl_protocol::Protocol::parse(&protocol) else {
                let _ = sender.send(ChannelModelsResult {
                    models: Vec::new(),
                    message: format!("未知的协议：{protocol}"),
                });
                return;
            };
            let profile = omnicrawl_llm::ProviderProfile {
                id: profile_id,
                provider,
                base_url,
                api_key,
                user_agent,
                default_protocol: parsed.as_str().to_string(),
            };
            let result = omnicrawl_llm::discover_models(&profile, parsed, timeout);
            let models: Vec<String> = result
                .models
                .iter()
                .map(|model| model.model_id.clone())
                .collect();
            let _ = sender.send(ChannelModelsResult {
                models,
                message: result.message,
            });
        });
    }

    /// 取回渠道模型检测结果并回填到面板。
    pub(super) fn drain_channel_models(&mut self) {
        let Some(receiver) = self.channel_models_task.take() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.set_channel_models(result.models, result.message);
                }
            }
            Err(mpsc::TryRecvError::Empty) => self.channel_models_task = Some(receiver),
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
    }
}
