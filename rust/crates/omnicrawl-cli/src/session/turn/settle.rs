//! 由 `turn.rs` 按职责拆出，语义未变。

use super::*;

/// 回合内的续跑痕迹：待续文本、逐次续跑事件与终态。
///
/// 对应 Python `run_stream` 内联在编排壳里的局部变量。收尾时按同一顺序写进会话转录：
/// `user_message` → `run_guard_continue`×N → `run_guard_paused` / `assistant_message`
/// → `run_guard_continue_exhausted`。
pub(crate) struct TurnRunGuard {
    /// 本轮发给模型的用户文本（短「继续」已还原成上一轮的任务原文）。
    pub(super) user_text: String,
    /// 待续任务文本：用户发「继续」时是上一轮的任务原文，否则是本轮输入。
    pub(super) pending_user_text: String,
    /// 本轮用户就是发「继续」进来的。
    pub(super) continue_requested: bool,
    /// 收尾时用的执行清单（Stop 分支同步一次）。
    pub(super) todos: Vec<TodoItem>,
    /// 逐段循环的回复与推理（最终按 Python 的 join 规则合并）。
    pub(super) reply_parts: Vec<String>,
    pub(super) reasoning_parts: Vec<String>,
    /// 整回合是否有文本流式发过。
    pub(super) content_streamed: bool,
    /// 已自动续跑的次数。
    pub(super) followups: usize,
    /// 最近一次续跑理由。
    pub(super) reason: String,
    /// 逐次续跑事件载荷，按发生顺序。
    pub(super) continue_events: Vec<Value>,
    /// 本回合被 `pause_work` 主动暂停。
    pub(super) paused: bool,
    /// 续跑上限已用尽且仍未完成。
    pub(super) exhausted: bool,
}

impl TurnRunGuard {
    pub(crate) fn begin(user_text: &str, continue_requested: bool, pending_user_text: &str) -> Self {
        let pending = if continue_requested && !pending_user_text.trim().is_empty() {
            pending_user_text.trim().to_string()
        } else {
            user_text.to_string()
        };
        Self {
            user_text: user_text.to_string(),
            pending_user_text: pending,
            continue_requested,
            todos: Vec::new(),
            reply_parts: Vec::new(),
            reasoning_parts: Vec::new(),
            content_streamed: false,
            followups: 0,
            reason: String::new(),
            continue_events: Vec::new(),
            paused: false,
            exhausted: false,
        }
    }

    /// 并进一段循环的收尾事实。
    pub(crate) fn record_episode(&mut self, result: &AgentLoopResult) {
        continuation::record_episode(
            &mut self.reply_parts,
            &mut self.reasoning_parts,
            &mut self.content_streamed,
            &result.final_text,
            &result.reasoning,
            result.content_streamed,
        );
    }

    /// 记一次续跑：次数、理由与事件载荷。
    pub(crate) fn record_continue(&mut self, followup: usize, reason: &'static str) {
        self.followups = followup;
        self.reason = reason.to_string();
        self.continue_events
            .push(continuation::continue_event_payload(
                followup,
                reason,
                &self.pending_user_text,
            ));
    }

    /// 本回合发给模型的最终回复。
    pub(crate) fn final_reply(&self) -> String {
        continuation::joined_reply(&self.reply_parts)
    }

    /// 合并后的推理文本。
    pub(crate) fn final_reasoning(&self) -> String {
        continuation::joined_reasoning(&self.reasoning_parts)
    }

