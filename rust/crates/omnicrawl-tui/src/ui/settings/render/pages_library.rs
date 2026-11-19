use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::row_hit;
use super::helpers::{fit, window_offset};
use super::list::render_pane_tail;
use super::super::hit::HitAction;
use super::super::state::{KnowledgeRow, MemoryRow, SettingsState};
use super::FORM_TAIL_HEIGHT;

/// 会话管理页：一列会话（标题 + 条数），底部状态行给出结果或确认提示。
///
/// 当前会话标 `（当前）` 且不可删——内核会拒绝，界面先讲清楚，避免按了没反应。
pub(super) fn render_sessions(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.session_rows();
    let tail_height = FORM_TAIL_HEIGHT.min(area.height);
    let list_area = Rect {
        height: area.height.saturating_sub(tail_height),
        ..area
    };
    let visible = list_area.height.max(1) as usize;
    let offset = window_offset(state.session_selected(), rows.len(), visible);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if rows.is_empty() {
        lines.push(Line::styled(
            fit("还没有可管理的会话。按 R 刷新。", area.width as usize),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    for (index, row) in rows.iter().enumerate().skip(offset).take(visible) {
        let selected = index == state.session_selected();
        let cursor = if selected { "› " } else { "  " };
        // 确认态要把「这一行马上就要被删掉」标出来，否则连按两次读不出区别。
        let confirming = state
            .session_confirming()
            .is_some_and(|pending| pending.session_id == row.session_id);
        let mark = if confirming {
            "  待确认删除"
        } else if row.is_current {
            "  （当前）"
        } else {
            ""
        };
        let text = format!("{cursor}{}{mark}", row.summary());
        let style = if confirming {
            theme::rich_style(theme::ACCENT_WHITE)
        } else if selected && focused {
            theme::rich_style(theme::ACCENT_WHITE)
        } else if row.is_current {
            theme::rich_style(theme::TEXT_MUTED)
        } else {
            theme::rich_style(theme::TEXT_SECONDARY)
        };
        state.record_hit(row_hit(list_area, lines.len()), HitAction::PaneRow(index));
        lines.push(Line::styled(fit(&text, area.width as usize), style));
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());

    let tail = Rect {
        y: area.y + area.height.saturating_sub(tail_height),
        height: tail_height,
        ..area
    };
    // 确认态优先于普通状态文本：这时最重要的是让人看清「再按一次就删」。
    let status = match state.session_confirming() {
        Some(pending) => format!("再按 D 确认删除：{}（Esc 取消）", pending.title),
        None => state.status().to_string(),
    };
    render_pane_tail(
        frame,
        tail,
        &status,
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}

/// 记忆功能页：四行（开关、遗忘时间、清空、打开目录）+ 底部状态。
pub(super) fn render_memory(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let tail_height = FORM_TAIL_HEIGHT.min(area.height);
    let list_area = Rect {
        height: area.height.saturating_sub(tail_height),
        ..area
    };
    let visible = list_area.height.max(1) as usize;
    let rows = MemoryRow::ORDER;
    let offset = window_offset(state.memory_selected(), rows.len(), visible);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset).take(visible) {
        let selected = index == state.memory_selected();
        let cursor = if selected { "› " } else { "  " };
        let value = state.memory_row_value(*row);
        let text = if value.is_empty() {
            format!("{cursor}{}", row.label())
        } else {
            format!("{cursor}{}：{value}", row.label())
        };
        let style = if selected && focused {
            theme::rich_style(theme::ACCENT_WHITE)
        } else if selected {
            theme::rich_style(theme::TEXT_SECONDARY)
        } else {
            theme::rich_style(theme::TEXT_MUTED)
        };
        state.record_hit(row_hit(list_area, lines.len()), HitAction::PaneRow(index));
        lines.push(Line::styled(fit(&text, area.width as usize), style));
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());

    let tail = Rect {
        y: area.y + area.height.saturating_sub(tail_height),
        height: tail_height,
        ..area
    };
    // 确认态压过普通状态：这时最重要的是让人看清「再按一次就清空」。
    let status = if state.memory_confirming_clear() {
        "再按 Enter 或 D 确认消除所有记忆（Esc 取消）".to_string()
    } else {
        state.status().to_string()
    };
    render_pane_tail(
        frame,
        tail,
        &status,
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}

pub(super) fn render_knowledge(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let tail_height = FORM_TAIL_HEIGHT.min(area.height);
    let list_area = Rect {
        height: area.height.saturating_sub(tail_height),
        ..area
    };
    let visible = list_area.height.max(1) as usize;
    let rows = KnowledgeRow::ORDER;
    let offset = window_offset(state.knowledge_selected(), rows.len(), visible);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset).take(visible) {
        let selected = index == state.knowledge_selected();
        let cursor = if selected { "› " } else { "  " };
        let value = state.knowledge_row_value(*row);
        let text = if value.is_empty() {
            format!("{cursor}{}", row.label())
        } else {
            format!("{cursor}{}：{value}", row.label())
        };
        let style = if selected && focused {
            theme::rich_style(theme::ACCENT_WHITE)
        } else if selected {
            theme::rich_style(theme::TEXT_SECONDARY)
        } else {
            theme::rich_style(theme::TEXT_MUTED)
        };
        state.record_hit(row_hit(list_area, lines.len()), HitAction::PaneRow(index));
        lines.push(Line::styled(fit(&text, area.width as usize), style));
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());

    let tail = Rect {
        y: area.y + area.height.saturating_sub(tail_height),
        height: tail_height,
        ..area
    };
    // 确认态压过普通状态：这时最重要的是让人看清「再按一次就清空」。
    let status = if state.knowledge_confirming_clear() {
        "再按 Enter 或 D 确认清除所有知识库（Esc 取消）".to_string()
    } else {
        state.status().to_string()
    };
    render_pane_tail(
        frame,
        tail,
        &status,
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}
