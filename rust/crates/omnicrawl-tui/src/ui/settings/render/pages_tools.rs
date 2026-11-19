use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::{menu_text_style, row_hit};
use super::helpers::window_offset;
use super::list::render_pane_tail;
use super::super::hit::HitAction;
use super::super::state::{
    SettingsState, ToolSwitchRow, TOOLS_APPROVAL_LABEL, TOOLS_APPROVAL_ROW,
    TOOLS_SWITCH_ROW_OFFSET,
};

pub(super) fn render_tools(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.tool_rows();
    // 底部 3 行留给状态（1 空行 + 2 行文本）与提示（1 行）。
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let offset = window_offset(state.tool_selected(), state.tool_row_count(), list_height);
    // `.tool-pane-row { padding: 0 1 }`：整行再缩进 1 格。
    let list_area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        height: area.height.min(list_height as u16),
        ..area
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    // 首行固定是「工具调用审查」（对映 Python 工具设置分节的「审批模式」分节）。
    if offset == 0 {
        state.record_hit(
            row_hit(list_area, 0),
            HitAction::PaneRow(TOOLS_APPROVAL_ROW),
        );
        lines.push(Line::from(vec![Span::styled(
            approval_row_text(
                state,
                state.tool_selected() == TOOLS_APPROVAL_ROW && focused,
            ),
            if state.tool_selected() == TOOLS_APPROVAL_ROW && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                menu_text_style()
            },
        )]));
    }
    // 第二行起是逐工具开关（联网搜索已抽成左栏的独立设置页）。
    let skip = offset.saturating_sub(TOOLS_SWITCH_ROW_OFFSET);
    lines.extend(
        rows.iter()
            .enumerate()
            .skip(skip)
            .take(list_height.saturating_sub(lines.len()))
            .map(|(index, row)| {
                state.record_hit(
                    row_hit(list_area, index + TOOLS_SWITCH_ROW_OFFSET - offset),
                    HitAction::PaneRow(index + TOOLS_SWITCH_ROW_OFFSET),
                );
                Line::from(tool_row_spans(
                    row,
                    index + TOOLS_SWITCH_ROW_OFFSET == state.tool_selected() && focused,
                ))
            }),
    );
    Paragraph::new(lines).render(list_area, frame.buffer_mut());

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
        theme::rich_style(theme::ACCENT_WHITE),
    );
}

/// 工具调用审查行的文本：`› 工具调用审查：自动审查`。
pub(super) fn approval_row_text(state: &SettingsState, selected: bool) -> String {
    format!(
        "{} {}：{}",
        if selected { "›" } else { " " },
        TOOLS_APPROVAL_LABEL,
        state.tool_approval_value()
    )
}

/// 工具开关行的文本：`› 名称：已启用`。
pub(super) fn tool_row_spans(row: &ToolSwitchRow, selected: bool) -> Vec<Span<'static>> {
    let state_text = if row.enabled {
        "已启用"
    } else {
        "已关闭"
    };
    let text = format!(
        "{} {}：{state_text}",
        if selected { "›" } else { " " },
        row.name
    );
    let style = if selected {
        theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
    } else {
        menu_text_style()
    };
    vec![Span::styled(text, style)]
}
