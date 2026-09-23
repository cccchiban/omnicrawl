//! 消息流：把记录渲染成显示行，并按滚动位置取出可见窗口。
//!
//! 记录先按真实列宽软折行，再进入窗口计算，因此终端自己的换行不会打乱滚动位置。
//!
//! 每行可带一个 [`LineHit`]：鼠标点击落在该行时由 [`hit_test`] 解析成工具卡展开 /
//! 收起动作（对映 Python `ToolDisclosure` 的提示行点击展开、展开态点击卡片收起）。
//! 点击目标的解析不能只看「第几条记录」——同一条记录会折成多行，因此命中信息跟着
//! 显示行一起产出。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{display_width, fit, pad, wrap_display};
use crate::state::{AppState, Record, ToolCard, ToolStatus};
use crate::ui::fullscreen::rendering::widgets::{SubAgentConversation, SubAgentProgressTree};

/// 工具卡正文在缩略态与展开态的行数上限（对映 Python `MAX_EXPANDED_BODY_LINES`）。
const TOOL_BODY_LIMIT: usize = 5;
/// 缩略态正文保留的首部 / 尾部有效行数（对映 `HEAD_BODY_LINES` / `TAIL_BODY_LINES`）。
const TOOL_HEAD_LINES: usize = 2;
const TOOL_TAIL_LINES: usize = 2;
/// 正文相对卡片左边框的缩进（对映 `BODY_INDENT`）。
const TOOL_BODY_INDENT: &str = "    ";
/// 思考段默认折叠时展示的最新行数。
const REASONING_TAIL: usize = 5;

/// 一行显示行上的可点击目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineHit {
    /// 工具卡省略区的提示行：点击展开被省略的正文。
    ToolHint { call_id: String },
    /// 工具卡其余部分：处于展开态时点击收起。
    ToolCard { call_id: String },
    /// 思考段：点击在折叠与展开之间切换（`index` 是该记录在消息流里的下标）。
    Reasoning { index: usize },
}

/// 显示行 + 它承载的点击目标。
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayLine {
    pub line: Line<'static>,
    pub hit: Option<LineHit>,
}

impl DisplayLine {
    fn plain(line: Line<'static>) -> Self {
        Self { line, hit: None }
    }

    fn with_hit(line: Line<'static>, hit: LineHit) -> Self {
        Self {
            line,
            hit: Some(hit),
        }
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let lines = display_lines(state, area.width);
    let window = window(&lines, area.height as usize, state.scroll_from_bottom);
    frame.render_widget(Paragraph::new(window), area);
}

/// 全部记录的显示行（已按宽度折行）。
///
/// 没有任何记录时（空会话首屏）只展示欢迎 Logo，与 Python 侧 `#welcome-logo`
/// 在首条消息出现前可见、清空会话后重新出现的语义一致。
pub fn display_lines(state: &AppState, width: u16) -> Vec<DisplayLine> {
    if state.records.is_empty() {
        return welcome_logo_lines(state);
    }
    let width = width.max(1) as usize;
    let mut lines: Vec<DisplayLine> = Vec::new();
    for (index, record) in state.records.iter().enumerate() {
        // 「思考显示」关闭时思考段整段不出现（连它上面那行空行也不占位）。
        if matches!(record, Record::Reasoning(_)) && !state.show_thinking {
            continue;
        }
        lines.push(DisplayLine::plain(Line::raw("")));
        match record {
            Record::User(text) => push_prefixed(&mut lines, "$ ", text, width, Color::Magenta),
            Record::Assistant(text) => push_prefixed(&mut lines, "◇ ", text, width, Color::Reset),
            Record::Reasoning(text) => push_reasoning(
                &mut lines,
                text,
                width,
                index,
                state.is_reasoning_expanded(index),
            ),
            Record::Notice(text) => push_prefixed(&mut lines, "· ", text, width, Color::DarkGray),
            Record::Tool(card) => push_tool(&mut lines, card, width, state),
            Record::SubagentTree(tree) => push_subagent_tree(&mut lines, tree),
            Record::SubagentConversation(panel) => {
                push_subagent_conversation(&mut lines, panel, width)
            }
        }
    }
    lines.push(DisplayLine::plain(Line::raw("")));
    lines
}

/// 欢迎 Logo 的显示行：入场动画播放中为乱码扫描帧，未播放或播完为静态白色块字。
///
/// Logo 比多数窗口宽，这里不折行：超出部分按 ratatui 的默认行为裁掉，避免把块字
/// 拆成两段错位。
fn welcome_logo_lines(state: &AppState) -> Vec<DisplayLine> {
    state
        .logo
        .text()
        .split_lines()
        .iter()
        .map(|line| DisplayLine::plain(Line::from(line.to_spans())))
        .collect()
}

/// 从底部起取可见窗口：`scroll_from_bottom` 为向上滚过的行数，滚到顶就停在最早的行。
pub fn window(
    lines: &[DisplayLine],
    height: usize,
    scroll_from_bottom: usize,
) -> Vec<Line<'static>> {
    let (start, end) = window_range(lines.len(), height, scroll_from_bottom);
    lines[start..end]
        .iter()
        .map(|rendered| rendered.line.clone())
        .collect()
}

