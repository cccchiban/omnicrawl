//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
        /// 追加一段子代理过程文本（子代理窗口内的 `assistant` 记录）。
        ///
        /// 正文是逐片到达的：合并进末尾那条正文记录，否则每一片都会单开一条消息。
        pub fn subagent_text(&mut self, text: String) {
            if let Some(Record::Assistant(body)) = self.records.last_mut() {
                body.push_str(&text);
            } else {
                self.records.push(Record::Assistant(text));
                return self.touch_appended();
            }
            let index = self.records.len() - 1;
            self.touch_record(index);
        }

        /// 追加一段子代理思考（子代理窗口内的 `reasoning` 记录）。
        ///
        /// 思考增量是逐片到达的：合并进末尾那条思考记录，否则每一片都会单开一个折叠块。
        /// 同时挂上逐帧显现游标，与主代理的思考段同速铺开。
        pub fn subagent_reasoning(&mut self, text: String) {
            let appended = text.chars().count();
            let index = self.records.len().saturating_sub(1);
            match self.records.last_mut() {
                Some(Record::Reasoning(body)) => body.push_str(&text),
                _ => {
                    self.records.push(Record::Reasoning(text));
                    return self.start_reasoning_reveal(self.records.len() - 1, appended);
                }
            }
            self.start_reasoning_reveal(index, appended);
        }

        /// 子代理工具开始：挂一张运行中的工具卡（与主代理的卡片同形）。
        pub fn subagent_tool_started(
            &mut self,
            call_id: String,
            name: String,
            arguments: serde_json::Value,
        ) {
            if !call_id.is_empty() && self.tool_card_mut(&call_id).is_some() {
                return;
            }
            let summary =
                host::summarize_arguments(arguments.as_object().unwrap_or(&serde_json::Map::new()));
            self.records.push(Record::Tool(ToolCard {
                call_id,
                name,
                summary,
                arguments,
                status: ToolStatus::Running,
                elapsed: None,
                started: Instant::now(),
                body: Vec::new(),
            }));
            self.touch_appended();
        }

        /// 子代理工具结束：收口对应卡片的状态、耗时与正文。
        pub fn subagent_tool_completed(
            &mut self,
            call_id: &str,
            ok: bool,
            output: &str,
            seconds: Option<f64>,
        ) {
            let Some(index) = self.tool_card_slot(call_id) else {
                return;
            };
            let Some(Record::Tool(card)) = self.records.get_mut(index) else {
                return;
            };
            card.status = if ok {
                ToolStatus::Ok
            } else {
                ToolStatus::Failed
            };
            card.elapsed = Some(Duration::from_secs_f64(seconds.unwrap_or(0.0)));
            card.body = output
                .lines()
                .map(|line| line.trim_end().to_string())
                .collect();
            self.touch_record(index);
        }

        /// 子代理窗口的运行状态行（画在输入区方框上边框上，与主代理同一位置）。
        ///
        /// 只翻转运行态、不重置起始时刻：`turn_started` 是 spinner 的时基，每帧重置会让
        /// 动画停在第一格。已经处于目标状态时直接返回，避免每帧把输入块置脏。
        pub fn set_subagent_running(&mut self, running: bool) {
            if self.turn.is_running() == running {
                return;
            }
            self.turn = if running {
                TurnState::Running {
                    turn_id: "subagent".to_string(),
                }
            } else {
                TurnState::Idle
            };
            self.turn_started = running.then(Instant::now);
            self.touch_composer();
        }

        /// 压缩后刷新遥测：上下文占用与 ↑ 换成压缩后的真实大小，↓/† 清零。
        ///
        /// 压缩发生在回合收尾之后，此后不会再发模型请求，`turn.token_usage` 会一直停在压缩前那次
        /// 的用量上；不在这里改写，底部遥测就会显示一个已经被替换掉的旧上下文大小。
        ///
        /// ↓（本次输出）与 †（缓存命中输入）属于**那一次已经过去的请求**，压缩后不再成立，因此清零
        /// 而不是保留；缓存率由 ↑/† 现算，清零后自然回到 `CH0%`。速率是会话累计均值，与上下文大小无关，不动。
        ///
        /// 底部文本由 `refresh_carousel` 每帧按最新遥测重建，这里只改数值即可在下一帧显示。
        pub fn refresh_after_compaction(&mut self, context_tokens: i64) {
            self.telemetry.input_tokens = context_tokens.max(0) as u64;
            self.telemetry.output_tokens = 0;
            self.telemetry.cached_input_tokens = 0;
        }

        /// 打开 `/sessions` 的会话选择菜单（输入框上方）。
        pub fn open_sessions_menu(&mut self, items: Vec<SessionMenuItem>) {
            self.sessions_menu.open(items);
            self.touch_composer();
        }

        /// 归并一条后台任务日志（工具卡形状）；不改动回合状态。
        ///
        /// 与 Python 的差异（Rust 侧刻意）：Python 把每个 Monitor 批次当 `tool` 消息追加进
        /// 对话区，同一个任务每 0.5 秒就多一条卡片；这里按 `monitor_id` 归并到同一张卡片——
        /// 状态行原地替换为最新快照，事件行追加到正文尾部。因此一个后台任务在会话流里
        /// 始终只占一张卡。`call_id` 用 `monitor:<id>` 前缀既做归并键，也避免与真实工具
        /// 调用的 id 相撞（工具卡靠它做展开/收起）。
        ///
        /// `text` 是 `format_monitor_display_batch` 的输出：首行是状态行，其余是本次事件行。
        pub fn push_monitor_batch(&mut self, monitor_id: &str, status: &str, text: String) {
            let call_id = format!("monitor:{monitor_id}");
            let mut lines = text.lines();
            // 首行是任务状态行，其余是本次增量的事件行。
            let header = lines.next().unwrap_or_default().to_string();
            let events: Vec<String> = lines.map(str::to_string).collect();
            let status = monitor_status(status);
            if let Some(index) = self.tool_card_slot(&call_id) {
                if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                    card.status = status;
                    // 状态行原地替换为最新快照，事件行接在尾部。
                    if card.body.is_empty() {
                        card.body.push(header);
                    } else {
                        card.body[0] = header;
                    }
                    card.body.extend(events);
                    // 终态后不再计时：卡片会一直留在会话流里，不能让耗时继续涨。
                    if status != ToolStatus::Running {
                        card.elapsed = Some(Instant::now().saturating_duration_since(card.started));
                    }
                }
                self.touch_record(index);
                return;
            }
            self.records.push(Record::Tool(ToolCard {
                call_id,
                name: "monitor".to_string(),
                summary: monitor_id.to_string(),
                // Monitor 批次不是模型发起的工具调用，没有参数可拼标题。
                arguments: serde_json::Value::Null,
                status,
                elapsed: None,
                started: Instant::now(),
                body: std::iter::once(header).chain(events).collect(),
            }));
            self.touch_appended();
        }

}
