use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::{box_inner, draw_box, menu_text_style, row_hit};
use super::forms::render_form;
use super::helpers::{fit, window_offset};
use super::overlay::{render_dropdown_overlay, render_pending, render_suggestion};
use super::pages_decision::render_decision_models;
use super::pages_embedding::render_embedding_models;
use super::pages_library::{render_knowledge, render_memory, render_sessions};
use super::pages_mcp::render_mcp;
use super::pages_misc::{render_subagents, render_tts, render_vision};
use super::pages_model::render_model_management;
use super::pages_tools::render_tools;
use super::super::hit::HitAction;
use super::super::state::{Focus, Pane, SettingsState};
use super::super::row_label;
use super::COLUMN_PADDING;

pub(super) fn render_rows(frame: &mut Frame, area: Rect, state: &SettingsState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = state.focus() == Focus::List;
    draw_box(frame, area, focused, None);
    // 左栏框体自带 1 格内边距（对映 CSS 的 `#settings-left-box { padding: 1 }`）。
    let inner = box_inner(area, 1);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let keys = state.rows();
    let visible = inner.height as usize;
    let offset = window_offset(state.selected(), keys.len(), visible);
    let lines: Vec<Line<'static>> = keys
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, key)| {
            state.record_hit(row_hit(inner, index - offset), HitAction::Row(index));
            let style = if index == state.selected() && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                menu_text_style()
            };
            Line::styled(fit(&row_label(key), inner.width as usize), style)
        })
        .collect();
    Paragraph::new(lines).render(inner, frame.buffer_mut());
}

pub(super) fn render_pane(frame: &mut Frame, area: Rect, state: &SettingsState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = state.focus() == Focus::Pane;
    draw_box(frame, area, focused, Some(state.pane_hint()));
    let inner = box_inner(area, COLUMN_PADDING);
    if inner.width < 3 || inner.height < 2 {
        return;
    }
    // 标题：框内缩进 2 格（对映 CSS 的 `margin: 1 2 0 2`）、加粗白字。
    let title_area = Rect {
        x: inner.x + 1,
        width: inner.width - 1,
        height: 1,
        ..inner
    };
    let title = Line::styled(
        fit(&state.title(), title_area.width as usize),
        theme::rich_style(theme::ACCENT_WHITE).add_modifier(Modifier::BOLD),
    );
    Paragraph::new(title).render(title_area, frame.buffer_mut());

    let body = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    };
    if body.height == 0 {
        return;
    }
    match state.pane() {
        Pane::Tools => render_tools(frame, body, state, focused),
        Pane::Mcp => render_mcp(frame, body, state, focused),
        Pane::Subagents => render_subagents(frame, body, state, focused),
        Pane::Suggestion => render_suggestion(frame, body, state, focused),
        Pane::Vision => render_vision(frame, body, state, focused),
        Pane::ModelManagement => render_model_management(frame, body, state, focused),
        Pane::DecisionModels => render_decision_models(frame, body, state, focused),
        Pane::EmbeddingModels => render_embedding_models(frame, body, state, focused),
        Pane::Form(_) => render_form(frame, body, state, focused),
        Pane::Tts => render_tts(frame, body, state, focused),
        Pane::Sessions => render_sessions(frame, body, state, focused),
        Pane::Memory => render_memory(frame, body, state, focused),
        Pane::Knowledge => render_knowledge(frame, body, state, focused),
        Pane::Pending => render_pending(frame, body),
    }
    render_dropdown_overlay(frame, body, state, focused);
}

/// 面板底部的状态行（1 空行 + 最多 2 行文本）。
///
/// 键位提示已画在面板**下边框**上（见 [`draw_box`]），这里不再重复；保留尾部两行预算
/// （[`FORM_TAIL_HEIGHT`]）不变，多出来的那行留空——它同时是状态行可折到 2 行的依据。
pub(super) fn render_pane_tail(frame: &mut Frame, area: Rect, status: &str, _hint: &str, _hint_style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let status_height = area.height.saturating_sub(1).min(2);
    let status_area = Rect {
        y: area.y + 1,
        height: status_height,
        ..area
    };
    if !status.is_empty() && status_height > 0 {
        let lines: Vec<Line<'static>> = crate::ui::wrap_display(status, width)
            .into_iter()
            .take(status_height as usize)
            .map(|line| Line::styled(line, theme::rich_style(theme::ACCENT_WHITE)))
            .collect();
        Paragraph::new(lines).render(status_area, frame.buffer_mut());
    }
}
