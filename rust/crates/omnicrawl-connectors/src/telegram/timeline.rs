//! Telegram 时间线：与飞书同构的显示装配。
//!
//! 语义基准与飞书 `feishu/timeline.rs` 一致：正文段、工具调用、思考、执行计划与子任务
//! 进度**各自独立成一条消息**、按发生顺序出现；条目自己的流式更新只编辑自己那条消息，
//! 互不覆盖。正文段以工具调用为界封口（模型一个 pass 一段），前后两段文本不会黏在同一
//! 条消息里。
//!
//! 与飞书的差别只在平台形态：Telegram 是纯文本消息（没有卡片与折叠面板），因此正文段用
//! `◇` 前缀、工具记录用 `●` 前缀、思考内容以 `🧠` 开头，摘要与正文的生成规则（`tool_summary`
//! / `tool_body` / `format_duration`）仍与飞书、TUI 共用同一份实现。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::format::{plan_finalize, truncate_for_stream, FinalizePlan};
use crate::render::{
    format_duration, normalize_todos, subagents_lines, todos_lines, tool_body, tool_status_icon,
    tool_summary, SubagentNode, SUBAGENT_EVENT_STATUS, SUBAGENT_TERMINAL_STATUSES,
    TOOL_TERMINAL_STATUSES,
};
use crate::text::clean_text;

/// 平台消息端口：创建消息、编辑消息与发送普通文本。
pub trait MessagePort: Send + Sync {
    /// 创建消息，返回 `message_id`；失败返回 None。
    fn create(&self, chat_id: i64, text: &str) -> Option<i64>;

    /// 原地编辑消息；内容与现状相同时由实现方跳过（Telegram 会回 400）。
    fn edit(&self, chat_id: i64, message_id: i64, text: &str) -> bool;

    /// 发送普通文本（按上限自动分段）。
    fn send_text(&self, chat_id: i64, text: &str);
}

/// 时间线条目共用的发送/编辑逻辑。
pub struct TimelineMessage {
    port: Arc<dyn MessagePort>,
    chat_id: i64,
    message_id: Option<i64>,
    available: bool,
    last_text: String,
}

impl TimelineMessage {
    pub fn new(port: Arc<dyn MessagePort>, chat_id: i64) -> TimelineMessage {
        TimelineMessage {
            port,
            chat_id,
            message_id: None,
            available: true,
            last_text: String::new(),
        }
    }

    pub fn chat_id(&self) -> i64 {
        self.chat_id
    }

    pub fn message_id(&self) -> Option<i64> {
        self.message_id
    }

    pub fn has_message(&self) -> bool {
        self.message_id.is_some()
    }

    pub fn available(&self) -> bool {
        self.available
    }

    /// 把内容交给平台：首次创建，之后原地编辑；内容没变不重复调用。
    ///
    /// 创建或编辑失败（限流、网络抖动）时只回报失败，**不**把条目判死：下一次交付会重试，
    /// 否则一次 429 就会让这一条永久停在最早的内容上。
    pub fn deliver(&mut self, text: &str) -> bool {
        if !self.available {
            return false;
        }
        if self.message_id.is_some() && self.last_text == text {
            return true;
        }
        if let Some(message_id) = self.message_id {
            if self.port.edit(self.chat_id, message_id, text) {
                self.last_text = text.to_string();
                return true;
            }
            return false;
        }
        match self.port.create(self.chat_id, text) {
            Some(message_id) => {
                self.message_id = Some(message_id);
                self.last_text = text.to_string();
                true
            }
            None => false,
        }
    }

    /// 条目之外的补充文本（超长部分、状态提示等）。
    pub fn send_text(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        self.port.send_text(self.chat_id, text);
        true
    }
}

/// 一个模型 pass 的正文消息：流式编辑同一条，工具调用处封口。
pub struct TextMessage {
    base: TimelineMessage,
}

impl TextMessage {
    pub fn new(port: Arc<dyn MessagePort>, chat_id: i64) -> TextMessage {
        TextMessage {
            base: TimelineMessage::new(port, chat_id),
        }
    }

