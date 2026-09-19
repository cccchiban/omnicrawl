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

use unicode_width::UnicodeWidthChar;

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
#[derive(Debug, Clone)]
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
            let connector = if index == total - 1 { "└─" } else { "├─" };
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
#[derive(Debug, Clone)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subagent_status_presentation_matches_python_table() {
        assert_eq!(
            SubAgentStatus::parse("waiting_approval").unwrap().presentation(),
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
        tree.update_task(
            "  ",
            "",
            "  换行\n描述   多空格  ",
            "queued",
            Some(0.0),
        );
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
}
