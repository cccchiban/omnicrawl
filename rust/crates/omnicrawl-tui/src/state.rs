//! 界面状态机：消息流记录、输入框、遥测与待决工具批次的接线。
//!
//! 所有状态变化都发生在主线程，事件来源只有两处：内核帧（[`AppState::apply`]）与
//! 用户输入（输入框与面板）。渲染只读，不修改状态。

use std::time::{Duration, Instant};

use omnicrawl_core::AgentLoopObservation;
use omnicrawl_ipc::{HostEvent, Id};

use crate::args::ApprovalMode;
use crate::host::{self, BatchContext, TodoItem, Waiting};

/// 相邻增量间隔超过这个时长视为待机（工具执行、模型停顿），不计入输出时长。
const IDLE_GAP: Duration = Duration::from_secs(2);

/// 输入框可见行数上限：超过后在编辑器内滚动。
pub const COMPOSER_MAX_LINES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
    started: Instant,
    pub body: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    User(String),
    Reasoning(String),
    Assistant(String),
    Tool(ToolCard),
    Notice(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnState {
    Idle,
    Running { turn_id: String },
}

impl TurnState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Running { turn_id } => Some(turn_id),
            Self::Idle => None,
        }
    }
}

/// 输出速度估计：累计估算 token 数与真正的连续输出时长，间隔超过 [`IDLE_GAP`] 不计时。
#[derive(Debug, Clone, Default)]
pub struct RateEstimator {
    tokens: f64,
    active: Duration,
    last: Option<Instant>,
    value: Option<f64>,
}

impl RateEstimator {
    pub fn record(&mut self, text: &str, now: Instant) {
        self.tokens += estimated_tokens(text) as f64;
        if let Some(last) = self.last {
            let gap = now.saturating_duration_since(last);
            if gap <= IDLE_GAP {
                self.active += gap;
            }
        }
        self.last = Some(now);
        if self.active > Duration::from_millis(200) {
            self.value = Some(self.tokens / self.active.as_secs_f64());
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    /// 模型流中断回滚：撤销本回合累计的量，重新计时。
    pub fn rollback(&mut self) {
        self.tokens = 0.0;
        self.active = Duration::ZERO;
        self.last = None;
        self.value = None;
    }
}

#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub context_window: Option<u64>,
    pub rate: RateEstimator,
}

/// 单行起步、按显示宽度软折行的输入框；最多显示 [`COMPOSER_MAX_LINES`] 行。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composer {
    text: String,
    cursor: usize,
}

impl Composer {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// 取走内容并清空；提交时用。
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        text
    }

    pub fn insert(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        let inserted: Vec<char> = normalized.chars().collect();
        let count = inserted.len();
        chars.splice(at..at, inserted);
        self.text = chars.into_iter().collect();
        self.cursor = at + count;
    }

    pub fn newline(&mut self) {
        self.insert("\n");
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        chars.remove(at - 1);
        self.text = chars.into_iter().collect();
        self.cursor = at - 1;
    }

    pub fn delete(&mut self) {
        let mut chars: Vec<char> = self.text.chars().collect();
        if self.cursor >= chars.len() {
            return;
        }
        chars.remove(self.cursor);
        self.text = chars.into_iter().collect();
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    pub fn move_home(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[..at]
            .iter()
            .rposition(|ch| *ch == '\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[at..]
            .iter()
            .position(|ch| *ch == '\n')
            .map(|index| at + index)
            .unwrap_or(chars.len());
    }

    /// 按显示宽度软折行；阶段一按列断行，不做英文单词级避断。
    pub fn wrapped_lines(&self, width: u16) -> Vec<String> {
        let width = width.max(1) as usize;
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut used = 0usize;
        for ch in self.text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut current));
                used = 0;
                continue;
            }
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cell > width && used > 0 {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(ch);
            used += cell;
        }
        lines.push(current);
        lines
    }

    /// 光标所在的行号与列（列按显示宽度，行按 `width` 软折行后的行）。
    pub fn cursor_position(&self, width: u16) -> (usize, usize) {
        let width = width.max(1) as usize;
        let mut row = 0usize;
        let mut column = 0usize;
        for (index, ch) in self.text.chars().enumerate() {
            if index >= self.cursor {
                break;
            }
            if ch == '\n' {
                row += 1;
                column = 0;
                continue;
            }
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if column + cell > width && column > 0 {
                row += 1;
                column = 0;
            }
            column += cell;
        }
        (row, column)
    }

    /// 需要展示的行与光标在其中的行号：超过行数上限时随光标滚动。
    pub fn visible_lines(&self, width: u16) -> (Vec<String>, usize) {
        let lines = self.wrapped_lines(width);
        let (row, _) = self.cursor_position(width);
        if lines.len() <= COMPOSER_MAX_LINES {
            return (lines, row.min(COMPOSER_MAX_LINES - 1));
        }
        let start = row
            .saturating_sub(COMPOSER_MAX_LINES - 1)
            .min(lines.len() - COMPOSER_MAX_LINES);
        (
            lines[start..start + COMPOSER_MAX_LINES].to_vec(),
            row - start,
        )
    }
}

