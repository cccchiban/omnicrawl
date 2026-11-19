//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

impl AppState {
    /// 用内核回给的会话历史重建对话视图（`/resume` 与 `/undo` 后的重放）。
    ///
    /// 只投影 user/assistant 的**文本**消息：工具卡、推理段、通知与子任务进度树属于
    /// 「本进程这次运行」的观感，转录里没有它们的等价物，因此重放后它们自然消失——
    /// 这正是 `/undo` 要的效果（被撤回的消息与工具卡不能留着）。内容是多模态数组或
    /// 非字符串的消息跳过，不把 JSON 塞进消息流。
    pub fn replay_history(&mut self, history: &[serde_json::Value]) {
        self.records.clear();
        // 回放重建的是「已发生的事实」的投影，工具卡本身不会回来，图片预览也跟着作废。
        self.image_previews.get_mut().clear();
        // 计划清单同属这一屏的会话现场：整视图重建时不先清掉，上一条会话（或上一段任务）
        // 的步骤会留在输入框上方（`/new` 之后的旧计划即此）。事件流里真有 `update_todos`
        // 时，下面重放会重新写回它。
        self.todos.clear();
        // 回放是「已发生的事实」，直接显示全文，不从半句开始铺。
        self.settle_reasoning_reveal();
        // 整块视图被重建：显示行缓存整体失效（后续都是追加，只算一次）。
        self.touch_conversation();
        // 计划清单画在输入块里，清空同样要重算那块的高度。
        self.touch_composer();
        for message in history {
            let role = message
                .get("role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let Some(content) = message.get("content").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let content = content.trim();
            if content.is_empty() {
                continue;
            }
            match role {
                "user" => self.push_user_message(content.to_string(), Vec::new(), Vec::new()),
                "assistant" => self.records.push(Record::Assistant(content.to_string())),
                _ => {}
            }
        }
        self.scroll_to_bottom();
    }

    /// 按持久化事件流重建对话视图（对映 Python `_replay_session_events`）。
    ///
    /// 与 [`Self::replay_history`] 的分工：消息投影把工具请求/结果写成了 assistant 文本，
    /// 只够恢复文字；历史页面必须消费事件流，才能还原工具卡、结果状态、计划清单与
    /// SubAgent 进度树。事件流为空是权威结果（例如 `/undo` 撤掉了唯一一轮），因此必须
    /// 清空视图，而不是保留旧消息。
    pub fn replay_events(&mut self, events: &[serde_json::Value]) {
        self.records.clear();
        // 同上：事件流重放出来的卡片没有附件原文，旧预览若留着会挂到不存在（或别的）卡片上。
        self.image_previews.get_mut().clear();
        // 计划清单同理：整视图重建从空清单开始，事件流里的 `update_todos` 再把它写回来
        // （Python 侧是 `_clear_conversation_view()` 里的 `_clear_todo_plan()` 同一口径）。
        self.todos.clear();
        // 回放是「已发生的事实」，直接显示全文，不从半句开始铺。
        self.settle_reasoning_reveal();
        // 整块视图被重建：显示行缓存整体失效（后续都是追加，只算一次）。
        self.touch_conversation();
        // 计划清单画在输入块里，清空同样要重算那块的高度。
        self.touch_composer();
        // 未收口的工具卡：`(call_id, tool, 记录下标, 请求时间)`。没有 call id 的旧事件按
        // 工具名延后匹配（与 Python 的 `pending_by_id` / `pending_by_tool` 同口径）。
        let mut pending: Vec<(String, String, usize, Option<DateTime<Utc>>)> = Vec::new();
        // 被拒绝的调用：`call_id` 或 `tool:<name>` → 原因；收口时优先用它当正文。
        let mut denied: Vec<(String, String)> = Vec::new();
        // 回合序号（每个 `user_message` 开启一个）与「已有合并计量行」的回合集合：
        // 新会话的逐批压缩/淘汰不再各自成行，由回合末那一条合并计量代表。
        let mut turn_index = 0usize;
        let merged_turns = replayed_merged_turns(events);
        for event in events {
            let event_type = event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let payload = match event.get("payload") {
                Some(serde_json::Value::Object(_)) => event["payload"].clone(),
                _ => serde_json::json!({}),
            };
            match event_type {
                "user_message" => {
                    if let Some(content) = replay_text(&payload, "content") {
                        // 随用户消息发出的图片也在事件里：重放照样出缩略图
                        // （旧事件没有该字段，退化成纯文本）。
                        // 文档路径不进事件（它只是提问正文的注脚），重放时不再显示。
                        let images = replay_user_images(&payload);
                        self.push_user_message(content, images, Vec::new());
                    }
                    turn_index += 1;
                }
                "assistant_message" => {
                    // 压缩后的会话正文放在 `session_content`，回放优先读它。
                    let content = replay_text(&payload, "session_content")
                        .or_else(|| replay_text(&payload, "content"));
                    if let Some(content) = content {
                        self.records.push(Record::Assistant(content));
                    }
                }
                "tool_call_requested" => {
                    let tool = replay_text(&payload, "tool").unwrap_or_default();
                    if tool.is_empty() {
                        continue;
                    }
                    if tool == host::TODO_TOOL {
                        // 计划清单是展示层状态，不在会话区生成工具卡（对映 Python
                        // `_handle_todo_update` 分支）。
                        let mut map = serde_json::Map::new();
                        let todos = payload
                            .get("arguments")
                            .and_then(|arguments| arguments.get("todos"))
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!([]));
                        map.insert("todos".to_string(), todos);
                        self.todos = host::parse_todos(&map);
                        continue;
                    }
                    if tool == host::ASK_USER_TOOL {
                        continue;
                    }
                    let call_id = replay_text(&payload, "tool_call_id").unwrap_or_default();
                    let empty = serde_json::Map::new();
                    let arguments = payload
                        .get("arguments")
                        .and_then(serde_json::Value::as_object)
                        .unwrap_or(&empty);
                    let summary = host::summarize_arguments(arguments);
                    let index = self.records.len();
                    self.records.push(Record::Tool(ToolCard {
                        call_id: call_id.clone(),
                        name: tool.clone(),
                        summary,
                        arguments: payload
                            .get("arguments")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        status: ToolStatus::Running,
                        elapsed: None,
                        started: Instant::now(),
                        body: String::new(),
                    }));
                    pending.push((call_id, tool, index, replay_time(event)));
                }
                "tool_call_denied" => {
                    let tool = replay_text(&payload, "tool").unwrap_or_default();
                    let reason = replay_text(&payload, "reason")
                        .unwrap_or_else(|| "工具调用未获批准。".to_string());
                    let call_id = replay_text(&payload, "tool_call_id").unwrap_or_default();
                    if !call_id.is_empty() {
                        denied.push((call_id, reason));
                    } else if !tool.is_empty() {
                        denied.push((format!("tool:{tool}"), reason));
                    }
                }
                "tool_result" => {
                    let tool = replay_text(&payload, "tool").unwrap_or_default();
                    if tool == host::TODO_TOOL || tool == host::ASK_USER_TOOL {
                        continue;
                    }
                    let call_id = replay_text(&payload, "tool_call_id").unwrap_or_default();
                    // 先按 call id 配对，再按工具名兜底；两者都没有就补一张空参数卡。
                    let matched = pending
                        .iter()
                        .position(|(id, _, _, _)| !call_id.is_empty() && *id == call_id)
                        .or_else(|| pending.iter().position(|(_, name, _, _)| *name == tool));
                    let (index, started) = match matched {
                        Some(at) => {
                            let entry = pending.remove(at);
                            (entry.2, entry.3)
                        }
                        None => {
                            let index = self.records.len();
                            self.records.push(Record::Tool(ToolCard {
                                call_id: call_id.clone(),
                                name: if tool.is_empty() {
                                    "未知工具".to_string()
                                } else {
                                    tool.clone()
                                },
                                summary: String::new(),
                                // 没有配对的 tool_call_requested：没有参数可拼标题。
                                arguments: serde_json::Value::Null,
                                status: ToolStatus::Running,
                                elapsed: None,
                                started: Instant::now(),
                                body: String::new(),
                            }));
                            (index, replay_time(event))
                        }
                    };
                    let ok = payload
                        .get("ok")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let finished = replay_time(event);
                    if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                        card.status = if ok {
                            ToolStatus::Ok
                        } else {
                            ToolStatus::Failed
                        };
                        card.elapsed = replay_elapsed(started, finished);
                        card.body = crate::ui::fullscreen::rendering::tool_diff::stored_result_text(
                            &card.name,
                            &replay_output(&payload),
                        );
                    }
                }
                other => {
                    if let Some(name) = replay_subagent_event_name(other) {
                        self.apply_subagent_event(name, &payload);
                    } else if other == "turn_cancelled" {
                        self.records.push(Record::Assistant(
                            replay_text(&payload, "summary").unwrap_or_else(|| {
                                "（上一回合被取消，未生成最终回复）".to_string()
                            }),
                        ));
                    } else if other == "session_interrupted" {
                        self.records
                            .push(Record::Notice("上一回合在会话恢复前中断。".to_string()));
                    } else if other == "compact_summary" {
                        if let Some(content) = replay_text(&payload, "content") {
                            self.records
                                .push(Record::Assistant(format!("会话压缩摘要：\n{content}")));
                        }
                    } else if other == "tool_call_summary" {
                        // 回合内的逐批工具调用压缩：被概括的逐条工具事件已被投影剔除，
                        // 这里不再单独画计量（回合末的 `turn_compaction` 已把逐批压缩与
                        // 淘汰合并成一行）。旧会话没有合并事件，仍按字符数重建那一行。
                        if !merged_turns.contains(&turn_index) {
                            if let Some(note) = replay_compaction_notice(&payload) {
                                self.records.push(Record::Notice(note));
                            }
                        }
                    } else if other == "tool_call_eviction" {
                        // 决策模型标记淘汰的记账：同上，新会话由回合末的合并计量行代表。
                        if !merged_turns.contains(&turn_index) {
                            if let Some(note) = replay_eviction_notice(&payload) {
                                self.records.push(Record::Notice(note));
                            }
                        }
                    } else if other == "turn_compaction" {
                        // 回合末的合并计量：逐批工具输出压缩与决策模型淘汰的总净省。
                        if let Some(note) = replay_merged_notice(&payload) {
                            self.records.push(Record::Notice(note));
                        }
                    }
                }
            }
        }
        // 尾部：仍挂着的卡片按「被拒绝」或「会话结束前没有结果」收口（对映 Python
        // `_replay_session_events` 末尾遍历未收口组件的分支）。
        for (call_id, tool, index, _) in pending {
            let denial = denied
                .iter()
                .find(|(key, _)| !call_id.is_empty() && *key == call_id)
                .or_else(|| {
                    denied
                        .iter()
                        .find(|(key, _)| *key == format!("tool:{tool}"))
                });
            let (status, reason) = match denial {
                Some((_, reason)) => (ToolStatus::Denied, reason.clone()),
                None => (
                    ToolStatus::Failed,
                    "工具调用在会话结束前未收到结果。".to_string(),
                ),
            };
            if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                card.status = status;
                card.body = reason;
            }
        }
        self.scroll_to_bottom();
    }

    /// 子任务事件 → 独立窗口：按 `batch_id` 记账，把事件转成窗口流水。
    ///
    /// 模型自己发起的批次（带 `call_id`）**不开窗**：它只在对应工具卡后面留一个
    /// 「查看会话」入口，用户点了才开。宿主自发的入口（`/review`）没有工具卡可挂，
    /// 收到首个事件时仍然直接开窗，并留一行受理提示。
    pub(super) fn apply_subagent_event(&mut self, name: &str, payload: &serde_json::Value) {
        let batch_id = payload_text(payload, "batch_id")
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "batch-{}",
                    payload_text(payload, "task_id").unwrap_or("task")
                )
            });
        let delivery = self.subagent_window.deliver(
            &self.subagent_stream_root,
            self.subagent_window_exe.as_deref(),
            name,
            payload,
        );
        // 这张卡上的「查看会话」可能刚出现（批次创建）或刚撤下（批次收尾）：
        // 前者来自载荷里的发起调用 id，后者只能从批次绑定反查。
        let call_id = payload_text(payload, "origin_call_id")
            .map(str::to_string)
            .or_else(|| self.subagent_window.call_of(&batch_id).map(str::to_string));
        match delivery {
            crate::subagent_window::Delivery::Opened { stream } => {
                self.records
                    .push(Record::SubagentWindow(SubagentWindowNotice {
                        batch_id,
                        stream: stream.display().to_string(),
                    }));
                self.touch_appended();
            }
            _ => {
                if let Some(index) = call_id
                    .as_deref()
                    .and_then(|call_id| self.tool_card_slot(call_id))
                {
                    self.touch_record(index);
                }
            }
        }
    }
}

