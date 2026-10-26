//! 由 `service.rs` 按职责拆出，语义未变。

use super::*;

impl AgentService {
        /// 会话存储根；未启用会话系统时报 503。
        pub(super) fn session_root(&self) -> Result<&Path, ApiError> {
            self.options
                .session_root
                .as_deref()
                .ok_or_else(session_disabled)
        }

        pub(super) fn session_store(&self) -> Result<SessionStore, ApiError> {
            let store = SessionStore::open(self.session_root()?.to_path_buf());
            store.ensure().map_err(session_store_error)?;
            Ok(store)
        }

        /// `GET /sessions`：`archived` 为真时只看归档，否则只看活跃（归档默认隐藏）。
        pub fn list_sessions(
            &self,
            limit: usize,
            archived: bool,
        ) -> Result<Vec<SessionIndexEntry>, ApiError> {
            let query = SessionListQuery {
                workspace_root: None,
                project_path: None,
                limit,
                include_archived: false,
                archived_only: archived,
            };
            self.session_store()?
                .list_sessions_filtered(&query)
                .map_err(session_store_error)
        }

        /// `GET /sessions/{id}/events`：转录里的有效事件（坏行记诊断后跳过）。
        pub fn session_events(&self, session_id: &str) -> Result<Vec<SessionEvent>, ApiError> {
            self.session_store()?
                .read_session_events_with_diagnostics(session_id)
                .map_err(session_store_error)
                .map(|result| result.events)
        }