pub struct AppState {
    pub project: String,
    pub model: String,
    pub approval: ApprovalMode,
    pub version: String,
    pub records: Vec<Record>,
    pub composer: Composer,
    pub scroll_from_bottom: usize,
    pub status: Option<String>,
    pub todos: Vec<TodoItem>,
    pub paused: bool,
    pub turn: TurnState,
    pub telemetry: Telemetry,
    /// 本回合开始时刻；状态行的 spinner 按它推进。
    pub turn_started: Option<Instant>,
    batch: Option<host::PendingBatch>,
}

impl AppState {
    pub fn new(project: String, model: String, approval: ApprovalMode) -> Self {
        Self {
            project,
            model,
            approval,
            version: format!("v{}", env!("CARGO_PKG_VERSION")),
            records: Vec::new(),
            composer: Composer::default(),
            scroll_from_bottom: 0,
            status: None,
            todos: Vec::new(),
            paused: false,
            turn: TurnState::Idle,
            telemetry: Telemetry::default(),
            turn_started: None,
            batch: None,
        }
    }

    pub fn waiting(&self) -> Option<&Waiting> {
        self.batch.as_ref().and_then(|batch| batch.waiting())
    }

    /// 待决批次的请求 id：界面把决定回给内核时要用它配对。
    pub fn batch_request_id(&self) -> Option<Id> {
        self.batch.as_ref().map(|batch| batch.request_id().clone())
    }

    /// 开始一个回合：用户消息落进消息流，输入框清空，滚动回到底部。
    pub fn begin_turn(&mut self, turn_id: String, text: String) {
        self.records.push(Record::User(text));
        self.turn = TurnState::Running { turn_id };
        self.turn_started = Some(Instant::now());
        self.status = None;
        self.paused = false;
        self.scroll_from_bottom = 0;
    }

    /// 提交输入框内容；空输入返回 `None`。
    pub fn submit(&mut self) -> Option<String> {
        if self.composer.is_empty() {
            return None;
        }
        Some(self.composer.take())
    }