    pub fn has_message(&self) -> bool {
        self.base.has_message()
    }

    pub fn message_id(&self) -> Option<i64> {
        self.base.message_id()
    }

    /// 流式刷新正文预览；清理后为空时不发空消息。
    pub fn stream(&mut self, text: &str) -> bool {
        let cleaned = clean_text(text);
        if cleaned.is_empty() {
            return false;
        }
        let display = truncate_for_stream(&format!("◇ {cleaned}"));
        self.base.deliver(&display)
    }

    /// 封口本条正文。
    ///
    /// 已有消息时原地定型；超长时消息内显示截断头并返回待补发的完整内容（沿用
    /// `plan_finalize` 的既有语义：「完整内容见下一条」）。没有消息或消息不可用时
    /// 返回待补发的全文，由调用方按普通文本发送。
    pub fn seal(&mut self, content: &str, suffix: &str) -> Option<String> {
        let cleaned = clean_text(content);
        if cleaned.is_empty() {
            return None;
        }
        let full = format!("◇ {cleaned}{suffix}");
        if !self.base.has_message() || !self.base.available() {
            return Some(full);
        }
        match plan_finalize(true, &full) {
            FinalizePlan::EditFull(text) => {
                if self.base.deliver(&text) {
                    None
                } else {
                    Some(full)
                }
            }
            FinalizePlan::EditHeadThenSend { head, full } => {
                self.base.deliver(&head);
                Some(full)
            }
            FinalizePlan::SendFull(text) => Some(text),
        }
    }
}

/// 一条工具调用记录：显示语义与飞书、TUI 的工具卡同源。
pub struct ToolRecord {
    pub key: String,
    pub name: String,
    pub summary: String,
    arguments: Option<Value>,
    pub status: String,
    started_at: Instant,
    finished_at: Option<Instant>,
    body: String,
}

impl ToolRecord {
    pub fn new(key: &str, name: &str, arguments: Value) -> ToolRecord {
        let summary = tool_summary(name, &arguments, "");
        ToolRecord {
            key: key.to_string(),
            name: name.to_string(),
            summary,
            arguments: Some(arguments),
            status: "调用中".to_string(),
            started_at: Instant::now(),
            finished_at: None,
            body: String::new(),
        }
    }

    pub fn running(&self) -> bool {
        self.status == "调用中"
    }

    pub fn duration_seconds(&self) -> f64 {
        let ended = self.finished_at.unwrap_or_else(Instant::now);
        ended.duration_since(self.started_at).as_secs_f64().max(0.0)
    }

    /// 渲染为纯文本：`● 摘要 · ✓ 成功 · 136ms`，正文另起一行。
    ///
    /// 运行中不带耗时（本地不做逐秒编辑，冻结的耗时会误导）。
    pub fn render(&self, with_body: bool) -> String {
        let mut line = format!(
            "● {} · {} {}",
            omnicrawl_session::redact_sensitive_text(&self.summary),
            tool_status_icon(&self.status),
            self.status
        );
        if self.finished_at.is_some() {
            line.push_str(&format!(" · {}", format_duration(self.duration_seconds())));
        }
        if !with_body || self.body.is_empty() {
            return line;
        }
        line.push('\n');
        line.push_str(&omnicrawl_session::redact_sensitive_text(&self.body));
        line
    }

    /// 结果落地：刷新摘要与正文，并释放参数（长参数不再常驻内存）。
    pub fn finish(&mut self, ok: bool, output: &str) {
        let arguments = self.arguments.clone().unwrap_or(Value::Null);
        self.summary = tool_summary(&self.name, &arguments, output);
        self.status = if ok { "成功" } else { "失败" }.to_string();
        self.body = tool_body(&self.name, &arguments, output);
        self.finished_at = Some(Instant::now());
        self.arguments = None;
    }

    /// 把尚未完成的记录收口为「已取消」。
    pub fn abort(&mut self) {
        if TOOL_TERMINAL_STATUSES.contains(&self.status.as_str()) {
            return;
        }
        self.status = "已取消".to_string();
        self.finished_at = Some(Instant::now());
    }
}

