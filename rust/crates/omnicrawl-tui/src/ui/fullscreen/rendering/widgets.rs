//! 全屏工作台可复用的展示组件（对映 `rendering/widgets.py`）。
//!
//! 本批落不依赖 Markdown 引擎的一组：子任务进度树、任务清单、子任务会话面板，
//! 以及它们共用的状态常量与换行辅助。Textual 的挂载、焦点与选择由装配层承接；
//! 这里保留纯状态与 Rich 文本构造（与 Python 的 `render_text` 同形），供渲染与测试
//! 复核。`AssistantMessage` / `ReasoningDisclosure` / `ToolDisclosure` 与
//! `ConfirmationScreen` 随后续批次补入。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::Value;
use unicode_width::UnicodeWidthChar;

use crate::ui::fullscreen::rendering::latex;
use crate::ui::fullscreen::rendering::markdown;
use crate::ui::fullscreen::rendering::tool_diff::{
    tool_disclosure_body, tool_disclosure_title, tool_operation, ASK_USER_TOOL_NAME,
    FILE_CHANGE_TOOLS,
};
use crate::ui::fullscreen::text::StyledText;

/// 对映 `_SUBAGENT_TERMINAL_STATUSES` 与 `_SUBAGENT_STATUS_PRESENTATION` 的键集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Queued,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

impl SubAgentStatus {
    /// 对映 `if status not in _SUBAGENT_STATUS_PRESENTATION`：无法识别的状态被忽略。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "waiting_approval" => Some(Self::WaitingApproval),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// `(图标, 状态标签, 样式串)`，与 `_SUBAGENT_STATUS_PRESENTATION` 逐条一致。
    pub fn presentation(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Queued => ("○", "等待中", "dim"),
            Self::Running => ("●", "运行中", "blue"),
            Self::WaitingApproval => ("◆", "等待审批", "yellow"),
            Self::Completed => ("✓", "完成", "green"),
            Self::Failed => ("×", "失败", "red"),
            Self::Cancelled => ("–", "已取消", "dim"),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// 对映 `time.perf_counter()`：进程内单调时钟，基准任意、只用于求差。
pub fn now_seconds() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// 折叠空白并按字符数截断；来源为空时回落到 `fallback`。
fn collapse_whitespace(value: &str, fallback: &str, limit: usize) -> String {
    let source = if value.is_empty() { fallback } else { value };
    source
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn is_wide(ch: char) -> bool {
    UnicodeWidthChar::width(ch).unwrap_or(0) >= 2
}

/// 进度树中的单个安全任务节点，不保存 prompt、结果或异常详情。
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentProgressItem {
    pub task_id: String,
    pub agent_type: String,
    pub description: String,
    pub status: SubAgentStatus,
    pub first_seen_at: f64,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
}

/// 按批次原地更新的 SubAgent 进度树。
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentProgressTree {
    pub batch_id: String,
    tasks: HashMap<String, SubAgentProgressItem>,
    task_order: Vec<String>,
    last_rendered_at: f64,
    rendered_plain: String,
}

impl SubAgentProgressTree {
    pub fn new(batch_id: &str) -> Self {
        let now = now_seconds();
        let mut tree = Self {
            batch_id: batch_id.to_string(),
            tasks: HashMap::new(),
            task_order: Vec::new(),
            last_rendered_at: now,
            rendered_plain: String::new(),
        };
        // Python `__init__` 末尾先渲染一次，使空树也有已渲染文本。
        tree.refresh_display(now);
        tree
    }

    /// 批次中仍有等待、运行或等待审批的任务时返回 `true`。
    pub fn is_active(&self) -> bool {
        self.tasks.values().any(|task| !task.status.is_terminal())
    }

    pub fn tasks(&self) -> impl Iterator<Item = &SubAgentProgressItem> {
        self.task_order.iter().filter_map(|id| self.tasks.get(id))
    }

    pub fn last_rendered_at(&self) -> f64 {
        self.last_rendered_at
    }

    /// 新增或更新任务节点；终态节点不会被迟到的活动事件回退。
    ///
    /// 返回是否应用了本次更新（状态无法识别、或节点已处于终态时为 `false`）。
    pub fn update_task(
        &mut self,
        task_id: &str,
        agent_type: &str,
        description: &str,
        status: &str,
        now: Option<f64>,
    ) -> bool {
        let Some(status) = SubAgentStatus::parse(status) else {
            return false;
        };
        let now = now.unwrap_or_else(now_seconds);
        let safe_task_id = {
            let cut: String = task_id.trim().chars().take(120).collect();
            if cut.is_empty() {
                "task".to_string()
            } else {
                cut
            }
        };
        let safe_agent_type = collapse_whitespace(agent_type, "subagent", 80)
            .trim()
            .to_string();
        let safe_description = collapse_whitespace(description, &safe_task_id, 120)
            .trim()
            .to_string();

        if let Some(task) = self.tasks.get_mut(&safe_task_id) {
            if task.status.is_terminal() {
                // 后台线程的迟到事件不得让已完成节点重新显示为等待或运行。
                return false;
            }
            if !safe_agent_type.is_empty() {
                task.agent_type = safe_agent_type;
            }
            if !safe_description.is_empty() {
                task.description = safe_description;
            }
            task.status = status;
        } else {
            self.tasks.insert(
                safe_task_id.clone(),
                SubAgentProgressItem {
                    task_id: safe_task_id.clone(),
                    agent_type: if safe_agent_type.is_empty() {
                        "subagent".to_string()
                    } else {
                        safe_agent_type
                    },
                    description: if safe_description.is_empty() {
                        safe_task_id.clone()
                    } else {
                        safe_description
                    },
                    status,
                    first_seen_at: now,
                    started_at: None,
                    finished_at: None,
                },
            );
            self.task_order.push(safe_task_id.clone());
        }

        if let Some(task) = self.tasks.get_mut(&safe_task_id) {
            if matches!(
                status,
                SubAgentStatus::Running | SubAgentStatus::WaitingApproval
            ) && task.started_at.is_none()
            {
                task.started_at = Some(now);
            }
            if status.is_terminal() {
                if task.started_at.is_none() {
                    task.started_at = Some(task.first_seen_at);
                }
                task.finished_at = Some(now);
            }
        }

        self.refresh_display(now);
        true
    }

    /// 仅在存在活动任务时刷新运行耗时，避免终态树持续重绘。
    pub fn refresh_elapsed(&mut self, now: Option<f64>) -> bool {
        if !self.is_active() {
            return false;
        }
        self.refresh_display(now.unwrap_or_else(now_seconds))
    }

    /// 构建当前树的 Rich 文本，供渲染与测试复核。
    pub fn render_text(&self, now: Option<f64>) -> StyledText {
        let now = now.unwrap_or(self.last_rendered_at);
        let mut rendered = StyledText::new();
        let total = self.task_order.len();
        let completed = self
            .task_order
            .iter()
            .filter(|task_id| {
                self.tasks
                    .get(*task_id)
                    .is_some_and(|task| task.status == SubAgentStatus::Completed)
            })
            .count();
        let root_label = if total > 1 {
            "◇ 并行子任务"
        } else {
            "◇ 子任务进度"
        };
        rendered.push(root_label, "bold");
        if total > 0 {
            rendered.push(&format!("  {completed}/{total} 完成"), "dim");
        }

        for (index, task_id) in self.task_order.iter().enumerate() {
            let Some(task) = self.tasks.get(task_id) else {
                continue;
            };
            let (icon, status_label, status_style) = task.status.presentation();
            let connector = if index == total - 1 {
                "└─"
            } else {
                "├─"
            };
            rendered.push("\n", "");
            rendered.push(&format!("{connector} "), "dim");
            rendered.push(&format!("{icon} "), status_style);
            rendered.push(&task.description, "");
            rendered.push(&format!("  {}", task.agent_type), "dim");
            rendered.push(&format!(" · {status_label}"), status_style);
            if let Some(started_at) = task.started_at {
                let ended_at = task.finished_at.unwrap_or(now);
                rendered.push(
                    &format!(" · {}", format_elapsed(ended_at - started_at)),
                    "dim",
                );
            }
        }
        rendered
    }

    /// 对映 `_refresh_display`：渲染结果与上次一致时跳过重绘。
    fn refresh_display(&mut self, now: f64) -> bool {
        self.last_rendered_at = now;
        let rendered = self.render_text(Some(now));
        if rendered.plain() == self.rendered_plain {
            return false;
        }
        self.rendered_plain = rendered.plain();
        true
    }
}

/// 对映 `SubAgentProgressTree._format_elapsed`。
pub fn format_elapsed(seconds: f64) -> String {
    let total_seconds = seconds.max(0.0) as i64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// 任务清单里的一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub step: String,
    pub completed: bool,
}

/// 显示 Agent 自动维护的紧凑执行清单。
#[derive(Debug, Clone, Default)]
pub struct TodoPlan {
    items: Vec<TodoItem>,
}

impl TodoPlan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    /// 计划区占用的紧凑行数；每个步骤恰好一行。
    pub fn row_count(&self) -> usize {
        self.items.len()
    }

    pub fn is_visible(&self) -> bool {
        !self.items.is_empty()
    }

    /// 替换计划内容，过滤空步骤并限制单步长度。
    pub fn update_items(&mut self, items: &serde_json::Value) {
        self.items = normalize_todo_items(items);
    }

    /// 直接接收已归一化的步骤（内核 `update_todos` 的自持路径）。
    pub fn update_steps(&mut self, items: Vec<TodoItem>) {
        self.items = items
            .into_iter()
            .filter(|item| !item.step.trim().is_empty())
            .take(20)
            .collect();
    }

    pub fn render_text(&self) -> StyledText {
        let mut rendered = StyledText::new();
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                rendered.push("\n", "");
            }
            if item.completed {
                rendered.push("▣", "green");
            } else {
                rendered.push("▢", "dim");
            }
            rendered.push(&format!(" {}", item.step), "");
        }
        rendered
    }
}

