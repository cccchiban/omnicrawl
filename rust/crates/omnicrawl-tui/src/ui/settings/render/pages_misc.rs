use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::{menu_text_style, row_hit};
use super::helpers::{fit, window_offset};
use super::list::render_pane_tail;
use super::pages_decision::decision_window_start_owners;
use super::super::hit::HitAction;
use super::super::state::SettingsState;

/// 视觉设置页：模型原生视觉行 + 代理开关行 + 故障转移列表 + 状态与提示。
pub(super) fn render_vision(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let mut lines: Vec<Line<'static>> = vec![
        Line::styled(
            fit(&state.vision_native_text(), area.width as usize),
            theme::rich_style(theme::TEXT_PRIMARY),
        ),
        Line::styled(
            fit(&state.vision_enabled_text(), area.width as usize),
            theme::rich_style(theme::ACCENT_AMBER),
        ),
    ];
    // 底部 4 行留给状态与提示，头部固定占 2 行。
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved).saturating_sub(2) as usize;
    let rows = state.vision_rows();
    let offset = window_offset(state.vision_selected(), rows.len(), list_height);
    for (index, text) in rows.iter().enumerate().skip(offset).take(list_height) {
        state.record_hit(
            row_hit(
                Rect {
                    x: area.x + 1,
                    width: area.width.saturating_sub(1),
                    ..area
                },
                lines.len(),
            ),
            HitAction::PaneRow(index),
        );
        let selected = index == state.vision_selected();
        let style = if selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        };
        lines.push(Line::styled(fit(text, area.width as usize), style));
    }
    if rows.is_empty() {
        lines.push(Line::styled(
            fit(
                "尚未添加视觉模型，请按 A 从现有渠道选择。",
                area.width as usize,
            ),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    Paragraph::new(lines).render(
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            ..area
        },
        frame.buffer_mut(),
    );

    let tail = Rect {
        y: area.y + area.height.saturating_sub(reserved),
        height: reserved,
        ..area
    };
    render_pane_tail(
        frame,
        tail,
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}

/// 子任务设置页：分区标题 + 行（`› 标签：值`），选中即改。
pub(super) fn render_subagents(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.subagent_rows();
    // 底部 4 行留给状态与提示（与工具开关页同款）。
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let offset = window_offset(state.subagent_selected(), rows.len(), list_height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut section = "";
    for (index, row) in rows.iter().enumerate().skip(offset).take(list_height) {
        state.record_hit(row_hit(area, lines.len()), HitAction::PaneRow(index));
        if row.section != section {
            section = row.section;
            lines.push(Line::styled(
                fit(&format!(" {section}"), area.width as usize),
                theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::BOLD),
            ));
        }
        let current = index == state.subagent_selected();
        let style = if current && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        };
        lines.push(Line::styled(
            fit(
                &format!(
                    "{} {}：{}",
                    if current { "›" } else { " " },
                    row.label,
                    row.value
                ),
                area.width as usize,
            ),
            style,
        ));
    }
    Paragraph::new(lines).render(
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            height: area.height.min(list_height.max(1) as u16),
            ..area
        },
        frame.buffer_mut(),
    );

    let tail = Rect {
        y: area.y + area.height.saturating_sub(reserved),
        height: reserved,
        ..area
    };
    render_pane_tail(
        frame,
        tail,
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}

/// TTS 页：行（`› 标签：值`）；可执行行以琥珀强调。
pub(super) fn render_tts(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.tts_rows();
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let mut entries: Vec<(Line<'static>, usize)> = Vec::new();
    for row in rows.iter() {
        let selected = row.focused;
        let marker = if selected { "›" } else { " " };
        let style = if selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else if row.action {
            theme::rich_style(theme::ACCENT_WHITE)
        } else {
            menu_text_style()
        };
        let text = if row.value.is_empty() {
            format!("{marker} {}", row.label)
        } else {
            format!("{marker} {}：{}", row.label, row.value)
        };
        entries.push((
            Line::styled(fit(&text, area.width as usize), style),
            row.index,
        ));
    }
    let owners: Vec<Option<usize>> = entries.iter().map(|(_, owner)| Some(*owner)).collect();
    let offset = decision_window_start_owners(&owners, state.tts_focused_index(), list_height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (line, owner) in entries.into_iter().skip(offset).take(list_height) {
        state.record_hit(row_hit(area, lines.len()), HitAction::PaneRow(owner));
        lines.push(line);
    }
    Paragraph::new(lines).render(
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            height: area.height.min(list_height.max(1) as u16),
            ..area
        },
        frame.buffer_mut(),
    );

    let tail = Rect {
        y: area.y + area.height.saturating_sub(reserved),
        height: reserved,
        ..area
    };
    render_pane_tail(
        frame,
        tail,
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}
