//! 生命周期与附件：装配期挂载、视口、退出与剪贴板附件。
//!
//! 进程退出、隔离会话收尾与图片粘贴都不属于回合语义，单独放这里。

use super::*;

impl App {


    /// 接管内核 stderr 通道：由 [`Self::drain_kernel_logs`] 把报错转成会话区提示。
    pub fn attach_kernel_logs(&mut self, logs: Receiver<String>) {
        self.kernel_logs = Some(logs);
    }


    /// 接管运行期诊断通道：由 [`Self::drain_diagnostics`] 把每条诊断写进会话流。
    ///
    /// 接收端由 `crate::diagnostics::install` 装在 `omnicrawl_core::diagnostics` 上，
    /// TUI 进程内所有库诊断（含宿主 crate）都从这一个口子进会话区。
    pub fn attach_diagnostics(&mut self, lines: Receiver<(Level, String)>) {
        self.diagnostic_lines = Some(lines);
    }


    /// 挂载主 Agent 隔离区会话：退出收尾时自动 apply + 清理。
    pub fn attach_isolation_session(&mut self, session: IsolationSession) {
        self.isolation = Some(session);
    }


    /// 退出收尾：按 `[agent_workspace]` 的 `apply_on_exit` / `cleanup_on_exit` 处理隔离区，
    /// 再按 auto 策略收尾 SubAgent worktree（只清理无变更的，成果绝不自动 apply）。
    ///
    /// 对映 Python `LocalToolAgent._finalize_attached_isolation`：TUI / API / 连接器
    /// 各入口共用同一收尾路径，摘要打印到 stderr。
    pub(super) fn finalize_isolation(&mut self) {
        let mut summaries: Vec<String> = Vec::new();
        if let Some(session) = self.isolation.take() {
            let config = load_agent_workspace_config(&ConfigEnvironment::from_process(), None)
                .unwrap_or_default();
            let summary = finalize_isolation_session(
                &session,
                config.apply_on_exit,
                &config.cleanup_on_exit,
                None,
                None,
            );
            if !summary.is_empty() {
                summaries.push(summary);
            }
        }
        let sub_summary = finalize_subagent_worktrees(None, "origin");
        if !sub_summary.is_empty() {
            summaries.push(sub_summary);
        }
        if !summaries.is_empty() {
            diagnostics::info(format!("[isolation] {}", summaries.join("；")));
        }
    }


    /// 记录当前终端区域（每帧由事件循环在渲染前更新）。
    pub fn set_viewport(&mut self, area: Rect) {
        self.viewport = area;
    }


    /// 重装命令菜单候选表：注册表声明 + 当前工作区发现的 Skill。
    ///
    /// Skill 清单随工作区变化，而候选表是启动时装载一次的，切换工作区后不重装
    /// 就会留下旧工作区的 `/skill:*`。
    pub(super) fn refresh_command_menu(&mut self) {
        let skills = self.prompt.skill_metas();
        self.state
            .composer
            .set_commands(commands::command_options_with_skills(&skills));
    }


    /// 本帧是否需要重画：三个刷新块（会话 / 输入框 / 底部）自上次绘制以来是否变过，
    /// 再加上终端是否换过尺寸。
    ///
    /// 全都没变时返回 `false`，事件循环就整帧跳过 `terminal.draw`——空闲时 TUI
    /// 不再以 20fps 空转（三块的内容也不会被无谓地重建）。
    /// 模态页（设置面板）自己铺满整屏，仍按老规矩每帧重画。
    pub fn needs_redraw(&mut self) -> bool {
        let current = (self.state.refresh_versions(), self.viewport);
        if self.settings.is_some() {
            self.drawn = Some(current);
            return true;
        }
        if self.drawn != Some(current) {
            self.drawn = Some(current);
            return true;
        }
        false
    }