/// 一次工具调用的独立消息：调用开始即出现，完成时原地收口。
pub struct ToolMessage {
    base: TimelineMessage,
    pub record: ToolRecord,
}

impl ToolMessage {
    pub fn new(port: Arc<dyn MessagePort>, chat_id: i64, record: ToolRecord) -> ToolMessage {
        ToolMessage {
            base: TimelineMessage::new(port, chat_id),
            record,
        }
    }

    pub fn running(&self) -> bool {
        self.record.running()
    }

    pub fn start(&mut self) -> bool {
        let content = self.record.render(false);
        self.base.deliver(&content)
    }

    pub fn finish(&mut self, ok: bool, output: &str) -> bool {
        self.record.finish(ok, output);
        let content = self.record.render(true);
        if self.base.deliver(&content) {
            return true;
        }
        // 从来没建起消息（创建失败）：整条记录改用普通文本兜底，内容不丢。
        // 已有消息只是这一次编辑失败时不重发全文，下一次交付会重试。
        if self.base.has_message() {
            return false;
        }
        self.base.available = false;
        self.base.send_text(&content);
        false
    }

    pub fn abort(&mut self) -> bool {
        if !self.running() {
            return false;
        }
        self.record.abort();
        if !self.base.has_message() {
            return false;
        }
        let content = self.record.render(false);
        self.base.deliver(&content)
    }
}

/// 思考内容消息（仅 `/thinking on` 时创建）：运行中只显示最新几行，封口给完整内容。
pub struct ReasoningMessage {
    base: TimelineMessage,
}

impl ReasoningMessage {
    pub fn new(port: Arc<dyn MessagePort>, chat_id: i64) -> ReasoningMessage {
        ReasoningMessage {
            base: TimelineMessage::new(port, chat_id),
        }
    }

    pub fn stream(&mut self, text: &str) -> bool {
        let content = format!("🧠 思考中…\n{}", crate::render::reasoning_body(text, true));
        let display = truncate_for_stream(&content);
        self.base.deliver(&display)
    }

    pub fn seal(&mut self, text: &str) -> bool {
        let content = format!("🧠 思考\n{}", crate::render::reasoning_body(text, false));
        let display = truncate_for_stream(&content);
        self.base.deliver(&display)
    }
}

/// 执行计划消息：首次更新时出现，之后原地替换整份清单。
pub struct PlanMessage {
    base: TimelineMessage,
    todos: Vec<(String, bool)>,
}

impl PlanMessage {
    pub fn new(port: Arc<dyn MessagePort>, chat_id: i64) -> PlanMessage {
        PlanMessage {
            base: TimelineMessage::new(port, chat_id),
            todos: Vec::new(),
        }
    }

    pub fn update(&mut self, items: Option<&Value>) -> bool {
        let normalized = normalize_todos(items);
        if normalized.is_empty() || normalized == self.todos {
            return false;
        }
        self.todos = normalized;
        let content = truncate_for_stream(&todos_lines(&self.todos).join("\n"));
        self.base.deliver(&content)
    }
}

/// 同一批子任务的进度消息：首次事件出现，之后原地更新进度树。
pub struct SubAgentMessage {
    base: TimelineMessage,
    nodes: BTreeMap<String, SubagentNode>,
    order: Vec<String>,
}

impl SubAgentMessage {
    pub fn new(port: Arc<dyn MessagePort>, chat_id: i64) -> SubAgentMessage {
        SubAgentMessage {
            base: TimelineMessage::new(port, chat_id),
            nodes: BTreeMap::new(),
            order: Vec::new(),
        }
    }

