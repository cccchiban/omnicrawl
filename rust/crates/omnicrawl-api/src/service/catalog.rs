//! 由 `service.rs` 按职责拆出，语义未变。

use super::*;

impl AgentService {
        pub(super) fn project_store(&self) -> Result<ProjectStore, ApiError> {
            let store = ProjectStore::open(self.session_root()?);
            store.ensure().map_err(session_store_error)?;
            Ok(store)
        }

        /// 启用主 Agent 模式（`/plan`）：重算 system prompt 与上下文消息后下发内核。
        ///
        /// 模式提示词由宿主装配（模板 + AGENTS.md + Skill），因此这里必须与握手走同一份装配
        /// 结果：先重建 [`PromptRuntime`] 再交给运行器下发。
        pub fn activate_mode(&self, mode: &str) -> Result<String, ApiError> {
            self.rebuild_prompt(mode).map(|(_, activated)| activated)
        }

        /// 关闭当前模式（`/speaking-style off`）：重算并下发不带模式区块的提示词。
        ///
        /// 返回被关闭的模式标识；本来就未启用时为空串（仍然下发一次，保持内核状态一致）。
        pub fn deactivate_mode(&self) -> Result<String, ApiError> {
            self.rebuild_prompt("").map(|(previous, _)| previous)
        }

