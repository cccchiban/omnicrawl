//! 命令层钩子：`commands::TuiHostAgent` 唯一能碰宿主的入口。
//!
//! 内核链路、工具表与配置读写的细节都收在这里，命令层不必知道。拆成独立 `impl App`
//! 块是因为命令层与界面编排的变更理由不同：改斜杠命令语义不该动输入处理。

use super::*;

impl App {
    // ---------- 命令层钩子 ----------
    //
    // `commands::TuiHostAgent` 只经这些入口碰宿主：内核链路、工具表与配置读写的
    // 细节都收在这里，命令层不必知道。

    pub(crate) fn command_workspace_root(&self) -> PathBuf {
        self.workspace.clone()
    }

    /// 命令层读取 Skill 索引与诊断：与提示词装配同源（含 `[skills].extra_paths`）。
    pub(crate) fn command_skills(&self) -> (Vec<SkillMeta>, Vec<SkillDiagnostic>) {
        (self.prompt.skill_metas(), self.prompt.skill_diagnostics())
    }

    /// 当前审批模式名（`manual` / `review` / `auto`，与 Python 的 `approval_mode()` 同形）。
    pub(crate) fn command_approval_mode(&self) -> &'static str {
        self.state.approval.as_str()
    }

    /// 运行期切换审批模式；持久化由命令处理器自己写 `config.toml`。
    ///
    /// 审批由宿主执行层判定，内核不知道也不需要知道模式，因此这里只改本进程状态。
    pub(crate) fn command_set_approval_mode(&mut self, mode: &str) -> Result<(), String> {
        let parsed = ApprovalMode::parse(mode)?;
        self.state.approval = parsed;
        self.options.approval = parsed;
        Ok(())
    }

    pub(crate) fn command_llm_config(&self) -> LlmConfig {
        self.llm.clone()
    }

    /// 启用主 Agent 模式：重算 system prompt（含模式区块）并即时下发内核。
    ///
    /// 模式改的是 system prompt，而 system prompt 由宿主装配（模板 + AGENTS.md + Skill），
    /// 因此这里必须与握手走同一份装配结果；内核只按帧里的文本发请求。
    pub(crate) fn command_activate_mode(&mut self, mode: &str) -> Result<String, String> {
        let activated = self.prompt.activate_mode(mode)?;
        self.push_prompt_to_kernel()?;
        Ok(activated)
    }

    /// 关闭当前模式：清掉模式区块后重算并下发，基础提示词保持不变。
    pub(crate) fn command_deactivate_mode(&mut self) -> Result<String, String> {
        let previous = self.prompt.deactivate_mode();
        self.push_prompt_to_kernel()?;
        Ok(previous)
    }

    /// 当前已启用的主 Agent 模式（未启用时为空串）。
    pub(crate) fn command_active_mode(&self) -> String {
        self.prompt.active_mode().to_string()
    }

    /// 把当前提示词装配结果（system prompt + 上下文消息）下发内核。
    pub(super) fn push_prompt_to_kernel(&mut self) -> Result<(), String> {
        let session_id = self.current_session_id();
        let settings = SessionModelSettings {
            system_prompt: Some(self.prompt.system_prompt()),
            context_messages: Some(self.prompt.context_messages_with_plugins(
                !self.registry.declarations().is_empty(),
                Some(self.plugins.as_ref()),
                (!session_id.is_empty()).then_some(session_id.as_str()),
                None,
            )?),
            ..SessionModelSettings::default()
        };
        let params = SessionSettingsParams {
            model: Some(Box::new(settings)),
            compaction: None,
        };
        let id = self.send(Command::SessionSettings(Box::new(params)));
        // 模式提示词已在本进程生效（下一轮请求就带上）；内核拒绝时如实补一句，
        // 不能因为「配置已生效」就把拒绝吞掉。
        self.prompt_request = Some(id);
        Ok(())
    }

    /// 当前模型标识：命中 models.toml 的自定义模型时给 key，否则给真实 model_id
    /// （与 Python `current_model()` 同口径）。
    pub(crate) fn command_current_model(&self) -> String {
        let environment = ConfigEnvironment::from_process();
        if let Ok(store) = load_model_store(&environment, None) {
            if let Some(record) = store
                .models
                .iter()
                .find(|record| record.model_id == self.llm.model)
            {
                return record.key.clone();
            }
        }
        self.llm.model.clone()
    }

    /// 把选择解析成新的模型视图；解析失败即报错，不做静默降级。
    pub(crate) fn command_resolve_model(&self, selection: &str) -> Result<LlmConfig, AgentError> {
        let environment = ConfigEnvironment::from_process();
        apply_model_selection(&environment, &self.llm, selection)
            .map_err(|error| AgentError::new(error.message()))
    }

    /// 应用解析好的模型视图：宿主视图＋本会话热更新一并跟上。
    pub(crate) fn command_apply_model_view(&mut self, llm: LlmConfig) {
        self.llm = llm;
        self.state.model = self.llm.model.clone();
        self.push_session_settings_raw(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                model: Some(self.llm.model.clone()),
                provider: Some(self.llm.provider.clone()),
                protocol: Some(self.llm.protocol.clone()),
                base_url: Some(self.llm.base_url.clone()),
                api_key: Some(self.llm.api_key.clone()),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        });
    }

    /// 校验并切换推理强度；返回归一化后的档位（写盘由命令处理器负责）。
    pub(crate) fn command_set_reasoning_effort(
        &mut self,
        effort: &str,
    ) -> Result<String, AgentError> {
        let normalized =
            normalize_reasoning_effort(effort).map_err(|error| AgentError::new(error.message()))?;
        self.llm.reasoning_effort = normalized.to_string();
        self.push_session_settings_raw(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                reasoning_effort: Some(normalized.to_string()),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        });
        Ok(normalized.to_string())
    }

    /// 插件子系统只读状态（与 Python `format_plugins_status` 的两个分支同形）。
    pub(crate) fn command_plugins_status(&self) -> String {
        if !self.plugins.configured() {
            return omnicrawl_controllers::control::plugins_status_text(None);
        }
        let rows: Vec<omnicrawl_controllers::control::PluginWorkerRow> = self
            .plugins
            .status_rows()
            .iter()
            .map(commands::plugin_row)
            .collect();
        omnicrawl_controllers::control::plugins_status_text(Some(&(self.plugins.enabled(), rows)))
    }

    /// 后台 SubAgent 任务的查询与取消：内核持有任务管理器，走一次快速往返。
    pub(crate) fn command_subagent_query(
        &mut self,
        action: &str,
        task_id: &str,
    ) -> Result<Value, String> {
        let params = omnicrawl_ipc::bridge::SubagentQueryParams {
            action: action.to_string(),
            task_id: task_id.to_string(),
        };
        let frame = self.request_kernel(Command::SubagentQuery(params), SUBAGENT_QUERY_TIMEOUT)?;
        Ok(frame.result.clone().unwrap_or(Value::Null))
    }

    // ---------- 会话生命周期（命令层钩子）----------
    //
    // 会话由内核持有，宿主这里是「薄客户端」：每个方法都是一次快速往返加一次视图同步。
    // 往返期间到达的其他帧先进 `deferred_frames`，因此不会丢通知。

    /// 当前会话 id（握手与各会话命令的回执里同步过来）。
    pub(crate) fn current_session_id(&self) -> String {
        self.session_id.clone()
    }

    /// 会话类命令的同步往返：内核只在本地会话目录上读写，毫秒级。
    ///
    /// 超时给得比子任务查询宽：`/resume` 要按转录重建历史，大会话的读盘量不小。
    pub(super) fn session_request(&mut self, command: Command) -> Result<Value, String> {
        let frame = self.request_kernel(command, SESSION_COMMAND_TIMEOUT)?;
        Ok(frame.result.clone().unwrap_or(Value::Null))
    }

    /// 提交 `/sessions`：读最近会话并在输入框上方打开可导航菜单。
    ///
    /// 列表读取失败或没有会话时回退到对话区提示（对映 Python `_show_sessions_menu`）。
    pub(super) fn open_sessions_menu(&mut self) {
        let entries = match self.command_session_list(false, 10) {
            Ok(entries) => entries,
            Err(message) => {
                self.state.notice(format!("会话列表读取失败：{message}"));
                return;
            }
        };
        if entries.is_empty() {
            self.state
                .notice("当前工作区还没有可恢复会话。".to_string());
            return;
        }
        let current = self.session_id.clone();
        let items: Vec<SessionMenuItem> = entries
            .iter()
            .map(|entry| SessionMenuItem {
                session_id: entry.session_id.clone(),
                updated_at: entry
                    .updated_at
                    .with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string(),
                message_count: entry.message_count,
                title: if entry.title.trim().is_empty() {
                    "未命名会话".to_string()
                } else {
                    entry.title.clone()
                },
                is_current: entry.session_id == current,
            })
            .collect();
        self.state.open_sessions_menu(items);
    }

    /// 会话列表（`/sessions` / `/archives`）：顺带把当前会话 id 对齐到内核。
    pub(crate) fn command_session_list(
        &mut self,
        archived: bool,
        limit: usize,
    ) -> Result<Vec<SessionIndexEntry>, String> {
        let params = SessionListParams {
            archived,
            limit: session_limit(limit),
        };
        let result = self.session_request(Command::SessionList(params))?;
        if let Some(current) = result.get("current_session_id").and_then(Value::as_str) {
            self.session_id = current.to_string();
        }
        session_entries(&result)
    }

    /// 重命名当前会话（`/rename`）。
    pub(crate) fn command_session_rename(&mut self, title: &str) -> Result<SessionSummary, String> {
        let params = SessionRenameParams {
            title: title.to_string(),
        };
        let result = self.session_request(Command::SessionRename(params))?;
        session_summary(&result)
    }

    /// 归档当前会话（`/archive`）：内核归档后已自动开好新会话，这里跟上新 id 并清空视图。
    pub(crate) fn command_session_archive(&mut self) -> Result<SessionSummary, String> {
        let result = self.session_request(Command::SessionArchive)?;
        if let Some(new_id) = result.get("new_session_id").and_then(Value::as_str) {
            self.session_id = new_id.to_string();
        }
        // 新会话没有历史：视图清空是「已归档并新开」的如实投影，不只是提示文案。
        self.state.replay_history(&[]);
        session_summary(&result)
    }

    /// 新开会话（`/new`）：返回旧会话 id 之外的新会话 id。
    pub(crate) fn command_session_new(&mut self) -> Result<String, String> {
        let result = self.session_request(Command::SessionNew)?;
        let Some(session_id) = result.get("session_id").and_then(Value::as_str) else {
            return Err("内核没有返回新会话 id。".to_string());
        };
        self.session_id = session_id.to_string();
        self.state.replay_history(&[]);
        Ok(self.session_id.clone())
    }

    /// `/resume`：切换内核会话，并用事件流重放对话视图。
    ///
    /// 先发一条 `session.events` 请内核给出有效事件流（与 Python
    /// `current_session_events()` 同源），读不到时才退回回执里的消息投影。
    pub(crate) fn command_session_resume(
        &mut self,
        session_id: &str,
    ) -> Result<SessionSummary, String> {
        let params = SessionResumeParams {
            session_id: session_id.to_string(),
        };
        let result = self.session_request(Command::SessionResume(params))?;
        if let Some(target) = result.get("session_id").and_then(Value::as_str) {
            self.session_id = target.to_string();
        }
        let history = result
            .get("history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match self.session_events_result() {
            Some(events) => self.state.replay_events(&events),
            None => self.state.replay_history(&history),
        }
        session_summary(&result)
    }

    /// 请内核给出当前会话的有效事件流（`session.events`）；读不到时为 `None`。
    pub(crate) fn session_events_result(&mut self) -> Option<Vec<Value>> {
        let result = self.session_request(Command::SessionEvents).ok()?;
        Some(
            result
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        )
    }

    /// 异步请求一次会话回放（`/undo` 回执与命令层的 `replay_conversation` 标记）。
    ///
    /// `fallback` 是事件流读不到时用的消息投影，`notice` 是回放完成后再进消息流的
    /// 命令提示（顺序不能颠倒：回放会清空消息流）。已经有一条回放在途时不重复下发。
    pub(super) fn request_session_replay(&mut self, fallback: Vec<Value>, notice: Option<String>) {
        if self
            .pending_commands
            .values()
            .any(|pending| matches!(pending, KernelCommand::Replay { .. }))
        {
            // 回放是幂等的整视图重建，在途时用回退投影先把视图对齐，不再排队。
            if !fallback.is_empty() {
                self.state.replay_history(&fallback);
            }
            if let Some(notice) = notice {
                self.state.notice(notice);
            }
            return;
        }
        self.state.set_status(Some("正在重放会话…".to_string()));
        let id = self.send(Command::SessionEvents);
        self.pending_commands
            .insert(id, KernelCommand::Replay { fallback, notice });
    }

    /// 提示历史（`/history`）：只读展示，不注入模型上下文。
    pub(crate) fn command_session_history(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PromptHistoryEntry>, String> {
        let params = SessionHistoryParams {
            query: query.to_string(),
            limit: session_limit(limit),
        };
        let result = self.session_request(Command::SessionHistory(params))?;
        let items = result
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        items
            .iter()
            .map(|value| {
                PromptHistoryEntry::from_dict(value).map_err(|error| error.message().to_string())
            })
            .collect()
    }

    /// 把宿主产生的文本作为 assistant 消息注入内核上下文（`/review` 的报告）。
    pub(crate) fn command_session_append(&mut self, content: &str) -> Result<bool, String> {
        let params = SessionAppendParams {
            role: "assistant".to_string(),
            content: content.to_string(),
        };
        let result = self.session_request(Command::SessionAppend(params))?;
        Ok(result
            .get("appended")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// 下发一条 `session.settings`：设置面板与命令层共用同一条热更新路径。
    pub(super) fn push_session_settings_raw(&mut self, params: SessionSettingsParams) {
        if params.model.is_none() && params.compaction.is_none() {
            return;
        }
        let id = self.send(Command::SessionSettings(Box::new(params)));
        // 命令层不跟踪内生推包的拒绝：设置面板自己会记（见 `settings_request`）。
        let _ = id;
    }
}
