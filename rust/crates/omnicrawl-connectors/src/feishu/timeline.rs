//! 飞书时间线条目：正文、工具、思考、执行计划与子任务进度各自独立成一条消息。
//!
//! 语义基准是 Python `omnicrawl/connectors/fsapp.py` 的 `_TimelineMessage` /
//! `_TextMessage` / `_ToolMessage` / `_ReasoningMessage` / `_PlanMessage` /
//! `_SubAgentMessage`：首次发送卡片、之后原地 patch 同一条消息；卡片不可用时正文
//! 回退为普通文本消息，保证内容不丢。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::render::{
    card_json, markdown_card, normalize_todos, reasoning_panel, render_tool_record, subagents_text,
    todos_text, tool_body, tool_summary, SubagentNode, SUBAGENT_EVENT_STATUS,
    SUBAGENT_TERMINAL_STATUSES, TOOL_TERMINAL_STATUSES,
};
use super::text::{clean_text, split_segment_for_card};

/// 平台消息端口：发送/更新卡片与发送文本。
pub trait MessagePort: Send + Sync {
    /// 创建消息，返回 message_id；失败返回 None。
    fn send_raw(
        &self,
        receive_id: &str,
        payload: &str,
        msg_type: &str,
        receive_id_type: &str,
    ) -> Option<String>;

    /// 更新卡片内容。
    fn patch_card(&self, message_id: &str, payload: &str) -> bool;

    /// 发送普通文本（内部按上限分段）。
    fn send_text(&self, receive_id: &str, text: &str, receive_id_type: &str) -> bool;
}

/// 时间线条目共用的发送/更新逻辑。
pub struct TimelineMessage {
    port: Arc<dyn MessagePort>,
    pub receive_id: String,
    pub receive_id_type: String,
    pub message_id: Option<String>,
    pub available: bool,
}

impl TimelineMessage {
    pub fn new(
        port: Arc<dyn MessagePort>,
        receive_id: &str,
        receive_id_type: &str,
    ) -> TimelineMessage {
        TimelineMessage {
            port,
            receive_id: receive_id.to_string(),
            receive_id_type: receive_id_type.to_string(),
            message_id: None,
            available: true,
        }
    }

    /// 把卡片内容交给飞书：首次发送，之后 patch 原消息。
    pub fn deliver(&mut self, payload: &str) -> bool {
        if !self.available {
            return false;
        }
        if let Some(message_id) = self.message_id.clone() {
            return self.port.patch_card(&message_id, payload);
        }
        match self.port.send_raw(
            &self.receive_id,
            payload,
            "interactive",
            &self.receive_id_type,
        ) {
            Some(message_id) => {
                self.message_id = Some(message_id);
                true
            }
            None => {
                self.available = false;
                false
            }
        }
    }

    pub fn send_text(&self, text: &str) -> bool {
        self.port
            .send_text(&self.receive_id, text, &self.receive_id_type)
    }
}

/// 一个模型 pass 的正文消息：`◇` 前缀流式更新，工具调用处封口。
pub struct TextMessage {
    base: TimelineMessage,
}

impl TextMessage {
    pub fn new(port: Arc<dyn MessagePort>, receive_id: &str, receive_id_type: &str) -> TextMessage {
        TextMessage {
            base: TimelineMessage::new(port, receive_id, receive_id_type),
        }
    }

    pub fn message_id(&self) -> Option<&str> {
        self.base.message_id.as_deref()
    }

    pub fn available(&self) -> bool {
        self.base.available
    }

    /// 流式刷新正文预览；首片内容被清理后为空时不发空消息。
    pub fn stream(&mut self, text: &str) -> bool {
        let (head, _tail) = split_segment_for_card(&clean_text(text));
        if head.is_empty() {
            return false;
        }
        self.base.deliver(&markdown_card(&format!("◇ {head}")))
    }

    /// 封口本条正文；返回仍需以文本消息补发的剩余内容。
    pub fn seal(&mut self, text: &str, suffix: &str) -> Option<String> {
        let cleaned = clean_text(text);
        if cleaned.is_empty() {
            return None;
        }
        if !self.base.available {
            return Some(format!("{cleaned}{suffix}"));
        }
        let (mut head, tail) = split_segment_for_card(&cleaned);
        if !tail.is_empty() {
            head.push_str("\n\n…（内容较长，其余部分以消息形式发送）");
        }
        if !self
            .base
            .deliver(&markdown_card(&format!("◇ {head}{suffix}")))
        {
            self.base.available = false;
            return Some(format!("{cleaned}{suffix}"));
        }
        if tail.is_empty() {
            None
        } else {
            Some(tail)
        }
    }
}

/// 一条工具调用记录，展示语义与 TUI 工具卡一致。
pub struct ToolRecord {
    pub key: String,
    pub name: String,
    pub summary: String,
    arguments: Option<Value>,
    pub status: String,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub body: String,
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