        /// 会话与提示历史的损坏诊断；`session_id` 为空时只报提示历史总览。
        pub fn session_diagnostics(&self, session_id: Option<&str>) -> Result<Value, ApiError> {
            let store = self.session_store()?;
            let (_entries, prompt_diagnostics) = store
                .prompt_history()
                .read_entries_with_diagnostics()
                .map_err(session_store_error)?;
            let prompt_payload: Vec<Value> = prompt_diagnostics
                .iter()
                .map(SessionRecordDiagnostic::to_dict)
                .collect();
            let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
                return Ok(json!({
                    "session_id": Value::Null,
                    "event_diagnostics": [],
                    "prompt_history_diagnostics": prompt_payload,
                }));
            };
            let result = store
                .read_session_events_with_diagnostics(session_id)
                .map_err(session_store_error)?;
            Ok(json!({
                "session_id": session_id,
                "event_count": result.events.len(),
                "event_diagnostics": result.diagnostics.iter().map(SessionRecordDiagnostic::to_dict).collect::<Vec<Value>>(),
                "prompt_history_diagnostics": prompt_payload,
                "has_errors": result.has_errors(),
            }))
        }

        /// `PATCH /sessions/current`：重命名当前会话。
        pub fn rename_current_session(&self, title: &str) -> Result<SessionIndexEntry, ApiError> {
            let store = self.session_store()?;
            store
                .rename_session(&self.current_session_id(), title, utc_now())
                .map_err(session_store_error)
        }

        /// `DELETE /sessions/{id}`：当前活跃会话不允许删除。
        pub fn delete_session(&self, session_id: &str) -> Result<(), ApiError> {
            if session_id.trim() == self.current_session_id().trim() {
                return Err(ApiError::new(
                    "SESSION_ACTIVE",
                    "不能删除当前活跃会话，请先切换到其他会话。",
                    StatusCode::CONFLICT,
                    None,
                ));
            }
            self.session_store()?
                .delete_session(session_id)
                .map_err(session_store_error)
        }

        /// `POST /sessions/current/export`：把客户端渲染好的 Markdown 落进 `exports/`。
        pub fn export_current_session_markdown(&self, markdown: &str) -> Result<PathBuf, ApiError> {
            let store = self.session_store()?;
            store
                .export_session_markdown(&self.current_session_id(), markdown, utc_now())
                .map_err(session_store_error)
        }

        pub fn read_session_artifact_text(
            &self,
            session_id: &str,
            artifact_path: &str,
        ) -> Result<String, ApiError> {
            self.session_store()?
                .read_artifact_text(session_id, artifact_path)
                .map_err(session_store_error)
        }

        /// 重起内核并切到指定会话；空 id 表示开新会话，返回内核报出的会话 id。
        ///
        /// 协议 v1 没有「换会话」命令，而内核的 `KernelSession::open` 已经支持按 id 续跑，
        /// 因此宿主侧的等价做法是重起内核并把目标会话写进 `initialize`。会话目录不随工作区
        /// 变化（与 Python 一致），所以工作区切换保持同一会话。
        pub fn switch_session(
            &self,
            session_id: &str,
            workspace_root: Option<&Path>,
        ) -> Result<String, ApiError> {
            let mut runner_guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能切换会话。"))?;
            let mut spawner_guard = self
                .spawner
                .lock()
                .map_err(|_| kernel_restart_unavailable())?;
            let Some(spawner) = spawner_guard.as_mut() else {
                return Err(kernel_restart_unavailable());
            };

            // 旧内核先收干净：进程、工具表与后台任务都要在新内核起来之前让出资源。
            if let Some(runner) = runner_guard.as_mut() {
                runner.shutdown();
            }
            *runner_guard = None;

            let workspace = workspace_root
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.workspace_root());
            let session_root = spawner
                .runner
                .session
                .as_ref()
                .map(|session| session.root.clone())
                .or_else(|| {
                    self.options
                        .session_root
                        .as_ref()
                        .map(|root| root.to_string_lossy().to_string())
                })
                .unwrap_or_default();
            spawner.runner.workspace_root = workspace.clone();
            // 提示词跟着工作区走：模板不变，但 AGENTS.md 与 Skill 目录都在工作区里，
            // 不重算就会把旧工作区的规范继续发给模型。
            if let Some(prompt) = spawner.runner.prompt.as_ref() {
                let model_name = spawner.runner.model.model.clone();
                let env = self.options.env.clone();
                match build_prompt_runtime(&env, &workspace, &model_name) {
                    Some(rebuilt) => spawner.runner.prompt = Some(rebuilt),
                    None => {
                        // 装配失败保持旧运行期：它至少还能发模板正文，比整段丢空强。
                        eprintln!("[api] 切换工作区后提示词装配失败，沿用上一份（不含新工作区规范）");
                        let _ = prompt;
                    }
                }
            }
            // 重起内核的会话块与启动时同源：漏了这两项就会悄悄回到内核默认策略。
            let (memory_root, compaction) = session_config_defaults(&self.options);
            spawner.runner.session = Some(KernelSessionConfig {
                root: session_root,
                session_id: session_id.to_string(),
                memory_root,
                workspace_root: Some(workspace.to_string_lossy().to_string()),
                compaction,
            });

            let ready = Arc::new(Mutex::new(String::new()));
            let slot = Arc::clone(&ready);
            let client =
                KernelClient::spawn_with_stderr(&spawner.program, move |line| {
                    if let Some(found) = parse_session_ready(line) {
                        if let Ok(mut guard) = slot.lock() {
                            *guard = found;
                        }
                    }
                })
                .map_err(|error| kernel_restart_failed(format!("启动内核进程失败：{error}")))?;
            let mut runner = TurnRunner::new(client, spawner.runner.clone(), &spawner.registry_options)
                .map_err(kernel_restart_failed)?;
            runner
                .handshake(&mut |_| {})
                .map_err(|error| kernel_restart_failed(format!("内核握手失败：{error}")))?;
            let opened = wait_for_session_id(&ready);
            *runner_guard = Some(runner);
            drop(spawner_guard);
            drop(runner_guard);

            // 新会话 ID 由内核经 stderr 报出；等不到就沿用请求值（ID 未知不影响回合）。
            let resolved = if opened.is_empty() {
                session_id.to_string()
            } else {
                opened
            };
            if let Ok(mut guard) = self.session_id.lock() {
                *guard = resolved.clone();
            }
            if workspace_root.is_some() {
                if let Ok(mut guard) = self.workspace.lock() {
                    *guard = workspace;
                }
                // 临时目录跟着工作区走：旧句柄盯的是旧根，先停掉再按新根重建。
                self.install_agent_temp();
            }
            Ok(resolved)
        }

        /// 会话状态视图：索引元数据 + 转录投影 + 运行守护残留（对应 Python `SessionState`）。
        ///
        /// 恢复类端点（`POST /sessions/{id}/resume`）用它回 `pending_user_text` 与 `todo_items`，
        /// 让客户端能在暂停后继续任务。
        pub fn session_state_payload(&self, session_id: &str) -> Result<Value, ApiError> {
            let store = self.session_store()?;
            let entry = store
                .list_sessions()
                .map_err(session_store_error)?
                .into_iter()
                .find(|item| item.session_id == session_id.trim())
                .ok_or_else(|| {
                    ApiError::new(
                        "SESSION_NOT_FOUND",
                        format!("未找到会话：{}", session_id.trim()),
                        StatusCode::NOT_FOUND,
                        None,
                    )
                })?;
            let events = store
                .read_events(&entry.session_id)
                .map_err(session_store_error)?;
            let (pending_user_text, todo_items) = recover_run_guard_state(&events);
            let messages: Vec<Value> = project_session_history(&events)
                .into_iter()
                .map(|(_role, message)| message)
                .collect();
            Ok(json!({
                "session_id": entry.session_id,
                "title": entry.title,
                "workspace_root": entry.workspace_root,
                "path": entry.path,
                "created_at": format_datetime(entry.created_at),
                "updated_at": format_datetime(entry.updated_at),
                "messages": messages,
                "last_event_type": entry.last_event_type,
                "event_count": entry.event_count,
                "archived_at": entry.archived_at.map(format_datetime),
                "pending_user_text": pending_user_text,
                "todo_items": todo_items,
            }))
        }

        /// `POST /sessions/current/archive`：把当前会话移进归档，返回归档后的索引条目。
        pub fn archive_current_session(&self) -> Result<SessionIndexEntry, ApiError> {
            let store = self.session_store()?;
            store
                .archive_session(&self.current_session_id(), utc_now())
                .map_err(session_store_error)
        }

        /// 撤销最近一轮（连接器 `/undo`）：整轮回退由内核执行，会改会话与工作区。
        pub fn undo_current_session(&self) -> Result<Value, ApiError> {
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能回退会话。"))?;
            match guard.as_mut() {
                Some(runner) => runner.undo_turn().map_err(kernel_command_failed),
                None => Err(ApiError::service_unavailable()),
            }
        }

        /// 把一段 assistant 文本追加进内核会话历史（`/review` 的报告注入）。
        ///
        /// 与 `session.append` 同口径：只允许 assistant 角色，空内容不注入。
        pub fn append_assistant_text(&self, content: &str) -> Result<bool, ApiError> {
            if content.trim().is_empty() {
                return Ok(false);
            }
            let params = omnicrawl_ipc::bridge::SessionAppendParams {
                role: "assistant".to_string(),
                content: content.to_string(),
            };
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，暂不能写入上下文。"))?;
            match guard.as_mut() {
                Some(runner) => {
                    let value = runner
                        .request_value(
                            omnicrawl_ipc::bridge::Command::SessionAppend(params),
                            "上下文追加",
                            &mut |_| {},
                        )
                        .map_err(kernel_command_failed)?;
                    Ok(value
                        .get("appended")
                        .and_then(Value::as_bool)
                        .unwrap_or(false))
                }
                None => Err(ApiError::service_unavailable()),
            }
        }

}
