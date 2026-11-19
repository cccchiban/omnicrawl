//! FIFO 排队预览条（对映 Python `PendingQueue`）：生成期间按 Enter 排队的消息
//! 显示在输入框上方，标题行给出总数，消息行给出摘要与行尾 `[ DELETE ]` 撤回热区，
//! 超出可见上限时附一行展开/收起提示。
//!
//! 消息行按方框内的真实列宽折行（一条消息可占多行物理行，最多
//! [`QUEUE_PREVIEW_ITEM_LINES`] 行，再长就末行以 `…` 收尾）：只有列宽在手才能算准
//! 折行，所以本模块按 `width` 一次构出「物理行 + 每行命中动作」，行数、渲染与鼠标
//! 命中都读这一份结果，三者不会错位。
//!
//! 纯计算（摘要、行序、展开/收起判定）复用 `fullscreen/status/indicators.rs` 的对映层。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::fullscreen::status::indicators::{
    queue_item_prefix, queue_preview_rows, queue_title, queue_toggle_label, QueueRow,
    QUEUE_PREVIEW_ITEM_LINES,
};
use super::{fit, pad, wrap_display};
use crate::state::{AppState, QUEUE_PREVIEW_MAX_ROWS};

/// 行尾撤回热区文案（对映 Python `QueueDelete` 的 `" [ DELETE ]"`）。
pub const DELETE_LABEL: &str = " [ DELETE ]";

/// 鼠标点击落在预览条某一行时的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueHit {
    /// 撤回该条消息并回填输入框。
    Withdraw(usize),
    /// 展开/收起被折叠的条目。
    Toggle,
}

/// 待提交消息的借用视图（摘要计算要 `&[String]`）：排队预览只显示文本，
/// 图片已在文本里以 `[ #n Image ]` 占位符的形式出现。
fn items(state: &AppState) -> Vec<String> {
    state
        .pending_inputs
        .iter()
        .map(|submission| submission.text.clone())
        .collect()
}

/// 预览条折叠成一行一条的逻辑行；空队列返回空表。
fn rows(state: &AppState) -> Vec<QueueRow> {
    queue_preview_rows(
        &items(state),
        state.pending_queue_expanded,
        QUEUE_PREVIEW_MAX_ROWS,
    )
}

/// 排好版的一帧：每行文本与它的点击动作一一对应。
struct Rendered {
    lines: Vec<Line<'static>>,
    hits: Vec<Option<QueueHit>>,
}

/// 按 `width` 列宽排出预览条。`width` 为 0 时不出行。
fn render_block(state: &AppState, width: usize) -> Rendered {
    let mut rendered = Rendered {
        lines: Vec::new(),
        hits: Vec::new(),
    };
    if width == 0 {
        return rendered;
    }
    let delete_width = super::display_width(DELETE_LABEL);
    // 消息行给行尾撤回热区让位，两种行都按这个宽度排：折行与热区因此不会互相挤压。
    let item_width = width.saturating_sub(delete_width).max(1);
    let delete_style = Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM | Modifier::BOLD);
    let toggle_style = delete_style.fg(Color::Cyan);

    for row in rows(state) {
        match row {
            QueueRow::Title => {
                rendered.lines.push(Line::from(
                    queue_title(state.pending_inputs.len()).to_spans(),
                ));
                rendered.hits.push(None);
            }
            QueueRow::Item { index, summary } => {
                // 前缀（`  1. `）单独占位，续行补等宽空格做悬挂缩进：折开的正文因此上下对齐。
                let prefix = queue_item_prefix(index);
                let prefix_width = super::display_width(&prefix);
                let body_width = item_width.saturating_sub(prefix_width).max(1);
                let mut wrapped = wrap_display(&summary, body_width);
                if wrapped.len() > QUEUE_PREVIEW_ITEM_LINES {
                    wrapped.truncate(QUEUE_PREVIEW_ITEM_LINES);
                    if let Some(last) = wrapped.last_mut() {
                        *last = fit(&format!("{last}…"), body_width);
                    }
                }
                let last_index = wrapped.len().saturating_sub(1);
                let indent = " ".repeat(prefix_width);
                for (line_index, text) in wrapped.iter().enumerate() {
                    let lead = if line_index == 0 {
                        prefix.as_str()
                    } else {
                        indent.as_str()
                    };
                    let body = Span::raw(pad(&format!("{lead}{text}"), item_width));
                    rendered.lines.push(if line_index == last_index {
                        Line::from(vec![
                            body,
                            Span::styled(DELETE_LABEL.to_string(), delete_style),
                        ])
                    } else {
                        Line::from(vec![body, Span::raw(" ".repeat(delete_width))])
                    });
                    // 一条消息折成几行都算它的热区：点哪一行都撤回这条。
                    rendered.hits.push(Some(QueueHit::Withdraw(index)));
                }
            }
            QueueRow::Toggle { .. } => {
                let count = state.pending_inputs.len();
                let label =
                    queue_toggle_label(count, QUEUE_PREVIEW_MAX_ROWS, state.pending_queue_expanded);
                rendered
                    .lines
                    .push(Line::styled(fit(&label, width), toggle_style));
                rendered.hits.push(Some(QueueHit::Toggle));
            }
        }
    }
    rendered
}