/// 对映 `TodoPlan.update_items`：`[{step|description|title, completed|status}]`。
pub fn normalize_todo_items(items: &serde_json::Value) -> Vec<TodoItem> {
    let mut normalized: Vec<TodoItem> = Vec::new();
    let Some(list) = items.as_array() else {
        return normalized;
    };
    for item in list.iter().take(20) {
        let Some(map) = item.as_object() else {
            continue;
        };
        let text = ["step", "description", "title"]
            .iter()
            .map(|key| value_text(map.get(*key)))
            .find(|text| !text.is_empty())
            .unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let completed = truthy(map.get("completed"))
            || matches!(
                map.get("status")
                    .and_then(|value| value.as_str())
                    .map(str::to_lowercase)
                    .as_deref(),
                Some("completed") | Some("done") | Some("complete")
            );
        normalized.push(TodoItem {
            step: text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(240)
                .collect(),
            completed,
        });
    }
    normalized
}

/// 对映 `str(value or "")`：只接受可无损转文本的标量。
fn value_text(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Bool(flag)) => flag.to_string(),
        Some(serde_json::Value::Number(number)) => number.to_string(),
        Some(_) => String::new(),
    }
}

/// 对映 Python `bool(value)`。
fn truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Number(number)) => {
            number.as_f64().is_some_and(|value| value != 0.0)
        }
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(serde_json::Value::Object(map)) => !map.is_empty(),
    }
}

/// 左侧 `│ ` 竖线 + 底部 `╰` 圆角转角包裹的子代理会话面板。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgentConversation {
    pub batch_id: String,
    pub agent_type: String,
    rows: Vec<(String, String)>,
    finished: bool,
}

impl SubAgentConversation {
    pub fn new(batch_id: &str, agent_type: &str) -> Self {
        let label = if agent_type.is_empty() {
            "subagent"
        } else {
            agent_type
        };
        Self {
            batch_id: batch_id.to_string(),
            agent_type: agent_type.to_string(),
            rows: vec![(format!("◇ {label} 子代理对话"), "bold".to_string())],
            finished: false,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.finished
    }

    /// 面板原始逻辑行，不包含按终端宽度产生的软折行。
    pub fn logical_text(&self) -> String {
        self.rows
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 追加一行内容；终态面板忽略后续行。
    pub fn append(&mut self, text: &str, style: &str) {
        if self.finished {
            return;
        }
        self.rows.push((text.to_string(), style.to_string()));
    }

    /// 收口面板：追加终态状态行并落 `╰` 圆角转角，不再接受新行。
    pub fn finish(&mut self, status: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let style = if status.contains("完成") || status.contains("成功") {
            "green"
        } else {
            "red"
        };
        self.rows.push((status.to_string(), style.to_string()));
    }

    pub fn rows(&self) -> &[(String, String)] {
        &self.rows
    }

    pub fn render_text(&self, width: usize) -> StyledText {
        let mut rendered = StyledText::new();
        for (text, style) in &self.rows {
            for line in wrap_subagent_line(text, width) {
                rendered.push("│ ", "dim");
                rendered.push(&line, style);
                rendered.push("\n", "");
            }
        }
        rendered.push("╰", "dim");
        rendered
    }
}

/// 按显示宽度换行：保留原始空白，CJK 字符占 2 列。
pub fn wrap_subagent_line(text: &str, width: usize) -> Vec<String> {
    let content_width = width.saturating_sub(2).max(4); // 预留 "│ " 两列
    let mut wrapped: Vec<String> = Vec::new();
    let raw_lines: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.lines().collect()
    };
    for raw in raw_lines {
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in raw.chars() {
            let char_width = if is_wide(ch) { 2 } else { 1 };
            if current_width + char_width > content_width && !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
                current.push(ch);
                current_width = char_width;
            } else {
                current.push(ch);
                current_width += char_width;
            }
        }
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        vec![String::new()]
    } else {
        wrapped
    }
}

/// 状态 → 语义 class（方案6：状态色点 + 缩进，无边框）。
pub const STATUS_CLASSES: &[(&str, &str)] = &[
    ("调用中", "tool-running"),
    ("成功", "tool-ok"),
    ("失败", "tool-fail"),
    ("等待确认", "tool-pending"),
    ("已取消", "tool-cancelled"),
    ("等待回复", "tool-running"),
    ("已收到回复", "tool-ok"),
];

