//! 单个回合的显示装配（QQ 版）。
//!
//! 与飞书/Telegram 不同，QQ 消息不可编辑：正文在工具边界「封口」为独立消息，工具/计划/
//! 子任务攒成一段进度靠 [`remaining_after`] 增量补发。因此这里不是「原地 patch 一条消息」
//! 的状态机，而是「已发正文段 + 剩余补发」的记账，不能与另两个平台共用实现。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::agent::{AgentDriver, ToolCall, ToolResult, TurnEvent};
use crate::render::{
    normalize_todos, subagents_text, todos_text, SubagentNode, ASK_USER_TOOL_NAME,
    SUBAGENT_EVENT_STATUS, SUBAGENT_TERMINAL_STATUSES, TODO_TOOL_NAME,
};

use super::reply::ReplyHandle;
use super::QqBot;
use std::time::Instant;
use crate::render::{operation_of, reasoning_body};
use crate::text::{clean_text, resolve_final_text, text_of};
use crate::qqbot::markdown;

pub(super) struct TaskTimeline<'a, D: AgentDriver> {
    bot: &'a QqBot<D>,
    reply: Arc<ReplyHandle>,
    deltas: String,
    all_deltas: String,
    reasoning: String,
    todos: Vec<(String, bool)>,
    subagent_order: Vec<String>,
    subagent_nodes: BTreeMap<String, SubagentNode>,
    /// 已经作为正文段发出去的内容（收尾时据此只补剩余部分）。
    sent_segments: Vec<String>,
    pub(super) sent_any: bool,
}

impl<'a, D: AgentDriver> TaskTimeline<'a, D> {
    pub(super) fn new(bot: &'a QqBot<D>, reply: Arc<ReplyHandle>) -> TaskTimeline<'a, D> {
        TaskTimeline {
            bot,
            reply,
            deltas: String::new(),
            all_deltas: String::new(),
            reasoning: String::new(),
            todos: Vec::new(),
            subagent_order: Vec::new(),
            subagent_nodes: BTreeMap::new(),
            sent_segments: Vec::new(),
            sent_any: false,
        }
    }

    pub(super) fn handle(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Delta(delta) => {
                self.deltas.push_str(&delta);
                self.all_deltas.push_str(&delta);
            }
            TurnEvent::ReasoningDelta(delta) => {
                if self.bot.show_thinking() {
                    self.reasoning.push_str(&delta);
                }
            }
            TurnEvent::ToolStarted { call, .. } => self.on_tool_start(&call),
            TurnEvent::ToolFinished { call, result } => self.on_tool_result(&call, &result),
            TurnEvent::TodoUpdate { todos } => {
                self.todos = normalize_todos(Some(&todos));
            }
            TurnEvent::SubagentEvent { name, payload } => self.on_subagent(&name, &payload),
            TurnEvent::StreamRollback => {
                // 模型流重试：作废的半截输出丢弃，避免把残句发给用户。
                self.deltas.clear();
                self.all_deltas.clear();
            }
            _ => {}
        }
    }

    /// 工具调用不再作为进度下发：只保留正文段，端上不显示「调用了什么、成功与否」。
    ///
    /// 仍然要在工具边界把正文封口——模型往往「先说一句 → 调工具 → 再说一句」，
    /// 不封口就会把两段话粘成一句，用户看不出中间停过。
    pub(super) fn on_tool_start(&mut self, call: &ToolCall) {
        let operation = operation_of(&call.name);
        if operation == ASK_USER_TOOL_NAME || operation == TODO_TOOL_NAME {
            return;
        }
        self.flush_text();
    }

    /// 工具结果不参与显示（`pending_tools` 已不再收集），这里只做清理。
    pub(super) fn on_tool_result(&mut self, _call: &ToolCall, _result: &ToolResult) {}

