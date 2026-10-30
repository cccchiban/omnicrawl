//! 工作区切换与慢命令：装配候选子系统、排空子 Agent、提交新状态。
//!
//! 事务性约定：候选子系统先在本地装配，装配通过后才提交；任一步失败都不动当前
//! 工作区与工具表，候选 MCP/后台任务在失败路径上主动 close，不留孤儿进程。

use super::*;

impl App {
    /// 运行中切换工作区（`/workspace <路径>` 的同步入口）。
    ///
    /// 窄口径对映 Python `WorkspaceSwitchingMixin.switch_workspace` 的主体：
    /// 解析校验 → 子 Agent 排空（仍有活跃任务且超期时拒绝）→ pending worktree 拦阻
    /// → 准备新子系统（工具表 / MCP 连接 / 提示词运行时）→
    /// `workspace.switch.before`（拒绝即中止）→ 收尾旧资源（旧工作区的 MCP 与
    /// 后台任务）→ 提交新状态 → 内核侧同一会话补写 `workspace_switched`。
    ///
    /// TUI 的 `/workspace` 走 [`Self::start_workspace_switch`]（装配在工作线程，界面
    /// 不冻）；这一条是同步实现，供不接管慢命令的宿主（测试替身等）使用。
    ///
    /// 事务性：候选子系统先在本地装配，装配成功后才提交；任一步失败都不动当前工作区
    /// 与工具表（候选 MCP/后台任务在失败路径上主动 close，不留孤儿进程）。
    pub(crate) fn command_switch_workspace(&mut self, path: &str) -> Result<PathBuf, String> {
        let Some(new_root) = self.workspace_switch_target(path)? else {
            // 目标就是当前工作区：Python 同样直接返回，不重建、不转录事件。
            let current = self.workspace.clone();
            return Ok(current);
        };
        let (registry_options, options, environment, timeout) =
            self.workspace_prepare_inputs(&new_root);
        let prepared =
            match prepare_workspace(&environment, &options, &new_root, registry_options, timeout) {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.notify_workspace_switch_error(&new_root, &error);
                    return Err(error);
                }
            };
        self.commit_workspace_switch(&new_root, path, prepared)
    }

    /// `/workspace <路径>` 的异步入口（宿主接管慢命令）：装配在工作线程，提交在主线程。
    ///
    /// 对映 Python 把延迟执行体交给工作线程：前置检查是几次本地内核往返（毫秒级，留在
    /// 主线程），真正慢的一段（MCP 连接与发现、工具表与提示词运行时重建）在线程里跑。
    pub(super) fn start_workspace_switch(&mut self, path: &str) {
        if let Some(task) = self.slow_task.as_ref() {
            self.state.notice(format!(
                "上一条慢命令（{}）还在执行中，稍候再试。",
                task.kind.working_status()
            ));
            return;
        }
        let new_root = match self.workspace_switch_target(path) {
            Ok(Some(root)) => root,
            Ok(None) => {
                let current = self.workspace.clone();
                self.state
                    .notice(workspace_switch_success_message(&current));
                return;
            }
            Err(message) => {
                self.state.notice(message);
                return;
            }
        };
        let (registry_options, options, environment, timeout) =
            self.workspace_prepare_inputs(&new_root);
        let requested = path.trim().to_string();
        let thread_root = new_root.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let prepared = prepare_workspace(
                &environment,
                &options,
                &thread_root,
                registry_options,
                timeout,
            );
            let _ = sender.send(SlowOutcome::WorkspacePrepared {
                root: thread_root,
                requested,
                prepared,
            });
        });
        self.state.set_status(Some(
            SlowTaskKind::WorkspaceSwitch.working_status().to_string(),
        ));
        self.state
            .notice(WORKSPACE_SWITCH_PENDING_MESSAGE.to_string());
        self.slow_task = Some(SlowTask {
            kind: SlowTaskKind::WorkspaceSwitch,
            outcome: receiver,
        });
    }

    /// `/mcp` 的异步入口：状态文本在工作线程读（`format_status` 会触发 MCP 发现与连接）。
    pub(super) fn start_mcp_status(&mut self) {
        if let Some(task) = self.slow_task.as_ref() {
            self.state.notice(format!(
                "上一条慢命令（{}）还在执行中，稍候再试。",
                task.kind.working_status()
            ));
            return;
        }
        let manager = self.registry.mcp().cloned();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            // MCP 清单由宿主工具表持有连接；未配置时给一句明确说明，而不是空串。
            let text = match manager {
                Some(manager) => manager.format_status(),
                None => {
                    "MCP 未配置：在 config.toml 的 [mcp] 段添加 Server 后可查看详情。".to_string()
                }
            };
            let _ = sender.send(SlowOutcome::McpStatus(text));
        });
        self.state
            .set_status(Some(SlowTaskKind::McpStatus.working_status().to_string()));
        self.slow_task = Some(SlowTask {
            kind: SlowTaskKind::McpStatus,
            outcome: receiver,
        });
    }

    /// 每帧轮询慢命令的产出：回填消息、收起状态行，工作区切换在这里提交。
    pub fn tick_slow_command(&mut self) {
        let received = match self.slow_task.as_ref() {
            Some(task) => task.outcome.try_recv(),
            None => return,
        };
        let outcome = match received {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // 线程崩了：不能把状态行永远留在「正在准备…」。
                self.slow_task = None;
                self.state.set_status(None);
                self.state
                    .notice("慢命令后台任务意外结束，请重试。".to_string());
                return;
            }
        };
        self.slow_task = None;
        self.state.set_status(None);
        match outcome {
            SlowOutcome::McpStatus(text) => self.state.notice(text),
            SlowOutcome::WorkspacePrepared {
                root,
                requested,
                prepared,
            } => match prepared {
                Ok(prepared) => {
                    if let Err(message) = self.commit_workspace_switch(&root, &requested, prepared)
                    {
                        self.state.notice(message);
                    }
                }
                Err(message) => {
                    // 装配阶段失败：如实回报，不动当前运行态。
                    // `_prepare_workspace_switch` 的 except），旧工作区一律不动。
                    self.notify_workspace_switch_error(&root, &message);
                    self.state.notice(message);
                }
            },
        }
    }

    /// 退出前收尾在途的慢命令：短暂等一次结果，拿到的候选子系统要关干净。
    ///
    /// 不等就会把候选 MCP 子进程变成孤儿（线程随进程退出而消失，候选资源无人 close）。
    pub fn settle_slow_task(&mut self) {
        let Some(task) = self.slow_task.take() else {
            return;
        };
        if let Ok(SlowOutcome::WorkspacePrepared {
            prepared: Ok(prepared),
            ..
        }) = task
            .outcome
            .recv_timeout(Duration::from_millis(SLOW_TASK_SETTLE_MS))
        {
            prepared.registry.close_monitors();
            prepared.registry.close_mcp();
        }
    }

    /// `/workspace` 的公共前置：回合门禁 → 解析校验 → 子 Agent 排空 → worktree 拦阻。
    ///
    /// 返回 `Ok(None)` 表示目标就是当前工作区（调用方只需回一句成功文案）。
    pub(super) fn workspace_switch_target(&mut self, path: &str) -> Result<Option<PathBuf>, String> {
        // 回合进行中不切换：本轮的工作区快照与工具批次已经按旧根展开。
        if self.state.turn.is_running() {
            return Err(
                "工作区切换失败：当前回合仍在执行，请等它结束或先取消后再切换。".to_string(),
            );
        }
        let new_root = resolve_switch_target(path).map_err(|error| error.message().to_string())?;
        if same_directory(&new_root, &self.workspace) {
            return Ok(None);
        }
        // 前置检查（对映 Python 的 `coordinator.cancel_and_wait` 与
        // `list_subagent_worktrees`）：先排空活跃子任务，再有未处理的 worktree 就拒绝。
        self.drain_subagents_before_switch()?;
        let pending_worktrees = self.pending_subagent_worktrees()?;
        if !pending_worktrees.is_empty() {
            return Err(pending_worktrees_error(&pending_worktrees)
                .message()
                .to_string());
        }
        Ok(Some(new_root))
    }

    /// 装配候选子系统所需的输入（同步路径与工作线程共用同一口径）。
    pub(super) fn workspace_prepare_inputs(
        &self,
        root: &Path,
    ) -> (RegistryOptions, Options, ConfigEnvironment, i64) {
        let mut options = self.registry_options.clone();
        // 旧工作区的后台任务不带进新工作区（与 Python 关闭旧 MonitorManager 同义）：
        // 置空让 `ToolRegistry::new` 按新根建一个新管理器，避免把旧工作区的
        // monitor 进程记到新工作区账上。取消令牌与工作区无关，照旧沿用。
        options.monitors = None;
        options.cancel = Some(self.registry.cancel_token());
        options.mcp = mcp_manager(root);
        (
            options,
            self.options.clone(),
            ConfigEnvironment::from_process(),
            self.options.command_timeout_seconds,
        )
    }

    /// 提交候选子系统：收尾旧资源 → 换状态 → 内核下发。
    ///
    /// 失败路径上候选 MCP/后台任务主动 close，不留孤儿进程；提交开始后不再有可失败步骤。
    pub(super) fn commit_workspace_switch(
        &mut self,
        new_root: &Path,
        requested: &str,
        prepared: PreparedWorkspace,
    ) -> Result<PathBuf, String> {
        let PreparedWorkspace {
            registry: candidate,
            prompt,
        } = prepared;
        let mut options = self.registry_options.clone();
        options.monitors = None;
        options.cancel = Some(self.registry.cancel_token());
        options.mcp = mcp_manager(new_root);
        // —— 提交：以下不再有可失败步骤 ——
        // 先暂停轮询并废弃旧工作区的游标（任务 ID 可能在新工作区复用），
        // 提交完再恢复；旧后台任务随旧工具表一起关闭。
        self.monitor_state.suspend_for_workspace_switch();
        self.registry.close_monitors();
        self.registry.close_mcp();
        self.workspace = new_root.to_path_buf();
        // 临时目录跟着工作区走：旧句柄的清理线程盯的是旧根，先停掉再按新根重建。
        if let Some(installation) = self.agent_temp.as_mut() {
            installation.close();
        }
        self.agent_temp =
            omnicrawl_workspace::install_agent_temp_workspace(&ConfigEnvironment::from_process(), new_root);
        if let Some(installation) = self.agent_temp.as_ref() {
            if let Some(note) = installation.note.as_ref() {
                self.state
                    .notice(format!("新工作区的临时目录未就绪：{note}"));
            }
        }
        self.registry_options = options;
        self.registry = Arc::new(candidate);
        self.prompt = prompt;
        self.refresh_command_menu();
        self.monitor_state.resume_polling();
        if let Some(settings) = self.settings.as_mut() {
            settings.refresh_tools(tool_switch_rows(&self.registry));
        }
        // 跳进程同步：把新工作区写回 config.toml（远程入口在任务开始前重读并跟随）；
        // 失败只提示不阻断，与命令层 `persist_workspace_root` 同口径。
        if let Err(error) = save_workspace_root(
            &ConfigEnvironment::from_process(),
            Path::new(requested),
            None,
        ) {
            self.state.notice(format!(
                "工作区持久化到 config.toml 失败：{}",
                error.message()
            ));
        }
        // 内核侧：先把新工具表与上下文消息推给内核（与新工作区的稳定前缀一致），
        // 再让它在同一会话里转录 `workspace_switched`。两者都不等回包：
        // `handle_frame` 对未匹配的响应帧只丢弃，不阻断界面。
        let context_messages = match self.prompt.context_messages(true) {
            Ok(messages) => messages,
            Err(error) => {
                // 同上：切完工作区装不出上下文时，界面必须看得到原因。
                self.state
                    .notice(format!("上下文装配失败：{error}（只推送了工具表）"));
                Vec::new()
            }
        };
        let _ = self.send(Command::SessionSettings(Box::new(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                tools: Some(self.registry.declarations()),
                context_messages: Some(context_messages),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        })));
        let _ = self.send(Command::WorkspaceSwitch(
            omnicrawl_ipc::bridge::WorkspaceSwitchParams {
                path: new_root.to_string_lossy().to_string(),
            },
        ));
        let message = workspace_switch_success_message(new_root);
        self.state.notice(message);
        Ok(new_root.to_path_buf())
    }

    /// 切换前的子 Agent 排空（对映 Python `coordinator.cancel_and_wait(reason, timeout, permanent=False)`）。
    ///
    /// 仍活跃的后台子任务逐个取消，然后轮询到它们离开任务表；超期未退出就报
    /// `subagent_drain_error`（原工作区与共享资源一律不动）。任务表本身在内核里，
    /// 因此这里走宿主→内核的同步往返（与 `/tasks` 同一条路径）。
    pub(super) fn drain_subagents_before_switch(&mut self) -> Result<(), String> {
        let mut active = self.active_subagent_tasks()?;
        if active.is_empty() {
            return Ok(());
        }
        for task_id in &active {
            // 逐个取消：单个取消失败不阻断其余任务（与 Python 让协调器统一取消同效）。
            let _ = self.command_subagent_query("cancel", task_id);
        }
        let deadline = Instant::now()
            + Duration::from_secs_f64(
                omnicrawl_controllers::shared::SUBAGENT_LIFECYCLE_WAIT_SECONDS,
            );
        while Instant::now() < deadline {
            active = self.active_subagent_tasks()?;
            if active.is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(subagent_drain_error().message().to_string())
    }

    /// 仍未离开任务表（非终态）的子任务 id。
    pub(super) fn active_subagent_tasks(&mut self) -> Result<Vec<String>, String> {
        let value = self.command_subagent_query("list", "")?;
        // 后台任务管理器未启用时没有任务，与「空表」同义。
        if value
            .get("unavailable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(Vec::new());
        }
        let tasks = value
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tasks
            .iter()
            .filter(|task| {
                let status = task
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                !is_terminal(status)
            })
            .filter_map(|task| {
                task.get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect())
    }

    /// 当前工作区仍未处理的 SubAgent worktree（切换前的拦阻项）。
    ///
    /// 托管根是全局的（`~/.omnicrawl/agent-worktrees`），可能还留着别的仓库的残留，
    /// 因此按元数据里的 `repo_root` 对齐当前工作区（对映 Python 的「本进程登记的会话」）。
    pub(super) fn pending_subagent_worktrees(&mut self) -> Result<Vec<WorktreeRef>, String> {
        let value = self.command_subagent_query("list_worktrees", "")?;
        let items = value
            .get("worktrees")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let current = self.workspace.clone();
        Ok(items
            .iter()
            .filter(|item| match item.get("repo_root").and_then(Value::as_str) {
                Some(root) if !root.is_empty() => {
                    same_directory(std::path::Path::new(root), &current)
                }
                // 元数据缺 repo_root（旧版本）：宁多拦一个，也不让未处理的 worktree 静默丢失。
                _ => true,
            })
            .map(|item| WorktreeRef {
                branch: item
                    .get("branch")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                task_id: item
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect())
    }

}
