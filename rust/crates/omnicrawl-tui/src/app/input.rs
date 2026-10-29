//! 终端输入编排：键盘、鼠标、拖选、滚动条、命令菜单与排队输入的按键派发。
//!
//! 只做「把事件翻译成 `AppState` 与内核命令」，业务语义在各 `apply_*` 与命令层。
//! 拆成独立 `impl App` 块，避免与内核编排、设置应用混在一个文件里。

use super::*;

impl App {
    /// 处理一个终端事件。
    pub fn handle_event(&mut self, event: Event) {
        // 用户输入一律视为「输入块脏」：键盘/鼠标/粘贴都可能改输入框、命令菜单、
        // 排队预览、悬停态或拖选。没有输入、也没有内核帧时，事件循环可以整帧跳过绘制。
        if matches!(
            event,
            Event::Key(_) | Event::Mouse(_) | Event::Paste(_) | Event::Resize(..)
        ) {
            self.state.touch_composer();
        }
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // 设置面板是模态页：它开着时按键都归它（与 Python 的 `SettingsScreen` 一致）；
                // 鼠标同理——面板铺满整屏，落点只能按面板自己的命中区判。
                if self.settings.is_some() {
                    self.handle_settings_key(key);
                    return;
                }
                self.handle_key(key);
            }
            // Ctrl+V 的**抬起**：Windows Terminal 把 `ctrl+v` 绑成自家粘贴动作，按下被它吃掉，
            // 只有抬起透传；图片粘贴挂在这里才对（详见 `handle_ctrl_v_release`）。
            // 抬起也可能来自文本粘贴，此时 `paste_clipboard_image` 自己返回 `false`，不干扰正文。
            Event::Key(key) if is_ctrl_v_release(key) => self.handle_ctrl_v_release(),
            Event::Mouse(mouse) if self.settings.is_some() => self.handle_settings_mouse(mouse),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            // 设置面板是模态页：粘贴落进当前字段的输入态（对映 Textual `Input::_on_paste`），
            // 没有正在编辑的字段时丢弃，不落到主输入框。
            Event::Paste(text) if self.settings.is_some() => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.paste(&text);
                }
            }
            Event::Paste(text) if !self.kernel_lost => {
                // 整段就是一个存在的图片路径 → 转成图片附件；否则按文本折叠
                // （多行折成 `[粘贴 #n +N 行]`，提交时还原）。
                if !self.paste_image_path(&text) {
                    self.state.composer.insert_paste(&text);
                }
            }
            Event::Resize(..) => {}
            _ => {}
        }
    }

    /// 鼠标事件：滚轮滚动消息区，左键拖选复制、按落点分派到排队预览或消息流。
    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        // 滚动条拖动优先，且不受「等审批」限制：按住右缘那格上下滑就是滚会话。
        if self.handle_scrollbar_mouse(&mouse) {
            return;
        }
        // 提问面板开着时不允许点击展开/撤回：回填内容会被提问模式吞掉
        // （与 Python `_withdraw_pending_input` 的守卫同源）。
        let interactive = self.state.waiting().is_none();
        // 按住 Shift 的拖选留给终端自己做（Windows Terminal 支持），否则会与应用抢选择。
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        // 点输入区边框上的 `[ ESC ]` 等同于按键盘 Esc（取消回合）——不受「等审批」限制。
        let on_esc = self
            .state
            .runtime_esc_area()
            .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)));
        // 欢迎页徽标的可点段（模型名 / 推理档 / 审批模式 / MCP 数量）：按下即跳设置页。
        let badge_link = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.welcome_badge_link_at(mouse.column, mouse.row)
            }
            _ => None,
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if on_esc => {
                if self.state.selection().is_some() {
                    self.state.clear_selection();
                } else if self.state.turn.is_running() {
                    self.cancel_turn();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if badge_link.is_some() => {
                self.state.clear_selection();
                if let Some(link) = badge_link {
                    self.open_settings_at(link.settings_key());
                }
            }
            MouseEventKind::Moved => self.update_conversation_hover(mouse.column, mouse.row),
            MouseEventKind::ScrollUp => self.state.scroll_by(-WHEEL_STEP),
            MouseEventKind::ScrollDown => self.state.scroll_by(WHEEL_STEP),
            MouseEventKind::Down(MouseButton::Left) if interactive => {
                // 点输入框：把插入点挪到点中的那一格（用户要求鼠标能定插入位置），
                // 同时起选区锚点：接着拖就是选取输入框里的文字。
                if let Some(index) = self.composer_index(mouse.column, mouse.row) {
                    self.state.clear_selection();
                    self.composer_drag = true;
                    self.state.composer.begin_selection(index);
                } else if let Some(point) = self.conversation_point(mouse.column, mouse.row) {
                    if !shift {
                        // 两处选区不同时亮着：否则松手时分不清该复制哪一个。
                        self.composer_drag = false;
                        self.state.composer.clear_selection();
                        self.state.begin_selection(point.0, point.1);
                    }
                }
                self.handle_click(mouse.column, mouse.row);
            }
            MouseEventKind::Drag(MouseButton::Left) if interactive && !shift => {
                if self.composer_drag {
                    self.extend_composer_selection(mouse.column, mouse.row);
                } else if let Some(point) = self.conversation_point(mouse.column, mouse.row) {
                    self.state.extend_selection(point.0, point.1);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if interactive => {
                if std::mem::take(&mut self.composer_drag) {
                    self.finish_composer_selection();
                } else {
                    self.finish_selection();
                }
            }
            _ => {}
        }
    }

    /// 屏幕坐标 → 会话流的（显示行下标, 行内显示列）；不在会话文本区时返回 `None`。
    ///
    /// 与渲染、命中测试共用[`ui::layout`]与会话区换算，滚动/折行/宽字符都不会错位。
    pub(super) fn conversation_point(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        let areas = ui::layout(self.viewport, &self.state);
        let area = areas.conversation;
        if !area.contains(Position::new(column, row)) {
            return None;
        }
        let relative = (row - area.y) as usize;
        let line =
            conversation::line_index(&self.state, area, self.state.scroll_from_bottom, relative)?;
        let text = conversation::text_area(area);
        Some((line, usize::from(column.saturating_sub(text.x))))
    }

    /// 输入框正文区的屏幕矩形（鼠标命中用）。
    ///
    /// 公开可见是为了让集成测试用同一套换算喂鼠标坐标——测试自己重算一遍布局，
    /// 测的就不再是真正在敲的那个区域了。
    pub fn composer_text_rect(&self) -> Rect {
        ui::composer::text_rect(ui::layout(self.viewport, &self.state).composer)
    }

    /// 屏幕坐标 → 输入框里的字符下标；不在输入框正文区时返回 `None`。
    ///
    /// 与渲染共用 [`ui::composer::text_rect`] 和 [`crate::state::Composer::char_index_at`]，
    /// 软折行、宽字符、内容超上限后的窗口滚动都不会错位。
    pub(super) fn composer_index(&self, column: u16, row: u16) -> Option<usize> {
        let area = self.composer_text_rect();
        if !area.contains(Position::new(column, row)) {
            return None;
        }
        Some(self.state.composer.char_index_at(
            area.width,
            usize::from(row - area.y),
            usize::from(column - area.x),
        ))
    }

    /// 拖选输入框：把拖动端挪到鼠标位置。
    ///
    /// 拖出正文区时不跨到会话流（那是另一条选区），而是贴到最近的一行：右侧越界自然成了
    /// 「到行尾」（列值大于行宽），下方越界贴到最后一行（输入框最多 5 行）。
    pub(super) fn extend_composer_selection(&mut self, column: u16, row: u16) {
        let area = self.composer_text_rect();
        // 只在正文区里算行号：越下越界贴到最后一行，越上贴到第一行（不会跨到会话流）。
        // 零高区域收不到按下事件，但拖选仍可能被叫到，所以先按 `max(1)` 挡一道除零。
        let height = usize::from(area.height.max(1));
        let row_in = usize::from(row.saturating_sub(area.y)).min(height - 1);
        let index = self.state.composer.char_index_at(
            area.width,
            row_in,
            usize::from(column.saturating_sub(area.x)),
        );
        self.state.composer.extend_selection(index);
    }

    /// 松左键：输入框里有选区就写剪切板（与会话区拖选同一套：复制完清掉高亮）。
    ///
    /// 只点一下（空选区）不算：那次点击的作用是定位插入点，光标已经落好了，
    /// 不该往剪切板里塞一个空字符串。
    pub(super) fn finish_composer_selection(&mut self) {
        let Some(text) = self.state.composer.selected_text() else {
            self.state.composer.clear_selection();
            return;
        };
        let characters = text.chars().count();
        let rows = text.lines().count();
        match crate::clipboard::copy_text(&text) {
            Ok(()) => {
                self.state.composer.clear_selection();
                let message = if rows > 1 {
                    format!("已复制输入框里选中的 {rows} 行到剪切板。")
                } else {
                    format!("已复制输入框里选中的 {characters} 个字符到剪切板。")
                };
                self.state.show_notice_line(message, Instant::now());
            }
            Err(error) => self.state.show_notice_line(
                format!("复制失败：{error}（可按住 Shift 拖选，用终端自带的复制）"),
                Instant::now(),
            ),
        }
    }

    /// 松左键：有选区就写剪切板并清掉高亮；没拖动（空选区）就当普通点击。
    /// 滚动条拖动：命中返回 `true`（这一下不再当点选/拖选处理）。
    pub(super) fn handle_scrollbar_mouse(&mut self, mouse: &MouseEvent) -> bool {
        let areas = ui::layout(self.viewport, &self.state);
        let Some(column) = conversation::scrollbar_column(areas.conversation) else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.column == column => {
                self.scrollbar_drag = true;
                self.drag_scrollbar_to(mouse.row);
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.scrollbar_drag => {
                self.drag_scrollbar_to(mouse.row);
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.scrollbar_drag => {
                self.scrollbar_drag = false;
                true
            }
            _ => false,
        }
    }

    pub(super) fn drag_scrollbar_to(&mut self, row: u16) {
        let areas = ui::layout(self.viewport, &self.state);
        if let Some(offset) =
            conversation::scroll_offset_for_row(&self.state, areas.conversation, row)
        {
            self.state.scroll_from_bottom = offset;
        }
    }

    pub(super) fn finish_selection(&mut self) {
        let Some(selection) = self.state.selection() else {
            return;
        };
        if selection.is_empty() {
            self.state.clear_selection();
            return;
        }
        let width =
            conversation::text_area(ui::layout(self.viewport, &self.state).conversation).width;
        let Some(text) = conversation::selection_text(&self.state, width) else {
            self.state.clear_selection();
            return;
        };
        let rows = text.lines().count();
        match crate::clipboard::copy_text(&text) {
            Ok(()) => {
                self.state.clear_selection();
                self.state
                    .show_notice_line(format!("已复制 {rows} 行到剪切板。"), Instant::now());
            }
            Err(error) => self.state.show_notice_line(
                format!("复制失败：{error}（可按住 Shift 拖选，用终端自带的复制）"),
                Instant::now(),
            ),
        }
    }

    /// 鼠标移动：关心输入区方框上边框里的 `[ ESC ]` 与工具卡标题行上的行内按钮
    /// （「立即结束」/「查看会话」），命中则点亮为淡黄色。
    ///
    /// `[ ESC ]` 的位置由渲染路径登记（`AppState::runtime_esc_area`），所以这里不必重算显示行——
    /// 长会话下那是一次不便宜的折行。卡片按钮的列区间随显示行缓存一起算好，
    /// 命中测试只做窗口换算。
    pub(super) fn update_conversation_hover(&mut self, column: u16, row: u16) {
        self.update_welcome_badge_hover(column, row);
        self.update_stop_button_hover(column, row);
        self.update_subagent_view_hover(column, row);
        self.update_retry_button_hover(column, row);
        if !self.state.turn.is_running() {
            self.state.runtime_esc_hover = false;
            return;
        }
        let hover = self
            .state
            .runtime_esc_area()
            .is_some_and(|area| area.contains(Position::new(column, row)));
        self.state.runtime_esc_hover = hover;
    }

    /// 悬停判定：鼠标是否落在欢迎页徽标某个可点段上（模型名 / 推理档 / 审批模式 / MCP 数）。
    ///
    /// 各段的屏幕矩形由渲染路径登记，这里只做落点比较；移开时立刻褪色。
    pub(super) fn update_welcome_badge_hover(&mut self, column: u16, row: u16) {
        let point = Position::new(column, row);
        let hover = self
            .state
            .welcome_badge_areas()
            .iter()
            .position(|area| area.is_some_and(|area| area.contains(point)));
        self.state.set_welcome_badge_hover(hover);
    }

    /// 落点命中的徽标段（点击跳转用）；没命中徽标时为 `None`。
    pub(super) fn welcome_badge_link_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<crate::ui::fullscreen::rendering::welcome_logo::BadgeLink> {
        self.state.welcome_badge_link_at(column, row)
    }

    /// 悬停判定：鼠标是否落在某个「重试」按钮上。
    pub(super) fn update_retry_button_hover(&mut self, column: u16, row: u16) {
        let area = ui::layout(self.viewport, &self.state).conversation;
        if !area.contains(Position::new(column, row)) {
            self.state.set_retry_button_hover(None);
            return;
        }
        let relative = usize::from(row - area.y);
        // 卡片按钮压在标题行右缘，命中它时不算重试按钮的悬停。
        let hovered = if self.on_card_button(area, relative, column) {
            None
        } else {
            match conversation::hit_test(&self.state, area, self.state.scroll_from_bottom, relative)
            {
                Some(LineHit::RetryTurn { index }) => Some(index),
                _ => None,
            }
        };
        self.state.set_retry_button_hover(hovered);
    }

    /// 悬停判定：鼠标是否落在某个「立即结束」按钮上。
    pub(super) fn update_stop_button_hover(&mut self, column: u16, row: u16) {
        let area = ui::layout(self.viewport, &self.state).conversation;
        if !area.contains(Position::new(column, row)) {
            self.state.set_stop_button_hover(None);
            return;
        }
        let relative = usize::from(row - area.y);
        let hovered = if self.on_card_button(area, relative, column) {
            self.monitor_call_id_at(area, relative)
        } else {
            None
        };
        self.state.set_stop_button_hover(hovered);
    }

    /// 悬停判定：鼠标是否落在某个「查看会话」按钮上。
    pub(super) fn update_subagent_view_hover(&mut self, column: u16, row: u16) {
        let area = ui::layout(self.viewport, &self.state).conversation;
        if !area.contains(Position::new(column, row)) {
            self.state.set_subagent_view_hover(None);
            return;
        }
        let relative = usize::from(row - area.y);
        let hovered = if self.on_card_button(area, relative, column) {
            self.subagent_view_call_id_at(area, relative)
        } else {
            None
        };
        self.state.set_subagent_view_hover(hovered);
    }

    /// 屏幕列是否落在该行卡片按钮的格子里。
    pub(super) fn on_card_button(&self, area: Rect, relative: usize, column: u16) -> bool {
        let text = conversation::text_area(area);
        conversation::card_button_columns(
            &self.state,
            area,
            self.state.scroll_from_bottom,
            relative,
        )
        .is_some_and(|(start, width)| {
            let column = usize::from(column.saturating_sub(text.x));
            column >= usize::from(start) && column < usize::from(start) + usize::from(width)
        })
    }

    /// 设置面板的鼠标事件：悬停只记录行（加亮交给渲染层），左键按命中区分派。
    ///
    /// 面板是整屏模态页，各页的滚动都跟着选中项走，因此滚轮不在这里接管。
    pub(super) fn handle_settings_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(settings) = self.settings.as_mut() {
                    let hover = settings.hit_at(mouse.column, mouse.row);
                    settings.set_hover(hover);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // 先取出事件再分派：`click` 要可变借用面板，而事件处理要可变借用 `App`。
                let event = {
                    let Some(settings) = self.settings.as_mut() else {
                        return;
                    };
                    let Some(action) = settings.hit_at(mouse.column, mouse.row) else {
                        return;
                    };
                    settings.click(action)
                };
                self.dispatch_settings_event(event);
            }
            _ => {}
        }
    }

    /// 左键点击：先在排队预览条上找命中，再到消息区找工具卡/思考段命中。
    pub(super) fn handle_click(&mut self, column: u16, row: u16) {
        let areas = ui::layout(self.viewport, &self.state);
        let point = Position::new(column, row);

        if areas.queue.contains(point) {
            let index = (row - areas.queue.y) as usize;
            match queue::hit(&self.state, index) {
                Some(QueueHit::Withdraw(index)) => {
                    if !self.state.withdraw_pending(index) {
                        return;
                    }
                }
                Some(QueueHit::Toggle) => self.state.toggle_queue_expanded(),
                None => {}
            }
            return;
        }

        if !areas.conversation.contains(point) {
            return;
        }
        let relative = (row - areas.conversation.y) as usize;
        // 标题行右缘的卡片按钮优先：先于卡片的收起/展开判定，也先于选区。
        if self.on_card_button(areas.conversation, relative, column) {
            if let Some(call_id) = self.monitor_call_id_at(areas.conversation, relative) {
                self.stop_monitor_poll(&call_id);
                return;
            }
            if let Some(call_id) = self.subagent_view_call_id_at(areas.conversation, relative) {
                self.state.open_subagent_window(&call_id);
                return;
            }
        }
        let hit = conversation::hit_test(
            &self.state,
            areas.conversation,
            self.state.scroll_from_bottom,
            relative,
        );
        match hit {
            Some(LineHit::ToolHint { call_id }) => self.state.expand_tool(&call_id),
            Some(LineHit::ToolStop { call_id }) => self.stop_monitor_poll(&call_id),
            Some(LineHit::RetryTurn { index }) => self.retry_turn(index),
            Some(LineHit::ToolCard { call_id }) => {
                // 对映 Python：缩略态点卡片无效，展开态点卡片才是收起。
                self.state.collapse_tool(&call_id);
            }
            Some(LineHit::Reasoning { index }) => {
                self.state.toggle_reasoning_expanded(index);
            }
            None => {}
        }
    }

    /// `turn.submit` 的失败响应：把回合收口成可重试的失败提示。
    ///
    /// 内核在重试耗尽后返回的是回合终止错误（`turn_error`），这时本回合不会再有
    /// `turn.finished`，必须在这里自己复位状态行与在途卡片。
    pub(super) fn fail_submitted_turn(&mut self, message: &str) {
        // 回合已经收口（用户按 Esc 提前 fail_turn、或失败前已收到 `turn.finished`）：
        // 再落一条失败提示就是重复上报，直接跳过。
        if !self.state.turn.is_running() {
            return;
        }
        // 取消是用户自己的动作（Esc），不提供重试按钮——再发一次等于撤销用户意图。
        if message.contains("已取消") || message.contains("回合已取消") {
            self.state.fail_turn(message.to_string());
            return;
        }
        let retryable = is_retryable_failure(message);
        self.state.fail_turn_with(message.to_string(), retryable);
    }

    /// 「重试」按钮：用原样的用户消息再提交一次，重新进入自动重试循环。
    pub(super) fn retry_turn(&mut self, index: usize) {
        if self.state.turn.is_running() {
            self.state
                .show_notice_line("当前回合还在执行，等它结束再重试。", Instant::now());
            return;
        }
        let Some(text) = self.state.retry_failure(index) else {
            return;
        };
        // 只重发文本：失败的那一轮已经交过一次图，重放图片会让同一张图在上下文里出现两遍。
        self.submit_to_kernel(text, Vec::new());
    }

    /// 某一行所属的后台任务卡 `call_id`（只有 monitor 卡才算）。
    pub(super) fn monitor_call_id_at(&self, area: Rect, relative: usize) -> Option<String> {
        match conversation::hit_test(&self.state, area, self.state.scroll_from_bottom, relative) {
            Some(LineHit::ToolCard { call_id }) | Some(LineHit::ToolStop { call_id }) => self
                .state
                .monitor_task_id(&call_id)
                .is_some()
                .then_some(call_id),
            _ => None,
        }
    }

    /// 某一行所属的子代理调用卡 `call_id`（只有派发过批次的卡才算）。
    pub(super) fn subagent_view_call_id_at(&self, area: Rect, relative: usize) -> Option<String> {
        match conversation::hit_test(&self.state, area, self.state.scroll_from_bottom, relative) {
            Some(LineHit::ToolCard { call_id })
                if self.state.subagent_view_batch(&call_id).is_some() =>
            {
                Some(call_id)
            }
            _ => None,
        }
    }

    /// 「立即结束」：打断该后台任务上正在跑的长轮询，让这一轮 poll 立刻带着当前结果返回。
    ///
    /// 只提前收口等待，不停任务、不动进程——`monitor` 任务的生死由模型侧的
    /// `action=stop` 决定，界面按钮不该替它做决定。
    pub(super) fn stop_monitor_poll(&mut self, call_id: &str) {
        let Some(monitor_id) = self.state.monitor_task_id(call_id) else {
            return;
        };
        let interrupted = self.registry.monitors().interrupt_wait(&monitor_id);
        let message = if interrupted {
            format!("已结束 {monitor_id} 的等待，本轮轮询立即返回。")
        } else {
            format!("{monitor_id} 当前没有正在等待的轮询，无事可做。")
        };
        self.state.show_notice_line(message, Instant::now());
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                // 有选区时 Ctrl+C 复制（终端惯例）；**输入框里的选区优先**——选中了文字
                // 再按 Ctrl+C 却把整个输入框清空，等于一手毁掉草稿；两处都没有才保持原行为。
                if self.state.composer.selected_text().is_some() {
                    self.finish_composer_selection();
                } else if self.state.selection().is_some() {
                    self.finish_selection();
                } else {
                    self.state.composer.clear();
                }
                return;
            }
            KeyCode::Char('q') if ctrl => {
                self.start_shutdown();
                return;
            }
            _ => {}
        }

        if self.kernel_lost {
            // 内核已退出：只剩「看现场」与「退出」两件事。Enter / Esc 与 Ctrl+Q 同义，
            // 上下翻页照旧滚动（好读清报错），其余按键吞掉——输入框不再接受文本，
            // 免得用户以为消息还能发出去、或误按 Enter 以为是在提交。
            match key.code {
                KeyCode::Enter | KeyCode::Esc => {
                    self.start_shutdown();
                    return;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {}
                _ => return,
            }
        }

        if self.handle_waiting_key(key, ctrl) {
            return;
        }

        // `/sessions` 菜单开着时优先吃上下键 / Enter / Esc。
        if self.handle_sessions_menu_key(key) {
            return;
        }

        // 命令菜单开着时先吃选择键（对映 `_handle_composer_command_key`）：
        // 上下键在候选间移动，Enter/Tab 只补全；输入已是完整命令时 Enter 放行提交。
        if self.handle_command_menu_key(key) {
            return;
        }

        match key.code {
            // 生成期间按 Enter 排队（FIFO），空闲时直接提交：与 Python
            // `_submit_composer_text` 的分叉一致。
            KeyCode::Enter => self.submit_or_queue(),
            KeyCode::Char('j') if ctrl => self.state.composer.newline(),
            KeyCode::Char('l') if ctrl => {
                self.state.clear_conversation();
            }
            KeyCode::Esc => {
                // 先清选区（有选区时 Esc 不该顺手取消回合）。
                if self.state.selection().is_some() {
                    self.state.clear_selection();
                } else if self.state.turn.is_running() {
                    self.cancel_turn();
                } else {
                    self.state.composer.clear();
                    self.state.scroll_to_bottom();
                }
            }
            KeyCode::Char(character) if !ctrl => self.state.composer.insert(&character.to_string()),
            KeyCode::Backspace => self.state.composer.backspace(),
            KeyCode::Delete => self.state.composer.delete(),
            KeyCode::Left => self.state.composer.move_left(),
            KeyCode::Right => self.state.composer.move_right(),
            KeyCode::Home => self.state.composer.move_home(),
            KeyCode::End => self.state.composer.move_end(),
            // 上下键最后才轮到输入框的历史回看：待决面板、`/sessions` 菜单与命令候选
            // 都已在上方吃掉它们，这里只服务「输入框」——没有历史（或下键时未在浏览）
            // 就退回原来的会话滚动（对映 Python `Composer.on_key` 的 `navigate_history`
            // 失败回退分支）。
            KeyCode::Up if !self.state.composer.navigate_history(-1) => {
                self.state.scroll_by(-SCROLL_STEP)
            }
            KeyCode::Down if !self.state.composer.navigate_history(1) => {
                self.state.scroll_by(SCROLL_STEP)
            }
            KeyCode::PageUp => self.state.scroll_by(-PAGE_STEP),
            KeyCode::PageDown => self.state.scroll_by(PAGE_STEP),
            _ => {}
        }
    }

    /// `/sessions` 会话菜单的选择键；返回 `true` 表示事件已被菜单消费。
    ///
    /// 上下键循环移动，Enter 把 `/resume <session_id>` 填回输入框（不直接执行，
    /// 对映 Python `_handle_sessions_menu_key`），Esc 或其余按键收起菜单。
    pub(super) fn handle_sessions_menu_key(&mut self, key: KeyEvent) -> bool {
        if !self.state.sessions_menu.is_open() {
            return false;
        }
        match key.code {
            KeyCode::Up => {
                self.state.sessions_menu.move_selection(-1);
                true
            }
            KeyCode::Down => {
                self.state.sessions_menu.move_selection(1);
                true
            }
            KeyCode::Enter => {
                if let Some(item) = self.state.sessions_menu.selected() {
                    let command = format!("/resume {}", item.session_id);
                    self.state.sessions_menu.close();
                    self.state.composer.set_text(&command);
                }
                true
            }
            KeyCode::Esc => {
                self.state.sessions_menu.close();
                true
            }
            // 其余键收起菜单，但不等同消费：让输入继续落到输入框。
            _ => {
                self.state.sessions_menu.close();
                false
            }
        }
    }

    /// 按钮命令菜单的选择键；返回 `true` 表示事件已被菜单消费。
    ///
    /// 菜单只在打开时吃键：其余按键继续交给输入框编辑。补全写入的文本会再走一次
    /// 菜单刷新（`Composer::set_text`），因此补全后的候选与光标位置总是一致的。
    pub(super) fn handle_command_menu_key(&mut self, key: KeyEvent) -> bool {
        let menu_key = match key.code {
            KeyCode::Up => MenuKey::Up,
            KeyCode::Down => MenuKey::Down,
            KeyCode::Enter => MenuKey::Enter,
            KeyCode::Tab => MenuKey::Tab,
            _ => return false,
        };
        match self.state.composer.menu_handle_key(menu_key) {
            // 输入已是完整命令：放行给提交流程（否则 `/settings` 这类命令永远打不开）。
            MenuAction::Passthrough => false,
            MenuAction::Complete { insert } => {
                self.state.composer.set_text(&insert);
                true
            }
            MenuAction::Redraw | MenuAction::Hidden => true,
        }
    }

    /// 待决面板优先吃按键；返回 `true` 表示事件已被消费。
    pub(super) fn handle_waiting_key(&mut self, key: KeyEvent, ctrl: bool) -> bool {
        let Some(batch_id) = self.state.batch_request_id() else {
            return false;
        };
        match self.state.waiting() {
            Some(Waiting::Approval(_)) => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        if let Some(step) = self.state.decide_approval(true) {
                            self.handle_batch_step(batch_id, step);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        // 拒绝的若是 MCP Tool，先落审计再推进批次（与 Python 的
                        // `record_denied_tool_call` 同一时点、同一文案）。
                        if let Some(call) = self.state.pending_approval_call() {
                            host::record_mcp_denial(
                                &self.registry,
                                &call.name,
                                &call.arguments,
                                &omnicrawl_controllers::approval::user_cancelled_reason(&call.name),
                            );
                        }
                        if let Some(step) = self.state.decide_approval(false) {
                            self.handle_batch_step(batch_id, step);
                        }
                    }
                    _ => {}
                }
                true
            }
            Some(Waiting::Question(panel)) => {
                let is_select = panel.is_select();
                match key.code {
                    KeyCode::Up if is_select => {
                        self.state.select_question(-1);
                        true
                    }
                    KeyCode::Down if is_select => {
                        self.state.select_question(1);
                        true
                    }
                    KeyCode::Enter if is_select => {
                        let custom = match self.state.waiting() {
                            Some(Waiting::Question(panel)) => panel.is_custom(),
                            _ => false,
                        };
                        // 选中「我有自己的想法...」就是自己作答：答案取输入框，不能用空选项提交。
                        let answer = if custom {
                            if self.state.composer.is_empty() {
                                return true;
                            }
                            self.state.composer.take()
                        } else {
                            match self.state.waiting() {
                                Some(Waiting::Question(panel)) => panel.answer(),
                                _ => String::new(),
                            }
                        };
                        if let Some(step) = self.answer_pending_question(answer) {
                            self.handle_batch_step(batch_id, step);
                        }
                        true
                    }
                    KeyCode::Enter => {
                        if !self.state.composer.is_empty() {
                            let answer = self.state.composer.take();
                            if let Some(step) = self.answer_pending_question(answer) {
                                self.handle_batch_step(batch_id, step);
                            }
                        }
                        true
                    }
                    KeyCode::Esc => {
                        if let Some(step) = self.answer_pending_question(String::new()) {
                            self.handle_batch_step(batch_id, step);
                        }
                        true
                    }
                    // 没有选项可服务（自由输入型提问）时，上下键轮到输入框的历史回看：
                    // 提问作答复用同一个输入框，回看规则与常态一致（对映 Python
                    // `Composer.on_key` 在 `option_navigation` 为假时继续走 `navigate_history`）。
                    KeyCode::Up if self.state.composer.navigate_history(-1) => true,
                    KeyCode::Down if self.state.composer.navigate_history(1) => true,
                    _ => {
                        // 自由输入型提问复用输入框，其余按键继续交给它编辑。
                        self.edit_composer(key, ctrl);
                        true
                    }
                }
            }
            None => false,
        }
    }

    pub(super) fn edit_composer(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Char(character) if !ctrl => self.state.composer.insert(&character.to_string()),
            KeyCode::Backspace => self.state.composer.backspace(),
            KeyCode::Delete => self.state.composer.delete(),
            KeyCode::Left => self.state.composer.move_left(),
            KeyCode::Right => self.state.composer.move_right(),
            KeyCode::Home => self.state.composer.move_home(),
            KeyCode::End => self.state.composer.move_end(),
            _ => {}
        }
    }
}
