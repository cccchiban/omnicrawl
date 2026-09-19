//! 消息流：把记录渲染成显示行，并按滚动位置取出可见窗口。
//!
//! 记录先按真实列宽软折行，再进入窗口计算，因此终端自己的换行不会打乱滚动位置。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{display_width, fit, pad, wrap_display};
use crate::state::{AppState, Record, ToolCard, ToolStatus};

/// 工具卡正文行数上限（阶段一不能展开，超出后中间折叠）。
const TOOL_BODY_LIMIT: usize = 5;
/// 思考段默认折叠时展示的最新行数。
const REASONING_TAIL: usize = 5;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let lines = display_lines(state, area.width);
    let window = window(&lines, area.height as usize, state.scroll_from_bottom);
    frame.render_widget(Paragraph::new(window), area);
}

/// 全部记录的显示行（已按宽度折行）。
pub fn display_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for record in &state.records {
        lines.push(Line::raw(""));
        match record {
            Record::User(text) => push_prefixed(&mut lines, "$ ", text, width, Color::Magenta),
            Record::Assistant(text) => push_prefixed(&mut lines, "◇ ", text, width, Color::Reset),
            Record::Reasoning(text) => push_reasoning(&mut lines, text, width),
            Record::Notice(text) => push_prefixed(&mut lines, "· ", text, width, Color::DarkGray),
            Record::Tool(card) => push_tool(&mut lines, card, width),
        }
    }
    lines.push(Line::raw(""));
    lines
}

/// 从底部起取可见窗口：`scroll_from_bottom` 为向上滚过的行数，滚到顶就停在最早的行。
pub fn window(
    lines: &[Line<'static>],
    height: usize,
    scroll_from_bottom: usize,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let offset = scroll_from_bottom.min(lines.len().saturating_sub(height));
    let end = lines.len() - offset;
    let start = end.saturating_sub(height);
    lines[start..end].to_vec()
}

/// 正文头尾采样：超出上限时保留首尾各两行，中间以一行提示代替。
pub fn preview(body: &[String], limit: usize) -> Vec<String> {
    if body.len() <= limit || limit < 4 {
        return body.to_vec();
    }
    let hidden = body.len() - (limit - 1);
    let mut shown: Vec<String> = body[..2].to_vec();
    shown.push(format!("… 省略 {hidden} 行"));
    shown.extend_from_slice(&body[body.len() - 2..]);
    shown
}

fn push_prefixed(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: usize,
    color: Color,
) {
    let body_width = width.saturating_sub(display_width(prefix)).max(1);
    let style = Style::new().fg(color);
    for (index, chunk) in wrap_display(&text.replace('\r', ""), body_width)
        .iter()
        .enumerate()
    {
        let head = if index == 0 {
            prefix.to_string()
        } else {
            " ".repeat(display_width(prefix))
        };
        lines.push(Line::from(vec![
            Span::styled(head, style),
            Span::styled(chunk.clone(), style),
        ]));
    }
}

fn push_reasoning(lines: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let body_width = width.saturating_sub(2).max(1);
    let wrapped = wrap_display(text, body_width);
    let folded = wrapped.len().saturating_sub(REASONING_TAIL);
    for chunk in &wrapped[folded..] {
        lines.push(Line::styled(
            format!("  {chunk}"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    let hint = if folded > 0 {
        format!(
            "  ⋯ 思考已折叠，仅显示最新 {REASONING_TAIL} 行（共 {} 行）",
            wrapped.len()
        )
    } else {
        "  ⋯ 思考".to_string()
    };
    lines.push(Line::styled(
        hint,
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ));
}

fn push_tool(lines: &mut Vec<Line<'static>>, card: &ToolCard, width: usize) {
    let color = status_color(card.status);
    if width < 6 {
        lines.push(Line::styled(
            fitted_tool_title(card, width),
            Style::new().fg(color),
        ));
        return;
    }
    let inner = width - 2;
    let title = fitted_tool_title(card, inner.saturating_sub(3));
    let fill = inner.saturating_sub(3 + display_width(&title));
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::new().fg(color)),
        Span::styled(title, Style::new().fg(color)),
        Span::styled(format!(" {}┐", "─".repeat(fill)), Style::new().fg(color)),
    ]));
    for line in preview(&card.body, TOOL_BODY_LIMIT) {
        let style = if line.starts_with("… ") {
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::new().fg(color)),
            Span::styled(pad(&fit(&line, inner), inner), style),
            Span::styled("│", Style::new().fg(color)),
        ]));
    }
    lines.push(Line::styled(
        "└".to_string() + &"─".repeat(width.saturating_sub(2)) + "┘",
        Style::new().fg(color),
    ));
}

/// 标题按「名称 | 参数摘要  状态 耗时」拼装，宽度不够时先牺牲摘要，保证状态与耗时可读。
fn fitted_tool_title(card: &ToolCard, available: usize) -> String {
    let (icon, status) = match card.status {
        ToolStatus::Running => ("●", "运行中"),
        ToolStatus::Ok => ("✓", "成功"),
        ToolStatus::Failed => ("✗", "失败"),
        ToolStatus::Denied => ("✗", "已拒绝"),
    };
    let elapsed = match card.elapsed {
        Some(duration) => format!("  {:.2}s", duration.as_secs_f64()),
        None => String::new(),
    };
    let head = format!("{icon} {} | ", card.name);
    let suffix = format!("  {status}{elapsed}");
    let room = available
        .saturating_sub(display_width(&head) + display_width(&suffix))
        .max(1);
    fit(
        &format!("{head}{}{suffix}", fit(&card.summary, room)),
        available,
    )
}

