//! 设置面板的分页编排：向量模型、结构化决策、TTS。
//!
//! 与 [`super::settings_apply`] 的区别：那里是「一次变更写进配置」，这里负责「一页的
//! 运行期重建与外部服务自启动」——写入成功后要立刻让新的句柄生效。

use super::*;

impl App {
    // ---------- 向量模型页 ----------

    /// 向量召回总开关：写 `config.toml` 的 `[memory_embedding] enabled`，并立刻重建召回运行期。
    pub(super) fn apply_embedding_enabled(&mut self, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_feature_enabled(&environment, "memory_embedding", enabled, None, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.rebuild_memory_embedding(&environment);
        if let Some(settings) = self.settings.as_mut() {
            settings.set_embedding_enabled(enabled);
        }
        Ok(format!(
            "向量召回：{}，已保存到 {}。",
            if enabled { "开启" } else { "关闭" },
            path.display()
        ))
    }

    /// 保存整份向量模型配置：`api_key` 留空表示不改动，按 key 从磁盘继承。
    ///
    /// 向量渠道同样不下发内核，因此写盘后只重建宿主侧的记忆向量召回运行期。
    pub(super) fn apply_embedding_models(
        &mut self,
        rows: &[EmbeddingRow],
        default_key: &str,
    ) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let previous = load_embedding_model_configuration(&environment, None)
            .unwrap_or_else(|_| EmbeddingModelConfiguration::default());
        let mut channels: Vec<EmbeddingChannelConfig> = Vec::new();
        for row in rows {
            let mut channel = embedding_config_from_row(row);
            if channel.api_key.is_empty() {
                if let Some(existing) = previous
                    .channels
                    .iter()
                    .find(|item| item.key == channel.key)
                {
                    channel.api_key = existing.api_key.clone();
                }
            }
            channels.push(channel);
        }
        let configuration = EmbeddingModelConfiguration {
            channels,
            default_key: default_key.to_string(),
        };
        let path = save_embedding_model_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.rebuild_memory_embedding(&environment);
        let reloaded =
            load_embedding_model_configuration(&environment, None).unwrap_or(configuration);
        let reloaded_enabled =
            load_feature_enabled(&environment, "memory_embedding", false, None, None)
                .unwrap_or(false);
        let rows: Vec<EmbeddingRow> = reloaded
            .channels
            .iter()
            .map(embedding_row_from_config)
            .collect();
        if let Some(settings) = self.settings.as_mut() {
            settings.refresh_embedding_models(
                rows,
                &reloaded.default_key,
                reloaded_enabled,
                "",
            );
        }
        Ok(format!("向量模型配置已保存到 {}。", path.display()))
    }

    /// 按当前配置重建记忆的向量召回运行期（保存后本会话即刻生效）。
    pub(super) fn rebuild_memory_embedding(&mut self, environment: &ConfigEnvironment) {
        let enabled = load_feature_enabled(environment, "memory_embedding", false, None, None)
            .unwrap_or(false);
        let embedding = Arc::new(
            omnicrawl_host::embedding_client::embedding_options_from_config(environment, enabled),
        );
        let memory = MemoryOptions {
            embedding,
            ..self.registry_options.memory.clone()
        };
        self.registry_options.memory = memory;
    }

    // ---------- 决策模型页（渠道 / 功能开关 / 自部署） ----------