    /// 本批实际使用的工具表：隔离批次用临时表，其余用共享表。
    pub(super) fn batch(&self) -> &Arc<ToolRegistry> {
        self.batch_registry.as_ref().unwrap_or(&self.registry)
    }


    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }


    /// 退出收尾：回收后台进程、收尾隔离工作区后请内核退出。
    pub fn shutdown(&mut self) {
        // 在途的慢命令先收尾：候选子系统得 close，否则 MCP 子进程会成孤儿。
        self.settle_slow_task();
        self.registry.close_monitors();
        self.registry.close_mcp();
        self.finalize_isolation();
        // 临时目录清理线程随进程退出而停：`join` 掉，避免留下悬挂工作。
        if let Some(installation) = self.agent_temp.as_mut() {
            installation.close();
        }
        self.request_shutdown();
    }


        pub(super) fn start_shutdown(&mut self) {
        // 退出前丢开未提交的排队消息（对映 Python `_pending_inputs.clear()`）。
        self.state.clear_pending();
        self.request_shutdown();
        self.quit = true;
    }


    /// 请内核退出；连接已断时什么都不做。
    pub fn request_shutdown(&mut self) {
        if self.kernel.is_closed() {
            return;
        }
        self.send(Command::Shutdown);
    }


    /// 粘贴的图片有没有去处：只有原生视觉能处理**用户消息**里的图片。
    ///
    /// `[vision]` 代理只分析工具产出的带图观察（内核 `apply_observations`），从不处理
    /// `turn.submit` 里的用户图片；把代理算进「有通路」会让看不见图的模型收到 base64，
    /// 上游直接拒掉整个回合。工具图片走哪条路由仍由 [`Self::attach_vision_images`] 决定。
    pub(super) fn vision_route_available(&self) -> bool {
        self.native_vision_enabled()
    }


    /// 把剪贴板里的位图粘进输入框；返回是否真的插入了一张图片。
    ///
    /// 三个前提缺一不可：剪贴板里是位图、设置面板没开着、原生视觉开着。
    /// 没开原生视觉时不插入占位符，只在提示行说明原因（用户要求）——图片插进去也送不出去，
    /// 不如当场说清为什么没插进去。剪贴板里是文本时返回 `false`，让终端自己送来的
    /// 文本粘贴照常处理。
    pub(super) fn paste_clipboard_image(&mut self) -> bool {
        if self.settings.is_some() {
            return false;
        }
        let Some(png) = crate::clipboard::read_image_png() else {
            return false;
        };
        if !self.vision_route_available() {
            self.state
                .show_notice_line("当前未开启原生视觉，粘贴的图片不会被发送。", Instant::now());
            // 已消费这次按键：图片不能进输入框，也不能让 `v` 落进正文。
            return true;
        }
        self.state.composer.insert_clipboard_image("image/png", png);
        self.state.touch_composer();
        true
    }


    /// 粘贴内容整段就是一个存在的图片文件路径时，把它转成图片附件；返回是否真的插入了。
    ///
    /// 只认「整段仅路径」：夹了引号、括号说明或别的正文就不转（详见 [`crate::image_path`]），
    /// 避免粘贴代码时把路径片段误当图片。没有视觉通路时保持文本原样、不插入也不提示——
    /// 路径对模型照样可读，正是「不转换」最合理的结果。
    pub(super) fn paste_image_path(&mut self, text: &str) -> bool {
        if self.settings.is_some() {
            return false;
        }
        if !self.vision_route_available() {
            return false;
        }
        let Some(image) = crate::image_path::read_pasted_image_path(text, &self.workspace) else {
            return false;
        };
        self.state
            .composer
            .insert_image(&image.media_type, image.bytes);
        self.state.touch_composer();
        true
    }



    /// Ctrl+V 的抬起事件：图片粘贴就挂在这里。
    ///
    /// Windows Terminal 把 `ctrl+v` 绑成它自己的粘贴动作，**按下被它吃掉**（只把抬起
    /// 透传过来），因此 `Ctrl+V` 的按下事件永远到不了这里——挂在按下上的图片粘贴是死代码。
    /// 抬起也照旧携带 Control，用它当触发点对 conhost 同样成立（那里按下与抬起都会到达）。
    pub(super) fn handle_ctrl_v_release(&mut self) {
        self.paste_clipboard_image();
    }
}
