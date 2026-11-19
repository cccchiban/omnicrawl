//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
        /// 把 `records` 裁到显示缓存仍然覆盖得到的尾部窗口。
        ///
        /// 显示缓存只保留最后 [`crate::ui::conversation::CONVERSATION_MAX_LINES`] 行，
        /// 被它丢掉前缀块的那些记录再也不会被渲染；但 `records` 本身原来没有上限，
        /// 长会话会一路把正文、工具卡与历史消息留在内存里。这里按缓存的
        /// `retained_from_record()` 精确裁剪，并把依赖记录下标的 UI 状态一起左移。
        ///
        /// 由事件循环在每帧绘制之后调用：缓存只在绘制时推进 `first_index`。
        pub fn prune_records_to_cache(&mut self) {
            let keep_from = self.conversation_cache.borrow().retained_from_record();
            if keep_from == 0 || keep_from > self.records.len() {
                return;
            }
            self.records.drain(..keep_from);
            self.conversation_cache
                .get_mut()
                .rebase_after_records_pruned(keep_from);
            // 展开态按记录下标索引：能保留的条目左移，落在被裁前缀里的直接丢弃。
            self.expanded_reasoning = self
                .expanded_reasoning
                .iter()
                .filter_map(|index| index.checked_sub(keep_from))
                .collect();
            self.retry_button_hover = self
                .retry_button_hover
                .and_then(|index| index.checked_sub(keep_from));
        }

        // ---- 工具卡图片预览 -----------------------------------------------------------

        /// 收下后台解码完的图片；返回是否出现了新的预览（会话区据此重画）。
        ///
        /// 新图意味着卡片正文要多铺几行占位块，必须让那一条记录的显示行重算——
        /// 否则图片到了屏幕上也不会出现（缓存里还是旧的行数）。
        pub fn tick_image_previews(&mut self) -> bool {
            let ready = self.image_previews.get_mut().drain();
            if ready.is_empty() {
                return false;
            }
            // 只重算那几张卡的显示行：图片从「正在准备」换成整块，或者（解码失败时）换回载荷正文。
            // 整块失效在这里太贵——一张 2000 行的会话每来一张图就要从头渲染一遍。
            for key in &ready {
                if let Some(slot) = self.image_block_slot(key) {
                    self.touch_record(slot);
                }
            }
            true
        }

        /// 是否还有图片在后台解码（事件循环据此按活动帧率醒来取结果）。
        pub fn has_pending_image_previews(&self) -> bool {
            self.image_previews.borrow().is_pending()
        }

        /// 把一次工具调用回的视觉附件交给图片预览（宿主回填结果时调用）。
        ///
        /// 附件一到就登记，卡片随即按**最终行数**占出图片区；解码在后台完成，
        /// 图片到达前先显示一行「正在准备图片」，不会出现「先排一次短的再把下面挤开」的跳变。
        pub fn register_tool_images(&mut self, call_id: &str, images: &[ToolImageAttachment]) {
            if call_id.is_empty() || images.is_empty() {
                return;
            }
            self.image_previews.borrow_mut().register(call_id, images);
            // 正文要从载荷换成图片区：那一条记录得重算显示行。
            if let Some(slot) = self.tool_card_slot(call_id) {
                self.touch_record(slot);
            }
        }

        /// 按 `call_id` 找工具卡在消息流里的下标。
        ///
        /// 匹配规则与 [`Self::tool_card_mut`] 完全一致（包括「空 id 退化为最近的运行中卡片」），
        /// 因此可以用它定位就地修改过的卡片、做增量失效；没有命中时返回 `None`。
        pub(super) fn tool_card_slot(&self, call_id: &str) -> Option<usize> {
            self.records
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, record)| {
                    let Record::Tool(card) = record else {
                        return None;
                    };
                    let hit = if call_id.is_empty() {
                        card.status == ToolStatus::Running
                    } else {
                        card.call_id == call_id
                    };
                    hit.then_some(index)
                })
        }

        /// 图片锚点落在哪条记录上：工具卡（`call_id`）或用户消息（`image_key`）。
        ///
        /// 两类记录共用同一份 [`ImagePreviews`]，解码完成后都要重算自己那一条的显示行，
        /// 因此这里按同一把键同时找两处，调用方不必知道键是哪一类。
        pub(super) fn image_block_slot(&self, key: &str) -> Option<usize> {
            if key.is_empty() {
                return self.tool_card_slot(key);
            }
            self.records
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, record)| match record {
                    Record::Tool(card) if card.call_id == key => Some(index),
                    Record::User(message) if message.image_key == key => Some(index),
                    _ => None,
                })
                .or_else(|| self.tool_card_slot(key))
        }

        /// 让仍在运行的工具卡保持「活」的耗时显示。
        ///
        /// 卡片标题里的耗时（`<1s` 时是毫秒）逐帧在变，而显示行是缓存的：
        /// 有回合在跑时逐帧把这些卡片重新置脏，帧率与以前一致，代价只落在这一两张卡上。
        pub(super) fn touch_running_tools(&mut self) {
            let dirty: Vec<usize> = self
                .records
                .iter()
                .enumerate()
                .filter(|(_, record)| {
                    matches!(record, Record::Tool(card) if card.status == ToolStatus::Running)
                })
                .map(|(index, _)| index)
                .collect();
            for index in dirty {
                self.touch_record(index);
            }
        }

        /// 展开一张工具卡的完整正文；提示行点击触发。
        pub fn expand_tool(&mut self, call_id: &str) {
            if call_id.is_empty() {
                return;
            }
            self.expanded_tools.insert(call_id.to_string());
            // 展开态只影响这一张卡片：增量重算它本身，不用整块重排。
            if let Some(index) = self.tool_card_slot(call_id) {
                self.touch_record(index);
            }
        }

        /// 点开着的工具卡收起；返回是否真的从展开态收了回去。
        ///
        /// 对映 Python `ToolDisclosure.on_click`：缩略态点卡片不做任何事（展开只能
        /// 通过提示行），所以这里没有 toggle，只有单向收起。
        pub fn collapse_tool(&mut self, call_id: &str) -> bool {
            if call_id.is_empty() || !self.expanded_tools.remove(call_id) {
                return false;
            }
            if let Some(index) = self.tool_card_slot(call_id) {
                self.touch_record(index);
            }
            true
        }

        pub fn is_tool_expanded(&self, call_id: &str) -> bool {
            self.expanded_tools.contains(call_id)
        }

        /// 点击思考段：在折叠与展开之间切换；返回切换后的展开态。
        pub fn toggle_reasoning_expanded(&mut self, index: usize) -> bool {
            let expanded = if self.expanded_reasoning.remove(&index) {
                false
            } else {
                self.expanded_reasoning.insert(index);
                true
            };
            // 用户主动查看：不再逐帧铺开，剩下的内容当场补全（展开态尤其要看到全文）。
            self.settle_reasoning_reveal();
            // 展开态变了：只需要重算这一段思考。
            self.touch_record(index);
            expanded
        }

        pub fn is_reasoning_expanded(&self, index: usize) -> bool {
            self.expanded_reasoning.contains(&index)
        }

        /// 当前文本选区（鼠标拖选）；没有选区时返回 `None`。
        pub fn selection(&self) -> Option<TextSelection> {
            self.selection
        }

        /// 按下左键：开一个新选区的锚点（`line` 为显示行下标，`column` 为行内显示列）。
        pub fn begin_selection(&mut self, line: usize, column: usize) {
            self.selection = Some(TextSelection {
                anchor: (line, column),
                head: (line, column),
            });
            self.touch_conversation_view();
        }

        /// 拖动中：把选区的另一端移到新位置。
        pub fn extend_selection(&mut self, line: usize, column: usize) {
            if let Some(selection) = self.selection.as_mut() {
                selection.head = (line, column);
            }
            self.touch_conversation_view();
        }

        /// 清掉选区（Esc / 点击空白 / 复制完成后）。
        pub fn clear_selection(&mut self) {
            self.selection = None;
            self.touch_conversation_view();
        }

        /// 宿主每帧调一次：把过期的提示行真的清掉（渲染只读，不负责回收）。
        /// 流式 / 压缩阶段的工具调用侧信道状态（渲染只读）。
        pub fn streaming_tool(&self, call_id: &str) -> Option<&StreamingTool> {
            self.streaming_tools.get(call_id)
        }

        /// 按 `call_id` 找最近一张工具卡；调用没有 id 时退化为最近一张仍在运行的工具卡。
        /// 输入区边框上 `[ ESC ]` 的屏幕矩形（上一帧渲染登记的，没有则为 `None`）。
        pub fn runtime_esc_area(&self) -> Option<ratatui::layout::Rect> {
            self.runtime_esc_area.get()
        }

        /// 渲染路径登记 `[ ESC ]` 的位置（渲染只拿 `&AppState`，因此用 `Cell` 写入）。
        pub fn set_runtime_esc_area(&self, area: Option<ratatui::layout::Rect>) {
            self.runtime_esc_area.set(area);
        }

        /// 输入框插入点本帧的屏幕坐标（上一帧渲染登记的，没有则为 `None`）。
        pub fn composer_caret(&self) -> Option<ratatui::layout::Position> {
            self.composer_caret.get()
        }

        /// 渲染路径登记输入框插入点位置（同 [`Self::set_runtime_esc_area`]，`Cell` 写入）。
        pub fn set_composer_caret(&self, position: Option<ratatui::layout::Position>) {
            self.composer_caret.set(position);
        }

        /// 欢迎页徽标各可点段的屏幕矩形（上一帧渲染登记的；没有徽标时全是 `None`）。
        pub fn welcome_badge_areas(&self) -> [Option<ratatui::layout::Rect>; BADGE_SEGMENT_COUNT] {
            self.welcome_badge_areas.get()
        }

        /// 渲染路径登记徽标各段的屏幕矩形（同 [`Self::set_runtime_esc_area`]）。
        pub fn set_welcome_badge_areas(
            &self,
            areas: [Option<ratatui::layout::Rect>; BADGE_SEGMENT_COUNT],
        ) {
            self.welcome_badge_areas.set(areas);
        }

        /// 正在被悬停的徽标段下标（渲染层据此把它描成淡黄色）。
        pub fn welcome_badge_hover(&self) -> Option<usize> {
            self.welcome_badge_hover
        }

        /// 落点命中的徽标段跳转目标（用上一帧渲染登记的矩形）；没命中徽标时为 `None`。
        pub fn welcome_badge_link_at(
            &self,
            column: u16,
            row: u16,
        ) -> Option<crate::ui::fullscreen::rendering::welcome_logo::BadgeLink> {
            let point = ratatui::layout::Position::new(column, row);
            let slot = self
                .welcome_badge_areas
                .get()
                .iter()
                .position(|area| area.is_some_and(|area| area.contains(point)))?;
            let badge = self.logo.subtitle()?;
            crate::ui::fullscreen::rendering::welcome_logo::badge_segments(badge)
                .get(slot)
                .map(|segment| segment.link)
        }

        /// 记录徽标悬停段：颜色随显示行一起烘焙，变化时要让会话块重算。
        pub fn set_welcome_badge_hover(&mut self, hover: Option<usize>) {
            if self.welcome_badge_hover == hover {
                return;
            }
            self.welcome_badge_hover = hover;
            self.touch_conversation();
        }

        /// 正在被悬停的「立即结束」按钮属于哪张卡（渲染层据此把它染成淡黄色）。
        pub fn stop_button_hover(&self) -> Option<&str> {
            self.stop_button_hover.as_deref()
        }

        /// 某张卡上的「立即结束」按钮该结束哪个后台任务；不是运行中的 monitor 卡时 `None`。
        ///
        /// 卡片已经不在（回放过的事件流里没有它）时返回 `None`。
        pub fn monitor_task_id(&self, call_id: &str) -> Option<String> {
            if let Some(monitor_id) = call_id.strip_prefix("monitor:").filter(|id| !id.is_empty()) {
                return Some(monitor_id.to_string());
            }
            let card = self.tool_card(call_id)?;
            card.monitor_task_id(self.streaming_tool(call_id))
        }

        /// 记录本轮悬停命中；变化时让受影响的两张卡重算显示行。
        ///
        /// 按钮颜色是随显示行一起烘焙的，所以只 `bump` 重画不够——旧卡要褪回灰色、
        /// 新卡要转成淡黄，两条记录都得重算；没变化时直接返回，鼠标划过每张卡不会白算。
        pub fn set_stop_button_hover(&mut self, call_id: Option<String>) {
            if self.stop_button_hover == call_id {
                return;
            }
            let previous = self.stop_button_hover.take();
            self.stop_button_hover = call_id;
            for key in [previous, self.stop_button_hover.clone()]
                .into_iter()
                .flatten()
            {
                if let Some(index) = self.tool_card_slot(&key) {
                    self.touch_record(index);
                }
            }
            self.touch_conversation_view();
        }

        /// 正在被悬停的「重试」按钮属于哪条失败记录（渲染层据此把它染成淡黄色）。
        pub fn retry_button_hover(&self) -> Option<usize> {
            self.retry_button_hover
        }

        /// 记录本轮重试按钮的悬停命中；变化时重算受影响的那条失败记录。
        pub fn set_retry_button_hover(&mut self, index: Option<usize>) {
            if self.retry_button_hover == index {
                return;
            }
            let previous = self.retry_button_hover;
            self.retry_button_hover = index;
            for slot in [previous, index].into_iter().flatten() {
                self.touch_record(slot);
            }
            self.touch_conversation_view();
        }

        /// 只读取工具卡（按 `call_id`；空 id 不匹配，避免误命中别的调用）。
        pub fn tool_card(&self, call_id: &str) -> Option<&ToolCard> {
            if call_id.is_empty() {
                return None;
            }
            self.records.iter().rev().find_map(|record| match record {
                Record::Tool(card) if card.call_id == call_id => Some(card),
                _ => None,
            })
        }

        pub(super) fn tool_card_mut(&mut self, call_id: &str) -> Option<&mut ToolCard> {
            self.records.iter_mut().rev().find_map(|record| {
                let Record::Tool(card) = record else {
                    return None;
                };
                let hit = if call_id.is_empty() {
                    card.status == ToolStatus::Running
                } else {
                    card.call_id == call_id
                };
                hit.then_some(card)
            })
        }

        pub(super) fn update_tool(
            &mut self,
            call: &omnicrawl_core::ToolCall,
            result: &omnicrawl_core::ToolResult,
            now: Instant,
        ) {
            let Some(index) = self.tool_card_slot(&call.id) else {
                return;
            };
            if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                card.status = match (result.ok, result.error_code.as_deref()) {
                    (true, _) => ToolStatus::Ok,
                    (false, Some(host::DENIED)) => ToolStatus::Denied,
                    (false, _) => ToolStatus::Failed,
                };
                card.elapsed = Some(now.saturating_duration_since(card.started));
                card.body = crate::ui::fullscreen::rendering::tool_diff::stored_result_text(
                    &card.name,
                    &result.output,
                );
            }
            self.touch_record(index);
        }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    /// 原始参数（模型给的那份）：标题行走对映层的 `tool_disclosure_title`，
    /// 它要按参数拼出路径 / 命令 / 行数统计，因此不能只留 `summary` 文本。
    pub arguments: serde_json::Value,
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
    pub(crate) started: Instant,
    pub body: String,
}