/// 从「可能还是半截」的 arguments 原文里尽力取出已经到达的字段。
///
/// 模型流里的 arguments 是分片到达的，中途必然是半截 JSON
/// （`{"path": "a.py", "content": "第一`）。但标题行与 `write_file` / `Edit_file` 的内容预览
/// 要跟着流一起长出来，所以这里做一次容错解析：先用 `serde_json` 正常解析，失败就按
/// 「补上收尾」的候选逐个再试（补未闭合的字符串 / 补未闭合的对象），最后退化成
/// 「截到最后一个完整字段」。
pub(crate) fn partial_arguments(text: &str) -> serde_json::Value {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        if value.is_object() {
            return value;
        }
    }
    let trimmed = text.trim_end();
    let quote = '"';
    let mut candidates = vec![format!("{trimmed}{quote}}}"), format!("{trimmed}}}")];
    if let Some(cut) = last_complete_field(trimmed) {
        // 截到最后一个完整字段：末尾那个逗号要去掉，否则补出来的对象带尾逗号仍然不合法。
        candidates.push(format!(
            "{}}}",
            trimmed[..cut].trim_end().trim_end_matches(',')
        ));
    }
    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&candidate) {
            if value.is_object() {
                return value;
            }
        }
    }
    serde_json::Value::Object(serde_json::Map::new())
}

