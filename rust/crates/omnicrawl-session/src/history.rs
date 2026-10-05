//! 有状态投影：把事件流增量投影成模型协议消息（含工具批合并、压缩边界与中断补位）。
//!
//! 语义基准是 Python `omnicrawl/state/session_projection.py` 的 `TurnHistoryProjector`
//! 与三个 `project_*` 入口。运行期把每条新落盘事件喂进状态机，轮次收尾时取走本轮新增的
//! 完整协议消息；重启恢复用同一状态机投影整份事件流——两条路径共用唯一实现，
//! 同一会话内的历史与重启后的历史才会逐字一致（前缀缓存因此不失效）。
//!
//! 运行期「已发往 Provider 的 arguments 原文」只在内存里（协议原文不落盘），
//! 由 `TurnHistoryProjector::with_raw_arguments_provider` 注入；拿不到时回落到
//! 落盘的 `arguments_json` 与 `arguments`，保证同一段历史只用一种参数写法。

use std::collections::BTreeSet;
use std::io;

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::event::SessionEvent;
use crate::naming::read_payload_non_negative_int;
use crate::projection::{
    event_to_model_message, function_tool_call, interrupted_tool_result_message,
    tool_result_message, tool_result_output_text, INTERRUPTED_TURN_DEFAULT_SUMMARY,
};

/// 只参与投影、不落盘的临时事件 id 前缀（与 Python 同形）。
pub const PROJECTION_ONLY_EVENT_ID_PREFIX: &str = "projection-only-";
/// 投影专用事件的会话 id：形状合法但不可能与真实会话撞车。
const PROJECTION_ONLY_SESSION_ID: &str = "00000000-000000-000000";

/// 一条投影结果：锚点事件 id 与协议消息。
pub type ProjectedEntry = (String, Value);

/// 按需淘汰落盘的事件类型：载荷的 `evicted_call_ids` 列出已无用的调用。
pub const EVICTED_EVENT_TYPE: &str = "tool_call_evicted";

/// 从历史事件里收集被淘汰的调用 ID（按发生顺序去重）。
///
/// 淘汰是**历史事实**，与压缩边界无关：压缩会按 `remaining_event_ids` 重选事件，边界之前
/// 的标记可能因此不被选中，若只按事件顺序应用，被淘汰的内容就会在压缩重建后复活。
/// 因此读取口径是「整份事件流」，剔除发生在投影出口（见 [`project_history_messages`]）。
pub fn collect_evicted_call_ids(events: &[SessionEvent]) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut ids: Vec<String> = Vec::new();
    for event in events {
        if event.event_type != EVICTED_EVENT_TYPE {
            continue;
        }
        for id in event
            .payload
            .get("evicted_call_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let id = id.trim();
            if !id.is_empty() && seen.insert(id.to_string()) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

/// 按调用 ID 整组剔除一批工具调用，返回真正移除的**消息条数**。
///
/// 一组 = assistant 里 `tool_calls` 的那一项 + 对应的 `tool` 结果消息（`tool_call_evicted`
/// 事件的粒度，见 `omnicrawl_controllers::turn::tool_prune`）。两条协议约束：
///
/// * `tool_calls` 项与其 `tool` 结果必须同时消失，否则 Provider 会判定协议无效；
/// * 同一批调用全被剔除时，assistant 消息还有正文就留下正文（只摘掉 `tool_calls` 字段），
///   没有正文就整条移除——不能留下一条空的工具调用消息。
///
/// 锚点随幸存条目一起保留：压缩边界（`remaining_event_ids`）之后仍按同一份锚点筛选。
/// 与 [`evict_tool_call_messages`] 共用这一份实现（后者只是把消息列表包一层锚点）。
pub fn evict_tool_calls(entries: &mut Vec<ProjectedEntry>, call_ids: &[String]) -> usize {
    let wanted: BTreeSet<&str> = call_ids
        .iter()
        .map(String::as_str)
        .filter(|id| !id.is_empty())
        .collect();
    if wanted.is_empty() {
        return 0;
    }

    let mut removed = 0usize;
    let mut kept: Vec<ProjectedEntry> = Vec::with_capacity(entries.len());
    for (anchor, mut message) in entries.drain(..) {
        match message.get("role").and_then(Value::as_str) {
            Some("tool") => {
                let call_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if wanted.contains(call_id) {
                    removed += 1;
                    continue;
                }
            }
            Some("assistant") => {
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    let total = calls.len();
                    let remaining: Vec<Value> = calls
                        .iter()
                        .filter(|call| !call_is_evicted(call, &wanted))
                        .cloned()
                        .collect();
                    if remaining.len() != total {
                        if remaining.is_empty() {
                            if assistant_has_text(&message) {
                                if let Some(object) = message.as_object_mut() {
                                    object.remove("tool_calls");
                                }
                            } else {
                                removed += 1;
                                continue;
                            }
                        } else if let Some(object) = message.as_object_mut() {
                            object.insert("tool_calls".to_string(), Value::Array(remaining));
                        }
                    }
                }
            }
            _ => {}
        }
        kept.push((anchor, message));
    }
    *entries = kept;
    removed
}

/// 按调用 ID 整组剔除运行期上下文（内核的实时 `messages`）。
pub fn evict_tool_call_messages(messages: &mut Vec<Value>, call_ids: &[String]) -> usize {
    let mut entries: Vec<ProjectedEntry> = std::mem::take(messages)
        .into_iter()
        .enumerate()
        .map(|(index, message)| (index.to_string(), message))
        .collect();
    let removed = evict_tool_calls(&mut entries, call_ids);
    *messages = entries.into_iter().map(|(_, message)| message).collect();
    removed
}

/// 一条 `tool_calls` 项是否属于被剔除的调用（`id` 为空的旧写法回落函数名）。
fn call_is_evicted(call: &Value, wanted: &BTreeSet<&str>) -> bool {
    let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
    if !id.is_empty() {
        return wanted.contains(id);
    }
    call.get("function")
        .and_then(Value::as_object)
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .map(|name| wanted.contains(name))
        .unwrap_or(false)
}

/// assistant 消息是否还有可见正文（只有正文才值得留下这条消息）。
fn assistant_has_text(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::String(text)) => !text.trim().is_empty(),
        // 多模态正文同样算有内容：不替 Provider 猜它能不能为空。
        Some(Value::Array(parts)) => !parts.is_empty(),
        _ => false,
    }
}