impl ToolCard {
    /// 当前耗时：终态用落定值，运行中按起始时刻实时算。
    ///
    /// 对映 Python `ToolDisclosure` 标题行的活动计时器：运行中的卡片也要显示已耗时，
    /// 收口后不再变化（渲染保持只读，只是读了一个随时间变化的量）。
    pub fn live_elapsed_seconds(&self) -> f64 {
        match self.elapsed {
            Some(duration) => duration.as_secs_f64(),
            None => self.started.elapsed().as_secs_f64(),
        }
    }

    /// 这张卡上的「立即结束」按钮该结束哪个后台任务；不是运行中的 monitor 卡时 `None`。
    ///
    /// 两类卡片都要认：模型调 `monitor` 工具形成的真实工具卡（`call_id` 是内核 id，
    /// 任务 id 在参数里），以及本地归并出来的 `monitor:<id>` 卡（任务 id 在 `call_id` 里）。
    /// 参数还在流里时卡片自己的参数是空的，任务 id 只存在于侧信道的半截 JSON 里。
    pub fn monitor_task_id(&self, streaming: Option<&StreamingTool>) -> Option<String> {
        if let Some(monitor_id) = self
            .call_id
            .strip_prefix("monitor:")
            .filter(|id| !id.is_empty())
        {
            return Some(monitor_id.to_string());
        }
        if self.status != ToolStatus::Running || !is_monitor_tool(&self.name) {
            return None;
        }
        [
            Some(&self.arguments),
            streaming.map(|entry| &entry.arguments),
        ]
        .into_iter()
        .flatten()
        .find_map(|arguments| {
            arguments
                .get("monitor_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|monitor_id| !monitor_id.is_empty())
                .map(str::to_string)
        })
    }
}

/// 这张卡是不是模型调 `monitor` 工具形成的卡（工具名可带命名空间前缀）。
///
/// 与 `ui::conversation::is_monitor_card` 同一判据：`monitor` 工具卡与本地归并出来的
/// `monitor:<id>` 卡都算后台任务卡。
pub fn is_monitor_tool(name: &str) -> bool {
    name.rsplit('.').next().unwrap_or(name) == "monitor"
}

/// 一次工具调用在「参数还在流里」阶段的侧信道状态（按 call_id 索引）。
///
/// 不塞进 `ToolCard` 的理由：卡片由批次执行（`tool.started`）创建，而这里的条目从模型
/// 刚吐出 `tool.started`（`turn.tool_call_started`）就存在，两者生命周期不同；压缩提示
/// 也要在批次执行之后继续挂在卡片上方，所以整轮都留着，回合结束再清。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingTool {
    /// 累积的 arguments 原文（可能还是半截 JSON）。
    pub text: String,
    /// 容错解析出来的参数：流式阶段标题行与文件预览都从这里取。
    pub arguments: serde_json::Value,
    /// 压缩阶段提示（`正在压缩…`）或失败文案（压缩超时/压缩失败）。
    pub compression: Option<String>,
    /// 提示是不是失败文案：渲染时用红色，好和正常的计量提示区分。
    pub compression_failed: bool,
    /// 是否仍在写参数（批次执行开始后置 false，卡片正文改由真实载荷驱动）。
    pub streaming: bool,
}