/// 工具展开正文的行数上限（不含标题行）。
pub const MAX_EXPANDED_BODY_LINES: usize = 5;
/// 正文被缩略时首部与尾部各保留的有效行数。
pub const HEAD_BODY_LINES: usize = 2;
pub const TAIL_BODY_LINES: usize = 2;
/// 方案6：正文相对标题的缩进宽度（4 空格）。
pub const BODY_INDENT: &str = "    ";
/// 省略区提示行文案：占位符是中间被省略的有效行数。
pub const EXPAND_HINT: &str = "点击展开 {lines} 行";
/// 豁免五行限制的工具（保持完整正文展示）。
pub const UNLIMITED_BODY_TOOLS: &[&str] = &["write_file", "Edit_file"];
// 正文流式渲染：块数与间隔共同决定观感（总时长约 0.36 秒）；行数不足阈值的短正文直接显示。
pub const STREAM_BODY_CHUNKS: usize = 12;
pub const STREAM_BODY_INTERVAL: f64 = 0.03;
pub const STREAM_BODY_MIN_LINES: usize = 6;
/// 状态行尾部的 `[ ESC ]` 提示；前导空格是状态正文与提示之间的分隔。
pub const ESC_HINT: &str = " [ ESC ]";

/// 给正文每一行加统一缩进（方案6：正文相对标题缩进）。
pub fn indent_body_lines(body: &StyledText, prefix: &str) -> StyledText {
    let mut rendered = StyledText::new();
    for (index, part) in body.split_lines().iter().enumerate() {
        if index > 0 {
            rendered.push("\n", "");
        }
        if !part.plain().trim().is_empty() {
            rendered.push(prefix, "");
        }
        rendered.append_text(part);
    }
    rendered
}

/// 省略区提示行文本：缩进单独成段并关闭下划线，避免下划线画到缩进区。
pub fn body_hint_text(hidden_lines: usize) -> StyledText {
    let mut hint = StyledText::new();
    hint.push(BODY_INDENT, "not underline");
    hint.push(
        &EXPAND_HINT.replace("{lines}", &hidden_lines.to_string()),
        "",
    );
    hint
}

/// 工具调用记录；正文默认缩略为头尾，省略区留一行可点击提示。
///
/// 标题行、正文区与提示行在 Python 侧是彼此独立的子组件；Rust 侧保留同一份状态，
/// 既给出整块 [`ToolDisclosure::content`]，也可分别取用各部件供渲染与热区判定。
#[derive(Debug, Clone)]
pub struct ToolDisclosure {
    pub tool_name: String,
    pub started_at: f64,
    pub status: String,
    pub duration_seconds: f64,
    arguments: Option<Value>,
    result_text: String,
    limit_body_lines: bool,
    expanded: bool,
    title_text: StyledText,
    body_source: StyledText,
    display_text: StyledText,
    body_line: StyledText,
    tail_line: Option<StyledText>,
    hint_text: Option<StyledText>,
    shown_title_plain: Option<String>,
    stream_full_source: Option<StyledText>,
    stream_shown_lines: usize,
    stream_step_lines: usize,
}

impl ToolDisclosure {
    pub fn new(tool_name: &str, arguments: Value, started_at: f64) -> Self {
        let status = if tool_name == ASK_USER_TOOL_NAME {
            "等待回复"
        } else {
            "调用中"
        };
        let mut disclosure = Self {
            tool_name: tool_name.to_string(),
            started_at,
            status: status.to_string(),
            duration_seconds: 0.0,
            arguments: Some(arguments),
            result_text: String::new(),
            limit_body_lines: !UNLIMITED_BODY_TOOLS.contains(&tool_name),
            expanded: false,
            title_text: StyledText::new(),
            body_source: StyledText::new(),
            display_text: StyledText::new(),
            body_line: StyledText::new(),
            tail_line: None,
            hint_text: None,
            shown_title_plain: None,
            stream_full_source: None,
            stream_shown_lines: 0,
            stream_step_lines: 0,
        };
        disclosure.refresh_display();
        disclosure
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    pub fn arguments(&self) -> Option<&Value> {
        self.arguments.as_ref()
    }

    pub fn result_text(&self) -> &str {
        &self.result_text
    }

    /// 当前显示内容（标题 + 正文区 + 省略提示行）。
    pub fn content(&self) -> &StyledText {
        &self.display_text
    }

    pub fn title_text(&self) -> &StyledText {
        &self.title_text
    }

    pub fn body_line(&self) -> &StyledText {
        &self.body_line
    }

    pub fn tail_line(&self) -> Option<&StyledText> {
        self.tail_line.as_ref()
    }

    pub fn hint_text(&self) -> Option<&StyledText> {
        self.hint_text.as_ref()
    }

    /// 终态正文是否仍在分块释放（供调用方在释放结束后恢复滚动）。
    pub fn is_body_streaming(&self) -> bool {
        self.stream_full_source.is_some()
    }

    pub fn stream_shown_lines(&self) -> usize {
        self.stream_shown_lines
    }

    pub fn stream_step_lines(&self) -> usize {
        self.stream_step_lines
    }

    /// 状态对应的语义 class（对映 `_apply_status_class` 的目标 class）。
    pub fn status_class(&self) -> Option<&'static str> {
        STATUS_CLASSES
            .iter()
            .find(|(status, _)| *status == self.status)
            .map(|(_, class)| *class)
    }

    /// 收口工具卡；返回是否进入了正文流式释放。
    pub fn finish(&mut self, ok: bool, output: &str, finished_at: f64, stream: bool) -> bool {
        self.status = if self.tool_name == ASK_USER_TOOL_NAME {
            if ok {
                "已收到回复"
            } else {
                "已取消"
            }
        } else if ok {
            "成功"
        } else {
            "失败"
        }
        .to_string();
        self.duration_seconds = (finished_at - self.started_at).max(0.0);
        self.result_text = output.to_string();
        self.refresh_display();
        let streaming = stream && self.start_body_stream();
        // 终态已渲染进正文源，清空参数与结果原文，避免大内容随历时长滞留。
        self.arguments = None;
        self.result_text = String::new();
        streaming
    }

    /// 调用期间实时刷新已耗时；终态记录不再重绘。
    pub fn refresh_elapsed(&mut self, now: Option<f64>) -> bool {
        if self.status != "调用中" && self.status != "等待回复" {
            return false;
        }
        self.duration_seconds = (now.unwrap_or_else(now_seconds) - self.started_at).max(0.0);
        self.refresh_display()
    }

    /// 展开被省略的正文（由省略区提示行点击触发）。
    pub fn expand_body(&mut self) {
        if self.expanded {
            return;
        }
        self.expanded = true;
        self.finish_body_stream();
        self.render_body();
    }

    /// 展开态点击工具卡回到缩略状态（提示行自身的点击由提示行消费）。
    pub fn collapse(&mut self) {
        if !self.expanded {
            return;
        }
        self.expanded = false;
        self.render_body();
    }

    /// 替换正文区文本，保留已渲染的标题与状态（工具输出压缩的收口路径）。
    pub fn update_body(&mut self, output: &str) {
        if FILE_CHANGE_TOOLS.contains(&tool_operation(&self.tool_name)) {
            return;
        }
        self.cancel_body_stream();
        self.body_source = tool_disclosure_body(&self.tool_name, &Value::Null, output);
        self.render_body();
    }