/// 工具批次的状态：一批连续的工具调用合并成一条 assistant 消息。
#[derive(Default)]
struct ToolGroup {
    calls: Vec<Value>,
    content: Option<String>,
    reasoning: String,
    anchor: String,
    flushed: bool,
    /// 尚未拿到结果的调用，按插入顺序保存 `(call_id, (tool, 锚点 id))`。
    unresolved: Vec<(String, (String, String))>,
}

impl ToolGroup {
    fn reset(&mut self) {
        self.calls.clear();
        self.content = None;
        self.reasoning.clear();
        self.anchor.clear();
        self.flushed = false;
        self.unresolved.clear();
    }

    fn find(&self, call_id: &str) -> Option<usize> {
        self.unresolved
            .iter()
            .position(|(existing, _)| existing == call_id)
    }

    /// 缺少可匹配 call_id 时，按最近的同名未完成调用解除配对。
    fn drop_last_by_tool(&mut self, tool: &str) {
        if let Some(index) = self
            .unresolved
            .iter()
            .rposition(|(_, (existing, _))| existing == tool)
        {
            self.unresolved.remove(index);
        }
    }
}

pub struct TurnHistoryProjector {
    entries: Vec<ProjectedEntry>,
    tools: ToolGroup,
    /// 运行期「已发往 Provider 的 arguments 原文」提供者：只有内存投影能拿到，
    /// 缺失或空串时回落到落盘的 `arguments_json` / `arguments`。
    raw_arguments_provider: Option<RawArgumentsProvider>,
}

/// `(call_id, tool) -> 发往 Provider 的 arguments 原文`；返回 `None` 或空串表示拿不到原文。
pub type RawArgumentsProvider = Box<dyn Fn(&str, &str) -> Option<String>>;