/// 一条用户消息：正文 + 图片预览锚点 + 随行文档路径。
///
/// 图片 Base64 只在提交/重放时短暂存在，交给 [`ImagePreviews`] 解码后立即释放；
/// 记录里只留锚点，否则每条历史消息都会把整张原图常驻在内存里。
/// 文档只留路径（本来就是磁盘上的文件，没有可缓存的内容），渲染时逐行显示在正文下方。
#[derive(Debug, Clone, PartialEq)]
pub struct UserRecord {
    pub text: String,
    /// 图片预览在 [`ImagePreviews`] 里的锚点（与工具卡用 `call_id` 同一思路）。
    ///
    /// 每条用户消息一个：同一条消息的多张图属于同一块，滚动/重算都按这个键找回。
    pub image_key: String,
    /// 随这条消息发出的文档完整路径（多个按粘贴顺序上下排列）。
    pub documents: Vec<String>,
}

impl UserRecord {
    /// 纯文本消息（没有粘贴图片，或旧重放路径没带图片）。
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            image_key: String::new(),
            documents: Vec::new(),
        }
    }

    /// 这条消息会不会出图。
    pub fn has_images(&self) -> bool {
        !self.image_key.is_empty()
    }

    /// 这条消息有没有随行文档。
    pub fn has_documents(&self) -> bool {
        !self.documents.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    User(UserRecord),
    Reasoning(String),
    Assistant(String),
    Tool(ToolCard),
    Notice(String),
    /// 回合失败的提示：正文 + 是否提供「重试」按钮。
    ///
    /// 只有**可重发**的失败才带按钮（网络中断、上游 5xx、重试用尽）：点击后用原样的
    /// 用户消息再提交一次，等价于用户手动重发，从而重新进入自动重试循环。
    /// 取消、上下文超限、工具状态被拒这类需要先改输入或先压缩的失败不带按钮。
    Failure {
        message: String,
        retryable: bool,
    },
    /// 一个 SubAgent 批次在宿主侧的独立窗口受理提示。
    ///
    /// 子代理的过程文本与工具调用不再进消息流（它们由 `subagent-window` 子进程渲染），
    /// 消息流里只留这一行，避免把主会话撑成进度树。
    SubagentWindow(SubagentWindowNotice),
}

