use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::{menu_text_style, row_hit};
use super::helpers::{fit, window_offset};
use super::list::render_pane_tail;
use super::super::hit::HitAction;
use super::super::state::SettingsState;
use super::MCP_TAIL_RESERVED;

/// MCP 设置页：全局行 / Server 列表 / 编辑器三个子模式。
pub(super) fn render_mcp(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    if state.mcp_editor_title().is_some() {
        render_mcp_editor(frame, area, state, focused);
    } else if state.mcp_servers_view() {
        render_mcp_servers(frame, area, state, focused);
    } else {
        render_mcp_globals(frame, area, state, focused);
    }
}

pub(super) fn render_mcp_tail(frame: &mut Frame, area: Rect, state: &SettingsState) {
    let tail = Rect {
        y: area.y + area.height.saturating_sub(MCP_TAIL_RESERVED),
        height: MCP_TAIL_RESERVED.min(area.height),
        ..area
    };
    render_pane_tail(
        frame,
        tail,
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::ACCENT_WHITE),
    );
}

pub(super) fn render_mcp_globals(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.mcp_rows();
    let reserved = MCP_TAIL_RESERVED.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
    let offset = window_offset(selected, rows.len(), list_height);
    let list_area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        height: area.height.min(list_height as u16),
        ..area
    };
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(list_height)
        .map(|(index, row)| {
            state.record_hit(
                row_hit(list_area, index - offset),
                HitAction::PaneRow(index),
            );
            let text = format!(
                "{}{}：{}",
                if row.selected { "› " } else { "  " },
                row.label,
                row.value
            );
            let style = if row.selected && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                menu_text_style()
            };
            Line::styled(fit(&text, list_area.width as usize), style)
        })
        .collect();
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
    render_mcp_tail(frame, area, state);
}

pub(super) fn render_mcp_servers(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.mcp_server_rows();
    let selected = state.mcp_server_selected();
    let reserved = MCP_TAIL_RESERVED.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let offset = window_offset(selected, rows.len().max(1), list_height);
    let list_area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        height: area.height.min(list_height as u16),
        ..area
    };
    let mut lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(list_height)
        .map(|(index, row)| {
            state.record_hit(
                row_hit(list_area, index - offset),
                HitAction::PaneRow(index),
            );
            let text = format!(
                "{}{}：{} · {} · {}",
                if index == selected { "› " } else { "  " },
                row.name,
                if row.enabled { "启用" } else { "禁用" },
                row.transport,
                row.risk_level
            );
            let style = if index == selected && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                menu_text_style()
            };
            Line::styled(fit(&text, list_area.width as usize), style)
        })
        .collect();
    if rows.is_empty() {
        lines.push(Line::styled(
            fit("（暂无 Server，按 A 添加）", list_area.width as usize),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
    render_mcp_tail(frame, area, state);
}

pub(super) fn render_mcp_editor(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let reserved = MCP_TAIL_RESERVED.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let list_area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        height: area.height.min(list_height as u16),
        ..area
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(title) = state.mcp_editor_title() {
        lines.push(Line::styled(
            fit(&title, list_area.width as usize),
            theme::rich_style(theme::ACCENT_WHITE).add_modifier(Modifier::BOLD),
        ));
    }
    for (index, row) in state.mcp_editor_rows().into_iter().enumerate() {
        state.record_hit(row_hit(list_area, lines.len()), HitAction::PaneRow(index));
        let text = format!(
            "{}{}：{}",
            if row.focused { "› " } else { "  " },
            row.label,
            row.value
        );
        let style = if row.focused && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        };
        lines.push(Line::styled(fit(&text, list_area.width as usize), style));
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
    render_mcp_tail(frame, area, state);
}