    /// 内核事件 → 消息流。
    pub fn apply(&mut self, event: &HostEvent, now: Instant) {
        match event {
            HostEvent::Delta(payload) => {
                self.telemetry.rate.record(&payload.text, now);
                self.append_streamed(&payload.text, false);
            }
            HostEvent::ReasoningDelta(payload) => {
                self.telemetry.rate.record(&payload.text, now);
                self.append_streamed(&payload.text, true);
            }
            HostEvent::Status(payload) | HostEvent::RetryStatus(payload) => {
                self.status = Some(payload.message.clone());
            }
            HostEvent::ProtocolWait => self.status = Some("等待协议…".to_string()),
            HostEvent::StreamRollback => {
                if matches!(self.records.last(), Some(Record::Assistant(_))) {
                    self.records.pop();
                }
                self.telemetry.rate.rollback();
            }
            HostEvent::TokenUsage(payload) => {
                self.telemetry.input_tokens = payload.input_tokens;
                self.telemetry.output_tokens = payload.output_tokens;
                self.telemetry.cached_input_tokens = payload.cached_input_tokens;
            }
            HostEvent::ToolStarted(payload) => {
                self.records.push(Record::Tool(ToolCard {
                    call_id: payload.call.id.clone(),
                    name: payload.call.name.clone(),
                    summary: host::summarize_arguments(&payload.call.arguments),
                    status: ToolStatus::Running,
                    elapsed: None,
                    started: now,
                    body: Vec::new(),
                }));
            }
            HostEvent::ToolFinished(payload) => {
                self.update_tool(&payload.call, &payload.result, now);
            }
            HostEvent::ToolOutputUpdate(payload) => {
                if let Some(card) = self.tool_card_mut(&payload.call.id) {
                    card.body = body_lines(&payload.result.output);
                }
            }
            HostEvent::SubagentEvent(payload) => {
                self.records
                    .push(Record::Notice(format!("并行子任务：{}", payload.name)));
            }
            HostEvent::TodoUpdate(payload) => {
                if let Some(items) = payload.todos.as_array() {
                    let mut map = serde_json::Map::new();
                    map.insert("todos".to_string(), serde_json::Value::Array(items.clone()));
                    self.todos = host::parse_todos(&map);
                }
            }
            HostEvent::TurnFinished(payload) => {
                self.turn = TurnState::Idle;
                self.turn_started = None;
                self.status = None;
                if payload.paused {
                    self.records.push(Record::Notice(
                        "已被模型暂停：本回合不再自动继续。".to_string(),
                    ));
                }
            }
        }
    }

    /// 回合失败或取消：状态复位并把原因写进消息流。
    pub fn fail_turn(&mut self, message: String) {
        self.turn = TurnState::Idle;
        self.turn_started = None;
        self.status = None;
        self.records.push(Record::Notice(message));
    }