/// 半截 JSON 里最后一个「完整顶层字段」的结束位置（逗号之后），没有就返回 None。
///
/// 扫描时跟踪字符串状态：字段值内部的逗号不能当分隔符。
fn last_complete_field(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut last = None;
    for (index, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth <= 1 => last = Some(index + 1),
            _ => {}
        }
    }
    last
}

/// `tool_call_summary` 事件 → 会话区计量行；载荷缺计量字段时返回 `None`。
///
/// 计量取事件自己记下的原始与概括字符数：回放时重算不出来（正文已含落盘说明），
/// 而实时通知与回放必须显示同一行，否则恢复会话后边界会变样。
fn replay_compaction_notice(payload: &serde_json::Value) -> Option<String> {
    let raw = payload
        .get("raw_chars")
        .and_then(serde_json::Value::as_u64)?;
    let summary = payload
        .get("summary_chars")
        .and_then(serde_json::Value::as_u64)?;
    // 没有净省就不显示：实时那条也不显示，回放必须与它逐字一致。
    omnicrawl_controllers::compression::saved_tokens_notice(raw as usize, summary as usize)
}

/// `tool_call_eviction` 事件 → 会话区计量行；载荷缺计量字段时返回 `None`。
///
/// 淘汰的净省量是剔除前后实测出来的（不是按字符数折算），事件里直接记着最终值，
/// 回放照原样显示即可——旧事件没有该字段时宁可不显示，也不显示一个假的 0。
fn replay_eviction_notice(payload: &serde_json::Value) -> Option<String> {
    let saved = payload
        .get("saved_tokens")
        .and_then(serde_json::Value::as_i64)?;
    Some(omnicrawl_controllers::turn::compaction::format_compaction_savings_notice(saved))
}

