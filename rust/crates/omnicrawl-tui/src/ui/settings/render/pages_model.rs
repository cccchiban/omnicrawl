use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;
use omnicrawl_config::models::channels::{protocol_label, provider_label};

use super::draw::{menu_text_style, row_hit};
use super::forms::render_channel_form;
use super::helpers::fit;
use super::list::render_pane_tail;
use super::pages_decision::decision_window_start_owners;
use super::super::hit::HitAction;
use super::super::state::{ModelParamRow, SettingsState, MODEL_PARAM_SECTION};

/// 模型管理页：表单态画渠道字段，列表态画「渠道列表 + 模型与参数分区」。
pub(super) fn render_model_management(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    match state.channel_form() {
        Some(form) => render_channel_form(frame, area, state, form, focused),
        None => render_model_list(frame, area, state, focused),
    }
    let tail = Rect {
        y: area.y + area.height.saturating_sub(4),
        height: area.height.min(4),
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

/// 模型管理页的列表态：渠道两行行式 + 「模型与参数」分区单行。
///
/// ```text
/// › [x] 渠道名 [默认]
///     OpenAI / Chat Completions  gpt-5.2  https://api.example.com/v1
///
///  模型与参数
///   › 当前模型渠道：主渠道
///     上下文长度：128K
///     上下文阈值：80%
///     推理强度：中
/// ```
pub(super) fn render_model_list(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.channel_rows();
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let selected = state.channel_selected();
    let default_key = state.channel_default_key();

    // 整页先排成「行 + 归属行号（分区标题行无归属）」的序列，再按选中行的跨度开窗口——
    // 渠道占 2 行，参数占 1 行，窗口长度按行算会让选中行半截露在框外。
    let mut entries: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let line_style = if index == selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        };
        let checked = if row.enabled { "x" } else { " " };
        let default_mark = if row.key == default_key {
            " [默认]"
        } else {
            ""
        };
        entries.push((
            Line::styled(
                format!(
                    "{} [{checked}] {}{default_mark}",
                    if index == selected { "›" } else { " " },
                    row.name
                ),
                line_style,
            ),
            Some(index),
        ));
        entries.push((
            Line::styled(
                format!(
                    "    {} / {}  {}  {}",
                    provider_label(&row.provider),
                    protocol_label(&row.protocol),
                    row.model_id,
                    row.base_url
                ),
                line_style,
            ),
            Some(index),
        ));
    }
    if rows.is_empty() {
        entries.push((
            Line::styled(
                fit("尚无渠道，按 N 新建第一条渠道。", area.width as usize),
                theme::rich_style(theme::TEXT_MUTED),
            ),
            None,
        ));
    }

    // 「模型与参数」分区：当前模型渠道 + 上下文长度/阈值 + 推理强度，选中即改。
    entries.push((
        Line::styled(
            fit(&format!(" {MODEL_PARAM_SECTION}"), area.width as usize),
            theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::BOLD),
        ),
        None,
    ));
    for (offset, (row, value, is_selected)) in state.model_param_rows().into_iter().enumerate() {
        let absolute = rows.len() + offset;
        let line_style = if is_selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        };
        entries.push((
            Line::styled(
                fit(
                    &format!(
                        "{} {}：{}",
                        if is_selected { "›" } else { " " },
                        row.label(),
                        value
                    ),
                    area.width as usize,
                ),
                line_style,
            ),
            Some(absolute),
        ));
    }

    let owners: Vec<Option<usize>> = entries.iter().map(|(_, owner)| *owner).collect();
    let start = model_window_start(&owners, selected, list_height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (line, owner) in entries.into_iter().skip(start).take(list_height) {
        if let Some(owner) = owner {
            state.record_hit(row_hit(area, lines.len()), HitAction::PaneRow(owner));
        }
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
}

/// 窗口起点：选中行属于哪个条目，就把它整个条目的行都算进可见高度。
pub(super) fn model_window_start(owners: &[Option<usize>], selected: usize, height: usize) -> usize {
    decision_window_start_owners(owners, selected, height)
}

/// 模型管理页列表态每行的归属行号（分区标题行为 `None`）；只用于算浮层落点。
pub(super) fn model_owner_lines(state: &SettingsState) -> Vec<Option<usize>> {
    let rows = state.channel_rows().len();
    let mut owners: Vec<Option<usize>> = (0..rows)
        .flat_map(|index| [Some(index), Some(index)])
        .collect();
    // 没有渠道时列表先画一行占位提示（与 `render_model_list` 同形）。
    if rows == 0 {
        owners.push(None);
    }
    owners.push(None);
    owners.extend((0..ModelParamRow::ORDER.len()).map(|offset| Some(rows + offset)));
    owners
}