    /// 按展开状态重绘正文区；标题未变时整卡内容未变，跳过重建。
    fn refresh_display(&mut self) -> bool {
        let arguments = self.arguments.clone().unwrap_or(Value::Null);
        let title = tool_disclosure_title(
            &self.tool_name,
            &arguments,
            &self.status,
            self.duration_seconds,
            true,
            &self.result_text,
        );
        if self.shown_title_plain.as_deref() == Some(title.plain().as_str()) {
            return false;
        }
        self.shown_title_plain = Some(title.plain());
        self.title_text = title;
        self.body_source = tool_disclosure_body(&self.tool_name, &arguments, &self.result_text);
        self.render_body();
        true
    }

    fn render_body(&mut self) {
        let source = self.body_source.clone();
        let (head, hidden_lines, tail) = self.body_parts(&source);
        let head_text = indent_body_lines(&head, BODY_INDENT);
        let tail_text = tail
            .as_ref()
            .map(|tail| indent_body_lines(tail, BODY_INDENT));
        let hint_text = if hidden_lines > 0 {
            Some(body_hint_text(hidden_lines))
        } else {
            None
        };

        let mut displayed = self.title_text.clone();
        for part in [Some(&head_text), hint_text.as_ref(), tail_text.as_ref()] {
            let Some(part) = part else {
                continue;
            };
            if part.plain().is_empty() {
                continue;
            }
            displayed.push("\n", "");
            displayed.append_text(part);
        }

        self.body_line = head_text;
        self.tail_line = tail_text;
        self.hint_text = hint_text;
        self.display_text = displayed;
    }

    /// 把正文拆成「保留的首部 / 省略的有效行数 / 保留的尾部」。
    fn body_parts(&self, body: &StyledText) -> (StyledText, usize, Option<StyledText>) {
        if self.expanded || !self.limit_body_lines {
            return (body.clone(), 0, None);
        }
        let parts = body.split_lines();
        if parts.len() <= MAX_EXPANDED_BODY_LINES {
            return (body.clone(), 0, None);
        }
        let effective: Vec<&StyledText> = parts
            .iter()
            .filter(|part| !part.plain().trim().is_empty())
            .collect();
        if effective.len() <= MAX_EXPANDED_BODY_LINES {
            return (body.clone(), 0, None);
        }
        let owned: Vec<StyledText> = effective.iter().map(|part| (*part).clone()).collect();
        let head = StyledText::join_lines(&owned[..HEAD_BODY_LINES], "\n");
        let tail = StyledText::join_lines(&owned[owned.len() - TAIL_BODY_LINES..], "\n");
        let hidden_lines = owned.len() - HEAD_BODY_LINES - TAIL_BODY_LINES;
        (head, hidden_lines, Some(tail))
    }

    /// 把终态正文改为分块释放（显示层流式）；返回是否真的进入流式。
    fn start_body_stream(&mut self) -> bool {
        let full = self.body_source.clone();
        let total = full.split_lines().len();
        if full.plain().trim().is_empty() || total <= STREAM_BODY_MIN_LINES {
            return false;
        }
        self.stream_full_source = Some(full);
        self.stream_shown_lines = 0;
        self.stream_step_lines = total.div_ceil(STREAM_BODY_CHUNKS).max(1);
        self.render_body_stream_step();
        true
    }

    /// 按已释放块数切出正文前缀重绘；补齐后收口为完整正文。
    pub fn advance_body_stream(&mut self) {
        self.render_body_stream_step();
    }

    fn render_body_stream_step(&mut self) {
        let Some(full) = self.stream_full_source.clone() else {
            return;
        };
        let lines = full.split_lines();
        self.stream_shown_lines =
            (self.stream_shown_lines + self.stream_step_lines).min(lines.len());
        if self.stream_shown_lines >= lines.len() {
            self.finish_body_stream();
            return;
        }
        self.body_source = StyledText::join_lines(&lines[..self.stream_shown_lines], "\n");
        self.render_body();
    }

    /// 结束流式释放并把正文恢复为完整源（展开或补齐时调用）。
    fn finish_body_stream(&mut self) {
        let full = self.stream_full_source.take();
        self.stream_shown_lines = 0;
        self.stream_step_lines = 0;
        if let Some(full) = full {
            self.body_source = full;
            self.render_body();
        }
    }

    /// 放弃未完成的流式释放。
    fn cancel_body_stream(&mut self) {
        self.stream_full_source = None;
        self.stream_shown_lines = 0;
        self.stream_step_lines = 0;
    }
}

/// 回合运行状态行：左侧动态文本 + 尾部 `[ ESC ]` 中断提示。
#[derive(Debug, Clone, Default)]
pub struct RuntimeStatus {
    label: StyledText,
    last_label_plain: String,
}

impl RuntimeStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn label(&self) -> &StyledText {
        &self.label
    }

    pub fn hint(&self) -> &'static str {
        ESC_HINT
    }

    /// 更新状态文本；与上一帧纯文本一致时跳过（对映 `update_status` 的去重）。
    pub fn update_status(&mut self, label: &StyledText) -> bool {
        let plain = label.plain();
        if plain == self.last_label_plain {
            return false;
        }
        self.last_label_plain = plain;
        self.label = label.clone();
        true
    }
}

/// 确认框焦点（对映 `←`/`→` 在「允许执行」「拒绝」之间的切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    Approve,
    Reject,
}

/// 受限工具的全屏模态确认框。
///
/// 挂载后默认聚焦「允许执行」，直接回车即可运行；`←`/`→` 切换焦点，
/// `Enter`/`Space` 触发当前焦点按钮，`Esc` 取消（交由宿主回合控制器统一处理）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmationScreen {
    prompt: String,
    focus: ConfirmAction,
    dismissed: Option<bool>,
}

impl ConfirmationScreen {
    pub const TITLE: &'static str = "需要确认";
    pub const HINT: &'static str = "←/→ 选择操作 · Enter 确认 · Esc 取消";
    pub const APPROVE_LABEL: &'static str = "允许执行";
    pub const REJECT_LABEL: &'static str = "拒绝";