/// `turn_compaction` 事件 → 会话区计量行；载荷缺计量字段时返回 `None`。
///
/// 这是回合末的**合并**计量：逐批工具输出压缩与决策模型淘汰在同一回合里各自不再成行，
/// 净省量已经由内核累加好，回放照原样显示即可。
fn replay_merged_notice(payload: &serde_json::Value) -> Option<String> {
    let saved = payload
        .get("saved_tokens")
        .and_then(serde_json::Value::as_i64)?;
    omnicrawl_controllers::turn::compaction::format_compaction_savings_option(saved)
}

/// 已经带合并计量行的回合序号集合（`user_message` 从 1 起算，与回放循环里的计数同口径）。
///
/// 回放要先知道「哪些回合已经有合并行」，才能在扫到同回合的 `tool_call_summary` /
/// `tool_call_eviction` 时跳过那些分项提示——否则新会话会同时画出合并行与分项行。
/// 旧会话没有 `turn_compaction` 事件，集合为空，分项提示照旧逐条重建。
fn replayed_merged_turns(events: &[serde_json::Value]) -> std::collections::HashSet<usize> {
    let mut turns = std::collections::HashSet::new();
    let mut turn_index = 0usize;
    for event in events {
        let event_type = event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        match event_type {
            "user_message" => turn_index += 1,
            "turn_compaction" => {
                turns.insert(turn_index);
            }
            _ => {}
        }
    }
    turns
}

