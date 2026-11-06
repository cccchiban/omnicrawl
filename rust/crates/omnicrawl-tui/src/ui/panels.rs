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
    // 网络审批与工具审批是两件独立的事，可能同时存在（沙盒里的命令正在跑，
    // 回合又推进到一个需确认的调用）：取较高者，渲染时两块都画。
    tool_panel_height(state, width).max(net_panel_height(state))
}

/// 网络审批面板高度：标题 + 目标 + 按键提示。
fn net_panel_height(state: &AppState) -> u16 {
    if state.net_approval.is_some() {
        5
    } else {
        0
    }
}

fn tool_panel_height(state: &AppState, width: u16) -> u16 {
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
                // 与渲染一致：模型选项最多占 `LIMIT - 1` 行，末行固定给自定义作答入口。
                panel.options.len().min(QUESTION_OPTION_LIMIT - 1) + 1
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
    // 网络审批画在上面：它挡着一条正在连接的沙盒命令，比工具审批更急。
    if let Some(panel) = &state.net_approval {
        render_net_approval(frame, area, panel, width);
        return;
    }
    render_tool_panel(frame, area, state, width);
}

fn render_net_approval(
    frame: &mut Frame,
    area: Rect,
    panel: &crate::state::NetApprovalPanel,
    width: u16,
) {
    let inner = width.saturating_sub(4).max(1) as usize;
    let lines = vec![
        Line::from(vec![
            Span::raw("沙盒内进程请求连接 "),
            Span::styled(fit(&panel.target, inner), Style::new().fg(Color::Yellow)),
        ]),
        Line::raw(format!("{} 秒后按超时拒绝", panel.remaining_seconds)),
        Line::styled(
            "[Y] 放行    [N] 拒绝",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title("网络审批")
                .border_style(Style::new().fg(Color::Yellow)),
        ),
        area,
    );
}

fn render_tool_panel(frame: &mut Frame, area: Rect, state: &AppState, width: u16) {
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
                // 末尾固定留一行给自定义作答入口（对映 Python 的「but I Think...」），
                // 因此模型选项再多也只占前几行。
                let options = panel
                    .options
                    .iter()
                    .take(QUESTION_OPTION_LIMIT.saturating_sub(1));
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
                let selected = panel.is_custom();
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "▸ " } else { "  " },
                        Style::new().fg(Color::Cyan),
                    ),
                    Span::styled(
                        fit(crate::host::QUESTION_CUSTOM_LABEL, inner.saturating_sub(2)),
                        if selected {
                            Style::new().fg(Color::Cyan)
                        } else {
                            Style::new().fg(Color::DarkGray)
                        },
                    ),
                ]));
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
    fn net_approval_panel_has_its_own_height() {
        let mut state = new_state();
        assert_eq!(panel_height(&state, 60), 0);
        state.net_approval = Some(crate::state::NetApprovalPanel {
            id: 7,
            target: "https://example.com:443".to_string(),
            remaining_seconds: 42,
        });
        // 独立于工具批次：没有 `Waiting` 也要占位置，否则面板画不出来。
        assert!(state.waiting().is_none());
        assert_eq!(panel_height(&state, 60), 5);
    }

    #[test]
    fn net_approval_and_tool_panel_share_the_band() {
        let mut state = new_state();
        state.start_batch(
            Id::Number(3),
            vec![omnicrawl_core::ToolCall {
                name: "bash".to_string(),
                arguments: serde_json::json!({"command": "curl https://example.com"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                id: "c1".to_string(),
                function_name: "bash".to_string(),
            }],
        );
        let tool_only = panel_height(&state, 60);
        state.net_approval = Some(crate::state::NetApprovalPanel {
            id: 8,
            target: "https://example.com:443".to_string(),
            remaining_seconds: 30,
        });
        // 两者可能同时存在（沙盒里的命令在跑，回合又推进到一个需确认的调用），
        // 取较高者，不能因为其中一块更矮就把另一块挤掉。
        assert_eq!(panel_height(&state, 60), tool_only.max(5));
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
        // 正文一行 + 两个选项 + 自定义入口 + 上下边框。
        assert_eq!(panel_height(&state, 60), 6);

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