    pub(super) fn on_subagent(&mut self, name: &str, payload: &Value) {
        let status = SUBAGENT_EVENT_STATUS
            .iter()
            .find(|(event, _status)| *event == name)
            .map(|(_event, status)| (*status).to_string());
        let Some(status) = status else { return };
        let task_id = payload
            .get("task_id")
            .map(text_of)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "task".to_string());
        let agent_type = payload.get("agent_type").map(text_of).unwrap_or_default();
        let description = payload.get("description").map(text_of).unwrap_or_default();
        let terminal = SUBAGENT_TERMINAL_STATUSES.contains(&status.as_str());
        if !self.subagent_nodes.contains_key(&task_id) {
            self.subagent_order.push(task_id.clone());
            self.subagent_nodes.insert(
                task_id.clone(),
                SubagentNode {
                    agent_type: if agent_type.is_empty() {
                        "subagent".to_string()
                    } else {
                        agent_type.clone()
                    },
                    description: description.clone(),
                    status: status.clone(),
                    started_at: None,
                    finished_at: None,
                },
            );
        }
        let node = self
            .subagent_nodes
            .get_mut(&task_id)
            .expect("刚插入的节点必须存在");
        if SUBAGENT_TERMINAL_STATUSES.contains(&node.status.as_str()) {
            return;
        }
        if !agent_type.is_empty() {
            node.agent_type = agent_type;
        }
        if !description.is_empty() {
            node.description = description;
        }
        node.status = status;
        node.started_at = Some(Instant::now());
        if terminal {
            node.finished_at = Some(Instant::now());
        }
    }

    /// 封口当前正文段：发一条 `◇ …` 消息（中间额度用尽时留到最终回答里）。
    pub(super) fn flush_text(&mut self) {
        let text = clean_text(&self.deltas);
        self.deltas.clear();
        if text.is_empty() {
            return;
        }
        if !self.reply.can_send_intermediate() {
            // 中间额度用满后不再下发任何过程内容，正文只随最终回答一起给出。
            eprintln!("[qqbot] 中间回复额度已用尽，本段正文不再单独下发。");
            return;
        }
        // 首行是块级语法（标题/列表/引用…）时 `◇` 单独占一行，否则会把块语法吃掉。
        let body = if markdown::starts_block(&text) && markdown::supports(&self.reply.target) {
            format!("◇\n{text}")
        } else {
            format!("◇ {text}")
        };
        if self.reply.send_intermediate(&body) > 0 {
            self.sent_segments.push(text);
            self.sent_any = true;
        }
    }

    /// 发一段合并后的进度：执行计划与子任务树各一条。
    ///
    /// 工具调用不再出现在这里（端上不显示调用了什么）。中间额度用尽后什么都不发
    /// （攒着的内容直接丢弃），把最后一条留给最终文本。
    pub(super) fn flush_progress(&mut self) {
        if !self.todos.is_empty() {
            let text = todos_text(&self.todos);
            if self.reply.can_send_intermediate() && self.reply.send_intermediate(&text) > 0 {
                self.sent_any = true;
            }
        }
        if !self.subagent_order.is_empty() {
            let nodes: Vec<(String, SubagentNode)> = self
                .subagent_order
                .iter()
                .filter_map(|key| {
                    self.subagent_nodes
                        .get(key)
                        .map(|node| (key.clone(), node.clone()))
                })
                .collect();
            let text = subagents_text(&self.subagent_order, &nodes);
            if self.reply.can_send_intermediate() && self.reply.send_intermediate(&text) > 0 {
                self.sent_any = true;
            }
        }
    }

    /// 回合中段的中间输出：进度 + 已说出口的正文。
    pub(super) fn flush_intermediate(&mut self) {
        self.flush_progress();
        self.flush_text();
    }

    /// 回合收尾：思考、剩余正文与最终回答。
    pub(super) fn finish(&mut self, final_text: &str) {
        if !self.reasoning.trim().is_empty() {
            // QQ 是纯文本通道：用与 Telegram 同一份纯文本渲染，而不是飞书的卡片 JSON。
            let reasoning = format!("🧠 思考\n{}", reasoning_body(self.reasoning.trim(), false));
            if self.reply.can_send_intermediate() && self.reply.send_intermediate(&reasoning) > 0 {
                self.sent_any = true;
            }
        }
        let streamed = clean_text(&self.deltas);
        self.deltas.clear();
        let text = resolve_final_text(&streamed, &clean_text(final_text));
        // 消息不可编辑：已经发过的正文段不再重发，只补剩下的部分。
        let tail = remaining_after(&self.sent_segments, &text);
        if tail.trim().is_empty() {
            return;
        }
        if self.reply.send_final(&tail) > 0 {
            self.sent_any = true;
        }
    }

    pub(super) fn all_deltas(&self) -> String {
        self.all_deltas.clone()
    }
}
/// 已经发过的正文段之后还剩多少：`text` 以若干已发正文段开头时返回尾部，否则原样返回。
///
/// 宿主的最终回答通常只是最后一个模型 pass 的文本，**不含**工具调用之前的正文段，因此
/// 逐段比对「已发段的尾部拼接」，而不是要求整段累计文本都出现在 `text` 里——那样一旦
/// 前几段是别的 pass 说的，前缀就匹配不上，最后一段会被整段重发一遍。
///
/// 比较时折叠空白（模型可能把两段用不同换行拼回来），但返回值取自原文，不重排内容。
pub(super) fn remaining_after(sent: &[String], text: &str) -> String {
    let squeezed = |value: &str| value.split_whitespace().collect::<String>();
    let pieces: Vec<String> = sent
        .iter()
        .map(|segment| squeezed(segment))
        .filter(|segment| !segment.is_empty())
        .collect();
    if pieces.is_empty() {
        return text.to_string();
    }
    let target = squeezed(text);
    if target.is_empty() {
        return String::new();
    }
    // 从最长到最短尝试已发段的尾部拼接：整段说过就整段跳过，部分说过只补剩余部分。
    let mut consumed_chars = 0_usize;
    for start in 0..pieces.len() {
        let joined: String = pieces[start..].concat();
        if target.starts_with(&joined) {
            consumed_chars = joined.chars().count();
            break;
        }
    }
    if consumed_chars == 0 {
        return text.to_string();
    }
    if consumed_chars == target.chars().count() {
        return String::new();
    }
    // 在原文里逐字符推进，直到折叠空白后的长度达到已发部分。
    let mut consumed = 0_usize;
    let mut seen = 0_usize;
    for (index, character) in text.char_indices() {
        if !character.is_whitespace() {
            seen += 1;
            if seen == consumed_chars {
                consumed = index + character.len_utf8();
                break;
            }
        }
    }
    text[consumed..].trim_start().to_string()
}