/// 可见窗口在全部显示行里的下标区间；`height` 为 0 时是空区间。
pub fn window_range(total: usize, height: usize, scroll_from_bottom: usize) -> (usize, usize) {
    if height == 0 {
        return (0, 0);
    }
    let offset = scroll_from_bottom.min(total.saturating_sub(height));
    let end = total - offset;
    (end.saturating_sub(height), end)
}

/// 点击落在消息区第 `row` 行（区内相对行号）时的目标。
///
/// 行号先换算成全部显示行里的绝对下标（与 [`window`] 同一套滚动计算），再取该行的
/// 命中信息；越界或该行没有目标时返回 `None`。
pub fn hit_test(
    state: &AppState,
    width: u16,
    height: usize,
    scroll_from_bottom: usize,
    row: usize,
) -> Option<LineHit> {
    let lines = display_lines(state, width);
    let (start, end) = window_range(lines.len(), height, scroll_from_bottom);
    let index = start.checked_add(row)?;
    if index >= end {
        return None;
    }
    lines.get(index).and_then(|line| line.hit.clone())
}

/// 正文采样：超出上限时保留首尾各两行有效行，中间以可点击提示行代替。
///
/// 对映 Python `ToolDisclosure._body_parts`：先按非空行判断是否真的需要缩略
/// （有效行不超过上限时原样显示），再把首尾有效行与省略的有效行数一并返回。
pub fn collapsed_body(body: &[String]) -> (Vec<String>, usize, Vec<String>) {
    let effective: Vec<&String> = body.iter().filter(|line| !line.trim().is_empty()).collect();
    if body.len() <= TOOL_BODY_LIMIT || effective.len() <= TOOL_BODY_LIMIT {
        return (body.to_vec(), 0, Vec::new());
    }
    let head: Vec<String> = effective[..TOOL_HEAD_LINES]
        .iter()
        .map(|line| (*line).clone())
        .collect();
    let tail: Vec<String> = effective[effective.len() - TOOL_TAIL_LINES..]
        .iter()
        .map(|line| (*line).clone())
        .collect();
    let hidden = effective.len() - TOOL_HEAD_LINES - TOOL_TAIL_LINES;
    (head, hidden, tail)
}

/// 省略区提示行文案（对映 `EXPAND_HINT`）。
pub fn expand_hint(hidden: usize) -> String {
    format!("点击展开 {hidden} 行")
}

/// 点开着的工具卡在展开态是否收起（对映 `ToolDisclosure.on_click`）。
pub fn hit_collapses_tool(state: &AppState, hit: &LineHit) -> Option<String> {
    match hit {
        LineHit::ToolCard { call_id } if state.is_tool_expanded(call_id) => Some(call_id.clone()),
        _ => None,
    }
}

fn push_prefixed(
    lines: &mut Vec<DisplayLine>,
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
        lines.push(DisplayLine::plain(Line::from(vec![
            Span::styled(head, style),
            Span::styled(chunk.clone(), style),
        ])));
    }
}

/// 子任务进度树：直接铺开对映层构造好的富文本（每行自带图标与状态的取色）。
///
/// Python 侧 `SubAgentProgressTree` 是 `Static`，高度随行数自适应；这里同样不折行，
/// 超宽部分按 ratatui 默认行为裁断。耗时按最近一次刷新的时刻落定（由每一帧的
/// [`AppState::refresh_subagent_trees`] 推进），因此渲染本身保持只读。
fn push_subagent_tree(lines: &mut Vec<DisplayLine>, tree: &SubAgentProgressTree) {
    for line in tree.render_text(None).split_lines() {
        lines.push(DisplayLine::plain(Line::from(line.to_spans())));
    }
}