    /// `user_message` 事件载荷：待续文本总是写；清单只在「继续」回合写。
    ///
    /// `images` 是随本轮提问粘贴的图片：它们是**用户消息的一部分**，落进转录才能在
    /// `/resume`、重启与压缩投影之后仍然重建出同一条带图的消息。
    pub(crate) fn user_message_payload(&self, images: &[omnicrawl_ipc::bridge::TurnImageAttachment]) -> Value {
        let mut payload = json!({
            "content": self.user_text,
            "pending_user_text": self.pending_user_text,
        });
        if !images.is_empty() {
            payload["images"] = Value::Array(
                images
                    .iter()
                    .map(|image| {
                        json!({
                            "media_type": image.media_type,
                            "data_base64": image.data_base64,
                            "detail": image.detail,
                        })
                    })
                    .collect(),
            );
        }
        if self.continue_requested && !self.todos.is_empty() {
            payload["todo_items"] = continuation::todo_items_value(&self.todos);
        }
        payload
    }

    /// 助手消息载荷：有推理文本时带 `reasoning_content`。
    pub(crate) fn assistant_payload(&self, final_text: &str) -> Value {
        let mut payload = json!({"content": final_text.trim()});
        let reasoning = self.final_reasoning();
        if !reasoning.is_empty() {
            payload["reasoning_content"] = Value::from(reasoning);
        }
        payload
    }

    /// `run_guard_paused` 事件载荷。
    pub(crate) fn paused_payload(&self) -> Value {
        continuation::paused_event_payload(&self.user_text, &self.pending_user_text, &self.todos)
    }

    /// `run_guard_continue_exhausted` 事件载荷。
    pub(crate) fn exhausted_payload(&self) -> Value {
        continuation::exhausted_event_payload(
            self.followups,
            &self.pending_user_text,
            &self.reason,
            &self.todos,
        )
    }
}

/// 被取消回合的历史摘要（语义基准 Python `TurnLoopMixin._cancelled_turn_summary`）。
///
/// 只写纯文本助手消息（不写未配对的 `tool_calls`），带上本轮已执行工具的次数统计；
/// 这是紧接着的下一轮能延续上下文的依据。
pub(crate) fn cancelled_turn_summary(executed_tools: &[String]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for name in executed_tools {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(existing, _)| existing == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name.to_string(), 1)),
        }
    }
    if counts.is_empty() {
        return "（上一回合被取消，未生成最终回复，未执行任何工具）".to_string();
    }
    let summary = counts
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name}×{count}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("，");
    format!("（上一回合被取消，未生成最终回复）已执行工具：{summary}")
}

/// 上下文超限的错误判定：与 Python 侧 `_CONTEXT_OVERFLOW_ERROR_MARKERS` 同源。
pub(crate) fn is_context_overflow(message: &str) -> bool {
    CONTEXT_OVERFLOW_ERROR_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
}

/// 压缩开始前的状态通知：把「上下文压缩中」发给宿主，让状态行在摘要请求期间有内容。
///
/// 阈值触发的压缩挂在回合收尾、`turn.finished` 之前，而这一刻状态行正停在「正在调用」或
/// 已经被清空——不报这一条，用户在这几秒里看不到任何「正在压缩」的迹象。
/// 显式 `/compact` 由宿主自己写状态行，这里同样发一条（同文案，幂等覆盖）。
pub(crate) fn compaction_progress(conn: &Rc<RefCell<Conn>>) -> Option<Rc<dyn Fn()>> {
    let conn = Rc::clone(conn);
    Some(Rc::new(move || {
        conn.borrow_mut().notify(HostEvent::Status(MessagePayload {
            message: "上下文压缩中…".to_string(),
        }));
    }))
}

