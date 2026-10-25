//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
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
                card.body = body_lines(&result.output);
            }
            self.touch_record(index);
        }

}