        /// 当前已启用的模式标识；未装配提示词时为空串。
        pub fn active_mode(&self) -> String {
            self.runner
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().and_then(|runner| runner.prompt()))
                .map(|prompt| prompt.active_mode().to_string())
                .unwrap_or_default()
        }

        /// 以当前装配为底重建提示词并下发；`mode` 为空串表示不带模式区块。
        ///
        /// 返回 `(切换前的模式, 切换后的模式)`；第一次查询与重建在同一把锁内完成，
        /// 避免调用方为了读旧状态再取一次锁。
        pub(super) fn rebuild_prompt(&self, mode: &str) -> Result<(String, String), ApiError> {
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能切换模式。"))?;
            let Some(runner) = guard.as_mut() else {
                return Err(ApiError::service_unavailable());
            };
            let Some(current) = runner.prompt() else {
                return Err(ApiError::new(
                    "PROMPT_DISABLED",
                    "提示词装配未启用，无法切换模式。",
                    StatusCode::SERVICE_UNAVAILABLE,
                    None,
                ));
            };
            let previous = current.active_mode().to_string();
            // 以当前装配为底重建：模板、工作区与 Skill 目录都不变，只换活动模式。
            let mut options = PromptOptions::new(self.workspace_root());
            options.agent_temp_dir =
                agent_temp_dir_display(&self.options.env, &self.workspace_root());
            options.workspace_detection_summary =
                detect_project_context(&self.options.env, None).detection_summary();
            options.system_prompt_override = None;
            let advisor =
                omnicrawl_config::features::advisor::load_advisor_config(&self.options.env, None)
                    .unwrap_or_default();
            options.advisor_active = advisor.enabled;
            options.advisor_blacklisted = advisor
                .disabled_for_models
                .iter()
                .any(|name| *name == self.current_model());
            options.mode = mode.trim().to_string();
            let rebuilt = PromptRuntime::load(&self.options.env, options)
                .map_err(|error| prompt_failed(error.to_string()))?;
            let activated = mode.trim().to_string();
            runner
                .apply_prompt(Arc::new(rebuilt), &mut |_| {})
                .map_err(kernel_command_failed)?;
            Ok((previous, activated))
        }

        /// `POST /sessions/current/compact`：显式压缩当前会话，返回内核回执。
        pub fn compact_current_session(&self) -> Result<Value, ApiError> {
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能压缩会话。"))?;
            match guard.as_mut() {
                Some(runner) => runner.compact_session().map_err(kernel_command_failed),
                None => Err(ApiError::service_unavailable()),
            }
        }

        /// `GET /models`：当前 Provider 的模型列表，保证当前模型一定在列表里。
        ///
        /// 端点、请求头与超时由 `omnicrawl-config` 构造，这里只提供发请求的端口。
        pub fn model_options(&self) -> Result<Vec<ModelOption>, ApiError> {
            let llm = load_llm_config(self.environment()).map_err(model_list_failed)?;
            let options = detect_model_options(
                &llm,
                MODEL_LIST_TIMEOUT_SECONDS,
                &model_discovery::fetch_model_list,
            )
            .map_err(model_list_failed)?;
            Ok(ensure_current_model_option(&options, &self.current_model()))
        }

        /// 双列目录；`refresh=true` 时先清空发现缓存再重建（`POST /models/refresh`）。
        pub fn model_catalog(&self, refresh: bool) -> Result<Catalog, ApiError> {
            let env = self.environment();
            let llm = load_llm_config(env).map_err(model_catalog_failed)?;
            if refresh {
                self.options.discovery_cache.clear();
            }
            let request = CatalogRequest {
                config: Some(&llm),
                refresh,
                ..CatalogRequest::default()
            };
            let ports = CatalogPorts {
                discover: &model_discovery::discover_profile,
                cache: &self.options.discovery_cache,
            };
            build_catalog(env, &request, &ports).map_err(model_catalog_failed)
        }

        pub fn list_projects(&self) -> Result<Vec<ProjectEntry>, ApiError> {
            self.project_store()?
                .list_projects()
                .map_err(session_store_error)
        }

        /// `GET /projects/overview`：显式项目 + 会话索引稳定目录的只读聚合。
        pub fn project_overview(&self) -> Result<Vec<ProjectOverview>, ApiError> {
            let sessions = self
                .session_store()?
                .list_sessions_filtered(&SessionListQuery {
                    limit: 100,
                    include_archived: true,
                    ..SessionListQuery::default()
                })
                .map_err(session_store_error)?;
            let entries: Vec<OverviewSessionEntry> = sessions
                .iter()
                .map(|entry| OverviewSessionEntry {
                    workspace_root: Some(entry.workspace_root.clone()),
                    updated_at: Some(entry.updated_at),
                    session_id: entry.session_id.clone(),
                    title: entry.title.clone(),
                })
                .collect();
            self.project_store()?
                .project_overview(&entries, None, 0)
                .map_err(session_store_error)
        }

        /// 创建项目：`path` 为空时落在工作区下按展示名派生的目录（与 Python 同规则）。
        pub fn create_project(&self, name: &str, path: &str) -> Result<ProjectEntry, ApiError> {
            let raw = path.trim();
            let resolved = if raw.is_empty() {
                self.workspace_root()
                    .join(project_directory_name(name))
                    .to_string_lossy()
                    .to_string()
            } else {
                raw.to_string()
            };
            self.project_store()?
                .create_project(name, &resolved, Some(utc_now()))
                .map_err(session_store_error)
        }

        pub fn import_project(&self, name: &str, path: &str) -> Result<ProjectEntry, ApiError> {
            self.project_store()?
                .import_project(name, path, Some(utc_now()))
                .map_err(session_store_error)
        }

        pub fn rename_project(&self, path: &str, name: &str) -> Result<ProjectEntry, ApiError> {
            self.project_store()?
                .rename_project(path, name, Some(utc_now()))
                .map_err(session_store_error)
        }

        pub fn pin_project(&self, path: &str, pinned: bool) -> Result<ProjectEntry, ApiError> {
            self.project_store()?
                .pin_project(path, pinned, Some(utc_now()))
                .map_err(session_store_error)
        }

        pub fn remove_project(&self, path: &str) -> Result<(), ApiError> {
            self.project_store()?
                .remove_project(path)
                .map_err(session_store_error)
        }

        /// `GET /history`：提示历史按时间倒序去重；`current_session_only` 只看当前会话。
        pub fn search_prompt_history(
            &self,
            query: &str,
            limit: i64,
            current_session_only: bool,
        ) -> Result<Vec<PromptHistoryEntry>, ApiError> {
            let session_id = if current_session_only {
                Some(self.current_session_id())
            } else {
                None
            };
            self.session_store()?
                .prompt_history()
                .search(None, session_id.as_deref(), query, limit)
                .map_err(session_store_error)
        }

        /// `GET /skills`：按工作区重新发现 Skill 索引（与 TUI 同源，不缓存到进程）。
        pub fn skills(&self) -> Vec<Map<String, Value>> {
            if let Some(prompt) = self.options.prompt.as_ref() {
                return prompt
                    .skill_metas()
                    .iter()
                    .map(|meta| meta.to_dict())
                    .collect();
            }
            let mut manager = SkillManager::new();
            let root = self.workspace_root();
            manager.discover(Some(root.as_path()), &self.skill_extra_paths());
            manager
                .list_all()
                .iter()
                .map(|meta| meta.to_dict())
                .collect()
        }

        /// `[skills].extra_paths`：读不到配置时按空处理（发现仍然可用）。
        pub(super) fn skill_extra_paths(&self) -> Vec<String> {
            omnicrawl_config::features::skills::load_skills_config(&self.options.env, None)
                .map(|config| config.extra_paths)
                .unwrap_or_default()
        }

        /// `POST /memory/clean`：清理项目级、当前会话级与用户级的过期记忆。
        pub fn clean_memory(&self) -> Result<Vec<String>, ApiError> {
            let user_root = user_config_dir(&self.options.env)
                .parent()
                .map(|home| home.join(".omnicrawl"))
                .unwrap_or_else(|| PathBuf::from(".omnicrawl"));
            let stores = [
                (
                    "project",
                    self.workspace_root().join(".omnicrawl/.oclmemory"),
                ),
                (
                    "session",
                    user_root
                        .join("Session_memory")
                        .join(self.current_session_id()),
                ),
                ("user", user_root.join("User_memory")),
            ];
            let mut deleted: Vec<String> = Vec::new();
            for (scope, root) in stores {
                let store = MemoryStore::open(root);
                let removed = store
                    .clean_expired_memories()
                    .map_err(session_store_error)?;
                deleted.extend(removed.into_iter().map(|path| format!("{scope}:{path}")));
            }
            Ok(deleted)
        }

}