    pub fn new(prompt: &str) -> Self {
        Self {
            prompt: prompt.to_string(),
            focus: ConfirmAction::Approve,
            dismissed: None,
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn focus(&self) -> ConfirmAction {
        self.focus
    }

    pub fn dismissed(&self) -> Option<bool> {
        self.dismissed
    }

    pub fn focus_approve(&mut self) {
        self.focus = ConfirmAction::Approve;
    }

    pub fn focus_reject(&mut self) {
        self.focus = ConfirmAction::Reject;
    }

    /// 触发当前焦点按钮并交出结果（`true` 表示允许执行）。
    pub fn press(&mut self) -> bool {
        let allowed = self.focus == ConfirmAction::Approve;
        self.dismissed = Some(allowed);
        allowed
    }

    /// Esc 取消：确认页一律以「拒绝」收口。
    pub fn cancel(&mut self) {
        self.dismissed = Some(false);
    }
}

/// 支持鼠标选择的 AI Markdown 回复（对映 `AssistantMessage`）。
///
/// 流式阶段由调用方按换行边界切块后逐块 [`AssistantMessage::append_stream_chunk`]，
/// 停顿/收口时用 [`AssistantMessage::update`] 做全量精确重绘（与 Python 同一套策略）。
///
/// 与 Python 一致：渲染前先把 LaTeX 公式经 `latex::latex_to_text` 转成 Unicode 近似文本。
#[derive(Debug, Clone, Default)]
pub struct AssistantMessage {
    markdown: String,
    last_markdown: Option<String>,
    prefix_written: bool,
    mounted: bool,
    rendered: StyledText,
}

impl AssistantMessage {
    /// 工作台给 AI 回复加的显示前缀（不属于 Markdown 内容）。
    pub const DISPLAY_PREFIX: &'static str = "◇ ";

    pub fn new(markdown: &str) -> Self {
        let mut message = Self::default();
        if markdown.is_empty() {
            return message;
        }
        message.update(markdown);
        message
    }

    pub fn markdown(&self) -> &str {
        &self.markdown
    }

    pub fn rendered(&self) -> &StyledText {
        &self.rendered
    }

    pub fn is_mounted(&self) -> bool {
        self.mounted
    }

    /// 用完整 Markdown 重绘当前消息（替换当前全部行）。
    pub fn update(&mut self, markdown: &str) {
        // 只有挂载前需要保留全文副本供 on_mount 重绘。
        if !self.mounted {
            self.last_markdown = Some(markdown.to_string());
        }
        self.prefix_written = markdown.starts_with(Self::DISPLAY_PREFIX);
        self.markdown = markdown.to_string();
        self.rendered = render_message_markdown(markdown);
    }

    /// 追加一段已就绪的流式内容；首块补 `◇ ` 前缀。
    pub fn append_stream_chunk(&mut self, markdown: &str) {
        if markdown.is_empty() {
            return;
        }
        let chunk = if self.prefix_written {
            markdown.to_string()
        } else {
            self.prefix_written = true;
            format!("{}{markdown}", Self::DISPLAY_PREFIX)
        };
        let rendered_chunk = markdown::render_markdown(&latex::latex_to_text(&chunk));
        self.rendered.append_text(&rendered_chunk);
        self.markdown.push_str(markdown);
    }

    /// 挂载后重走渲染管线，并释放构造期保存的全文副本。
    pub fn on_mount(&mut self) {
        self.mounted = true;
        if let Some(pending) = self.last_markdown.take() {
            if !pending.is_empty() {
                self.update(&pending);
            }
        }
    }

    /// 供鼠标复制使用的纯文本（对映 `get_selection` 的文本来源）。
    pub fn selection_text(&self) -> String {
        self.rendered
            .split_lines()
            .iter()
            .map(|line| line.plain().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 剥离 `◇ ` 显示前缀后渲染：前缀是工作台加的，不属于 Markdown 内容。
///
/// 前缀先剥离再走 LaTeX 转换（数学 fenced 必须从行首开始），与 Python 的
/// `RichMarkdown(display_prefix + latex_to_text(render_markdown))` 同序。
fn render_message_markdown(markdown: &str) -> StyledText {
    let (display_prefix, body) = match markdown.strip_prefix(AssistantMessage::DISPLAY_PREFIX) {
        Some(body) => (AssistantMessage::DISPLAY_PREFIX, body),
        None => ("", markdown),
    };
    markdown::render_markdown(&format!("{display_prefix}{}", latex::latex_to_text(body)))
}

/// 可折叠、不抢占输入焦点的单次模型思考记录（对映 `ReasoningDisclosure`）。
///
/// 默认折叠只展示最新 [`ReasoningDisclosure::COLLAPSED_HEIGHT`] 行；流式阶段按换行边界
/// 增量渲染，停顿 [`ReasoningDisclosure::STREAM_SETTLE_SECONDS`] 后（或 [`ReasoningDisclosure::flush_tail`]）
/// 做一次全量精确重绘。`reasoning_text` 始终累积完整内容。
#[derive(Debug, Clone)]
pub struct ReasoningDisclosure {
    chunks: Vec<String>,
    nl_count: usize,
    ends_newline: bool,
    render_buffer: String,
    last_delta_at: f64,
    expanded: bool,
    render_pending: bool,
    collapsed_height: usize,
    rendered: StyledText,
}

impl Default for ReasoningDisclosure {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            nl_count: 0,
            ends_newline: true,
            render_buffer: String::new(),
            last_delta_at: 0.0,
            expanded: false,
            render_pending: false,
            collapsed_height: 0,
            rendered: StyledText::new(),
        }
    }
}

impl ReasoningDisclosure {
    pub const STREAM_RENDER_INTERVAL_SECONDS: f64 = 0.05;
    /// 折叠态展示的最新思考行数。
    pub const COLLAPSED_HEIGHT: usize = 5;
    /// 流式渲染块的最大长度：超过且不含换行时强制落盘一次。
    pub const STREAM_CHUNK_LIMIT: usize = 512;
    /// 流式停顿多久后做一次全量精确重绘。
    pub const STREAM_SETTLE_SECONDS: f64 = 0.5;

    pub fn new() -> Self {
        Self::default()
    }

    /// 完整累积文本（模型原始思考）。
    pub fn reasoning_text(&self) -> String {
        self.chunks.concat()
    }

    /// 当前思考文本的逻辑行数（增量累计，避免逐片 splitlines）。
    pub fn line_count(&self) -> usize {
        self.nl_count + usize::from(!self.ends_newline)
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    pub fn is_render_pending(&self) -> bool {
        self.render_pending
    }

    pub fn collapsed_height(&self) -> usize {
        self.collapsed_height
    }

    pub fn rendered(&self) -> &StyledText {
        &self.rendered
    }

    /// 尚未落盘的流式缓冲（保留跨行完整性）。
    pub fn pending_buffer(&self) -> &str {
        &self.render_buffer
    }

    pub fn last_delta_at(&self) -> f64 {
        self.last_delta_at
    }

    /// 追加一段思考增量；返回本次是否落盘了流式块。
    pub fn append_delta(&mut self, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }
        self.chunks.push(delta.to_string());
        self.nl_count += delta.matches('\n').count();
        self.ends_newline = delta.ends_with('\n');
        self.render_buffer.push_str(delta);
        let flushed = if self.render_buffer.contains('\n')
            || self.render_buffer.chars().count() >= Self::STREAM_CHUNK_LIMIT
        {
            self.flush_stream_chunk()
        } else {
            false
        };
        self.last_delta_at = now_seconds();
        self.render_pending = true;
        if !self.expanded {
            // 折叠态按当前可见行数轻量更新高度并锚定底部（与全量重绘口径一致）。
            self.collapsed_height = self.collapsed_rows();
        }
        flushed
    }

    /// 把已就绪的流式缓冲按块增量渲染；只落盘到最后一个换行为止的完整行。
    fn flush_stream_chunk(&mut self) -> bool {
        if self.render_buffer.is_empty() {
            return false;
        }
        let buffer = std::mem::take(&mut self.render_buffer);
        let (chunk, tail) = match buffer.rfind('\n') {
            Some(index) => (
                buffer[..=index].to_string(),
                buffer[index + 1..].to_string(),
            ),
            None => (buffer, String::new()),
        };
        self.render_buffer = tail;
        if !chunk.is_empty() {
            let rendered_chunk = markdown::uniform_gray(&latex::latex_to_text(&chunk));
            self.rendered.append_text(&rendered_chunk);
        }
        true
    }

    /// 推理阶段结束：取消挂起刷新并渲染最终 Markdown。
    pub fn flush_tail(&mut self) {
        self.render_pending = false;
        if self.reasoning_text().is_empty() {
            return;
        }
        self.render_markdown();
    }

    /// 对映 `_render_markdown_now`：停顿定时器到点后的全量重绘。
    pub fn render_now(&mut self) {
        self.render_pending = false;
        if self.reasoning_text().is_empty() {
            return;
        }
        self.render_markdown();
    }

    /// 点击折叠区切换展开/收起（双击手势由调用方过滤 `chain != 1`）。
    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
        if !self.reasoning_text().is_empty() {
            self.render_markdown();
        }
    }

    /// 用完整 Markdown 重绘思考内容（统一灰阶）。
    pub fn render_markdown(&mut self) {
        let text = self.reasoning_text();
        if text.is_empty() {
            return;
        }
        self.render_buffer.clear();
        self.rendered = markdown::uniform_gray(&latex::latex_to_text(&text));
        if self.expanded {
            self.collapsed_height = 0;
        } else {
            self.collapsed_height = self.collapsed_rows();
        }
    }

    /// 折叠态应占的行数（不超过 [`Self::COLLAPSED_HEIGHT`]）。
    pub fn collapsed_rows(&self) -> usize {
        let rows = self.rendered.split_lines().len().max(1);
        rows.min(Self::COLLAPSED_HEIGHT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subagent_status_presentation_matches_python_table() {
        assert_eq!(
            SubAgentStatus::parse("waiting_approval")
                .unwrap()
                .presentation(),
            ("◆", "等待审批", "yellow")
        );
        assert_eq!(
            SubAgentStatus::parse("completed").unwrap().presentation(),
            ("✓", "完成", "green")
        );
        assert!(SubAgentStatus::parse("unknown").is_none());
        assert!(SubAgentStatus::Completed.is_terminal());
        assert!(!SubAgentStatus::Queued.is_terminal());
    }

    #[test]
    fn format_elapsed_switches_to_hours_only_when_needed() {
        assert_eq!(format_elapsed(0.0), "00:00");
        assert_eq!(format_elapsed(65.9), "01:05");
        assert_eq!(format_elapsed(3661.0), "01:01:01");
        assert_eq!(format_elapsed(-5.0), "00:00");
    }

    #[test]
    fn wrap_subagent_line_counts_cjk_columns_and_keeps_blank_lines() {
        // 内容宽度 = max(4, width - 2)，即 width=6 时每行 4 列。
        assert_eq!(wrap_subagent_line("abcdef", 6), vec!["abcd", "ef"]);
        assert_eq!(wrap_subagent_line("a b c d", 6), vec!["a b ", "c d"]);
        assert_eq!(wrap_subagent_line("中文中文", 6), vec!["中文", "中文"]);
        assert_eq!(wrap_subagent_line("", 20), vec![""]);
        // 宽度过小时下限压到 4 列。
        assert_eq!(wrap_subagent_line("a b", 2), vec!["a b"]);
    }

    #[test]
    fn progress_tree_tracks_status_and_ignores_late_events() {
        let mut tree = SubAgentProgressTree::new("batch-1");
        assert!(!tree.is_active());
        assert!(tree.update_task("t1", "review", "审查代码", "running", Some(10.0)));
        assert!(tree.is_active());
        assert!(!tree.update_task("t1", "review", "审查代码", "bogus", Some(11.0)));
        // 终态节点的迟到活动事件被忽略，不会回退状态。
        assert!(tree.update_task("t1", "review", "审查代码", "completed", Some(20.0)));
        assert!(!tree.is_active());
        assert!(!tree.update_task("t1", "review", "审查代码", "running", Some(21.0)));
        let task = tree.tasks().next().unwrap();
        assert_eq!(task.status, SubAgentStatus::Completed);
        assert_eq!(task.started_at, Some(10.0));
        assert_eq!(task.finished_at, Some(20.0));
    }

    #[test]
    fn progress_tree_renders_root_label_connectors_and_elapsed() {
        let mut tree = SubAgentProgressTree::new("batch-2");
        tree.update_task("t1", "explore", "找入口", "running", Some(0.0));
        tree.update_task("t2", "review", "查缺陷", "completed", Some(5.0));
        let text = tree.render_text(Some(90.0));
        let plain = text.plain();
        assert!(plain.starts_with("◇ 并行子任务  1/2 完成"));
        assert!(plain.contains("├─ ● 找入口  explore · 运行中 · 01:30"));
        assert!(plain.contains("└─ ✓ 查缺陷  review · 完成 · 00:00"));
        // 单任务时根标签换成单体措辞，且末行连接符是 └─。
        let mut single = SubAgentProgressTree::new("batch-3");
        single.update_task("only", "explore", "独苗", "queued", Some(1.0));
        let plain = single.render_text(Some(1.0)).plain();
        assert!(plain.starts_with("◇ 子任务进度  0/1 完成"));
        assert!(plain.contains("└─ ○ 独苗"));
    }

    #[test]
    fn progress_tree_sanitizes_untrusted_fields() {
        let mut tree = SubAgentProgressTree::new("batch-4");
        tree.update_task("  ", "", "  换行\n描述   多空格  ", "queued", Some(0.0));
        let task = tree.tasks().next().unwrap();
        assert_eq!(task.task_id, "task");
        assert_eq!(task.agent_type, "subagent");
        assert_eq!(task.description, "换行 描述 多空格");
    }

    #[test]
    fn todo_plan_normalizes_and_renders_marks() {
        let mut plan = TodoPlan::new();
        assert!(!plan.is_visible());
        plan.update_items(&json!([
            {"step": "  先做  A  ", "completed": false},
            {"description": "再做 B", "status": "done"},
            {"title": "第三步", "completed": true},
            {"step": "   "},
            "不是对象",
            {"step": "超限", "completed": false}
        ]));
        assert_eq!(plan.row_count(), 4);
        assert_eq!(plan.items()[0].step, "先做 A");
        assert!(!plan.items()[0].completed);
        assert!(plan.items()[1].completed);
        assert!(plan.is_visible());
        let plain = plan.render_text().plain();
        assert_eq!(plain, "▢ 先做 A\n▣ 再做 B\n▣ 第三步\n▢ 超限");
    }

    #[test]
    fn todo_plan_caps_twenty_steps_and_trims_long_ones() {
        let long = "字".repeat(300);
        let items: Vec<serde_json::Value> = (0..25)
            .map(|index| json!({"step": format!("{index}-{long}")}))
            .collect();
        let mut plan = TodoPlan::new();
        plan.update_items(&serde_json::Value::Array(items));
        assert_eq!(plan.row_count(), 20);
        assert_eq!(plan.items()[0].step.chars().count(), 240);
        assert!(plan.items()[0].step.starts_with("0-"));
    }

    #[test]
    fn subagent_conversation_prefixes_every_line_and_seals_with_corner() {
        let mut conversation = SubAgentConversation::new("batch-5", "review");
        assert!(conversation.is_active());
        conversation.append("第一行", "dim");
        conversation.append("第二行很长需要折行", "");
        conversation.finish("子代理完成");
        assert!(!conversation.is_active());
        conversation.append("收口后的行被忽略", "");
        assert_eq!(
            conversation.logical_text(),
            "◇ review 子代理对话\n第一行\n第二行很长需要折行\n子代理完成"
        );
        let plain = conversation.render_text(40).plain();
        assert!(plain.starts_with("│ ◇ review 子代理对话\n"));
        assert!(plain.ends_with("╰"));
        assert_eq!(conversation.rows().last().unwrap().1, "green");
    }

    #[test]
    fn subagent_conversation_failure_status_is_red() {
        let mut conversation = SubAgentConversation::new("batch-6", "explore");
        conversation.finish("派生评审失败");
        assert_eq!(conversation.rows().last().unwrap().1, "red");
    }

    #[test]
    fn indent_body_lines_skips_blank_lines() {
        let body = StyledText::styled("first\n\nsecond", "");
        assert_eq!(
            indent_body_lines(&body, BODY_INDENT).plain(),
            "    first\n\n    second"
        );
        assert_eq!(
            indent_body_lines(&StyledText::new(), BODY_INDENT).plain(),
            ""
        );
    }

    #[test]
    fn body_hint_text_keeps_indent_out_of_underline() {
        let hint = body_hint_text(6);
        assert_eq!(hint.plain(), "    点击展开 6 行");
        assert_eq!(hint.spans()[0].style, "not underline");
        assert_eq!(hint.spans()[1].style, "");
    }

    #[test]
    fn tool_disclosure_starts_in_running_state() {
        let bash = ToolDisclosure::new("bash", json!({"command": "ls"}), 0.0);
        assert_eq!(bash.status(), "调用中");
        assert_eq!(bash.status_class(), Some("tool-running"));
        assert!(bash.content().plain().contains("调用中"));

        let ask = ToolDisclosure::new(
            crate::ui::fullscreen::rendering::tool_diff::ASK_USER_TOOL_NAME,
            json!({}),
            0.0,
        );
        assert_eq!(ask.status(), "等待回复");
        assert_eq!(ask.status_class(), Some("tool-running"));
    }

    #[test]
    fn tool_disclosure_finish_records_status_and_frees_payload() {
        let mut card = ToolDisclosure::new("bash", json!({"command": "ls"}), 0.0);
        assert!(!card.finish(true, "ok", 1.5, false));
        assert_eq!(card.status(), "成功");
        assert_eq!(card.status_class(), Some("tool-ok"));
        assert_eq!(card.duration_seconds, 1.5);
        assert!(card.arguments().is_none());
        assert_eq!(card.result_text(), "");
        assert!(card
            .content()
            .plain()
            .starts_with("● bash ls · ✓ 成功 · 1.5s"));
        assert!(card.content().plain().contains("ok"));

        let mut failed = ToolDisclosure::new("grep", json!({}), 2.0);
        failed.finish(false, "err", 1.0, false);
        assert_eq!(failed.status(), "失败");
        assert_eq!(failed.status_class(), Some("tool-fail"));
        // 负耗时被夹到 0。
        assert_eq!(failed.duration_seconds, 0.0);

        let mut ask = ToolDisclosure::new(
            crate::ui::fullscreen::rendering::tool_diff::ASK_USER_TOOL_NAME,
            json!({}),
            0.0,
        );
        ask.finish(true, "", 0.5, false);
        assert_eq!(ask.status(), "已收到回复");
        assert_eq!(ask.status_class(), Some("tool-ok"));
        let mut cancelled = ToolDisclosure::new(
            crate::ui::fullscreen::rendering::tool_diff::ASK_USER_TOOL_NAME,
            json!({}),
            0.0,
        );
        cancelled.finish(false, "", 0.5, false);
        assert_eq!(cancelled.status(), "已取消");
        assert_eq!(cancelled.status_class(), Some("tool-cancelled"));
    }

    #[test]
    fn tool_disclosure_refresh_elapsed_only_while_running() {
        let mut card = ToolDisclosure::new("bash", json!({}), 10.0);
        assert!(card.refresh_elapsed(Some(11.25)));
        assert_eq!(card.duration_seconds, 1.25);
        card.finish(true, "ok", 12.0, false);
        assert!(!card.refresh_elapsed(Some(99.0)));
        assert_eq!(card.duration_seconds, 2.0);
    }

    #[test]
    fn tool_disclosure_folds_long_body_to_head_and_tail() {
        let output: Vec<String> = (1..=10).map(|index| format!("l{index}")).collect();
        let mut card = ToolDisclosure::new("bash", json!({}), 0.0);
        card.finish(true, &output.join("\n"), 0.0, false);
        assert_eq!(card.body_line().plain(), "    l1\n    l2");
        assert_eq!(
            card.tail_line().map(StyledText::plain),
            Some("    l9\n    l10".to_string())
        );
        assert_eq!(
            card.hint_text().map(StyledText::plain),
            Some("    点击展开 6 行".to_string())
        );
        let plain = card.content().plain();
        assert!(plain.contains("    l1\n    l2\n    点击展开 6 行\n    l9\n    l10"));
    }

    #[test]
    fn tool_disclosure_keeps_short_and_unlimited_bodies_whole() {
        // 短正文（≤5 行）不缩略。
        let mut short = ToolDisclosure::new("bash", json!({}), 0.0);
        short.finish(true, "a\nb\nc", 0.0, false);
        assert_eq!(short.hint_text(), None);
        assert_eq!(short.body_line().plain(), "    a\n    b\n    c");
        assert_eq!(short.tail_line(), None);

        // write_file 豁免行数上限。
        let long: Vec<String> = (1..=20).map(|index| format!("line{index}")).collect();
        let mut unlimited = ToolDisclosure::new("write_file", json!({"path": "a.txt"}), 0.0);
        unlimited.finish(true, &long.join("\n"), 0.0, false);
        assert_eq!(unlimited.hint_text(), None);
        assert!(unlimited.body_line().plain().contains("line20"));
    }

    #[test]
    fn tool_disclosure_expand_and_collapse_switch_sampling() {
        let output: Vec<String> = (1..=10).map(|index| format!("l{index}")).collect();
        let mut card = ToolDisclosure::new("bash", json!({}), 0.0);
        card.finish(true, &output.join("\n"), 0.0, false);
        assert!(!card.is_expanded());
        card.expand_body();
        assert!(card.is_expanded());
        assert_eq!(card.hint_text(), None);
        assert!(card.body_line().plain().contains("l10"));
        card.collapse();
        assert!(!card.is_expanded());
        assert!(card.hint_text().is_some());
    }

    #[test]
    fn tool_disclosure_streams_terminal_body_in_chunks() {
        let output: Vec<String> = (1..=12).map(|index| format!("l{index}")).collect();
        let mut card = ToolDisclosure::new("bash", json!({}), 0.0);
        assert!(card.finish(true, &output.join("\n"), 0.0, true));
        assert!(card.is_body_streaming());
        assert_eq!(card.stream_step_lines(), 1);
        assert_eq!(card.stream_shown_lines(), 1);
        assert!(!card.content().plain().contains("l12"));

        for _ in 0..11 {
            card.advance_body_stream();
        }
        assert!(!card.is_body_streaming());
        assert!(card.content().plain().contains("l12"));
    }

    #[test]
    fn tool_disclosure_skips_streaming_for_short_body() {
        let mut card = ToolDisclosure::new("bash", json!({}), 0.0);
        assert!(!card.finish(true, "a\nb", 0.0, true));
        assert!(!card.is_body_streaming());
    }

    #[test]
    fn tool_disclosure_update_body_respects_file_change_tools() {
        let mut card = ToolDisclosure::new("bash", json!({}), 0.0);
        card.finish(true, "原始输出", 0.0, false);
        card.update_body("压缩后输出");
        assert!(card.content().plain().contains("压缩后输出"));

        let mut file_card = ToolDisclosure::new("Edit_file", json!({"path": "a.txt"}), 0.0);
        file_card.finish(true, "已替换 1 处", 0.0, false);
        let before = file_card.content().plain();
        file_card.update_body("压缩后输出");
        assert_eq!(file_card.content().plain(), before);
    }

    #[test]
    fn runtime_status_skips_identical_frames() {
        let mut status = RuntimeStatus::new();
        assert_eq!(status.hint(), ESC_HINT);
        assert!(status.update_status(&StyledText::styled("⠋ 思考中", "bold")));
        assert!(!status.update_status(&StyledText::styled("⠋ 思考中", "dim")));
        assert!(status.update_status(&StyledText::styled("⠙ 思考中", "bold")));
        assert_eq!(status.label().plain(), "⠙ 思考中");
    }

    #[test]
    fn confirmation_screen_defaults_to_approve_and_reports_choice() {
        let mut screen = ConfirmationScreen::new("执行 bash：rm -rf /tmp/x ?");
        assert_eq!(screen.prompt(), "执行 bash：rm -rf /tmp/x ?");
        assert_eq!(screen.focus(), ConfirmAction::Approve);
        assert_eq!(screen.dismissed(), None);
        assert!(screen.press());
        assert_eq!(screen.dismissed(), Some(true));
        // 文案与 Python 的确认框逐字一致。
        assert_eq!(ConfirmationScreen::TITLE, "需要确认");
        assert_eq!(ConfirmationScreen::APPROVE_LABEL, "允许执行");
        assert_eq!(ConfirmationScreen::REJECT_LABEL, "拒绝");
    }

    #[test]
    fn confirmation_screen_arrow_keys_move_focus_and_escape_rejects() {
        let mut screen = ConfirmationScreen::new("确认？");
        screen.focus_reject();
        assert_eq!(screen.focus(), ConfirmAction::Reject);
        assert!(!screen.press());
        assert_eq!(screen.dismissed(), Some(false));

        let mut cancelled = ConfirmationScreen::new("确认？");
        cancelled.focus_reject();
        cancelled.focus_approve();
        cancelled.cancel();
        assert_eq!(cancelled.dismissed(), Some(false));
        assert_eq!(cancelled.focus(), ConfirmAction::Approve);
    }

    #[test]
    fn assistant_message_updates_and_keeps_display_prefix() {
        let message = AssistantMessage::new("◇ **加粗** 正文");
        assert!(!message.is_mounted());
        assert_eq!(message.markdown(), "◇ **加粗** 正文");
        let plain = message.rendered().plain();
        assert!(plain.starts_with("◇ "));
        assert!(
            markdown::render_markdown("普通 **加粗** 文本")
                .spans()
                .len()
                > 2
        );
    }

    #[test]
    fn assistant_message_stream_chunk_writes_prefix_once() {
        let mut message = AssistantMessage::default();
        message.append_stream_chunk("第一段");
        message.append_stream_chunk(" 第二段");
        assert_eq!(message.markdown(), "第一段 第二段");
        let plain = message.rendered().plain();
        assert!(plain.starts_with("◇ 第一段"));
        assert!(plain.matches(AssistantMessage::DISPLAY_PREFIX).count() == 1);
        // 空片段不改变任何状态。
        message.append_stream_chunk("");
        assert_eq!(message.markdown(), "第一段 第二段");
    }

    #[test]
    fn assistant_message_mount_rerenders_saved_copy() {
        let mut message = AssistantMessage::new("初始内容");
        message.on_mount();
        assert!(message.is_mounted());
        assert!(message.rendered().plain().contains("初始内容"));
        // 挂载后的 update 不再保存全文副本，重绘仍生效。
        message.update("新内容");
        assert!(message.rendered().plain().contains("新内容"));
    }

    #[test]
    fn assistant_message_selection_text_trims_line_ends() {
        let message = AssistantMessage::new("一行\n\n两行");
        let text = message.selection_text();
        assert!(text.contains("一行"));
        assert!(text.contains("两行"));
        assert!(!text.contains(" \n"));
    }

    #[test]
    fn reasoning_disclosure_accumulates_and_flushes_on_newline() {
        let mut reasoning = ReasoningDisclosure::new();
        assert!(!reasoning.append_delta("思考中"));
        assert_eq!(reasoning.pending_buffer(), "思考中");
        assert_eq!(reasoning.reasoning_text(), "思考中");
        assert_eq!(reasoning.line_count(), 1);
        assert!(reasoning.append_delta("第一行\n第二行"));
        assert_eq!(reasoning.pending_buffer(), "第二行");
        assert!(reasoning.rendered().plain().contains("思考中第一行"));
        assert!(reasoning.is_render_pending());
        assert!(reasoning.last_delta_at() > 0.0);
        // 空增量被忽略。
        assert!(!reasoning.append_delta(""));
    }

    #[test]
    fn reasoning_disclosure_line_count_tracks_newlines() {
        let mut reasoning = ReasoningDisclosure::new();
        reasoning.append_delta("a\nb\n");
        assert_eq!(reasoning.line_count(), 2);
        reasoning.append_delta("c");
        assert_eq!(reasoning.line_count(), 3);
    }

    #[test]
    fn reasoning_disclosure_flush_tail_renders_everything_gray() {
        let mut reasoning = ReasoningDisclosure::new();
        reasoning.append_delta("第一行\n第二行");
        reasoning.flush_tail();
        assert!(!reasoning.is_render_pending());
        assert_eq!(reasoning.pending_buffer(), "");
        let plain = reasoning.rendered().plain();
        assert!(plain.contains("第一行"));
        assert!(plain.contains("第二行"));
        assert!(reasoning
            .rendered()
            .spans()
            .iter()
            .all(|span| span.style.contains("bright_black")));
    }

    #[test]
    fn reasoning_disclosure_collapses_to_five_rows_and_toggles() {
        let mut reasoning = ReasoningDisclosure::new();
        let text: Vec<String> = (1..=9).map(|index| format!("- 行{index}")).collect();
        reasoning.append_delta(&text.join("\n"));
        assert_eq!(
            reasoning.collapsed_height(),
            ReasoningDisclosure::COLLAPSED_HEIGHT
        );
        reasoning.toggle_expanded();
        assert!(reasoning.is_expanded());
        assert_eq!(reasoning.collapsed_height(), 0);
        reasoning.toggle_expanded();
        assert!(!reasoning.is_expanded());
        assert_eq!(
            reasoning.collapsed_height(),
            ReasoningDisclosure::COLLAPSED_HEIGHT
        );
    }

    #[test]
    fn reasoning_disclosure_forces_flush_past_chunk_limit() {
        let mut reasoning = ReasoningDisclosure::new();
        let long = "x".repeat(ReasoningDisclosure::STREAM_CHUNK_LIMIT);
        assert!(reasoning.append_delta(&long));
        assert_eq!(reasoning.pending_buffer(), "");
        assert_eq!(
            reasoning.reasoning_text().chars().count(),
            ReasoningDisclosure::STREAM_CHUNK_LIMIT
        );
    }

    #[test]
    fn reasoning_disclosure_render_now_is_quiet_when_empty() {
        let mut reasoning = ReasoningDisclosure::new();
        reasoning.render_now();
        assert_eq!(reasoning.rendered().plain(), "");
        reasoning.flush_tail();
        assert_eq!(reasoning.rendered().plain(), "");
    }
}