/// 上下文超限后的恢复：压缩当前未完成回合，返回可继续的历史投影。
pub(crate) fn recover_context_overflow(
    conn: &Rc<RefCell<Conn>>,
    previous_history: &[Value],
    model_config: &KernelModelConfig,
) -> Option<Vec<Value>> {
    let mut session = conn.borrow_mut().session.take()?;
    let api_key = read_api_key(model_config).unwrap_or_default();
    let snapshot = conn.borrow().usage.borrow().last_request_messages.clone();
    let last_request_messages = evicted_prefix(conn, &snapshot);
    let projection = match recover_after_overflow(
        &session,
        model_config,
        &api_key,
        &last_request_messages,
        previous_history,
        compaction_progress(conn),
    ) {
        Ok(report) => {
            for notice in &report.notices {
                // 边界提示走会话区（`turn.notice`）：画在状态行会在回合结束时消失，
                // 留下的只有摘要本身，用户看不到「上一段已被替换」的可见边界。
                conn.borrow_mut().notify(HostEvent::Notice(MessagePayload {
                    message: format!(
                        "{notice}（检测到上下文超限，已压缩当前任务上下文并自动继续。）"
                    ),
                }));
            }
            if !report.compacted {
                eprintln!("[kernel] 上下文超限后的模型压缩失败：{}", report.diagnostic);
            }
            report.history
        }
        Err(detail) => {
            eprintln!("[kernel] 上下文超限后的模型压缩失败：{detail}");
            None
        }
    };
    match projection.as_ref() {
        Some(history) => session.history = history.clone(),
        None => {
            if let Err(detail) = session.reload_history() {
                eprintln!("[kernel] 重建运行期历史失败：{detail}");
            }
        }
    }
    conn.borrow_mut().session = Some(session);
    projection
}

/// 回合末的整轮工具调用概括：把本轮全部工具调用压成一段写进转录。
///
/// 投影会据此剔除原来的逐条 `tool_call_requested` / `tool_result` 并插入概括文本；
/// 未启用、没有调用或概括失败时什么都不写，上下文保持原样。
///
/// 摘要请求复用的前缀：把本轮淘汰标注作用到「最近一次主请求」的快照上。
///
/// 前缀与待压缩窗口必须来自同一份上下文。淘汰在回合收尾**先于**压缩执行，真实的下一轮
/// 上下文里已经没有这批调用；若摘要请求照旧复用未淘汰的快照，模型会在前缀里读到它们的
/// 全文，却拿不到对应的索引条目（`events_index` 已按淘汰口径剔除），只能拿别的 ref 硬凑，
/// 摘要随后被判为「引用了不存在的事件」。
///
/// 淘汰点贴近上下文尾部（见 `turn::tool_prune`），因此前缀的前段仍与主请求逐字相同，
/// 前缀缓存基本不受影响。
pub(crate) fn evicted_prefix(conn: &Rc<RefCell<Conn>>, messages: &[Value]) -> Vec<Value> {
    let marks = conn.borrow().evict_marks.borrow().clone();
    let mut messages = messages.to_vec();
    if !marks.is_empty() {
        evict_tool_call_messages(&mut messages, &marks);
    }
    messages
}