fn status_color(status: ToolStatus) -> Color {
    match status {
        ToolStatus::Running => Color::Reset,
        ToolStatus::Ok => Color::Green,
        ToolStatus::Failed => Color::Red,
        ToolStatus::Denied => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;
    use crate::state::AppState;
    use std::time::Instant;

    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    #[test]
    fn records_render_prefixes_and_wrap_to_width() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.begin_turn(
            "t1".to_string(),
            "这是一个很长的用户问题需要折行".to_string(),
        );
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::Delta(omnicrawl_ipc::bridge::TextPayload {
                text: "回答".to_string(),
            }),
            Instant::now(),
        );
        let lines = display_lines(&state, 20);
        let rendered = texts(&lines);
        assert!(
            rendered.iter().any(|line| line.starts_with("$ 这是")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with("◇ 回答")),
            "{rendered:?}"
        );
        for line in &rendered {
            assert!(display_width(line) <= 20, "行超宽：{line:?}");
        }
    }

    #[test]
    fn reasoning_collapses_to_latest_lines_with_hint() {
        let text: String = (1..=8).map(|index| format!("想法{index}\n")).collect();
        let mut lines = Vec::new();
        push_reasoning(&mut lines, text.trim_end(), 20);
        let rendered = texts(&lines);
        assert_eq!(rendered.len(), REASONING_TAIL + 1, "五行正文 + 一行提示");
        assert!(
            rendered[0].contains("想法4"),
            "应显示最新五行：{rendered:?}"
        );
        assert!(
            rendered[rendered.len() - 1].contains("思考已折叠"),
            "{rendered:?}"
        );
    }

    #[test]
    fn tool_card_box_is_closed_and_body_is_sampled() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.begin_turn("t1".to_string(), "跑工具".to_string());
        let now = Instant::now();
        let call = omnicrawl_core::ToolCall {
            name: "bash".to_string(),
            arguments: serde_json::json!({"command": "pytest -q"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: "c1".to_string(),
            function_name: "bash".to_string(),
        };
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::ToolStarted(
                omnicrawl_ipc::bridge::ToolStartedPayload {
                    step: 1,
                    call: call.clone(),
                },
            ),
            now,
        );
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::ToolFinished(
                omnicrawl_ipc::bridge::ToolEventPayload {
                    call,
                    result: omnicrawl_core::ToolResult {
                        ok: true,
                        output: (1..=9).map(|index| format!("行{index}\n")).collect(),
                        full_output: String::new(),
                        error_code: None,
                        retryable: false,
                    },
                },
            ),
            now + std::time::Duration::from_millis(1500),
        );

        let lines = display_lines(&state, 40);
        let rendered = texts(&lines);
        let title = rendered
            .iter()
            .find(|line| line.contains("bash"))
            .expect("应有工具卡标题");
        assert!(title.contains("成功"), "{title}");
        assert!(title.contains("1.50s"), "{title}");
        let box_lines: Vec<&String> = rendered
            .iter()
            .filter(|line| line.starts_with('│'))
            .collect();
        assert_eq!(box_lines.len(), TOOL_BODY_LIMIT, "正文限五行");
        assert!(
            box_lines.iter().any(|line| line.contains("省略 5 行")),
            "{box_lines:?}"
        );
        assert!(rendered
            .iter()
            .any(|line| line.starts_with('└') && line.ends_with('┘')));
    }

    #[test]
    fn preview_keeps_head_and_tail() {
        let body: Vec<String> = (1..=9).map(|index| format!("行{index}")).collect();
        let shown = preview(&body, 5);
        assert_eq!(shown.len(), 5);
        assert_eq!(shown[0], "行1");
        assert_eq!(shown[1], "行2");
        assert_eq!(shown[2], "… 省略 5 行");
        assert_eq!(shown[3], "行8");
        assert_eq!(shown[4], "行9");

        assert_eq!(preview(&body[..3], 5).len(), 3, "未超上限时原样返回");
    }

    #[test]
    fn window_follows_bottom_until_scrolled_up() {
        let lines: Vec<Line<'static>> = (0..10)
            .map(|index| Line::raw(format!("行{index}")))
            .collect();
        let bottom = texts(&window(&lines, 3, 0));
        assert_eq!(bottom, vec!["行7", "行8", "行9"]);
        let scrolled = texts(&window(&lines, 3, 2));
        assert_eq!(scrolled, vec!["行5", "行6", "行7"]);
        let over = texts(&window(&lines, 4, 99));
        assert_eq!(
            over,
            vec!["行0", "行1", "行2", "行3"],
            "滚到顶就停在最早的行"
        );
        assert!(texts(&window(&lines, 4, 6)) == vec!["行0", "行1", "行2", "行3"]);
    }
}