/// 窗口受理提示：批次号 + 流水文件（用户可据此自己重开窗口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentWindowNotice {
    pub batch_id: String,
    pub stream: String,
}

impl SubagentWindowNotice {
    /// 消息流里那一行（渲染成 `· ` 提示行）。
    pub fn line(&self) -> String {
        format!(
            "子代理在独立窗口运行（批次 {}，结束自动关闭）；流水：{}",
            self.batch_id, self.stream
        )
    }
}

/// 子代理窗口流水的默认落盘目录：与诊断日志同根（`~/.omnicrawl/logs/subagents`）。
///
/// 放在日志目录下是因为它和 `tui.log` 一样是「现场」，任务结束后仍值得回看。
pub(crate) fn default_subagent_stream_root() -> PathBuf {
    user_config_dir(&ConfigEnvironment::from_process())
        .join("logs")
        .join("subagents")
}

/// Monitor 任务状态 → 卡片状态：字符串取值与 `MonitorManager` 的快照同源。
///
/// `stopped`（被显式终止）归「成功」是因为它同样是终态；用词不精确但原始状态字符串
/// 仍在卡片正文里（`Monitor · id · stopped`），不必为了措辞新增一个渲染分支。
pub(crate) fn monitor_status(status: &str) -> ToolStatus {
    match status {
        "running" => ToolStatus::Running,
        "failed" => ToolStatus::Failed,
        _ => ToolStatus::Ok,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnState {
    Idle,
    Running { turn_id: String },
}

impl TurnState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Running { turn_id } => Some(turn_id),
            Self::Idle => None,
        }
    }
}

/// 上一个回合的结局：终端标签页标题据此显示 ✓ / ⩍ / ✕。
///
/// `None` 表示还没跑过回合（标题仍是待机标记）。进行中不看这里，看 [`TurnState`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// 回合正常收尾。
    Done,
    /// 回合收尾但需用户留意：被模型暂停（`turn.finished` 的 `paused`）。
    Warning,
    /// 回合以失败结束：上游错误、取消、内核退出。
    Failed,
}

/// 子任务状态是否已是终态（独立窗口用它挡住迟到的活动事件）。
pub fn subagent_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "approval_cancelled"
    )
}