/// 回放取文本字段：非空字符串才算命中（对映 Python 的 `isinstance(value, str) and value`）。
fn replay_text(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 回放 `user_message` 载荷里的图片附件；没有（或形状不对）时返回空表。
///
/// 与 `turn.submit` 的 `TurnImageAttachment` 同形状：旧事件没有该字段，退回纯文本。
fn replay_user_images(payload: &serde_json::Value) -> Vec<TurnImageAttachment> {
    let Some(items) = payload.get("images").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(TurnImageAttachment {
                media_type: item.get("media_type")?.as_str()?.to_string(),
                data_base64: item.get("data_base64")?.as_str()?.to_string(),
                detail: item
                    .get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

/// 事件时间戳（`created_at`，RFC3339）；缺失或不可解析时为空。
fn replay_time(event: &serde_json::Value) -> Option<DateTime<Utc>> {
    event
        .get("created_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|value| value.with_timezone(&Utc))
}

/// 回放工具卡耗时：两端时间戳齐备且顺序正确时给出差值，否则不显示耗时。
fn replay_elapsed(
    started: Option<DateTime<Utc>>,
    finished: Option<DateTime<Utc>>,
) -> Option<Duration> {
    let delta = finished?.signed_duration_since(started?);
    let millis = delta.num_milliseconds();
    if millis < 0 {
        None
    } else {
        Some(Duration::from_millis(millis as u64))
    }
}

/// 回放取工具正文：优先 UI 正文，其次模型可见输出（与 Python `_replay_tool_output` 同口径）。
fn replay_output(payload: &serde_json::Value) -> String {
    for key in ["full_output", "output", "model_output", "output_preview"] {
        if let Some(value) = replay_text(payload, key) {
            return value;
        }
    }
    "无输出".to_string()
}

/// 取非空字符串字段；缺失、类型不符或空串都按「未提供」处理（对映 Python 的 `x or 默认值`）。
fn payload_text<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
}

/// 转录里的 SubAgent 事件类型 → 实时通知名（对映 Python `subagent_event_names`）。
fn replay_subagent_event_name(event_type: &str) -> Option<&'static str> {
    match event_type {
        "subagent_task_queued" => Some("subagent.task.queued"),
        "subagent_task_started" => Some("subagent.task.started"),
        "subagent_task_running" => Some("subagent.task.running"),
        "subagent_task_waiting_approval" => Some("subagent.task.waiting_approval"),
        "subagent_task_completed" => Some("subagent.task.completed"),
        "subagent_task_failed" => Some("subagent.task.failed"),
        "subagent_task_cancelled" => Some("subagent.task.cancelled"),
        "subagent_task_approval_cancelled" => Some("subagent.task.approval_cancelled"),
        _ => None,
    }
}