/// 子代理流式对话面板：`│` 包裹的会话面板按可用宽度换行。
fn push_subagent_conversation(
    lines: &mut Vec<DisplayLine>,
    panel: &SubAgentConversation,
    width: usize,
) {
    for line in panel.render_text(width).split_lines() {
        lines.push(DisplayLine::plain(Line::from(line.to_spans())));
    }
}

/// 思考段：折叠态显示最新 [`REASONING_TAIL`] 行，展开态显示全部（对映 Python
/// `ReasoningDisclosure` 的点击切换）。
fn push_reasoning(
    lines: &mut Vec<DisplayLine>,
    text: &str,
    width: usize,
    index: usize,
    expanded: bool,
) {
    let body_width = width.saturating_sub(2).max(1);
    let wrapped = wrap_display(text, body_width);
    let folded = if expanded {
        0
    } else {
        wrapped.len().saturating_sub(REASONING_TAIL)
    };
    for chunk in &wrapped[folded..] {
        lines.push(DisplayLine::with_hit(
            Line::styled(format!("  {chunk}"), Style::new().fg(Color::DarkGray)),
            LineHit::Reasoning { index },
        ));
    }
    let hint = match (expanded, folded) {
        (true, _) => format!("  ⋯ 思考（已展开，共 {} 行）", wrapped.len()),
        (false, 0) => "  ⋯ 思考".to_string(),
        (false, _) => format!(
            "  ⋯ 思考已折叠，仅显示最新 {REASONING_TAIL} 行（共 {} 行）",
            wrapped.len()
        ),
    };
    lines.push(DisplayLine::with_hit(
        Line::styled(
            hint,
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
        LineHit::Reasoning { index },
    ));
}

fn push_tool(lines: &mut Vec<DisplayLine>, card: &ToolCard, width: usize, state: &AppState) {
    let color = status_color(card.status);
    let expanded = state.is_tool_expanded(&card.call_id);
    if width < 6 {
        lines.push(DisplayLine::with_hit(
            Line::styled(fitted_tool_title(card, width), Style::new().fg(color)),
            LineHit::ToolCard {
                call_id: card.call_id.clone(),
            },
        ));
        return;
    }
    let inner = width - 2;
    let title = fitted_tool_title(card, inner.saturating_sub(3));
    let fill = inner.saturating_sub(3 + display_width(&title));
    lines.push(DisplayLine::with_hit(
        Line::from(vec![
            Span::styled("┌─ ", Style::new().fg(color)),
            Span::styled(title, Style::new().fg(color)),
            Span::styled(format!(" {}┐", "─".repeat(fill)), Style::new().fg(color)),
        ]),
        LineHit::ToolCard {
            call_id: card.call_id.clone(),
        },
    ));

    let (head, hidden, tail) = if expanded {
        (card.body.clone(), 0, Vec::new())
    } else {
        collapsed_body(&card.body)
    };
    let body_style = Style::new().fg(Color::DarkGray);
    let mut body_rows: Vec<(String, bool)> = Vec::new();
    for line in head {
        body_rows.push((line, false));
    }
    if hidden > 0 {
        body_rows.push((format!("{TOOL_BODY_INDENT}{}", expand_hint(hidden)), true));
    }
    for line in tail {
        body_rows.push((line, false));
    }
    for (line, is_hint) in body_rows {
        let rendered = if is_hint {
            Line::from(vec![
                Span::styled("│ ", Style::new().fg(color)),
                Span::styled(
                    pad(&fit(&line, inner), inner),
                    Style::new()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC | Modifier::UNDERLINED),
                ),
                Span::styled("│", Style::new().fg(color)),
            ])
        } else {
            Line::from(vec![
                Span::styled("│ ", Style::new().fg(color)),
                Span::styled(pad(&fit(&line, inner), inner), body_style),
                Span::styled("│", Style::new().fg(color)),
            ])
        };
        let hit = if is_hint {
            LineHit::ToolHint {
                call_id: card.call_id.clone(),
            }
        } else {
            LineHit::ToolCard {
                call_id: card.call_id.clone(),
            }
        };
        lines.push(DisplayLine::with_hit(rendered, hit));
    }
    lines.push(DisplayLine::with_hit(
        Line::styled(
            "└".to_string() + &"─".repeat(width.saturating_sub(2)) + "┘",
            Style::new().fg(color),
        ),
        LineHit::ToolCard {
            call_id: card.call_id.clone(),
        },
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

    fn plain(display: &[DisplayLine]) -> Vec<String> {
        display.iter().map(|line| text_of(&line.line)).collect()
    }

    /// 起一个带正文的工具卡（`read` 之外的工具才有正文）。
    fn state_with_tool(ok: bool, body_lines: usize) -> AppState {
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
                        ok,
                        output: (1..=body_lines)
                            .map(|index| format!("行{index}\n"))
                            .collect(),
                        full_output: String::new(),
                        error_code: None,
                        retryable: false,
                    },
                },
            ),
            now + std::time::Duration::from_millis(1500),
        );
        state
    }

    #[test]
    fn empty_conversation_shows_welcome_logo_only() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        let rendered = plain(&display_lines(&state, 40));
        assert_eq!(rendered.len(), 8, "空会话首屏只有 8 行块字：{rendered:?}");
        assert!(rendered[2].starts_with("  ██████"), "{:?}", rendered[2]);

        // 首条记录进来后 Logo 让位；清空会话后重新出现（动画已落定为静态字形）。
        state.notice("第一条提示".to_string());
        let with_record = plain(&display_lines(&state, 40));
        assert!(with_record
            .iter()
            .any(|line| line.starts_with("· 第一条提示")));
        assert!(!with_record.iter().any(|line| line.contains("██████")));
        state.records.clear();
        assert_eq!(plain(&display_lines(&state, 40)).len(), 8);
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
        let rendered = plain(&lines);
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
    fn subagent_tree_record_renders_one_line_per_task() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::SubagentEvent(
                omnicrawl_ipc::bridge::SubagentEventPayload {
                    name: "subagent.task.running".to_string(),
                    payload: serde_json::json!({
                        "task_id": "t1",
                        "batch_id": "batch-1",
                        "agent_type": "reviewer",
                        "description": "审查改动",
                    }),
                },
            ),
            Instant::now(),
        );
        let rendered = plain(&display_lines(&state, 40));
        let header = rendered
            .iter()
            .find(|line| line.contains("子任务进度"))
            .expect("应有进度树标题");
        assert!(header.contains("0/1 完成"), "{header}");
        assert!(
            rendered.iter().any(|line| line.starts_with("└─ ● ")),
            "运行中任务一行一项：{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("审查改动") && line.contains("· 运行中")),
            "{rendered:?}"
        );
        // 进度树不参与点击命中（Python 侧无交互动画）。
        assert!(display_lines(&state, 40)
            .iter()
            .all(|line| line.hit.is_none()));
    }

    #[test]
    fn reasoning_collapses_to_latest_lines_with_hint() {
        let text: String = (1..=8).map(|index| format!("想法{index}\n")).collect();
        let mut lines = Vec::new();
        push_reasoning(&mut lines, text.trim_end(), 20, 3, false);
        let rendered = plain(&lines);
        assert_eq!(rendered.len(), REASONING_TAIL + 1, "五行正文 + 一行提示");
        assert!(
            rendered[0].contains("想法4"),
            "应显示最新五行：{rendered:?}"
        );
        assert!(
            rendered[rendered.len() - 1].contains("思考已折叠"),
            "{rendered:?}"
        );
        assert_eq!(
            lines[0].hit,
            Some(LineHit::Reasoning { index: 3 }),
            "思考段可点击，命中带上记录下标"
        );

        // 展开态显示全部行，提示语随之变化。
        let mut expanded = Vec::new();
        push_reasoning(&mut expanded, text.trim_end(), 20, 3, true);
        let rendered = plain(&expanded);
        assert_eq!(rendered.len(), 9, "八行正文 + 一行提示");
        assert!(rendered[0].contains("想法1"), "{rendered:?}");
        assert!(rendered[8].contains("已展开，共 8 行"), "{rendered:?}");
    }

    #[test]
    fn tool_card_box_is_closed_and_collapsed_body_is_sampled() {
        let state = state_with_tool(true, 9);
        let display = display_lines(&state, 40);
        let rendered = plain(&display);
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
        assert_eq!(box_lines.len(), TOOL_BODY_LIMIT, "缩略态正文限五行");
        assert!(
            box_lines.iter().any(|line| line.contains("点击展开 5 行")),
            "{box_lines:?}"
        );
        assert!(rendered
            .iter()
            .any(|line| line.starts_with('└') && line.ends_with('┘')));

        // 提示行带 ToolHint 命中，其余卡片行是 ToolCard。
        let hint = display
            .iter()
            .find(|line| matches!(line.hit, Some(LineHit::ToolHint { .. })))
            .expect("应有提示行命中");
        assert!(text_of(&hint.line).contains("点击展开 5 行"));
    }

    #[test]
    fn expanding_a_tool_card_shows_full_body_and_drops_the_hint() {
        let mut state = state_with_tool(true, 9);
        state.expand_tool("c1");
        let rendered = plain(&display_lines(&state, 40));
        let box_lines: Vec<&String> = rendered
            .iter()
            .filter(|line| line.starts_with('│'))
            .collect();
        assert_eq!(box_lines.len(), 9, "展开态显示完整正文");
        assert!(
            !box_lines.iter().any(|line| line.contains("点击展开")),
            "{box_lines:?}"
        );
        assert!(box_lines.iter().any(|line| line.contains("行9")));

        // 收起后回到缩略态。
        assert!(!state.toggle_tool_expanded("c1"));
        assert_eq!(
            plain(&display_lines(&state, 40))
                .iter()
                .filter(|line| line.starts_with('│'))
                .count(),
            TOOL_BODY_LIMIT
        );
    }

    #[test]
    fn collapsed_body_keeps_head_and_tail() {
        let body: Vec<String> = (1..=9).map(|index| format!("行{index}")).collect();
        let (head, hidden, tail) = collapsed_body(&body);
        assert_eq!(head, vec!["行1".to_string(), "行2".to_string()]);
        assert_eq!(hidden, 5);
        assert_eq!(tail, vec!["行8".to_string(), "行9".to_string()]);

        // 有效行不超过上限时原样返回（含空行的正文按非空行计数）。
        let short: Vec<String> = vec!["行1".to_string(), String::new(), "行2".to_string()];
        let (head, hidden, tail) = collapsed_body(&short);
        assert_eq!(head.len(), 3);
        assert_eq!(hidden, 0);
        assert!(tail.is_empty());
    }

    #[test]
    fn hit_test_maps_window_rows_to_tool_targets() {
        let mut state = state_with_tool(true, 9);
        // 卡片共 1 标题 + 5 正文 + 1 底边 = 7 行，前面还有空行与用户行。
        let total = display_lines(&state, 40).len();
        let height = total;
        let hint_row = (0..height)
            .find(|row| {
                matches!(
                    hit_test(&state, 40, height, 0, *row),
                    Some(LineHit::ToolHint { .. })
                )
            })
            .expect("提示行应可命中");
        assert_eq!(
            hit_test(&state, 40, height, 0, hint_row),
            Some(LineHit::ToolHint {
                call_id: "c1".to_string()
            })
        );
        // 展开提示行之外的卡片行命中卡片本身。
        let title_row = hint_row - 3;
        assert_eq!(
            hit_test(&state, 40, height, 0, title_row),
            Some(LineHit::ToolCard {
                call_id: "c1".to_string()
            })
        );
        assert_eq!(
            hit_test(&state, 40, height, 0, height + 5),
            None,
            "越界无命中"
        );

        // 展开态下卡片点击应触发收起。
        state.expand_tool("c1");
        let hit = hit_test(&state, 40, height, 0, title_row).expect("标题行命中");
        assert_eq!(
            hit_collapses_tool(&state, &hit),
            Some("c1".to_string()),
            "展开态点击卡片应收起"
        );
    }

    #[test]
    fn window_follows_bottom_until_scrolled_up() {
        let lines: Vec<DisplayLine> = (0..10)
            .map(|index| DisplayLine::plain(Line::raw(format!("行{index}"))))
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
        assert_eq!(window_range(10, 0, 0), (0, 0));
    }
}
