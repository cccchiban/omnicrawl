//! FIFO 排队预览条（对映 Python `PendingQueue`）：生成期间按 Enter 排队的消息
//! 显示在输入框上方，标题行给出总数，消息行给出摘要与行尾 `[ DELETE ]` 撤回热区，
//! 超出可见上限时附一行展开/收起提示。
//!
//! 纯计算（行数、摘要、行序）复用 `fullscreen/status/indicators.rs` 的对映层，
//! 本模块只做「对映行 → ratatui 行 + 点击命中区」。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::fullscreen::status::indicators::{
    pending_queue_rows, queue_preview_rows, queue_title, queue_toggle_label, QueueRow,
};
use super::{fit, pad};
use crate::state::{AppState, QUEUE_PREVIEW_MAX_ROWS, QUEUE_PREVIEW_SUMMARY_LIMIT};

/// 行尾撤回热区文案（对映 Python `QueueDelete` 的 `" [ DELETE ]"`）。
pub const DELETE_LABEL: &str = " [ DELETE ]";

/// 排队预览条当前占用的行数；空队列不占位。
pub fn height(state: &AppState) -> u16 {
    pending_queue_rows(
        state.pending_inputs.len(),
        state.pending_queue_expanded,
        QUEUE_PREVIEW_MAX_ROWS,
    ) as u16
}

/// 鼠标点击落在预览条某一行时的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueHit {
    /// 撤回该条消息并回填输入框。
    Withdraw(usize),
    /// 展开/收起被折叠的条目。
    Toggle,
}

/// 待提交消息的借用视图（摘要计算要 `&[String]`）。
fn items(state: &AppState) -> Vec<String> {
    state.pending_inputs.iter().cloned().collect()
}

/// 预览行序列；空队列返回空表。
pub fn rows(state: &AppState) -> Vec<QueueRow> {
    queue_preview_rows(
        &items(state),
        state.pending_queue_expanded,
        QUEUE_PREVIEW_MAX_ROWS,
        QUEUE_PREVIEW_SUMMARY_LIMIT,
    )
}

/// 第 `row` 行（0 起）的点击动作；标题行与越界返回 `None`。
pub fn hit(state: &AppState, row: usize) -> Option<QueueHit> {
    match rows(state).get(row)? {
        QueueRow::Title => None,
        QueueRow::Item { index, .. } => Some(QueueHit::Withdraw(*index)),
        QueueRow::Toggle { .. } => Some(QueueHit::Toggle),
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let summary_width = width.saturating_sub(super::display_width(DELETE_LABEL));
    let delete_style = Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM | Modifier::BOLD);
    let toggle_style = delete_style.fg(Color::Cyan);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for row in rows(state) {
        match row {
            QueueRow::Title => lines.push(Line::from(
                queue_title(state.pending_inputs.len()).to_spans(),
            )),
            QueueRow::Item { index, summary } => {
                lines.push(Line::from(vec![
                    Span::raw(pad(
                        &super::fullscreen::status::indicators::queue_item_text(index, &summary),
                        summary_width,
                    )),
                    Span::styled(DELETE_LABEL.to_string(), delete_style),
                ]));
            }
            QueueRow::Toggle { .. } => {
                let count = state.pending_inputs.len();
                let label =
                    queue_toggle_label(count, QUEUE_PREVIEW_MAX_ROWS, state.pending_queue_expanded);
                lines.push(Line::styled(fit(&label, width), toggle_style));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;
    use crate::ui::fullscreen::status::indicators::queue_item_text;

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

    #[test]
    fn empty_queue_takes_no_rows() {
        let state = state();
        assert_eq!(height(&state), 0);
        assert!(rows(&state).is_empty());
        assert_eq!(hit(&state, 0), None);
    }

    #[test]
    fn queue_rows_follow_fifo_with_delete_hotzone() {
        let mut state = state();
        state.queue_pending("第一条留言\n第二行".to_string());
        state.queue_pending("  第二条   带空白  ".to_string());
        assert_eq!(height(&state), 3, "标题 + 两条消息");

        let lines: Vec<String> = rows(&state)
            .iter()
            .map(|row| match row {
                QueueRow::Title => "标题".to_string(),
                QueueRow::Item { index, summary } => format!("{index}:{summary}"),
                QueueRow::Toggle { label } => label.clone(),
            })
            .collect();
        assert_eq!(
            lines,
            vec!["标题", "0:第一条留言", "1:第二条 带空白"],
            "摘要只取首行并压缩空白"
        );
        assert_eq!(hit(&state, 0), None, "标题行不可点击");
        assert_eq!(hit(&state, 1), Some(QueueHit::Withdraw(0)));
        assert_eq!(hit(&state, 2), Some(QueueHit::Withdraw(1)));
        assert_eq!(hit(&state, 9), None, "越界没有命中");
    }

    #[test]
    fn overflow_collapses_until_toggled() {
        let mut state = state();
        for index in 1..=5 {
            state.queue_pending(format!("消息{index}"));
        }
        assert_eq!(height(&state), 5, "标题 + 3 条 + 提示行");
        assert_eq!(hit(&state, 4), Some(QueueHit::Toggle));

        state.toggle_queue_expanded();
        assert_eq!(height(&state), 7, "展开后显示全部 5 条 + 提示行");
        assert!(
            matches!(rows(&state).last(), Some(QueueRow::Toggle { label }) if label.contains("收起")),
            "{:?}",
            rows(&state)
        );

        // 队列缩回可见上限内时展开态自动退出（撤回同样走这条同步路径）。
        assert!(state.withdraw_pending(0));
        assert!(state.withdraw_pending(0));
        assert_eq!(state.pending_inputs.len(), QUEUE_PREVIEW_MAX_ROWS);
        assert!(!state.pending_queue_expanded, "缩回上限内即退出展开态");
        assert_eq!(height(&state), 4, "标题 + 3 条，不再有提示行");
    }

    #[test]
    fn rendered_rows_keep_terminal_width() {
        let mut state = state();
        state.queue_pending("一条很长的排队消息需要被截断到行宽之内".to_string());
        let width = 30usize;
        let row = rows(&state)
            .into_iter()
            .find_map(|row| match row {
                QueueRow::Item { index, summary } => Some(queue_item_text(index, &summary)),
                _ => None,
            })
            .expect("有消息行");
        let line = Line::from(vec![
            Span::raw(pad(&row, width - super::super::display_width(DELETE_LABEL))),
            Span::raw(DELETE_LABEL.to_string()),
        ]);
        assert_eq!(super::super::display_width(&text_of(&line)), width);
        assert!(text_of(&line).ends_with(DELETE_LABEL));
    }
}
