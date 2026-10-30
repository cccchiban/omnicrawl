//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
        /// 待决审批对应的调用（工具名 + 当前参数）；没有待决审批时 `None`。
        ///
        /// 拒绝时的 MCP 审计要用它，而且要用批次里的**当前**参数——审计应与用户看到的、
        /// 执行层拿到的版本一致。
        pub fn pending_approval_call(&self) -> Option<omnicrawl_core::ToolCall> {
            let batch = self.batch.as_ref()?;
            if !matches!(batch.waiting(), Some(host::Waiting::Approval(_))) {
                return None;
            }
            batch.current().map(|(_, call)| call.clone())
        }

        /// 待决批次的请求 id：界面把决定回给内核时要用它配对。
        pub fn batch_request_id(&self) -> Option<Id> {
            self.batch.as_ref().map(|batch| batch.request_id().clone())
        }

        /// 开始处理一个工具批次，推进到等待点、执行点或整批结束。
        pub fn start_batch(
            &mut self,
            request_id: Id,
            calls: Vec<omnicrawl_core::ToolCall>,
        ) -> host::BatchStep {
            self.start_batch_quiet(request_id, calls, false)
        }

        /// 开始一个子代理批次的工具批次：照旧执行并回填结果，但不在主会话里挂工具卡
        /// （子代理的过程由它自己的窗口渲染）。
        pub fn start_subagent_batch(
            &mut self,
            request_id: Id,
            calls: Vec<omnicrawl_core::ToolCall>,
        ) -> host::BatchStep {
            self.start_batch_quiet(request_id, calls, true)
        }

        pub(super) fn start_batch_quiet(
            &mut self,
            request_id: Id,
            calls: Vec<omnicrawl_core::ToolCall>,
            quiet: bool,
        ) -> host::BatchStep {
            let mut batch = host::PendingBatch::new(request_id, calls);
            let step = {
                let mut ctx = self.context();
                batch.advance(&mut ctx)
            };
            self.batch = Some(batch);
            self.batch_quiet = quiet;
            // 待决面板与任务清单都在输入块里。
            self.touch_composer();
            step
        }

        pub fn select_question(&mut self, delta: isize) {
            if let Some(batch) = self.batch.as_mut() {
                batch.select_question(delta);
            }
            self.touch_composer();
        }

        /// 回答待决提问；返回下一步（派发执行或整批结束）。
        pub fn answer_question(&mut self, answer: String) -> Option<host::BatchStep> {
            let mut batch = self.batch.take()?;
            let step = {
                let mut ctx = self.context();
                batch.answer(answer, &mut ctx)
            };
            let step = step?;
            self.batch = Some(batch);
            // 待决面板与任务清单都在输入块里。
            self.touch_composer();
            Some(step)
        }

        /// 审批待决工具调用；返回下一步（派发执行或整批结束）。
        pub fn decide_approval(&mut self, approved: bool) -> Option<host::BatchStep> {
            let mut batch = self.batch.take()?;
            let step = {
                let mut ctx = self.context();
                batch.decide(approved, &mut ctx)
            };
            let step = step?;
            self.batch = Some(batch);
            self.touch_composer();
            Some(step)
        }

        /// 执行层回填一个调用的结果；返回是否整批就绪。
        pub fn record_tool_result(
            &mut self,
            index: usize,
            result: omnicrawl_core::ToolResult,
            vision: Option<host::VisionPayload>,
        ) -> bool {
            // 图片预览与「模型是否在看图」无关：附件到了就先交给后台解码，随后出示缩略图。
            if let Some(vision) = vision.as_ref() {
                let call_id = self
                    .batch
                    .as_ref()
                    .and_then(|batch| batch.calls().get(index))
                    .map(|call| call.id.clone())
                    .unwrap_or_default();
                self.register_tool_images(&call_id, &vision.images);
            }
            let ready = match self.batch.as_mut() {
                Some(batch) => batch.record_result(index, result, vision),
                None => false,
            };
            // 待决面板的进度（还差几个工具）画在输入块里。
            self.touch_composer();
            ready
        }

        /// 整批就绪时取走观察并卸下批次；`attach_images` 决定是否把图片注入下一步请求。
        pub fn take_observations(&mut self, attach_images: bool) -> Option<Vec<AgentLoopObservation>> {
            if !self.batch.as_ref().is_some_and(|batch| batch.is_ready()) {
                return None;
            }
            let batch = self.batch.take()?;
            self.batch_quiet = false;
            Some(batch.observations(attach_images))
        }

        /// 执行超时：把未回填的调用写成超时结果、把仍在运行的工具卡收口，并留一条提示；
        /// 返回是否整批就绪。
        pub fn fill_tool_timeout(&mut self, timeout_seconds: i64) -> bool {
            let Some(batch) = self.batch.as_mut() else {
                return false;
            };
            let timed_out = batch.fill_timeout(timeout_seconds);
            let ready = batch.is_ready();
            let now = Instant::now();
            for (_, call) in &timed_out {
                // 与 Python `_tool_timeout_result` 一致：只有 ok=false 与文案，没有错误码。
                let result = omnicrawl_core::ToolResult {
                    ok: false,
                    output: format!("工具执行超时（超过 {timeout_seconds} 秒未完成），已中止等待。"),
                    full_output: String::new(),
                    error_code: None,
                    retryable: false,
                };
                self.update_tool(call, &result, now);
            }
            if !timed_out.is_empty() && !self.batch_quiet {
                self.records.push(Record::Notice(format!(
                    "工具执行超过 {timeout_seconds} 秒未完成，已按超时继续（后台结果会被丢弃）。"
                )));
            }
            ready
        }

        /// 取消当前批次（`Esc`）：批次卸下，已在执行的工具由取消令牌回收。
        pub fn cancel_batch(&mut self) {
            self.batch = None;
            self.batch_quiet = false;
            self.touch_composer();
        }

        /// 执行阶段开始：先落一张「运行中」工具卡。
        /// 把仍在「调用中」的卡片收口为失败并补一句原因（回合结束、回合失败时用）。
        pub(super) fn close_running_cards(&mut self, message: &str) {
            let now = Instant::now();
            let pending: Vec<usize> = self
                .records
                .iter()
                .enumerate()
                .filter_map(|(index, record)| match record {
                    Record::Tool(card) if card.status == ToolStatus::Running => Some(index),
                    _ => None,
                })
                .collect();
            for index in pending {
                if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                    card.status = ToolStatus::Failed;
                    card.elapsed = Some(now.saturating_duration_since(card.started));
                    if card.body.is_empty() {
                        card.body = message.to_string();
                    }
                }
                self.touch_record(index);
            }
        }

        /// 批次里**已经有结果**、但卡片还停在「调用中」的调用收口；返回收口的数量。
        ///
        /// `update_todos` / `pause_work` / `ask_user` 由批次推进逻辑在宿主内直接写结果
        /// （`PendingBatch::advance` / `answer`），从不经过执行层，也就永远不会走到
        /// `finish_tool_run`——卡片于是停在「调用中」、计时一直涨。用户报的
        /// 「ask_user 提问结束后不显示结束、还在计时」就是这个成因。
        pub fn settle_internal_batch_calls(&mut self) -> usize {
            let Some(batch) = self.batch.as_ref() else {
                return 0;
            };
            let pairs: Vec<(omnicrawl_core::ToolCall, omnicrawl_core::ToolResult)> = batch
                .calls()
                .iter()
                .zip(batch.results())
                .filter_map(|(call, result)| result.clone().map(|result| (call.clone(), result)))
                .collect();
            let now = Instant::now();
            let mut settled = 0;
            for (call, result) in pairs {
                // 空 id 不能参与匹配（`tool_card_mut` 对空 id 的语义是「最近一张运行中的卡」）。
                if call.id.is_empty() {
                    continue;
                }
                let pending = match self.tool_card(&call.id) {
                    Some(card) => card.status == ToolStatus::Running,
                    // 卡片不存在也补一张：这些调用在 Python 侧本来就有工具卡。
                    None => true,
                };
                if !pending {
                    continue;
                }
                self.begin_tool_run(&call, now);
                self.finish_tool_run(&call, &result, now);
                settled += 1;
            }
            settled
        }

        pub fn begin_tool_run(&mut self, call: &omnicrawl_core::ToolCall, now: Instant) {
            // 流式阶段（`turn.tool_call_started`）可能已经为这次调用立过卡片：
            // 同一 `call_id` 只保留**一张**卡片并就地置为执行中。否则会留下两张卡，
            // 而流式那张永远停在「调用中」——看起来就像工具调用一直不结束，
            // 也像「上一个工具还没完就开始跑下一轮」。
            if let Some(entry) = self.streaming_tools.get_mut(&call.id) {
                // 批次真正开始执行：参数以卡片上的真实载荷为准，不再用半截 JSON 预览。
                entry.streaming = false;
            }
            let arguments = serde_json::Value::Object(call.arguments.clone());
            let summary = host::summarize_arguments(&call.arguments);
            if !call.id.is_empty() {
                if let Some(card) = self.tool_card_mut(&call.id) {
                    card.name = call.name.clone();
                    card.summary = summary;
                    card.arguments = arguments;
                    card.status = ToolStatus::Running;
                    card.elapsed = None;
                    return;
                }
            }
            // 子代理批次：执行照旧，但卡片不属于主代理的会话区。
            if self.batch_quiet {
                return;
            }
            self.records.push(Record::Tool(ToolCard {
                call_id: call.id.clone(),
                name: call.name.clone(),
                summary,
                arguments,
                status: ToolStatus::Running,
                elapsed: None,
                started: now,
                body: String::new(),
            }));
        }

        /// 执行阶段结束：更新对应工具卡的状态、耗时与正文。
        pub fn finish_tool_run(
            &mut self,
            call: &omnicrawl_core::ToolCall,
            result: &omnicrawl_core::ToolResult,
            now: Instant,
        ) {
            self.update_tool(call, result, now);
        }

        pub(super) fn context(&mut self) -> BatchContext<'_> {
            BatchContext {
                approval: self.approval,
                todos: &mut self.todos,
                paused: &mut self.paused,
                // TUI 侧没有宿主工具事实表可给：删除意图识别按无事实处理。
                tools: None,
            }
        }

        pub(super) fn append_streamed(&mut self, text: &str, reasoning: bool) {
            let target = |record: &Record| match record {
                Record::Reasoning(_) => reasoning,
                Record::Assistant(_) => !reasoning,
                _ => false,
            };
            // 记录尾部可能压着一条会话区提示（`turn.notice`）：它插在正文中间只是中间多一行说明，
            // 但正文合并必须跳过它，否则一段回复会被拆成两条记录、中间硬生生断开。
            let target_index = self
                .records
                .iter()
                .rposition(|record| !matches!(record, Record::Notice(_)));
            if matches!(target_index.and_then(|index| self.records.get(index)), Some(record) if target(record))
            {
                let index = target_index.expect("目标记录存在");
                if let Some(record) = self.records.get_mut(index) {
                    match record {
                        Record::Reasoning(body) | Record::Assistant(body) => body.push_str(text),
                        _ => unreachable!("target 只匹配思考与正文记录"),
                    }
                }
                // 思考段逐帧显现：只按本次增量的字符数推进目标，不对整段重数。
                if reasoning {
                    let reveal = self.reasoning_reveal.get_mut();
                    if reveal.index != Some(index) {
                        reveal.start(index);
                    }
                    reveal.extend(index, text.chars().count());
                }
                // 正文逐片追加：只重算这一条记录（分块增量缓存的关键路径）。
                self.touch_record(index);
            } else {
                let body = text.to_string();
                self.records.push(if reasoning {
                    Record::Reasoning(body)
                } else {
                    Record::Assistant(body)
                });
                if reasoning {
                    let index = self.records.len() - 1;
                    let length = text.chars().count();
                    let reveal = self.reasoning_reveal.get_mut();
                    reveal.start(index);
                    reveal.extend(index, length);
                }
                self.touch_appended();
            }
        }

}
