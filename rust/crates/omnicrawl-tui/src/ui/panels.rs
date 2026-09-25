//! 任务清单条与待决面板（提问/审批）。
//!
//! 运行状态行不在本模块：它作为会话流里的最后一条临时消息渲染（对映
//! `.message.runtime-status-message`），见 [`crate::ui::conversation`]。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use super::fullscreen::terminal::theme;
use super::{fit, wrap_display};
use crate::host::Waiting;
use crate::state::AppState;

/// 状态行轮换的 Braille 帧，与终端 UI 既有实现一致。
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 任务清单最多显示的行数（对映 CSS `#todo-plan { max-height: 20 }`）。
const TODO_LIMIT: usize = 20;
/// 任务清单的左内边距（对映 CSS `#todo-plan { padding: 0 1 }`）。
const TODO_PAD: &str = " ";
/// 提问正文与选项的显示上限。
const QUESTION_BODY_LIMIT: usize = 4;
const QUESTION_OPTION_LIMIT: usize = 6;

pub fn todo_height(state: &AppState) -> u16 {
    state.todos.len().min(TODO_LIMIT) as u16
}

pub fn panel_height(state: &AppState, width: u16) -> u16 {
    match state.waiting() {
        None => 0,
        Some(Waiting::Approval(panel)) => {
            let body = wrap_display(&panel.summary, width.saturating_sub(4).max(1) as usize)
                .len()
                .min(2);
            (body + 4) as u16
        }
        Some(Waiting::Question(panel)) => {
            let body = wrap_display(&panel.prompt, width.saturating_sub(4).max(1) as usize)
                .len()
                .min(QUESTION_BODY_LIMIT);
            let options = if panel.is_select() {
                panel.options.len().min(QUESTION_OPTION_LIMIT)
            } else {
                1
            };
            (body + options + 2) as u16
        }
    }
}

/// 任务清单：每步一行 `▣/▢`（对映 Python `TodoPlan.render_text`），无表头、无边框，
/// 完成步是绿色实心框，未完是弱化空心框，步骤文本保持终端默认前景。
pub fn render_todos(frame: &mut Frame, area: Rect, state: &AppState, width: u16) {
    if area.height == 0 {
        return;
    }
    let room = (width as usize).saturating_sub(2 + super::display_width(TODO_PAD));
    let lines: Vec<Line<'static>> = state
        .todos
        .iter()
        .take(TODO_LIMIT)
        .map(|todo| {
            let (marker, style) = if todo.completed {
                ("▣", theme::rich_style(theme::ACCENT_GREEN))
            } else {
                ("▢", theme::rich_style(theme::TEXT_MUTED))
            };
            Line::from(vec![
                Span::raw(TODO_PAD),
                Span::styled(marker, style),
                Span::raw(" "),
                Span::styled(
                    fit(&todo.step, room),
                    theme::rich_style(theme::TEXT_PRIMARY),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

pub fn render_panel(frame: &mut Frame, area: Rect, state: &AppState, width: u16) {
    if area.height == 0 {
        return;
    }
    let inner = width.saturating_sub(4).max(1) as usize;
    match state.waiting() {
        None => {}
        Some(Waiting::Approval(panel)) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::raw("工具 "),
                    Span::styled(panel.tool.clone(), Style::new().fg(Color::Yellow)),
                ]),
                Line::raw(fit(&panel.summary, inner)),
                Line::styled(
                    "[Y] 批准    [N] 拒绝",
                    Style::new()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ];
            lines.truncate(area.height.saturating_sub(2) as usize);
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .title("工具审批")
                        .border_style(Style::new().fg(Color::Yellow)),
                ),
                area,
            );
        }
        Some(Waiting::Question(panel)) => {
            let mut lines: Vec<Line<'static>> = wrap_display(&panel.prompt, inner)
                .into_iter()
                .take(QUESTION_BODY_LIMIT)
                .map(Line::raw)
                .collect();
            if panel.is_select() {
                let options = panel.options.iter().take(QUESTION_OPTION_LIMIT);
                for (index, option) in options.enumerate() {
                    let selected = index == panel.selected;
                    lines.push(Line::from(vec![
                        Span::styled(
                            if selected { "▸ " } else { "  " },
                            Style::new().fg(Color::Magenta),
                        ),
                        Span::styled(
                            fit(option, inner.saturating_sub(2)),
                            if selected {
                                Style::new().fg(Color::Magenta)
                            } else {
                                Style::new()
                            },
                        ),
                    ]));
                }
            } else {
                lines.push(Line::styled(
                    "在输入框写下答案后回车",
                    Style::new()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .title("当前问题")
                        .border_style(Style::new().fg(Color::Magenta)),
                ),
                area,
            );
        }
    }
}

pub fn spinner_frame(index: usize) -> &'static str {
    SPINNER[index % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;
    use crate::host::Waiting;
    use omnicrawl_ipc::Id;

    fn new_state() -> AppState {
        AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual)
    }

    #[test]
    fn approval_panel_appears_only_while_waiting() {
        let mut state = new_state();
        assert_eq!(panel_height(&state, 60), 0);
        state.start_batch(
            Id::Number(1),
            vec![omnicrawl_core::ToolCall {
                name: "bash".to_string(),
                arguments: serde_json::json!({"command": "pytest -q"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                id: "c1".to_string(),
                function_name: "bash".to_string(),
            }],
        );
        assert!(matches!(state.waiting(), Some(Waiting::Approval(_))));
        assert!(panel_height(&state, 60) >= 4, "审批面板要装下工具名与说明");
    }

    #[test]
    fn question_panel_height_accounts_for_options() {
        let mut state = new_state();
        state.start_batch(
            Id::Number(2),
            vec![question_call(serde_json::json!({
                "question": "选哪个？",
                "kind": "select",
                "options": ["A", "B"]
            }))],
        );
        assert!(matches!(state.waiting(), Some(Waiting::Question(_))));
        // 正文一行 + 两个选项 + 上下边框。
        assert_eq!(panel_height(&state, 60), 5);

        let mut free = new_state();
        free.start_batch(
            Id::Number(3),
            vec![question_call(
                serde_json::json!({"question": "补充点什么？"}),
            )],
        );
        // 没有选项时只有一行输入提示。
        assert_eq!(panel_height(&free, 60), 4);
    }

    fn question_call(arguments: serde_json::Value) -> omnicrawl_core::ToolCall {
        omnicrawl_core::ToolCall {
            name: crate::host::ASK_USER_TOOL.to_string(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            id: "q1".to_string(),
            function_name: crate::host::ASK_USER_TOOL.to_string(),
        }
    }

    #[test]
    fn spinner_cycles_through_frames() {
        assert_eq!(spinner_frame(0), "⠋");
        assert_eq!(spinner_frame(1), "⠙");
        assert_eq!(spinner_frame(10), "⠋");
    }

    #[test]
    fn todo_height_hides_empty_list() {
        let mut state = new_state();
        assert_eq!(todo_height(&state), 0, "空计划不占行");
        state.todos.push(crate::host::TodoItem {
            id: "1".to_string(),
            step: "写骨架".to_string(),
            completed: false,
        });
        // 每步一行、无表头（对映 Python `TodoPlan.row_count`）。
        assert_eq!(todo_height(&state), 1);
        for index in 0..30 {
            state.todos.push(crate::host::TodoItem {
                id: index.to_string(),
                step: "更多".to_string(),
                completed: true,
            });
        }
        assert_eq!(todo_height(&state), TODO_LIMIT as u16);
    }
}