    /// 渲染为 markdown；运行中不带耗时（本地不做逐秒 patch，冻结的耗时会误导）。
    pub fn render(&self, with_body: bool) -> String {
        let duration = self.finished_at.map(|_ended| self.duration_seconds());
        render_tool_record(&self.summary, &self.status, duration, &self.body, with_body)
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

/// 一次工具调用的独立消息：开始即出现，完成时原地收口。
pub struct ToolMessage {
    base: TimelineMessage,
    pub record: ToolRecord,
}

impl ToolMessage {
    pub fn new(
        port: Arc<dyn MessagePort>,
        receive_id: &str,
        receive_id_type: &str,
        record: ToolRecord,
    ) -> ToolMessage {
        ToolMessage {
            base: TimelineMessage::new(port, receive_id, receive_id_type),
            record,
        }
    }

    pub fn running(&self) -> bool {
        self.record.running()
    }

    pub fn start(&mut self) -> bool {
        let content = self.record.render(false);
        self.base.deliver(&markdown_card(&content))
    }

    pub fn finish(&mut self, ok: bool, output: &str) -> bool {
        self.record.finish(ok, output);
        let content = self.record.render(true);
        if self.base.deliver(&markdown_card(&content)) {
            return true;
        }
        // 卡片创建或更新失败：整条记录改用文本消息兜底。
        self.base.available = false;
        self.base.send_text(&content);
        false
    }

    pub fn abort(&mut self) -> bool {
        if !self.running() {
            return false;
        }
        self.record.abort();
        if self.base.message_id.is_none() {
            return false;
        }
        let content = self.record.render(false);
        self.base.deliver(&markdown_card(&content))
    }
}

/// 思考折叠面板消息（仅 `/thinking on` 时创建）。
pub struct ReasoningMessage {
    base: TimelineMessage,
}

impl ReasoningMessage {
    pub fn new(
        port: Arc<dyn MessagePort>,
        receive_id: &str,
        receive_id_type: &str,
    ) -> ReasoningMessage {
        ReasoningMessage {
            base: TimelineMessage::new(port, receive_id, receive_id_type),
        }
    }

    pub fn stream(&mut self, text: &str) -> bool {
        let panel = reasoning_panel(text, true);
        self.base.deliver(&card_json(&[panel]))
    }

    pub fn seal(&mut self, text: &str) -> bool {
        let panel = reasoning_panel(text, false);
        self.base.deliver(&card_json(&[panel]))
    }
}

/// 执行计划消息：首次更新时出现，之后原地替换整份清单。
pub struct PlanMessage {
    base: TimelineMessage,
    todos: Vec<(String, bool)>,
}

impl PlanMessage {
    pub fn new(port: Arc<dyn MessagePort>, receive_id: &str, receive_id_type: &str) -> PlanMessage {
        PlanMessage {
            base: TimelineMessage::new(port, receive_id, receive_id_type),
            todos: Vec::new(),
        }
    }

    pub fn update(&mut self, items: Option<&Value>) -> bool {
        let normalized = normalize_todos(items);
        if normalized.is_empty() || normalized == self.todos {
            return false;
        }
        self.todos = normalized;
        let content = todos_text(&self.todos);
        self.base.deliver(&markdown_card(&content))
    }
}

/// 同一批子任务的进度消息：首次事件出现，之后原地更新进度树。
pub struct SubAgentMessage {
    base: TimelineMessage,
    nodes: BTreeMap<String, SubagentNode>,
    order: Vec<String>,
}

impl SubAgentMessage {
    pub fn new(
        port: Arc<dyn MessagePort>,
        receive_id: &str,
        receive_id_type: &str,
    ) -> SubAgentMessage {
        SubAgentMessage {
            base: TimelineMessage::new(port, receive_id, receive_id_type),
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
        match self.nodes.get_mut(&task_id) {
            None => {
                let node = SubagentNode {
                    agent_type: if agent_type.is_empty() {
                        "subagent".to_string()
                    } else {
                        super::render::safe_label(Some(&Value::String(agent_type)), 80)
                    },
                    description: super::render::safe_label(
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
                let mut node = node;
                if matches!(status.as_str(), "running" | "waiting_approval") {
                    node.started_at = Some(now);
                }
                if SUBAGENT_TERMINAL_STATUSES.contains(&status.as_str()) {
                    node.started_at = Some(now);
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
                        super::render::safe_label(Some(&Value::String(agent_type)), 80);
                }
                if !description.is_empty() {
                    node.description =
                        super::render::safe_label(Some(&Value::String(description)), 120);
                }
                node.status = status.clone();
                if matches!(status.as_str(), "running" | "waiting_approval")
                    && node.started_at.is_none()
                {
                    node.started_at = Some(now);
                }
                if SUBAGENT_TERMINAL_STATUSES.contains(&status.as_str()) {
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
        let content = subagents_text(&order, &nodes);
        self.base.deliver(&markdown_card(&content))
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