impl Default for TurnHistoryProjector {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnHistoryProjector {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            tools: ToolGroup::default(),
            raw_arguments_provider: None,
        }
    }

    /// 带上「arguments 原文」提供者：同一段历史只用一种参数写法（原文优先）。
    pub fn with_raw_arguments_provider(provider: RawArgumentsProvider) -> Self {
        Self {
            entries: Vec::new(),
            tools: ToolGroup::default(),
            raw_arguments_provider: Some(provider),
        }
    }

    pub fn set_raw_arguments_provider(&mut self, provider: RawArgumentsProvider) {
        self.raw_arguments_provider = Some(provider);
    }

    /// 丢弃全部状态；压缩边界替换历史后由调用方重新累积后续事件。
    pub fn reset(&mut self) {
        self.entries.clear();
        self.tools.reset();
    }

    /// 按事件类型推进状态机；无关事件类型不产生消息。
    pub fn feed(&mut self, event: &SessionEvent) {
        match event.event_type.as_str() {
            "tool_call_requested" => {
                self.feed_tool_call(&event.event_id, &event.payload);
                return;
            }
            "tool_result" => {
                self.feed_tool_result(&event.event_id, &event.payload);
                return;
            }
            // 拒绝结果由随后的 tool_result 事件补全；不单独投影协议消息。
            "tool_call_denied" => return,
            "user_message" => {
                self.flush_tool_group();
                self.append_event_message(event);
                return;
            }
            "session_interrupted" => {
                self.flush_tool_group();
                self.entries.push((
                    event.event_id.clone(),
                    json!({"role": "assistant", "content": INTERRUPTED_TURN_DEFAULT_SUMMARY}),
                ));
                return;
            }
            "compact_summary" => {
                self.feed_compact_summary(event);
                return;
            }
            "tool_call_summary" => {
                self.feed_tool_call_summary(event);
                return;
            }
            "assistant_message" | "turn_cancelled" | "run_guard_paused" => {
                self.flush_tool_group();
            }
            _ => {}
        }
        self.append_event_message(event);
    }

    /// 结束投影：补齐未收尾的工具组并取走全部条目（同时清空状态）。
    pub fn take(&mut self) -> Vec<ProjectedEntry> {
        self.flush_tool_group();
        std::mem::take(&mut self.entries)
    }

    /// 取走已完成的消息，但保留挂起的工具组（压缩重建后续事件时使用）。
    pub fn drain(&mut self) -> Vec<ProjectedEntry> {
        std::mem::take(&mut self.entries)
    }

    fn append_event_message(&mut self, event: &SessionEvent) {
        if let Some(message) = event_to_model_message(event) {
            self.entries.push((event.event_id.clone(), message));
        }
    }

    /// 把挂起的工具调用落成一条 assistant tool_calls 消息。
    fn flush_tool_calls(&mut self) {
        if self.tools.calls.is_empty() || self.tools.flushed {
            return;
        }
        // content 原样保留（可能是 null 或空串）：任何归一化都会让恢复重建与运行时
        // 发送的消息出现字段差异，从而让前缀缓存无法命中。
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        message.insert(
            "content".to_string(),
            match self.tools.content.clone() {
                Some(text) => Value::String(text),
                None => Value::Null,
            },
        );
        if !self.tools.reasoning.is_empty() {
            message.insert(
                "reasoning_content".to_string(),
                Value::String(self.tools.reasoning.clone()),
            );
        }
        message.insert(
            "tool_calls".to_string(),
            Value::Array(self.tools.calls.clone()),
        );
        self.entries
            .push((self.tools.anchor.clone(), Value::Object(message)));
        self.tools.flushed = true;
    }

    /// 结束当前工具组：先落调用消息，再为未返回结果的调用补中断占位。
    fn flush_tool_group(&mut self) {
        self.flush_tool_calls();
        for (call_id, (tool, anchor)) in self.tools.unresolved.clone() {
            self.entries
                .push((anchor, interrupted_tool_result_message(&tool, &call_id)));
        }
        self.tools.reset();
    }

    fn feed_tool_call(&mut self, event_id: &str, payload: &Map<String, Value>) {
        let tool = payload_tool(payload);
        if tool.is_empty() {
            return;
        }
        if self.tools.flushed {
            // 上一条 assistant 工具消息已经结束；新到调用属于下一条消息。
            self.flush_tool_group();
        }
        let call_id = non_empty(payload, "tool_call_id").unwrap_or_else(|| tool.clone());
        let function_name = non_empty(payload, "function_name").unwrap_or_else(|| tool.clone());
        let arguments = payload.get("arguments").cloned().unwrap_or(Value::Null);

        if self.tools.calls.is_empty() {
            let raw_content = payload.get("assistant_content");
            self.tools.content = match raw_content {
                Some(Value::String(text)) => Some(text.clone()),
                Some(Value::Null) | None => None,
                Some(_) => None,
            };
            // reasoning_content 只在该批次的首次调用事件里取一次：同一批共享一条
            // assistant 消息，逐条覆盖会让消息内容抖动。
            self.tools.reasoning = non_empty(payload, "assistant_reasoning_content")
                .map(|text| text.trim().to_string())
                .unwrap_or_default();
        }

        let raw_arguments = self
            .raw_arguments_provider
            .as_ref()
            .and_then(|provider| provider(&call_id, &tool))
            .filter(|text| !text.trim().is_empty())
            .or_else(|| non_empty(payload, "arguments_json").map(|text| text.trim().to_string()))
            .unwrap_or_else(|| dumps_default(&arguments));

        self.tools
            .calls
            .push(function_tool_call(&call_id, &function_name, &raw_arguments));
        self.tools.anchor = event_id.to_string();
        self.tools
            .unresolved
            .push((call_id.clone(), (tool, event_id.to_string())));
    }

    fn feed_tool_result(&mut self, event_id: &str, payload: &Map<String, Value>) {
        let tool = payload_tool(payload);
        let call_id = non_empty(payload, "tool_call_id").unwrap_or_else(|| tool.clone());
        if self.tools.calls.is_empty() && self.tools.unresolved.is_empty() {
            // 没有配对调用来源的孤立结果无法构成合法协议消息，跳过。
            return;
        }
        self.flush_tool_calls();
        let ok = payload.get("ok").map(python_truthy).unwrap_or(false);
        self.entries.push((
            event_id.to_string(),
            tool_result_message(&tool, ok, &tool_result_output_text(payload), &call_id),
        ));
        match self.tools.find(&call_id) {
            Some(index) => {
                self.tools.unresolved.remove(index);
            }
            None => self.tools.drop_last_by_tool(&tool),
        }
    }

    /// 压缩摘要边界：历史被替换为「摘要 + 保留窗口」。
    fn feed_compact_summary(&mut self, event: &SessionEvent) {
        self.flush_tool_group();
        let summary_message = event_to_model_message(event);
        let recent: Vec<ProjectedEntry> = match remaining_event_ids(&event.payload) {
            Some(wanted) => self
                .entries
                .iter()
                .filter(|(anchor, _)| wanted.contains(anchor.as_str()))
                .cloned()
                .collect(),
            None => {
                let remaining_count = read_payload_non_negative_int(
                    event
                        .payload
                        .get("remaining_message_count")
                        .unwrap_or(&json!(0)),
                );
                if remaining_count == 0 {
                    Vec::new()
                } else {
                    let start = self.entries.len().saturating_sub(remaining_count as usize);
                    self.entries[start..].to_vec()
                }
            }
        };
        let mut next = Vec::new();
        if let Some(message) = summary_message {
            next.push((event.event_id.clone(), message));
        }
        next.extend(recent);
        self.entries = next;
    }
    /// 回合末工具调用概括边界：本轮全部工具调用被压成一段，原逐条请求/结果从上下文剔除。
    ///
    /// `covered_event_ids` 列出被概括的事件；它们从已有条目里删掉，概括文本按边界位置就地插入。
    /// 概括之前的事件（用户消息、更早的助手消息）与概括之后的最终回复都不受影响——
    /// 用户要求保留最后一段模型输出。
    fn feed_tool_call_summary(&mut self, event: &SessionEvent) {
        self.flush_tool_group();
        let Some(covered) = payload_string_ids(event.payload.get("covered_event_ids")) else {
            return;
        };
        self.entries
            .retain(|(anchor, _)| !covered.contains(anchor.as_str()));
        if let Some(message) = event_to_model_message(event) {
            self.entries.push((event.event_id.clone(), message));
        }
    }
}