/// 排队预览条当前占用的行数（按 `width` 折行后的物理行数）；空队列不占位。
pub fn height(state: &AppState, width: u16) -> u16 {
    render_block(state, usize::from(width)).lines.len() as u16
}

/// 第 `row` 行（0 起）的点击动作；标题行与越界返回 `None`。
pub fn hit(state: &AppState, width: u16, row: usize) -> Option<QueueHit> {
    *render_block(state, usize::from(width)).hits.get(row)?
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let lines = render_block(state, usize::from(area.width)).lines;
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;

    fn state() -> AppState {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.turn = crate::state::TurnState::Running {
            turn_id: "t1".to_string(),
        };
        state
    }

    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    const WIDTH: u16 = 60;

    #[test]
    fn empty_queue_takes_no_rows() {
        let state = state();
        assert_eq!(height(&state, WIDTH), 0);
        assert!(render_block(&state, usize::from(WIDTH)).lines.is_empty());
        assert_eq!(hit(&state, WIDTH, 0), None);
    }

    #[test]
    fn queue_rows_follow_fifo_with_delete_hotzone() {
        let mut state = state();
        state.queue_pending("第一条留言\n第二行".to_string(), Vec::new(), Vec::new());
        state.queue_pending("  第二条   带空白  ".to_string(), Vec::new(), Vec::new());
        assert_eq!(height(&state, WIDTH), 3, "标题 + 两条消息");

        let rendered = render_block(&state, usize::from(WIDTH));
        let texts: Vec<String> = rendered.lines.iter().map(text_of).collect();
        assert_eq!(texts[0], "⏳ 2 条消息排队", "标题行带排队总数：{texts:?}");
        assert!(texts[1].starts_with("  1. 第一条留言"), "{:?}", texts[1]);
        assert!(texts[2].starts_with("  2. 第二条 带空白"), "{:?}", texts[2]);
        assert!(
            texts[1].ends_with(DELETE_LABEL),
            "行尾有撤回热区：{texts:?}"
        );
        assert_eq!(rendered.hits[0], None, "标题行不可点击");
        assert_eq!(rendered.hits[1], Some(QueueHit::Withdraw(0)));
        assert_eq!(rendered.hits[2], Some(QueueHit::Withdraw(1)));
        assert_eq!(hit(&state, WIDTH, 9), None, "越界没有命中");
    }

    #[test]
    fn long_message_wraps_instead_of_being_cut() {
        let mut state = state();
        let long = "这是一条很长的排队消息".repeat(12);
        state.queue_pending(long.clone(), Vec::new(), Vec::new());

        let width = 40usize;
        let item_width = width - super::super::display_width(DELETE_LABEL);
        let lines = render_block(&state, width).lines;
        assert!(lines.len() > 2, "长消息要折成多行：{:?}", lines.len());
        assert_eq!(
            lines.len(),
            1 + QUEUE_PREVIEW_ITEM_LINES,
            "折叠上下限：标题 + 消息最多 {} 行",
            QUEUE_PREVIEW_ITEM_LINES
        );
        let texts: Vec<String> = lines.iter().map(text_of).collect();
        for text in &texts[1..] {
            assert_eq!(
                super::super::display_width(text),
                width,
                "消息行都排满栏宽：{text:?}"
            );
        }
        let body: String = texts[1..]
            .iter()
            .map(|text| text.trim_end_matches(DELETE_LABEL))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            body.starts_with("  1. 这是一条很长的排队消息"),
            "首行内容原样保留、只折行不截断：{body:?}"
        );
        // 续行按前缀列宽悬挂缩进：折开的正文上下对齐，不会顶到左边框。
        let prefix_width = super::super::display_width(&queue_item_prefix(0));
        for text in &texts[2..] {
            assert_eq!(
                text.chars().take(prefix_width).collect::<String>(),
                " ".repeat(prefix_width),
                "续行要悬挂缩进：{text:?}"
            );
        }
        assert!(
            texts.last().expect("有行").contains('…'),
            "超出上限时末行以省略号收尾：{texts:?}"
        );
        assert_eq!(
            super::super::display_width(&texts[2]),
            width,
            "续行同样铺满栏宽（热区含在内）"
        );
        assert!(item_width > prefix_width, "测试栏宽要留得下前缀");
        // 折开的每一行都属于同一条消息的热区。
        let rendered = render_block(&state, width);
        for index in 1..rendered.lines.len() {
            assert_eq!(rendered.hits[index], Some(QueueHit::Withdraw(0)));
        }
    }

    #[test]
    fn overflow_collapses_until_toggled() {
        let mut state = state();
        for index in 1..=5 {
            state.queue_pending(format!("消息{index}"), Vec::new(), Vec::new());
        }
        assert_eq!(height(&state, WIDTH), 5, "标题 + 3 条 + 提示行");
        assert_eq!(hit(&state, WIDTH, 4), Some(QueueHit::Toggle));

        state.toggle_queue_expanded();
        assert_eq!(height(&state, WIDTH), 7, "展开后显示全部 5 条 + 提示行");
        let last = render_block(&state, usize::from(WIDTH))
            .lines
            .last()
            .map(text_of)
            .expect("有提示行");
        assert!(last.contains("收起"), "{last:?}");

        // 队列缩回可见上限内时展开态自动退出（撤回同样走这条同步路径）。
        assert!(state.withdraw_pending(0));
        assert!(state.withdraw_pending(0));
        assert_eq!(state.pending_inputs.len(), QUEUE_PREVIEW_MAX_ROWS);
        assert!(!state.pending_queue_expanded, "缩回上限内即退出展开态");
        assert_eq!(height(&state, WIDTH), 4, "标题 + 3 条，不再有提示行");
    }

    #[test]
    fn rendered_rows_keep_terminal_width() {
        let mut state = state();
        state.queue_pending(
            "一条很长的排队消息需要折行到栏宽之内而不是被截断".to_string(),
            Vec::new(),
            Vec::new(),
        );
        for width in [30u16, 45, 80] {
            let rendered = render_block(&state, usize::from(width));
            for line in rendered.lines.iter().skip(1) {
                assert_eq!(
                    super::super::display_width(&text_of(line)),
                    usize::from(width),
                    "消息行都正好铺满栏宽（{width}）：{:?}",
                    text_of(line)
                );
            }
            assert_eq!(
                height(&state, width),
                rendered.lines.len() as u16,
                "高度按折行后的物理行算"
            );
        }
    }
}