    /// 保存整份决策模型配置：`api_key` 留空表示不改动，按 key 从磁盘继承。
    ///
    /// 决策渠道不下发内核（`initialize` 没有对映字段），因此写盘后只重建宿主侧的
    /// 审查 / 重排 / 托管运行期，再让界面与磁盘对齐。
    pub(super) fn apply_decision_models(
        &mut self,
        rows: &[DecisionRow],
        default_key: &str,
    ) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let previous = load_decision_model_configuration(&environment, None)
            .unwrap_or_else(|_| DecisionModelConfiguration::default());
        let mut channels: Vec<DecisionChannelConfig> = Vec::new();
        for row in rows {
            let mut channel = decision_config_from_row(row);
            if channel.api_key.is_empty() {
                if let Some(existing) = previous
                    .channels
                    .iter()
                    .find(|item| item.key == channel.key)
                {
                    channel.api_key = existing.api_key.clone();
                }
            }
            channels.push(channel);
        }
        let configuration = DecisionModelConfiguration {
            channels,
            default_key: default_key.to_string(),
        };
        let path = save_decision_model_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 五条调用通道都按新渠道重建，保存后本会话即刻生效。
        self.rebuild_review(&environment);
        self.rebuild_custody(&environment);
        self.rebuild_prune(&environment)?;
        self.rebuild_suggest(&environment);
        self.rebuild_rerank(&environment)?;
        let reloaded =
            load_decision_model_configuration(&environment, None).unwrap_or(configuration);
        let rows: Vec<DecisionRow> = reloaded
            .channels
            .iter()
            .map(decision_row_from_config)
            .collect();
        if let Some(settings) = self.settings.as_mut() {
            settings.sync_decision_models(rows, &reloaded.default_key);
        }
        Ok(format!("决策模型配置已保存到 {}。", path.display()))
    }

    /// 写回一个决策模型功能开关（选中即保存），并按新开关重建对应运行期。
    pub(super) fn apply_decision_switch(&mut self, key: &str, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_decision_switch(&environment, key, enabled, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 开关影响的是宿主侧哪些请求走决策模型，因此五条通道都要按新值重建。
        self.rebuild_review(&environment);
        self.rebuild_custody(&environment);
        self.rebuild_prune(&environment)?;
        self.rebuild_suggest(&environment);
        self.rebuild_rerank(&environment)?;
        let label = DECISION_SWITCHES
            .iter()
            .find(|spec| spec.key == key)
            .map(|spec| spec.label)
            .unwrap_or(key);
        Ok(format!(
            "{label}：{}，已保存到 {}。",
            if enabled { "开启" } else { "关闭" },
            path.display()
        ))
    }

    /// 重建四处决策模型重排运行期（记忆搜索、知识库检索、Skill 检索与联网搜索各一份）。
    pub(super) fn rebuild_rerank(&mut self, environment: &ConfigEnvironment) -> Result<(), String> {
        self.registry_options.memory.rerank =
            Arc::new(omnicrawl_host::tools::rerank_options_from_config(
                environment,
                DECISION_SWITCH_MEMORY_SEARCH,
            ));
        self.registry_options.knowledge_rerank =
            Arc::new(omnicrawl_host::tools::rerank_options_from_config(
                environment,
                DECISION_SWITCH_KB_SEARCH,
            ));
        self.registry_options.skill_rerank =
            Arc::new(omnicrawl_host::tools::rerank_options_from_config(
                environment,
                DECISION_SWITCH_SKILL_SEARCH,
            ));
        // 联网搜索的排序运行期嵌在搜索选项里，只换这一项：引擎与凭据保持当前值。
        self.registry_options.web_search.rerank =
            Arc::new(omnicrawl_host::tools::rerank_options_from_config(
                environment,
                DECISION_SWITCH_WEB_SEARCH,
            ));
        self.rebuild_registry()
    }

    // ---------- 决策模型页的自部署分区（OneJev 本地服务） ----------

    /// 自部署分区的一次动作：写配置、装环境、下载/删除权重、切渠道、启停服务。
    pub(super) fn apply_decision_local(&mut self, change: &DecisionLocalChange) -> Result<String, String> {
        match change {
            DecisionLocalChange::Save { size, device } => self.save_decision_local(size, device),
            DecisionLocalChange::PrepareEnvironment => self.start_onejev_environment(),
            DecisionLocalChange::RemoveEnvironment => self.remove_onejev_environment(),
            DecisionLocalChange::Download { size } => self.start_onejev_download(size),
            DecisionLocalChange::Delete { size } => self.delete_onejev_size(size),
            DecisionLocalChange::UseChannel { size } => self.use_onejev_channel(size),
            DecisionLocalChange::StartService => self.start_onejev_service(),
            DecisionLocalChange::StopService => self.stop_onejev_service(),
        }
    }

    /// 写回尺寸与设备（枚举行改档即保存）；服务在跑且规格变了就按新规格重启。
    pub(super) fn save_decision_local(&mut self, size: &str, device: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let previous = load_local_deployment(&environment, None);
        let configuration = LocalDeploymentConfig {
            size: size.to_string(),
            device: device.to_string(),
            ..previous.clone()
        };
        let path = save_local_deployment(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let spec_changed =
            previous.size != configuration.size || previous.device != configuration.device;
        let mut note = String::new();
        // 规格变了才动服务：一份服务只能跑一个尺寸，换尺寸必须换进程（停的是本机共享的那份）。
        // 规格没变时什么都不做——服务可能正被别的实例用着，静默重启只会打断它们。
        let running = self.onejev_server.state() != &OneJevServerState::Stopped
            || omnicrawl_onejev::service_ready(&resolve_onejev_root(
                &environment,
                &configuration.model_dir,
            ));
        if spec_changed && running {
            match self.restart_onejev_service(&configuration) {
                Ok(message) => note = format!(" {message}"),
                Err(error) => note = format!(" 服务未能按新规格重启：{error}"),
            }
        }
        let device_label = match configuration.device.as_str() {
            "cpu" => "CPU",
            "cuda" => "CUDA",
            _ => "自动",
        };
        Ok(format!(
            "自部署已设为 {}（设备 {}），已保存到 {}{}。",
            configuration.size,
            device_label,
            path.display(),
            note
        ))
    }

    /// 自部署分区的初值：尺寸清单 + 已下载状态 + 环境与服务状态。
    pub(super) fn decision_local_values(&self, environment: &ConfigEnvironment) -> DecisionLocalValues {
        let configuration = load_local_deployment(environment, None);
        let root = resolve_onejev_root(environment, &configuration.model_dir);
        let sizes: Vec<(String, String, bool)> = ONEJEV_SIZES
            .iter()
            .map(|size| {
                (
                    size.key.to_string(),
                    size.label.to_string(),
                    onejev_model_ready(&root, size),
                )
            })
            .collect();
        let env_ready =
            onejev_environment_state(&root) == omnicrawl_onejev::EnvironmentState::Ready;
        // 服务状态以 PID 文件与服务日志为准：它可能由别的 OmniCrawl 实例启动，
        // 界面必须如实说明「已就绪但属于其他实例」，否则用户会以为可以在这里随便停。
        // 这里刻意不发起网络探测——探测在服务未跑时会阻塞到超时，而这正是打开设置面板的路径。
        let owned = self.onejev_server.state() == &OneJevServerState::Ready;
        let listening = omnicrawl_onejev::service_ready(&root);
        let service_status = if listening {
            let name = omnicrawl_onejev::running_model(&root)
                .or_else(|| self.onejev_server.model_name().map(str::to_string))
                .unwrap_or_else(|| configuration.size.clone());
            if owned {
                format!("已就绪，{name}（本实例启动）")
            } else {
                format!("已就绪，{name}（其他 OmniCrawl 实例启动，本实例复用）")
            }
        } else {
            match self.onejev_server.state() {
                OneJevServerState::Ready => "已就绪（本实例记录的服务未再见就绪标记）".to_string(),
                other => other.label(),
            }
        };
        DecisionLocalValues {
            size: configuration.size.clone(),
            device: configuration.device.clone(),
            root: root.display().to_string(),
            sizes,
            env_ready,
            env_status: if env_ready {
                "已就绪".to_string()
            } else {
                format!(
                    "未安装（缺少 torch + qev：{}）",
                    omnicrawl_onejev::paths::venv_dir(&root).display()
                )
            },
            service_status,
            service_running: listening,
            task: self
                .onejev_progress
                .as_ref()
                .map(|running| running.name.clone()),
            progress: self
                .onejev_progress
                .as_ref()
                .map(|running| running.progress.snapshot())
                .unwrap_or_default(),
        }
    }


    /// 回合结束后朗读助手回复（`[tts].speak_replies`）。
    ///
    /// 合成要几秒，放后台线程；同一时刻只留一轮，上一轮还在合成时直接跳过本轮的
    /// 朗读——积压的语音放出来只会与界面上的当前对话错位。
    pub(super) fn speak_reply(&mut self, final_text: &str) {
        if final_text.trim().is_empty() {
            return;
        }
        let options = match self.registry_options.tts.as_ref() {
            Some(options) if options.configuration.speak_replies => Arc::clone(options),
            _ => return,
        };
        if self.reply_speaking.swap(true, Ordering::SeqCst) {
            return;
        }
        let text = final_text.to_string();
        let speaking = Arc::clone(&self.reply_speaking);
        thread::spawn(move || {
            if let Err(error) = options.speak_text(&text) {
                diagnostics::warn(format!("[tui] 朗读回复失败：{error}"));
            }
            speaking.store(false, Ordering::SeqCst);
        });
    }

    /// TTS 页的一次动作：保存整页草稿。
    pub(super) fn apply_tts(&mut self, change: &TtsChange) -> Result<String, String> {
        match change {
            TtsChange::Save(draft) => self.apply_tts_save(draft),
        }
    }

    /// 保存 `[tts]` / `[tts_cloud]` 与当前服务商段，并重建工具表。
    ///
    /// 启用状态决定 `tts_synthesize` 是否注册；未改动的另一家那一段保持磁盘原值。
    pub(super) fn apply_tts_save(&mut self, draft: &TtsDraft) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let previous_tts = load_tts_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous_cloud = load_tts_cloud_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous_api = load_tts_api_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous_gemini = load_tts_gemini_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;

        // 输出目录界面上不编辑，按磁盘上的原值保留。
        let configuration = TtsConfiguration {
            enabled: draft.enabled,
            auto_play: draft.auto_play,
            speak_replies: draft.speak_replies,
            output_dir: previous_tts.output_dir.clone(),
        };
        let cloud = TtsCloudConfiguration {
            provider: draft.provider.clone(),
        };
        let (api, gemini) = tts_channel_configurations(draft, &previous_api, &previous_gemini)?;

        let path = save_tts_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        save_tts_cloud_configuration(&environment, &cloud, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 两家都写回：界面把草稿里的另一家原样带出来，因此不会覆盖用户改过的值。
        save_tts_api_configuration(&environment, &api, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        save_tts_gemini_configuration(&environment, &gemini, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;

        let previous_options = self.registry_options.tts.clone();
        self.registry_options.tts = tts_options_from(&environment, &configuration, &self.workspace);
        if let Err(error) = self.rebuild_registry() {
            // 磁盘与运行期一起回滚（同 `apply_image_gen_form`）。
            if let Err(rollback) = save_tts_configuration(&environment, &previous_tts, None) {
                self.state
                    .notice(format!("TTS 设置回滚写盘失败：{rollback}"));
            }
            let _ = save_tts_cloud_configuration(&environment, &previous_cloud, None);
            let _ = save_tts_api_configuration(&environment, &previous_api, None);
            let _ = save_tts_gemini_configuration(&environment, &previous_gemini, None);
            self.registry_options.tts = previous_options;
            return Err(error);
        }
        Ok(format!(
            "TTS 设置已保存到 {}（{}）。",
            path.display(),
            if !configuration.enabled {
                "语音合成已停用"
            } else {
                "本会话即刻生效"
            }
        ))
    }

}
