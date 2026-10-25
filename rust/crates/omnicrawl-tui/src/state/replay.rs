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
                    "user" => self.push_user_message(content.to_string(), Vec::new()),
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
                            let images = replay_user_images(&payload);
                            self.push_user_message(content, images);
                        }
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
                            body: Vec::new(),
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
                                    body: Vec::new(),
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
                            card.body = body_lines(&replay_output(&payload));
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
                            // 回合末的整轮工具调用压缩：被概括的逐条工具事件已被投影剔除，
                            // 这里补一行与实时通知同款的计量边界（用户要求「和上下文压缩一样」）。
                            if let Some(note) = replay_compaction_notice(&payload) {
                                self.records.push(Record::Notice(note));
                            }
                        } else if other == "tool_call_eviction" {
                            // 决策模型标记淘汰的记账：实时那一行由内核发 `turn.notice`，
                            // 回放这里按同一份载荷重建（数字与当时看到的一致）。
                            if let Some(note) = replay_eviction_notice(&payload) {
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
                    card.body = vec![reason];
                }
            }
            self.scroll_to_bottom();
        }

        /// 子任务事件 → 独立窗口：按 `batch_id` 记账，首次出现时开窗并留一行受理提示。
        ///
        /// 过程文本、工具调用与任务状态都由 `subagent-window` 子进程渲染，主会话不再画进度树；
        /// 这里只做两件本地的事：把事件转成窗口流水、批次首次出现时往消息流补一条提示。
        pub(super) fn apply_subagent_event(&mut self, name: &str, payload: &serde_json::Value) {
            let batch_id = payload_text(payload, "batch_id")
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "batch-{}",
                        payload_text(payload, "task_id").unwrap_or("task")
                    )
                });
            let opened = self.subagent_window.deliver(
                &self.subagent_stream_root,
                self.subagent_window_exe.as_deref(),
                name,
                payload,
            );
            if let crate::subagent_window::Delivery::Opened { stream } = opened {
                self.records
                    .push(Record::SubagentWindow(SubagentWindowNotice {
                        batch_id,
                        stream: stream.display().to_string(),
                    }));
                self.touch_appended();
            }
        }

}
