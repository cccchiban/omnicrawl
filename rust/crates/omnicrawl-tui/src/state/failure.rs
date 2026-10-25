//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
        /// 回合失败或取消：状态复位并把原因写进消息流。
        ///
        /// `retryable` 决定这条失败提示带不带「重试」按钮：网络中断、上游 5xx、自动重试用尽
        /// 这类「原样重发就有机会成功」的失败要带；取消、上下文超限这类需要先改输入的不带。
        pub fn fail_turn(&mut self, message: String) {
            self.fail_turn_with(message, false)
        }

        /// 带重试能力的回合失败收尾。
        pub fn fail_turn_with(&mut self, message: String, retryable: bool) {
            self.turn = TurnState::Idle;
            self.turn_started = None;
            self.status = None;
            // 回合失败/取消时不能留下一直转圈的卡片。
            self.close_running_cards("工具调用在回合结束前未收到结果。");
            // 中途停下的思考不再逐帧铺开：当场补全到已到达的长度，免得停在半句。
            self.settle_reasoning_reveal();
            self.records.push(Record::Failure { message, retryable });
            self.touch_appended();
            self.touch_composer();
        }

        /// 最近一条失败记录的下标（重试按钮以它为锚点重发）。
        pub fn last_failure_slot(&self) -> Option<usize> {
            self.records
                .iter()
                .rposition(|record| matches!(record, Record::Failure { .. }))
        }

        /// 失败记录是否可重试。
        pub fn failure_is_retryable(&self, index: usize) -> bool {
            matches!(
                self.records.get(index),
                Some(Record::Failure {
                    retryable: true,
                    ..
                })
            )
        }

        /// 重试某条失败回合：取失败前最近一条用户消息的文本重发。
        ///
        /// 只返回文本：失败那一轮已经把图片交过一次，重放会让同一张图在上下文里出现两遍。
        /// 失败记录已被后续回合盖过（找不到用户消息）时返回 `None`。
        pub fn retry_failure(&mut self, index: usize) -> Option<String> {
            if !self.failure_is_retryable(index) {
                return None;
            }
            let previous = self
                .records
                .get(..index)?
                .iter()
                .rposition(|record| matches!(record, Record::User(_)))?;
            let Some(Record::User(user)) = self.records.get(previous) else {
                return None;
            };
            let text = user.text.clone();
            // 旧的失败提示已经过时：标记成不可重试，避免连点重复重发。
            if let Some(Record::Failure { retryable, .. }) = self.records.get_mut(index) {
                *retryable = false;
            }
            self.touch_record(index);
            Some(text)
        }

}