    pub fn scroll_by(&mut self, delta: isize) {
        if delta < 0 {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(delta.unsigned_abs());
        } else {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(delta as usize);
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_from_bottom = 0;
    }

    /// 开始处理一个工具批次，推进到等待点、执行点或整批结束。
    pub fn start_batch(
        &mut self,
        request_id: Id,
        calls: Vec<omnicrawl_core::ToolCall>,
    ) -> host::BatchStep {
        let mut batch = host::PendingBatch::new(request_id, calls);
        let step = {
            let mut ctx = self.context();
            batch.advance(&mut ctx)
        };
        self.batch = Some(batch);
        step
    }

    pub fn select_question(&mut self, delta: isize) {
        if let Some(batch) = self.batch.as_mut() {
            batch.select_question(delta);
        }
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
        Some(step)
    }

    /// 执行层回填一个调用的结果；返回是否整批就绪。
    pub fn record_tool_result(
        &mut self,
        index: usize,
        result: omnicrawl_core::ToolResult,
        vision: Option<host::VisionPayload>,
    ) -> bool {
        match self.batch.as_mut() {
            Some(batch) => batch.record_result(index, result, vision),
            None => false,
        }
    }

    /// 整批就绪时取走观察并卸下批次；`native_vision` 决定是否把图片注入下一步请求。
    pub fn take_observations(&mut self, native_vision: bool) -> Option<Vec<AgentLoopObservation>> {
        if !self.batch.as_ref().is_some_and(|batch| batch.is_ready()) {
            return None;
        }
        let batch = self.batch.take()?;
        Some(batch.observations(native_vision))
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
        if !timed_out.is_empty() {
            self.records.push(Record::Notice(format!(
                "工具执行超过 {timeout_seconds} 秒未完成，已按超时继续（后台结果会被丢弃）。"
            )));
        }
        ready
    }

    /// 取消当前批次（`Esc`）：批次卸下，已在执行的工具由取消令牌回收。
    pub fn cancel_batch(&mut self) {
        self.batch = None;
    }

    /// 执行阶段开始：先落一张「运行中」工具卡。
    pub fn begin_tool_run(&mut self, call: &omnicrawl_core::ToolCall, now: Instant) {
        self.records.push(Record::Tool(ToolCard {
            call_id: call.id.clone(),
            name: call.name.clone(),
            summary: host::summarize_arguments(&call.arguments),
            status: ToolStatus::Running,
            elapsed: None,
            started: now,
            body: Vec::new(),
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

    fn context(&mut self) -> BatchContext<'_> {
        BatchContext {
            approval: self.approval,
            todos: &mut self.todos,
            paused: &mut self.paused,
        }
    }

    fn append_streamed(&mut self, text: &str, reasoning: bool) {
        let target = |record: &Record| match record {
            Record::Reasoning(_) => reasoning,
            Record::Assistant(_) => !reasoning,
            _ => false,
        };
        match self.records.last_mut() {
            Some(record) if target(record) => match record {
                Record::Reasoning(body) | Record::Assistant(body) => body.push_str(text),
                _ => unreachable!("target 只匹配思考与正文记录"),
            },
            _ => {
                let body = text.to_string();
                self.records.push(if reasoning {
                    Record::Reasoning(body)
                } else {
                    Record::Assistant(body)
                });
            }
        }
    }

    /// 按 `call_id` 找最近一张工具卡；调用没有 id 时退化为最近一张仍在运行的工具卡。
    fn tool_card_mut(&mut self, call_id: &str) -> Option<&mut ToolCard> {
        self.records.iter_mut().rev().find_map(|record| {
            let Record::Tool(card) = record else {
                return None;
            };
            let hit = if call_id.is_empty() {
                card.status == ToolStatus::Running
            } else {
                card.call_id == call_id
            };
            hit.then_some(card)
        })
    }

    fn update_tool(
        &mut self,
        call: &omnicrawl_core::ToolCall,
        result: &omnicrawl_core::ToolResult,
        now: Instant,
    ) {
        let Some(card) = self.tool_card_mut(&call.id) else {
            return;
        };
        card.status = match (result.ok, result.error_code.as_deref()) {
            (true, _) => ToolStatus::Ok,
            (false, Some(host::DENIED)) => ToolStatus::Denied,
            (false, _) => ToolStatus::Failed,
        };
        card.elapsed = Some(now.saturating_duration_since(card.started));
        card.body = body_lines(&result.output);
    }
}

fn body_lines(output: &str) -> Vec<String> {
    output.lines().map(|line| line.to_string()).collect()
}

/// Token 估算：CJK 字符按 1 token，其他字符按 4 字符 1 token。
pub fn estimated_tokens(text: &str) -> u64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_core::{ToolCall, ToolResult};
    use omnicrawl_ipc::bridge::{
        TextPayload, TodoUpdatePayload, TokenUsagePayload, ToolEventPayload, ToolStartedPayload,
        TurnFinishedPayload,
    };
    use serde_json::json;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: json!({"path": "a.py"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: id.to_string(),
            function_name: name.to_string(),
        }
    }

    fn state() -> AppState {
        AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Manual,
        )
    }

    #[test]
    fn deltas_merge_into_one_record_per_segment() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload { text: "想".into() }),
            now,
        );
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "一下".into(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "回答".into(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "结束".into(),
            }),
            now,
        );

        assert_eq!(
            state.records.len(),
            3,
            "用户 + 思考 + 正文：{:?}",
            state.records
        );
        assert_eq!(state.records[0], Record::User("问一句".to_string()));
        assert_eq!(state.records[1], Record::Reasoning("想一下".to_string()));
        assert_eq!(state.records[2], Record::Assistant("回答结束".to_string()));
    }

    #[test]
    fn tool_cards_track_status_body_and_elapsed() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "跑一下".to_string());
        let tool_call = call("c1", "bash");
        state.apply(
            &HostEvent::ToolStarted(ToolStartedPayload {
                step: 1,
                call: tool_call.clone(),
            }),
            now,
        );
        state.apply(
            &HostEvent::ToolFinished(ToolEventPayload {
                call: tool_call.clone(),
                result: ToolResult {
                    ok: false,
                    output: "第一行\n第二行".to_string(),
                    full_output: String::new(),
                    error_code: Some("denied".to_string()),
                    retryable: false,
                },
            }),
            now + Duration::from_millis(120),
        );

        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Denied);
                assert_eq!(card.body, vec!["第一行".to_string(), "第二行".to_string()]);
                assert_eq!(card.elapsed, Some(Duration::from_millis(120)));
            }
            other => panic!("应当有工具卡，实际：{other:?}"),
        }
    }

    #[test]
    fn stream_rollback_drops_half_streamed_record() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string());
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "半截".into(),
            }),
            now,
        );
        state.apply(&HostEvent::StreamRollback, now);
        assert_eq!(state.records.len(), 1, "正文记录应被撤销");
        assert!(state.telemetry.rate.value().is_none(), "回滚后速度归零");
    }

    #[test]
    fn token_usage_and_finish_reset_status() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string());
        state.apply(
            &HostEvent::Status(omnicrawl_ipc::bridge::MessagePayload {
                message: "正在分析".to_string(),
            }),
            now,
        );
        assert_eq!(state.status.as_deref(), Some("正在分析"));
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5,
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "完成".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 0,
                paused: false,
            }),
            now,
        );
        assert!(!state.turn.is_running());
        assert_eq!(state.status, None);
        assert_eq!(state.telemetry.input_tokens, 10);
        assert_eq!(state.telemetry.cached_input_tokens, 5);
    }

    #[test]
    fn todos_arrive_from_both_tool_and_notification() {
        let mut state = state();
        let now = Instant::now();
        let step = state.start_batch(
            Id::Number(1),
            vec![ToolCall {
                name: host::TODO_TOOL.to_string(),
                arguments: json!({"todos": [{"id": "1", "step": "写骨架", "completed": false}]})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                id: "c1".to_string(),
                function_name: host::TODO_TOOL.to_string(),
            }],
        );
        assert_eq!(step, host::BatchStep::Complete, "自持工具不需要用户介入");
        assert_eq!(state.todos.len(), 1);
        assert_eq!(state.todos[0].step, "写骨架");

        state.apply(
            &HostEvent::TodoUpdate(TodoUpdatePayload {
                todos: json!([{"id": "2", "step": "接审批", "completed": true}]),
            }),
            now,
        );
        assert_eq!(state.todos.len(), 1);
        assert!(state.todos[0].completed);
    }

    #[test]
    fn batch_waiting_for_approval_can_be_decided_through_state() {
        let mut state = state();
        let step = state.start_batch(Id::Number(9), vec![call("c1", "bash")]);
        assert_eq!(step, host::BatchStep::Awaiting, "manual 模式应停在审批");
        assert!(matches!(state.waiting(), Some(Waiting::Approval(_))));

        let jobs = match state.decide_approval(true).expect("批准后应有下一步") {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("批准后应派发执行，实际：{other:?}"),
        };
        assert_eq!(jobs.len(), 1);

        // 执行层回填结果：整批就绪后取走观察。
        let call = jobs[0].1.clone();
        assert!(state.record_tool_result(
            jobs[0].0,
            omnicrawl_core::ToolResult {
                ok: true,
                output: "文件内容".to_string(),
                full_output: "文件内容".to_string(),
                error_code: None,
                retryable: false,
            },
            None
        ));
        let observations = state.take_observations(false).expect("整批就绪应能取观察");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].tool_call.id, call.id);
        assert_eq!(observations[0].result.output, "文件内容");
        assert!(state.batch_request_id().is_none(), "整批结束后不再挂起");
    }

    #[test]
    fn cancelling_a_batch_drops_it_without_observations() {
        let mut state = state();
        state.start_batch(Id::Number(11), vec![call("c1", "bash")]);
        assert!(state.waiting().is_some());
        state.cancel_batch();
        assert!(state.waiting().is_none());
        assert!(state.take_observations(false).is_none());
    }

    #[test]
    fn tool_timeout_reaps_running_cards_and_notes_the_user() {
        let mut state = AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Auto,
        );
        let tool_call = call("c1", "bash");
        let step = state.start_batch(Id::Number(21), vec![tool_call.clone()]);
        let jobs = match step {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("auto 模式应派发执行，实际：{other:?}"),
        };
        assert_eq!(jobs.len(), 1);
        state.begin_tool_run(&jobs[0].1, Instant::now());

        assert!(state.fill_tool_timeout(600), "超时回填后整批就绪");

        match state
            .records
            .iter()
            .rev()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Failed, "运行中的卡片要被收口");
                assert!(
                    card.body.iter().any(|line| line.contains("工具执行超时")),
                    "{:?}",
                    card.body
                );
            }
            other => panic!("应当有工具卡，实际：{other:?}"),
        }
        assert!(
            state.records.iter().any(
                |record| matches!(record, Record::Notice(text) if text.contains("已按超时继续"))
            ),
            "应当给用户一条超时提示：{:?}",
            state.records
        );
        let observations = state.take_observations(false).expect("整批就绪");
        assert!(observations[0].result.output.contains("工具执行超时"));
        assert_eq!(
            observations[0].result.error_code, None,
            "与 Python 一致：超时结果只有文案，不带错误码"
        );
    }

    #[test]
    fn tool_runs_are_rendered_as_cards() {
        let mut state = state();
        let now = Instant::now();
        let tool_call = call("c1", "read");
        state.begin_tool_run(&tool_call, now);
        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Running);
                assert_eq!(card.name, "read");
                assert!(card.elapsed.is_none());
            }
            other => panic!("应落一张运行中的工具卡，实际：{other:?}"),
        }
        state.finish_tool_run(
            &tool_call,
            &omnicrawl_core::ToolResult {
                ok: false,
                output: "出错了".to_string(),
                full_output: "出错了".to_string(),
                error_code: Some("FS_EDIT_NOT_FOUND".to_string()),
                retryable: false,
            },
            now + Duration::from_millis(30),
        );
        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Failed);
                assert_eq!(card.body, vec!["出错了".to_string()]);
                assert_eq!(card.elapsed, Some(Duration::from_millis(30)));
            }
            other => panic!("应更新同一张工具卡，实际：{other:?}"),
        }
    }

    #[test]
    fn composer_edits_by_character_not_byte() {
        let mut composer = Composer::default();
        composer.insert("你好ab");
        composer.move_left();
        composer.insert("世界");
        assert_eq!(composer.text, "你好a世界b");
        assert_eq!(composer.cursor, 5);
        // 光标停在 'b' 之前，退格删掉它左边的 '界'。
        composer.backspace();
        assert_eq!(composer.text, "你好a世b");
        composer.move_home();
        composer.delete();
        assert_eq!(composer.text, "好a世b");
        composer.move_end();
        composer.insert("\n第二行");
        assert_eq!(composer.text, "好a世b\n第二行");
    }

    #[test]
    fn composer_wraps_by_display_width() {
        let mut composer = Composer::default();
        // 全角字符各占 2 列：宽度 4 时每行两个。
        composer.insert("中文测试");
        assert_eq!(composer.wrapped_lines(4), vec!["中文", "测试"]);
        composer.move_home();
        assert_eq!(composer.cursor_position(4), (0, 0));
        assert_eq!(composer.wrapped_lines(1), vec!["中", "文", "测", "试"]);
    }

    #[test]
    fn composer_scrolls_to_cursor_after_five_lines() {
        let mut composer = Composer::default();
        for index in 1..=7 {
            if index > 1 {
                composer.newline();
            }
            composer.insert(&format!("第{index}行"));
        }
        let (lines, cursor_row) = composer.visible_lines(40);
        assert_eq!(lines.len(), COMPOSER_MAX_LINES);
        assert_eq!(lines[cursor_row], "第7行", "光标所在行必须在窗口内");
    }

    #[test]
    fn rate_estimator_ignores_long_idle_gaps() {
        let mut rate = RateEstimator::default();
        let start = Instant::now();
        rate.record("一二三四", start);
        assert!(rate.value().is_none(), "只有一次增量时没有可用的时长");
        rate.record("五六七八", start + Duration::from_millis(500));
        let first = rate.value().expect("累计时长超过 0.2 秒后应有速度");
        assert!((first - 8.0 / 0.5).abs() < 0.01, "实际：{first}");
        // 间隔超过 2 秒视为待机：token 计入，时长不计入。
        rate.record("九十", start + Duration::from_secs(3));
        let value = rate.value().expect("仍应保留平均值");
        assert!((value - 10.0 / 0.5).abs() < 0.01, "实际：{value}");
    }

    #[test]
    fn token_estimate_counts_cjk_as_one_token() {
        assert_eq!(estimated_tokens("你好世界"), 4);
        assert_eq!(estimated_tokens("abcdefgh"), 2);
        assert_eq!(estimated_tokens("你好abcd"), 3);
    }
}