/// 回合收尾：落会话事件（用户消息与最终回复）→ 跑一次压缩 → 通知提示并更新运行期历史。
///
/// 返回值是本回合压缩**之后**的上下文 Token 估算（没有压缩时为 `None`），由调用方随
/// `turn.finished` 发给宿主，用来把底部遥测换成压缩后的真实大小。
pub(crate) fn run_session_tail(
    conn: &Rc<RefCell<Conn>>,
    session: &mut KernelSession,
    run_guard: &TurnRunGuard,
    working_messages: &mut Vec<Value>,
    final_text: &str,
    task_hint: &str,
    turn_id: &str,
) -> Option<i64> {
    // `user_message` 已在发请求之前落盘（见 `run_turn`），这里只写本轮终态与续跑痕迹。
    // 续跑痕迹的落盘顺序与 Python 完全一致：先逐次 `run_guard_continue`，再写本轮终态。
    for payload in &run_guard.continue_events {
        if let Err(detail) = session.append(continuation::CONTINUE_EVENT, payload.clone()) {
            eprintln!("[kernel] 会话写入续跑事件失败：{detail}");
        }
    }
    if run_guard.paused {
        // 暂停回合同样写入完整协议轨迹：恢复投影会把 `run_guard_paused` 还原成说明消息。
        if let Err(detail) = session.append(continuation::PAUSED_EVENT, run_guard.paused_payload())
        {
            eprintln!("[kernel] 会话写入暂停事件失败：{detail}");
        }
    } else if !final_text.trim().is_empty() {
        if let Err(detail) =
            session.append("assistant_message", run_guard.assistant_payload(final_text))
        {
            eprintln!("[kernel] 会话写入助手回复失败：{detail}");
        }
    }
    if run_guard.exhausted {
        if let Err(detail) = session.append(
            continuation::CONTINUE_EXHAUSTED_EVENT,
            run_guard.exhausted_payload(),
        ) {
            eprintln!("[kernel] 会话写入续跑终态失败：{detail}");
        }
    }

    // 「先标注-后压缩」的收口：本轮裁决过的调用在这里一次性移出上下文。
    //
    // 位置就是优先级：本块在 `compact_after_turn`（上下文压缩）与 `summarize_turn_after_turn`
    // （工具调用压缩）之前——先按标注把上下文瘦下来，再让两套压缩在瘦后的上下文上工作：
    // * `working_messages` 正是压缩的判定与摘要输入，先剔除即压缩看到的是淘汰后的规模；
    // * 重建运行期历史后，下一次请求看到的就是淘汰后的上下文；
    // * 工具调用概括的输入事件在 `summarize_turn_after_turn` 里同步剔除这批 ID。
    // 被淘汰的原文因此既不会被压缩摘要盖回去，也不会被概括再描述一遍。
    let evict_marks = conn.borrow().evict_marks.borrow().clone();
    // 逐批工具调用压缩在本回合累加出来的净省与次数：与淘汰合并成回合末的一行计量。
    let (mut saved_in_turn, mut compacted_batches) = {
        let mut connection = conn.borrow_mut();
        (
            std::mem::take(&mut connection.tool_compaction_saved),
            std::mem::take(&mut connection.tool_compaction_batches),
        )
    };
    // 淘汰后的上下文大小：本轮若没有压缩/概括刷新遥测，就由它兜底（见函数末尾）。
    let mut eviction_context_tokens: Option<i64> = None;
    if !evict_marks.is_empty() {
        // 净省量用与遥测同源的估算器前后实测：剔除前后的运行期消息各算一次，
        // 差值就是这次淘汰真实腾出来的上下文。
        let tokens_before = estimate_messages_tokens(working_messages);
        let removed = evict_tool_call_messages(working_messages, &evict_marks);
        let tokens_after = estimate_messages_tokens(working_messages);
        let saved_tokens = (tokens_before - tokens_after).max(0);
        if let Err(detail) = session.reload_history() {
            eprintln!("[kernel] 工具调用淘汰后重建运行期历史失败：{detail}");
        }
        // 剔除已完成：算一次淘汰后的真实上下文大小，供 `turn.finished` 刷新底部遥测。
        eviction_context_tokens = post_compaction_context_tokens(conn, session.history.as_slice());
        // 分项记账留一条事件供诊断；会话区的计量行由回合末的合并事件重建。
        if let Err(detail) = session.append(
            EVICTION_MEASURE_EVENT_TYPE,
            eviction_measure_payload(evict_marks.len(), saved_tokens),
        ) {
            eprintln!("[kernel] 会话写入工具调用淘汰记账失败：{detail}");
        }
        saved_in_turn += saved_tokens;
        compacted_batches += 1;
        eprintln!(
            "[kernel] 工具调用淘汰：剔除 {} 组、{} 条消息，省 {saved_tokens} Token（回合收尾统一处理）。",
            evict_marks.len(),
            removed
        );
    }
    // 会话区只留一行：逐批工具输出压缩与决策模型淘汰都是「这一回合省了多少」的分项，
    // 提示因此合并、等回合结束才出现（不再在工具卡后面逐批抖出来）。
    //
    // 发生过压缩/淘汰就落事件（净省为 0 也落）：回放靠这条事件知道「本回合已经有合并行」，
    // 于是跳过同回合的分项事件；只在那两笔都没有的回合才什么都不写。
    if compacted_batches > 0 {
        if let Err(detail) = session.append(
            COMPACTION_MEASURE_EVENT_TYPE,
            compaction_measure_payload(saved_in_turn),
        ) {
            eprintln!("[kernel] 会话写入回合压缩计量失败：{detail}");
        }
        if let Some(notice) = format_compaction_savings_option(saved_in_turn) {
            conn.borrow_mut()
                .notify(HostEvent::Notice(MessagePayload { message: notice }));
        }
    }

    let model = conn.borrow().model.clone();
    let Some(model) = model else {
        // 没有模型配置就没有可发的摘要请求：保留转录，历史按转录重建。
        if let Err(detail) = session.reload_history() {
            eprintln!("[kernel] 重建运行期历史失败：{detail}");
        }
        return eviction_context_tokens;
    };
    let api_key = read_api_key(&model).unwrap_or_default();
    let (usage, last_request_input_tokens, last_request_messages) = {
        let connection = conn.borrow();
        let totals = connection.usage.borrow();
        (
            totals.snapshot(),
            totals.last_request_input_tokens,
            totals.last_request_messages.clone(),
        )
    };
    match compact_after_turn(
        session,
        &model,
        &api_key,
        usage,
        last_request_input_tokens,
        &evicted_prefix(conn, &last_request_messages),
        working_messages.as_slice(),
        task_hint,
        compaction_progress(conn),
    ) {
        Ok(report) => {
            if !report.compacted && !report.diagnostic.is_empty() {
                eprintln!(
                    "[kernel] 模型摘要未通过校验，已跳过本回合压缩：{}",
                    report.diagnostic
                );
            }
            // 触发压缩的回合把计量发给宿主：宿主据此分发 `context.compaction.after_turn`
            // 压缩计量通知（Python 在同一位置由 agent runtime 内部派发）。未触发则不发。
            if report
                .measurement_payload
                .get("trigger_reached")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let post_turn_context_tokens = report
                    .measurement_payload
                    .get("post_turn_context_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let trigger_context_tokens = report
                    .measurement_payload
                    .get("trigger_context_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                // 压缩刚落地：用重建后的历史算一次压缩后大小，随计量一起给宿主刷新遥测。
                let post_compaction_context_tokens = report
                    .history
                    .as_deref()
                    .and_then(|history| post_compaction_context_tokens(conn, history));
                conn.borrow_mut()
                    .notify(HostEvent::ContextCompaction(ContextCompactionPayload {
                        post_turn_context_tokens,
                        trigger_context_tokens,
                        turn_id: turn_id.to_string(),
                        post_compaction_context_tokens,
                    }));
            }
            for notice in report.notices {
                // 阈值触发的压缩同样在会话区留边界：`turn.finished` 一到状态行就清空，
                // 边界只能落在对话流里（与自动压缩的摘要一起构成「上一段被替换」的痕迹）。
                // 工具段在前、非工具段在后，与链路的执行顺序一致。
                conn.borrow_mut()
                    .notify(HostEvent::Notice(MessagePayload { message: notice }));
            }
            match report.history {
                Some(history) => session.history = history,
                None => {
                    if let Err(detail) = session.reload_history() {
                        eprintln!("[kernel] 重建运行期历史失败：{detail}");
                    }
                }
            }
            // 阈值触发的压缩已经把整轮工具调用概括过：它写在 `compact_summary` 边界里。
            // 工具调用压缩已改为运行中逐批决策（见 `before_tool_batch`），回合收尾不再
            // 汇合做全量压缩；这里兜底把逐批压缩/淘汰后的上下文大小报给宿主。
            eviction_context_tokens
                .or_else(|| post_compaction_context_tokens(conn, session.history.as_slice()))
        }
        Err(detail) => {
            eprintln!("[kernel] 上下文压缩失败，已跳过本回合：{detail}");
            if let Err(detail) = session.reload_history() {
                eprintln!("[kernel] 重建运行期历史失败：{detail}");
            }
            eviction_context_tokens
        }
    }
}

