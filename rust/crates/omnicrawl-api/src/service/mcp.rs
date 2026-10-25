//! 由 `service.rs` 按职责拆出，语义未变。

use super::*;

impl AgentService {
        /// MCP 子系统是否启用（管理器缺失按未启用处理）。
        pub fn mcp_enabled(&self) -> bool {
            self.mcp_handle()
                .map(|manager| manager.enabled())
                .unwrap_or(false)
        }

        /// 当前 MCP 管理器句柄：装配时那份，或设置写入后热更新换上的那份。
        pub(super) fn mcp_handle(&self) -> Option<Arc<McpClientManager>> {
            match self.mcp.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }

        /// 当前最新的工具选项：以内核重起点上那份为准，没有时回落启动输入。
        ///
        /// 不能直接用 `self.options.registry_options`（那是启动时的快照）：设置端点在运行期改过
        /// 的字段（如 MCP 句柄）会因此被打回启动值，下一次内核重起就带上过期配置。
        ///
        /// 加锁顺序固定为 runner → spawner（与 [`Self::reload_registry_options`] 一致），
        /// 因此持 runner 锁时调用它安全，不会与另一条路径构成死锁对。
        pub(super) fn current_registry_options(&self) -> RegistryOptions {
            let from_spawner = self.spawner.lock().ok().and_then(|guard| {
                guard
                    .as_ref()
                    .map(|spawner| spawner.registry_options.clone())
            });
            from_spawner.unwrap_or_else(|| self.options.registry_options.clone())
        }

        /// 按最新配置重建工具表并把新声明下发给内核。
        ///
        /// 设置端点写盘之后让运行期跟上的通用入口：`tools` 里的开关决定了哪些工具对模型可见，
        /// 而「模型可见即不可调用」，所以改完必须当场重建，不能等到下次启动。
        ///
        /// 回合在途时按 `try_lock` 快速失败（调用方如实回报，不假装已生效）；嵌入模式
        /// （`with_runner`）没有可重建的运行器，此时只更新下一次内核重起要用的选项。
        pub(super) fn reload_registry_options(
            &self,
            registry_options: &RegistryOptions,
            label: &str,
        ) -> Result<McpReload, ApiError> {
            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，设置要等回合结束后才能生效。"))?;
            let mut requires_restart = true;
            if let Some(runner) = guard.as_mut() {
                runner
                    .rebuild_registry(registry_options)
                    .map_err(|message| {
                        ApiError::new(
                            "SETTINGS_RUNTIME_FAILED",
                            format!("{label}重建工具表失败：{message}"),
                            StatusCode::BAD_GATEWAY,
                            None,
                        )
                    })?;
                let declarations = runner.registry().declarations();
                let params = SessionSettingsParams {
                    model: Some(Box::new(SessionModelSettings {
                        tools: Some(declarations),
                        ..SessionModelSettings::default()
                    })),
                    compaction: None,
                };
                runner
                    .apply_session_settings(params, &mut |_| {})
                    .map_err(|message| {
                        ApiError::new(
                            "SETTINGS_RUNTIME_FAILED",
                            format!("{label}下发工具声明失败：{message}"),
                            StatusCode::BAD_GATEWAY,
                            None,
                        )
                    })?;
                requires_restart = false;
            }
            drop(guard);

            // 下一次内核重起（切会话 / 切工作区）也要用新选项，否则会被启动时的旧值覆盖回去。
            if let Ok(mut spawner) = self.spawner.lock() {
                if let Some(spawner) = spawner.as_mut() {
                    spawner.registry_options = registry_options.clone();
                }
            }
            Ok(McpReload {
                diagnostics: Vec::new(),
                tool_count: 0,
                requires_restart,
            })
        }

        /// `GET /mcp`：管理器缺失时报「未接入」，否则用管理器自己的状态摘要。
        pub fn mcp_status(&self) -> String {
            match self.mcp_handle() {
                Some(manager) => manager.format_status(),
                None => "MCP 未接入：配置读取失败已跳过。".to_string(),
            }
        }