    /// 按 task_id 原地更新子任务节点；终态节点拒绝迟到的活动事件。
    pub fn update(&mut self, event_name: &str, payload: &Value) -> bool {
        let Some(status) = SUBAGENT_EVENT_STATUS
            .iter()
            .find(|(name, _status)| *name == event_name)
            .map(|(_name, status)| (*status).to_string())
        else {
            return false;
        };
        let task_id = payload
            .get("task_id")
            .map(text_of)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "task".to_string());
        let now = Instant::now();
        let agent_type = payload.get("agent_type").map(text_of).unwrap_or_default();
        let description = payload.get("description").map(text_of).unwrap_or_default();
        let terminal = SUBAGENT_TERMINAL_STATUSES.contains(&status.as_str());
        match self.nodes.get_mut(&task_id) {
            None => {
                let mut node = SubagentNode {
                    agent_type: if agent_type.is_empty() {
                        "subagent".to_string()
                    } else {
                        crate::render::safe_label(Some(&Value::String(agent_type)), 80)
                    },
                    description: crate::render::safe_label(
                        Some(&Value::String(if description.is_empty() {
                            task_id.clone()
                        } else {
                            description
                        })),
                        120,
                    ),
                    status: status.clone(),
                    started_at: None,
                    finished_at: None,
                };
                if matches!(status.as_str(), "running" | "waiting_approval") || terminal {
                    node.started_at = Some(now);
                }
                if terminal {
                    node.finished_at = Some(now);
                }
                self.nodes.insert(task_id.clone(), node);
                self.order.push(task_id);
            }
            Some(node) => {
                if SUBAGENT_TERMINAL_STATUSES.contains(&node.status.as_str()) {
                    return false;
                }
                if !agent_type.is_empty() {
                    node.agent_type =
                        crate::render::safe_label(Some(&Value::String(agent_type)), 80);
                }
                if !description.is_empty() {
                    node.description =
                        crate::render::safe_label(Some(&Value::String(description)), 120);
                }
                node.status = status.clone();
                if matches!(status.as_str(), "running" | "waiting_approval")
                    && node.started_at.is_none()
                {
                    node.started_at = Some(now);
                }
                if terminal {
                    if node.started_at.is_none() {
                        node.started_at = Some(now);
                    }
                    node.finished_at = Some(now);
                }
            }
        }
        let order = self.order.clone();
        let nodes: Vec<(String, SubagentNode)> = self
            .nodes
            .iter()
            .map(|(key, node)| (key.clone(), node.clone()))
            .collect();
        let content = truncate_for_stream(&subagents_lines(&order, &nodes).join("\n"));
        self.base.deliver(&content)
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录型端口：与 bot 的调用序列一一对应。
    #[derive(Default)]
    struct RecordingPort {
        calls: Mutex<Vec<(String, i64, Option<i64>, String)>>,
        /// 让前 N 次编辑失败，模拟限流（429）与网络抖动。
        fail_edits: Mutex<usize>,
    }

    impl RecordingPort {
        fn calls(&self) -> Vec<(String, i64, Option<i64>, String)> {
            self.calls.lock().expect("调用记录锁中毒").clone()
        }

        fn fail_next_edits(&self, count: usize) {
            *self.fail_edits.lock().expect("失败计数锁中毒") = count;
        }
    }

    impl MessagePort for RecordingPort {
        fn create(&self, chat_id: i64, text: &str) -> Option<i64> {
            let mut calls = self.calls.lock().expect("调用记录锁中毒");
            calls.push(("create".to_string(), chat_id, None, text.to_string()));
            Some(calls.len() as i64)
        }

        fn edit(&self, chat_id: i64, message_id: i64, text: &str) -> bool {
            self.calls.lock().expect("调用记录锁中毒").push((
                "edit".to_string(),
                chat_id,
                Some(message_id),
                text.to_string(),
            ));
            let mut remaining = self.fail_edits.lock().expect("失败计数锁中毒");
            if *remaining > 0 {
                *remaining -= 1;
                return false;
            }
            true
        }

        fn send_text(&self, chat_id: i64, text: &str) {
            self.calls.lock().expect("调用记录锁中毒").push((
                "send".to_string(),
                chat_id,
                None,
                text.to_string(),
            ));
        }
    }

    fn port() -> Arc<RecordingPort> {
        Arc::new(RecordingPort::default())
    }

