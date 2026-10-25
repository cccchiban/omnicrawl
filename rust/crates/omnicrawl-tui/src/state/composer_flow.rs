//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
        pub fn waiting(&self) -> Option<&Waiting> {
            self.batch.as_ref().and_then(|batch| batch.waiting())
        }

        /// 开始一个回合：用户消息落进消息流，输入框清空，滚动回到底部。
        pub fn begin_turn(&mut self, turn_id: String, text: String, images: Vec<TurnImageAttachment>) {
            self.push_user_message(text, images);
            self.turn = TurnState::Running { turn_id };
            self.turn_started = Some(Instant::now());
            self.status = None;
            self.paused = false;
            self.scroll_from_bottom = 0;
            // 状态行（方框上边框）与排队预览都跟着回合状态变。
            self.touch_composer();
        }

        /// 把一条用户消息推进记录流，并给它挂上图片预览。
        ///
        /// 图片在这里就交给后台解码（与工具附件同一入口）：记录按**最终行数**占出图片区，
        /// 解码期间先显示一行提示，不会出现「先排一次短的再被图片撑开」的跳变。
        pub fn push_user_message(&mut self, text: String, images: Vec<TurnImageAttachment>) {
            let image_key = if images.is_empty() {
                String::new()
            } else {
                self.next_user_image_key()
            };
            if !image_key.is_empty() {
                let attachments: Vec<ToolImageAttachment> = images
                    .iter()
                    .map(|image| ToolImageAttachment {
                        media_type: image.media_type.clone(),
                        data_base64: image.data_base64.clone(),
                        filename: String::new(),
                        detail: image.detail.clone(),
                    })
                    .collect();
                self.image_previews
                    .borrow_mut()
                    .register(&image_key, &attachments);
            }
            self.records.push(Record::User(UserRecord {
                text,
                images,
                image_key,
            }));
            self.touch_appended();
        }

        /// 用户消息图片块的锚点：进程内自增，不与工具调用的 `call_id` 撞车。
        pub(super) fn next_user_image_key(&mut self) -> String {
            self.user_image_sequence += 1;
            format!("user-image-{}", self.user_image_sequence)
        }

        /// 提交输入框内容；空输入返回 `None`。
        ///
        /// 文本与图片一起取走：图片占位符留在文本里（`[ #1 Image ]`），附件单独返回，
        /// 两者必须同时清空，否则会漏发或残留。
        pub fn submit(&mut self) -> Option<PendingSubmission> {
            if self.composer.is_empty() {
                return None;
            }
            let images = self.composer.take_images();
            let text = self.composer.take();
            self.touch_composer();
            Some(PendingSubmission { text, images })
        }

        /// 生成期间按 Enter：把输入排进 FIFO 队列（对映 Python `_pending_inputs.append`）。
        ///
        /// 图片跟着一起排队：生成期间粘贴的图片若只留在输入框状态里，回合结束时那条消息
        /// 会悄悄丢掉图片。
        pub fn queue_pending(&mut self, text: String, images: Vec<PendingImage>) {
            self.pending_inputs
                .push_back(PendingSubmission { text, images });
            self.sync_queue_expanded();
            self.touch_composer();
        }

        /// 撤回第 `index` 条排队消息并回填输入框；返回是否真的撤回。
        ///
        /// 守卫与 Python `_withdraw_pending_input` 一致：只在回合进行中、索引有效、
        /// 且当前没有待回答的提问（question / 确认）时执行——否则回填的内容会被
        /// 提问模式吞掉。撤回只回填、不自动发送，用户改完再按 Enter。
        pub fn withdraw_pending(&mut self, index: usize) -> bool {
            if !self.turn.is_running() || index >= self.pending_inputs.len() {
                return false;
            }
            if self.waiting().is_some() {
                return false;
            }
            let Some(submission) = self.pending_inputs.remove(index) else {
                return false;
            };
            self.composer.set_text(&submission.text);
            self.composer.restore_images(submission.images);
            self.sync_queue_expanded();
            self.touch_composer();
            true
        }

        /// 展开/收起排队预览中被折叠的条目；不超过可见上限时不动作。
        pub fn toggle_queue_expanded(&mut self) {
            if !queue::can_toggle_queue_expanded(self.pending_inputs.len(), QUEUE_PREVIEW_MAX_ROWS) {
                return;
            }
            self.pending_queue_expanded = !self.pending_queue_expanded;
            self.touch_composer();
        }

        /// 队列缩回可见上限内时退出展开态，避免残留无效的展开/收起行。
        pub(super) fn sync_queue_expanded(&mut self) {
            if self.pending_inputs.len() <= QUEUE_PREVIEW_MAX_ROWS {
                self.pending_queue_expanded = false;
            }
        }

        /// 当前排队条数：排空循环用它界定本轮最多能派发几条。
        pub fn pending_len(&self) -> usize {
            self.pending_inputs.len()
        }

        /// 取队首消息准备提交；回合进行中时不取（对映 Python 的排空循环守卫）。
        ///
        /// 模态页（设置面板）是否放行由调用方判断，与 Python 的
        /// `len(self.screen_stack) == 1` 守卫同义。
        pub fn take_next_pending(&mut self) -> Option<PendingSubmission> {
            if self.turn.is_running() {
                return None;
            }
            let submission = self.pending_inputs.pop_front()?;
            self.sync_queue_expanded();
            self.touch_composer();
            Some(submission)
        }

        /// 丢开全部排队消息（退出前收尾，对映 Python `_pending_inputs.clear()`）。
        pub fn clear_pending(&mut self) {
            self.pending_inputs.clear();
            self.pending_queue_expanded = false;
            self.touch_composer();
        }

}