        /// `PUT /settings/mcp` 的运行期一半：按新配置重连 MCP，并把新工具表推给内核。
        ///
        /// 与 Python `agent.apply_mcp_config` 同义（那里 Agent 就在本进程，改配置即重连），
        /// 差别是这里工具表属于宿主、由 `TurnRunner` 持有，所以要多一步「重建工具表 + 把新
        /// 声明下发给内核」。会话不受影响：换的是工具面，不动模型上下文。
        ///
        /// 回合在途时按 `try_lock` 快速失败（与 `apply_session_settings` 同一理由）；嵌入模式
        /// （`with_runner`）没有可重建的运行器，此时只换状态句柄并回报「需重启生效」。
        pub fn reload_mcp(&self, config: omnicrawl_mcp::McpConfig) -> Result<McpReload, ApiError> {
            let workspace = self.workspace_root();
            let manager = Arc::new(McpClientManager::new(config, workspace));
            manager.discover();
            let diagnostics: Vec<String> = manager
                .diagnostics()
                .iter()
                .map(|diagnostic| {
                    let server = diagnostic.server_name.clone().unwrap_or_default();
                    format!(
                        "[{}] {}: {}",
                        diagnostic.severity, server, diagnostic.message
                    )
                })
                .collect();
            let tool_count = manager.tools().len();

            let mut guard = self
                .runner
                .try_lock()
                .map_err(|_| kernel_busy("生成任务进行中，MCP 设置要等回合结束后才能生效。"))?;
            let mut requires_restart = true;
            if let Some(runner) = guard.as_mut() {
                let mut registry_options = self.current_registry_options();
                registry_options.mcp = Some(Arc::clone(&manager));
                runner
                    .rebuild_registry(&registry_options)
                    .map_err(|message| {
                        ApiError::new(
                            "MCP_RUNTIME_FAILED",
                            format!("重建工具表失败：{message}"),
                            StatusCode::BAD_GATEWAY,
                            None,
                        )
                    })?;
                // 新声明整体替换内核侧的工具表：模型下一轮才看得到新增/消失的 MCP 能力。
                let declarations = runner.registry().declarations();
                let params = SessionSettingsParams {
                    model: Some(Box::new(SessionModelSettings {
                        tools: Some(declarations),
                        ..SessionModelSettings::default()
                    })),
                    compaction: None,
                };
                runner
                    .apply_session_settings(params, &mut |_| {})
                    .map_err(|message| {
                        ApiError::new(
                            "MCP_RUNTIME_FAILED",
                            format!("下发工具声明失败：{message}"),
                            StatusCode::BAD_GATEWAY,
                            None,
                        )
                    })?;
                requires_restart = false;
            }
            drop(guard);

            // 旧连接先关（先建新的、再关旧的，失败路径上不会出现「无 MCP」窗口）。
            if let Some(previous) = self
                .mcp
                .lock()
                .map(|mut guard| guard.replace(Arc::clone(&manager)))
                .unwrap_or(None)
            {
                previous.close();
            }
            // 下一次内核重起（切会话 / 切工作区）也要用新配置，否则会被旧句柄覆盖回去。
            if let Ok(mut spawner) = self.spawner.lock() {
                if let Some(spawner) = spawner.as_mut() {
                    spawner.registry_options.mcp = Some(manager);
                }
            }
            Ok(McpReload {
                diagnostics,
                tool_count,
                requires_restart,
            })
        }

        /// `PUT /settings/tools` 的运行期一半：按新开关重建工具表并把新声明下发给内核。
        ///
        /// 与 [`Self::reload_mcp`] 同一套事务：配置已由路由写盘，这里负责让运行期跟上——
        /// 否则「设置里关掉的工具」要等下次启动才真的从模型可见面里消失。
        ///
        /// 回合在途时按 `try_lock` 快速失败；嵌入模式（`with_runner`）没有可重建的运行器，
        /// 此时只更新下一次内核重起要用的选项并回报「需重启生效」。
        pub fn reload_tool_switches(&self, disabled_tools: Vec<String>) -> Result<McpReload, ApiError> {
            let mut registry_options = self.current_registry_options();
            registry_options.disabled_tools = disabled_tools;
            self.reload_registry_options(&registry_options, "工具开关")
        }

}