    #[test]
    fn text_message_creates_once_then_edits() {
        let port = port();
        let mut message = TextMessage::new(port.clone(), 7);
        assert!(message.stream("第一段"));
        assert!(message.stream("第一段加长"));
        assert!(message.seal("第一段加长后的定型", "").is_none());
        let calls = port.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[0].0, "create");
        assert_eq!(calls[0].3, "◇ 第一段");
        assert_eq!(calls[1].0, "edit");
        assert_eq!(calls[1].3, "◇ 第一段加长");
        assert_eq!(calls[2].0, "edit");
        assert_eq!(calls[2].3, "◇ 第一段加长后的定型");
    }

    #[test]
    fn identical_content_is_not_resent() {
        let port = port();
        let mut message = TextMessage::new(port.clone(), 7);
        assert!(message.stream("同一段"));
        assert!(message.stream("同一段"));
        assert_eq!(port.calls().len(), 1);
    }

    /// 限流（429）或网络抖动只让这一次交付失败，条目不能就此判死：
    /// 后续增量必须还能把内容推进到下一条状态，否则正文会永久冻结在最早那版。
    #[test]
    fn failed_edit_does_not_freeze_the_entry() {
        let port = port();
        let mut message = TextMessage::new(port.clone(), 7);
        assert!(message.stream("第一段"));
        port.fail_next_edits(1);
        assert!(!message.stream("第一段加长"));
        assert!(message.stream("第一段加长再改"));
        let calls = port.calls();
        assert_eq!(calls.last().expect("有调用记录").3, "◇ 第一段加长再改");
        assert!(message.seal("第一段加长再改", "").is_none());
    }

    #[test]
    fn seal_without_message_returns_full_text() {
        let port = port();
        let mut message = TextMessage::new(port.clone(), 7);
        let remaining = message.seal("只有最终回答", "");
        assert_eq!(remaining, Some("◇ 只有最终回答".to_string()));
        assert!(port.calls().is_empty());
        assert_eq!(message.seal("   ", ""), None);
    }

    #[test]
    fn long_text_is_edited_as_head_with_remainder() {
        let port = port();
        let mut message = TextMessage::new(port.clone(), 7);
        message.stream("开头");
        let long = "长".repeat(5000);
        let remaining = message.seal(&long, "");
        let calls = port.calls();
        let head = &calls.last().expect("有编辑调用").3;
        assert!(head.contains("完整内容见下一条"), "{head}");
        let remaining = remaining.expect("超长正文应返回剩余内容");
        assert!(
            remaining.chars().count() == 5002,
            "剩余内容应完整：{}",
            remaining.chars().count()
        );
    }

    #[test]
    fn tool_message_reports_status_and_body() {
        let port = port();
        let record = ToolRecord::new("c1", "bash", serde_json::json!({"command": "echo hi"}));
        let mut message = ToolMessage::new(port.clone(), 7, record);
        assert!(message.start());
        assert!(message.finish(true, "hi\n"));
        let calls = port.calls();
        assert_eq!(calls[0].3, "● bash echo hi · … 调用中");
        let finished = &calls[1].3;
        assert!(finished.contains("✓ 成功"), "{finished}");
        assert!(finished.contains("\nhi"), "{finished}");
    }

    #[test]
    fn plan_message_replaces_in_place() {
        let port = port();
        let mut message = PlanMessage::new(port.clone(), 7);
        message.update(Some(&serde_json::json!([
            {"step": "第一步", "completed": true},
            {"step": "第二步", "completed": false},
        ])));
        message.update(Some(&serde_json::json!([
            {"step": "第一步", "completed": true},
            {"step": "第二步", "completed": true},
        ])));
        let calls = port.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].0, "create");
        assert_eq!(calls[1].0, "edit");
        assert!(calls[1].3.contains("执行计划 · 2/2 完成"), "{}", calls[1].3);
    }

    #[test]
    fn subagent_progress_updates_single_message() {
        let port = port();
        let mut message = SubAgentMessage::new(port.clone(), 7);
        message.update(
            "subagent.task.started",
            &serde_json::json!({"task_id": "a", "description": "查日志"}),
        );
        message.update(
            "subagent.task.completed",
            &serde_json::json!({"task_id": "a"}),
        );
        let calls = port.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].3.contains("查日志"), "{}", calls[0].3);
        assert!(calls[1].3.contains("完成"), "{}", calls[1].3);
    }
}