/// 事件载荷里的字符串 ID 列表；形状不合法时返回 `None`。
fn payload_string_ids(value: Option<&Value>) -> Option<BTreeSet<String>> {
    let items = value?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// 把有效事件流投影为（锚点事件 id，消息）序列。
///
/// 出口处统一按 `tool_call_evicted` 剔除被淘汰的调用——不依赖事件顺序：压缩边界会重选事件，
/// 若只在流中就地应用，边界之前的标记可能被落下，被淘汰的内容就会在压缩后复活。
pub fn project_history_messages(events: &[SessionEvent]) -> Vec<ProjectedEntry> {
    let mut projector = TurnHistoryProjector::new();
    for event in events {
        projector.feed(event);
    }
    let mut entries = projector.take();
    let evicted = collect_evicted_call_ids(events);
    if !evicted.is_empty() {
        evict_tool_calls(&mut entries, &evicted);
    }
    entries
}

/// 投影整份会话历史，并正确处理压缩边界。
pub fn project_session_history(events: &[SessionEvent]) -> Vec<ProjectedEntry> {
    let mut boundary_index: Option<usize> = None;
    let mut boundary_ids: BTreeSet<String> = BTreeSet::new();
    for index in (0..events.len()).rev() {
        if events[index].event_type != "compact_summary" {
            continue;
        }
        if let Some(ids) = remaining_event_ids(&events[index].payload) {
            boundary_index = Some(index);
            boundary_ids = ids;
        }
        break;
    }

    let Some(boundary_index) = boundary_index else {
        return project_history_messages(events);
    };

    // 压缩边界之后的全部事件都是压缩后新增的（恢复指令、后续工具调用、后续最终回复），
    // 必须完整保留；边界之前只保留摘要声明的保留窗口。
    let mut selected: Vec<SessionEvent> = vec![events[boundary_index].clone()];
    selected.extend(
        events
            .iter()
            .enumerate()
            .filter(|(index, event)| {
                *index != boundary_index
                    && event.event_type != "compact_summary"
                    && (*index > boundary_index || boundary_ids.contains(event.event_id.as_str()))
            })
            .map(|(_, event)| event.clone()),
    );
    project_history_messages(&selected)
}

/// 按压缩摘要的保留窗口重建运行期历史消息。
pub fn project_compaction_boundary_history(
    summary_payload: &Map<String, Value>,
    events: &[SessionEvent],
) -> Vec<Value> {
    let summary_event =
        projection_only_event("compact_summary", summary_payload).expect("投影专用事件可构造");
    let messages = |entries: Vec<ProjectedEntry>| {
        entries
            .into_iter()
            .map(|(_, message)| message)
            .collect::<Vec<Value>>()
    };
    let Some(wanted) = remaining_event_ids(summary_payload) else {
        let mut all = vec![summary_event];
        all.extend(events.iter().cloned());
        return messages(project_history_messages(&all));
    };

    let mut selected = vec![summary_event];
    selected.extend(
        events
            .iter()
            .filter(|event| {
                event.event_type != "compact_summary" && wanted.contains(event.event_id.as_str())
            })
            .cloned(),
    );
    messages(project_history_messages(&selected))
}

/// 构造只参与投影、不落盘的临时事件（锚点 id 固定，不会与真实事件冲突）。
pub fn projection_only_event(
    event_type: &str,
    payload: &Map<String, Value>,
) -> Result<SessionEvent, crate::error::SessionStoreError> {
    SessionEvent::from_dict(&json!({
        "version": 1,
        "session_id": PROJECTION_ONLY_SESSION_ID,
        "event_id": format!("{PROJECTION_ONLY_EVENT_ID_PREFIX}{event_type}"),
        "parent_id": null,
        "type": event_type,
        "created_at": "1970-01-01T00:00:00+00:00",
        "payload": Value::Object(payload.clone()),
    }))
}

/// 摘要声明的保留窗口；形状不合法时返回 `None`（调用方走旧的按数量兼容分支）。
fn remaining_event_ids(payload: &Map<String, Value>) -> Option<BTreeSet<String>> {
    let items = payload.get("remaining_event_ids")?.as_array()?;
    if items
        .iter()
        .any(|item| item.as_str().map(str::is_empty).unwrap_or(true))
    {
        return None;
    }
    Some(
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

fn payload_tool(payload: &Map<String, Value>) -> String {
    payload
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn non_empty(payload: &Map<String, Value>, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Python `json.dumps(value, ensure_ascii=False)`：保留插入序、默认分隔符带空格。
fn dumps_default(value: &Value) -> String {
    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, PythonFormatter);
    if value.serialize(&mut serializer).is_err() {
        return String::new();
    }
    String::from_utf8(buffer).unwrap_or_default()
}

struct PythonFormatter;

impl serde_json::ser::Formatter for PythonFormatter {
    fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W>(&mut self, writer: &mut W) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        writer.write_all(b": ")
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

#[cfg(test)]
mod tool_call_summary_tests {
    use super::*;
    use crate::event::SessionEvent;

    fn event(event_type: &str, payload: Value) -> SessionEvent {
        let payload = payload.as_object().cloned().unwrap_or_default();
        SessionEvent::create(
            "20260101-000000-abcdef",
            event_type,
            payload,
            None,
            chrono::Utc::now(),
        )
        .expect("事件可构造")
    }

    /// 概括边界把被覆盖的工具事件从上下文剔除，并就地插入概括文本。
    #[test]
    fn summary_removes_covered_tool_events_and_inserts_the_paragraph() {
        let user = event("user_message", json!({"content": "任务"}));
        let call = event(
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c1"}),
        );
        let result = event(
            "tool_result",
            json!({"tool": "bash", "tool_call_id": "c1", "ok": true, "output": "很长很长"}),
        );
        let summary = event(
            "tool_call_summary",
            json!({"content": "做了什么", "covered_event_ids": [call.event_id, result.event_id]}),
        );
        let events = vec![user.clone(), call, result, summary];
        let projected = project_history_messages(&events);
        let messages: Vec<Value> = projected.into_iter().map(|(_, message)| message).collect();

        assert_eq!(messages.len(), 2, "概括替换掉两条工具事件：{messages:?}");
        assert_eq!(messages[0]["content"], "任务");
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap_or_default()
                .starts_with("本轮工具调用概括："),
            "概括按前缀插入：{messages:?}"
        );
    }

    /// 概括之后落盘的最终回复（结果报告）照旧保留在上下文的最后一段。
    #[test]
    fn the_final_reply_after_the_summary_stays_last() {
        let call = event(
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c1"}),
        );
        let summary = event(
            "tool_call_summary",
            json!({"content": "做了什么", "covered_event_ids": [call.event_id]}),
        );
        let final_reply = event("assistant_message", json!({"content": "结果报告"}));
        let projected = project_history_messages(&[call, summary, final_reply]);
        let messages: Vec<Value> = projected.into_iter().map(|(_, message)| message).collect();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["content"], "结果报告", "最后一段模型输出保持在末尾");
    }

    /// 按需淘汰：批内只走指定那一组，其余调用项与结果照旧配对。
    #[test]
    fn evicting_one_group_keeps_the_rest_of_the_batch() {
        let mut messages = vec![
            json!({"role": "user", "content": "开始"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "c0", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                    {"id": "c1", "type": "function", "function": {"name": "grep", "arguments": "{}"}},
                ],
            }),
            json!({"role": "tool", "tool_call_id": "c0", "content": "结果一"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "结果二"}),
        ];

        let removed = evict_tool_call_messages(&mut messages, &["c1".to_string()]);
        assert_eq!(removed, 1, "只移除 c1 的结果消息；assistant 仍留着 c0 的调用项");
        assert_eq!(messages.len(), 3);
        let calls = messages[1]["tool_calls"].as_array().expect("仍有调用项");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "c0");
        assert_eq!(messages[2]["tool_call_id"], "c0");
    }

    /// 整批都淘汰：没有正文的 assistant 消息整条移除，有正文的只摘掉 `tool_calls`。
    #[test]
    fn evicting_every_group_never_leaves_a_bare_tool_call_message() {
        let mut messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "c0", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
            }),
            json!({"role": "tool", "tool_call_id": "c0", "content": "结果"}),
        ];
        assert_eq!(evict_tool_call_messages(&mut messages, &["c0".to_string()]), 2);
        assert!(messages.is_empty(), "纯工具消息整条移除：{messages:?}");

        let mut with_text = vec![
            json!({
                "role": "assistant",
                "content": "先看一眼目录。",
                "tool_calls": [{"id": "c0", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
            }),
            json!({"role": "tool", "tool_call_id": "c0", "content": "结果"}),
        ];
        assert_eq!(
            evict_tool_call_messages(&mut with_text, &["c0".to_string()]),
            1
        );
        assert_eq!(with_text.len(), 1, "正文留下：{with_text:?}");
        assert_eq!(with_text[0]["content"], "先看一眼目录。");
        assert!(with_text[0].get("tool_calls").is_none());
    }

    /// 不在名单里的消息一律不动：孤儿结果、其它批次、用户消息都不受影响。
    #[test]
    fn unrelated_messages_are_untouched() {
        let mut messages = vec![
            json!({"role": "user", "content": "提问"}),
            json!({"role": "assistant", "content": "回答"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "c9", "type": "function", "function": {"name": "bash", "arguments": "{}"}}],
            }),
            json!({"role": "tool", "tool_call_id": "c9", "content": "结果"}),
        ];
        assert_eq!(
            evict_tool_call_messages(&mut messages, &["c0".to_string()]),
            0
        );
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3]["tool_call_id"], "c9");

        // 空名单与空 ID 都不动消息。
        let mut untouched = messages.clone();
        assert_eq!(evict_tool_call_messages(&mut untouched, &[]), 0);
        assert_eq!(evict_tool_call_messages(&mut untouched, &["".to_string()]), 0);
        assert_eq!(untouched, messages);
    }

    /// 旧写法（调用项没有 id）按函数名匹配。
    #[test]
    fn missing_call_id_falls_back_to_the_function_name() {
        let mut messages = vec![
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"type": "function", "function": {"name": "bash", "arguments": "{}"}}],
            }),
            json!({"role": "tool", "tool_call_id": "bash", "content": "结果"}),
        ];
        assert_eq!(
            evict_tool_call_messages(&mut messages, &["bash".to_string()]),
            2
        );
        assert!(messages.is_empty());
    }

    /// 淘汰事件驱动的投影：被判无用的那一组整组消失，其余照旧配对。
    ///
    /// 这是「重启后同一份上下文」的保证：淘汰是历史事实（事件），`/resume` 与运行期
    /// 走同一份投影，因此被淘汰的内容不会在恢复后重新出现。
    #[test]
    fn evicted_event_removes_exactly_the_evicted_group() {
        let user = event("user_message", json!({"content": "任务"}));
        let first = event(
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c0"}),
        );
        let first_result = event(
            "tool_result",
            json!({"tool": "bash", "tool_call_id": "c0", "ok": true, "output": "过程性探查"}),
        );
        let second = event(
            "tool_call_requested",
            json!({"tool": "read", "tool_call_id": "c1"}),
        );
        let second_result = event(
            "tool_result",
            json!({"tool": "read", "tool_call_id": "c1", "ok": true, "output": "关键内容"}),
        );
        let evicted = event(
            "tool_call_evicted",
            json!({"evicted_call_ids": ["c0"]}),
        );
        let events = vec![
            user,
            first,
            first_result,
            second,
            second_result,
            evicted,
        ];
        let projected = project_history_messages(&events);
        let messages: Vec<Value> = projected.into_iter().map(|(_, message)| message).collect();

        // 幸存的一组仍以「assistant tool_calls + tool 结果」成对出现。
        assert_eq!(messages.len(), 3, "用户消息 + 幸存的一组：{messages:?}");
        assert_eq!(messages[0]["content"], "任务");
        let calls = messages[1]["tool_calls"].as_array().expect("仍有调用项");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(messages[2]["tool_call_id"], "c1");
        assert!(
            messages[2]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("关键内容"),
            "幸存结果的内容照旧可见：{messages:?}"
        );
    }

    /// 淘汰一整批：没有正文的 assistant 工具消息随之消失，不留下非法协议消息。
    #[test]
    fn evicting_a_whole_batch_leaves_no_dangling_tool_call_message() {
        let call = event(
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c0"}),
        );
        let result = event(
            "tool_result",
            json!({"tool": "bash", "tool_call_id": "c0", "ok": true, "output": "过程性探查"}),
        );
        let evicted = event("tool_call_evicted", json!({"evicted_call_ids": ["c0"]}));
        let events = vec![call, result, evicted];
        let projected = project_history_messages(&events);

        assert!(projected.is_empty(), "整批淘汰后不留消息：{projected:?}");
    }

    /// 形状不合法的淘汰事件什么都不动（不能因为一条坏事件把上下文清空）。
    #[test]
    fn malformed_evicted_events_are_ignored() {
        let call = event(
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c0"}),
        );
        let result = event(
            "tool_result",
            json!({"tool": "bash", "tool_call_id": "c0", "ok": true, "output": "结果"}),
        );
        for payload in [
            json!({}),
            json!({"evicted_call_ids": "not-an-array"}),
            json!({"evicted_call_ids": [1, 2]}),
        ] {
            let events = vec![call.clone(), result.clone(), event("tool_call_evicted", payload)];
            let projected = project_history_messages(&events);
            assert_eq!(projected.len(), 2, "坏事件不动上下文：{projected:?}");
        }
    }

    /// 淘汰事件读回调用 ID：跨事件去重，形状不合法的键忽略。
    #[test]
    fn evicted_ids_are_read_back_from_the_event_stream() {
        let events = vec![
            event(
                "tool_call_evicted",
                json!({"evicted_call_ids": ["c1", "c2"]}),
            ),
            event("assistant_message", json!({"content": "回复"})),
            event(
                "tool_call_evicted",
                json!({"evicted_call_ids": ["c2", "  ", "c3"]}),
            ),
            event("tool_call_evicted", json!({"evicted_call_ids": "not-an-array"})),
        ];
        assert_eq!(
            collect_evicted_call_ids(&events),
            vec!["c1".to_string(), "c2".to_string(), "c3".to_string()]
        );
    }

    /// 压缩边界之后仍不复活：标记落在压缩的保留窗口之外时，剔除照样生效。
    ///
    /// 这是「统一在投影出口剔除」的理由：压缩按 `remaining_event_ids` 重选事件，边界之前的
    /// 标记不会被选中，若只在事件顺序里就地应用，被淘汰的内容会在压缩后重新出现。
    #[test]
    fn eviction_survives_a_compaction_boundary() {
        let call = event(
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c0"}),
        );
        let result = event(
            "tool_result",
            json!({"tool": "bash", "tool_call_id": "c0", "ok": true, "output": "过程性探查"}),
        );
        let evicted = event("tool_call_evicted", json!({"evicted_call_ids": ["c0"]}));
        // 压缩摘要声明的保留窗口**不含**淘汰标记，也不含那两条工具事件。
        let summary = event(
            "compact_summary",
            json!({"content": "之前的进展", "remaining_event_ids": []}),
        );
        let final_reply = event("assistant_message", json!({"content": "结果报告"}));
        let events = vec![call, result, evicted, summary, final_reply];

        let projected = project_session_history(&events);
        let messages: Vec<Value> = projected.into_iter().map(|(_, message)| message).collect();
        assert_eq!(messages.len(), 2, "摘要 + 最终回复：{messages:?}");
        assert!(
            messages
                .iter()
                .all(|message| message.get("tool_calls").is_none() && message["role"] != "tool"),
            "被淘汰的调用在压缩后不得复活：{messages:?}"
        );
    }

    /// 投影侧的剔除保留锚点：压缩边界之后仍按同一份锚点筛选保留窗口。
    #[test]
    fn evicting_keeps_the_anchors_of_surviving_entries() {
        let mut entries: Vec<ProjectedEntry> = vec![
            (
                "e1".to_string(),
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"id": "c0", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                        {"id": "c1", "type": "function", "function": {"name": "grep", "arguments": "{}"}},
                    ],
                }),
            ),
            (
                "e2".to_string(),
                json!({"role": "tool", "tool_call_id": "c0", "content": "结果一"}),
            ),
            (
                "e3".to_string(),
                json!({"role": "tool", "tool_call_id": "c1", "content": "结果二"}),
            ),
        ];
        assert_eq!(evict_tool_calls(&mut entries, &["c1".to_string()]), 1);
        assert_eq!(
            entries
                .iter()
                .map(|(anchor, _)| anchor.as_str())
                .collect::<Vec<_>>(),
            vec!["e1", "e2"],
            "锚点随幸存条目保留：{entries:?}"
        );
    }
}
