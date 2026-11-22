use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::{body_text_style, menu_text_style, row_hit};
use super::helpers::{fit, window_offset};
use super::list::render_pane_tail;
use super::pages_model::{model_owner_lines, model_window_start};
use super::super::hit::HitAction;
use super::super::picker::window_bounds;
use super::super::state::{
    visible_decision_fields, ChannelField, ChannelFormView, DecisionField, DecisionFormView,
    DropdownField, ModelParamRow, Pane, SettingsState,
};
use super::super::{channel_field_value, decision_field_value};
use super::FORM_TAIL_HEIGHT;

/// 表单页：一行一个字段（`› 标签：值 ▼`），候选展开时用公共浮层。
pub(super) fn render_form(frame: &mut Frame, area: Rect, state: &SettingsState, _focused: bool) {
    let rows_area = form_rows_area(area, 0);
    let rows = state.form_rows();
    let visible = rows_area.height.max(1) as usize;
    let offset = window_offset(
        state.form_focus_position(),
        state.form_field_count(),
        visible,
    );
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, row)| {
            state.record_hit(
                row_hit(rows_area, index - offset),
                HitAction::PaneRow(index),
            );
            let marker = if row.focused { "›" } else { " " };
            let value = if row.has_menu {
                format!("{} ▼", row.value)
            } else {
                row.value.clone()
            };
            let label_style = body_text_style();
            Line::from(vec![
                Span::styled(format!("{marker} {}：", row.label), label_style),
                Span::styled(value, body_text_style()),
            ])
        })
        .collect();
    Paragraph::new(lines).render(
        Rect {
            x: rows_area.x + 1,
            width: rows_area.width.saturating_sub(1),
            height: rows_area.height.min((visible as u16).max(1)),
            ..rows_area
        },
        frame.buffer_mut(),
    );

    let tail = Rect {
        y: area.y + area.height.saturating_sub(FORM_TAIL_HEIGHT),
        height: area.height.min(FORM_TAIL_HEIGHT),
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

/// 渠道表单：一行一个字段；枚举字段带 `▼`，编辑中的文本字段带光标块。
///
/// 带 `state` 是为了登记命中区（表单自己只是只读视图，不携带任何状态）。
pub(super) fn render_channel_form(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    form: ChannelFormView<'_>,
    _focused: bool,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, field) in ChannelField::ORDER.iter().copied().enumerate() {
        state.record_hit(row_hit(area, lines.len()), HitAction::PaneRow(index));
        let is_current = field == form.field;
        let marker = if is_current { "›" } else { " " };
        let editing = is_current && form.input.is_some();
        let value = match (editing, form.dropdown) {
            // 输入态显示编辑缓冲的最后一行（光标在末尾，长值也能看到正在打的字）。
            (true, _) => form
                .input
                .and_then(|composer| composer.wrapped_lines(area.width.saturating_sub(20)).pop())
                .map(|text| format!("{text}▌"))
                .unwrap_or_default(),
            (false, Some(dropdown)) if dropdown.field == field => {
                format!("{} ▼（↑↓ 选择）", channel_field_value(form.row, field))
            }
            (false, _) => {
                // 模型 ID 也是可展开的（自动检测候选），与两个枚举字段一样带 `▼`。
                let suffix = if matches!(
                    field,
                    ChannelField::Provider | ChannelField::Protocol | ChannelField::ModelId
                ) {
                    " ▼"
                } else {
                    ""
                };
                format!("{}{suffix}", channel_field_value(form.row, field))
            }
        };
        let label = format!("{marker} {}：", field.label());
        // 右侧设置项名称统一灰色（用户要求）：焦点不再把名称染成琥珀色。
        let label_style = body_text_style();
        lines.push(Line::from(vec![
            Span::styled(label, label_style),
            Span::styled(value, body_text_style()),
        ]));
    }
    let visible = area.height.saturating_sub(4) as usize;
    // 展开的候选排在表单下面（内联列表，不做 Python 的浮层下拉框）。
    if let Some(dropdown) = form.dropdown {
        // 字段行、候选之间的空行与「新渠道」提示行先占掉，剩下的才是候选列表的高度；
        // 连一行候选都放不下时不展开，免得只剩一个省略行。
        let reserved = lines.len() + 1 + usize::from(form.is_new);
        let available = visible.max(1).saturating_sub(reserved);
        if available > 0 {
            lines.push(Line::raw(""));
            // 窗口固定占满可用行；放得下两行省略提示时才画前后提示（与压缩页选择器同款）。
            let options = dropdown.options.len();
            let show_ellipsis = options > available && available >= 3;
            let window_size = if show_ellipsis {
                available.saturating_sub(2).max(1)
            } else {
                available
            };
            let (start, end) = window_bounds(options, dropdown.selected, window_size);
            if show_ellipsis && start > 0 {
                lines.push(Line::styled(
                    fit(&format!("  ... 前面 {start} 个"), area.width as usize),
                    theme::rich_style(theme::TEXT_MUTED),
                ));
            }
            for (index, option) in dropdown.options.iter().enumerate().take(end).skip(start) {
                state.record_hit(row_hit(area, lines.len()), HitAction::Option(index));
                let highlighted = index == dropdown.selected;
                let style = if highlighted {
                    Style::default().bg(Color::Yellow)
                } else {
                    menu_text_style()
                };
                lines.push(Line::styled(
                    fit(&format!("  {option}"), area.width as usize),
                    style,
                ));
            }
            if show_ellipsis && end < options {
                lines.push(Line::styled(
                    fit(
                        &format!("  ... 后面 {} 个", options - end),
                        area.width as usize,
                    ),
                    theme::rich_style(theme::TEXT_MUTED),
                ));
            }
        }
    }
    if form.is_new {
        lines.push(Line::styled(
            "（新渠道，Ctrl+S 保存后写盘）".to_string(),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    lines.truncate(visible.max(1));
    Paragraph::new(lines).render(
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            height: area.height.min(visible.max(1) as u16),
            ..area
        },
        frame.buffer_mut(),
    );
}

/// 决策渠道表单：一行一个字段；枚举字段带 `▼`，编辑中的文本字段带光标块。
pub(super) fn render_decision_form(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    form: DecisionFormView<'_>,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, field) in visible_decision_fields(&form.row.mode)
        .into_iter()
        .enumerate()
    {
        state.record_hit(row_hit(area, lines.len()), HitAction::PaneRow(index));
        let is_current = field == form.field;
        let marker = if is_current { "›" } else { " " };
        let editing = is_current && form.input.is_some();
        let value = match (editing, form.dropdown) {
            (true, _) => form
                .input
                .and_then(|composer| composer.wrapped_lines(area.width.saturating_sub(20)).pop())
                .map(|text| format!("{text}▌"))
                .unwrap_or_default(),
            (false, Some(dropdown)) if dropdown.field == field => {
                format!("{} ▼（↑↓ 选择）", decision_field_value(form.row, field))
            }
            (false, _) => {
                let suffix = if field == DecisionField::Mode {
                    " ▼"
                } else {
                    ""
                };
                format!("{}{suffix}", decision_field_value(form.row, field))
            }
        };
        let label = format!("{marker} {}：", field.label());
        let label_style = body_text_style();
        lines.push(Line::from(vec![
            Span::styled(label, label_style),
            Span::styled(value, body_text_style()),
        ]));
    }
    let visible = area.height.saturating_sub(4) as usize;
    if let Some(dropdown) = form.dropdown {
        let reserved = lines.len() + 1 + usize::from(form.is_new);
        let available = visible.max(1).saturating_sub(reserved);
        if available > 0 {
            lines.push(Line::raw(""));
            let options = dropdown.options.len();
            let show_ellipsis = options > available && available >= 3;
            let window_size = if show_ellipsis {
                available.saturating_sub(2).max(1)
            } else {
                available
            };
            let (start, end) = window_bounds(options, dropdown.selected, window_size);
            if show_ellipsis && start > 0 {
                lines.push(Line::styled(
                    fit(&format!("  ... 前面 {start} 个"), area.width as usize),
                    theme::rich_style(theme::TEXT_MUTED),
                ));
            }
            for (index, option) in dropdown.options.iter().enumerate().take(end).skip(start) {
                state.record_hit(row_hit(area, lines.len()), HitAction::Option(index));
                let highlighted = index == dropdown.selected;
                let style = if highlighted {
                    Style::default().bg(Color::Yellow)
                } else {
                    menu_text_style()
                };
                lines.push(Line::styled(
                    fit(&format!("  {option}"), area.width as usize),
                    style,
                ));
            }
            if show_ellipsis && end < options {
                lines.push(Line::styled(
                    fit(
                        &format!("  ... 后面 {} 个", options - end),
                        area.width as usize,
                    ),
                    theme::rich_style(theme::TEXT_MUTED),
                ));
            }
        }
    }
    if form.is_new {
        lines.push(Line::styled(
            "（新渠道，Ctrl+S 保存后写盘）".to_string(),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    lines.truncate(visible.max(1));
    Paragraph::new(lines).render(
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            height: area.height.min(visible.max(1) as u16),
            ..area
        },
        frame.buffer_mut(),
    );
}

/// 展开的候选浮层的顶行；面板与该字段对不上时返回 `None`。
///
/// 浮层紧贴被展开的那个字段：模型管理页贴那一行参数的下沿，表单页贴那一行字段的下沿。
pub(super) fn dropdown_overlay_y(
    pane: Pane,
    field: DropdownField,
    area: Rect,
    state: &SettingsState,
) -> Option<u16> {
    match (pane, field) {
        // 模型管理页：渠道两行一条、分区参数一行一条，先算出选中行在列表里的显示行号。
        (Pane::ModelManagement, DropdownField::ModelParam(row)) => {
            let height = area.height.saturating_sub(4) as usize;
            let owner = state
                .channel_rows()
                .len()
                .checked_add(ModelParamRow::ORDER.iter().position(|item| *item == row)?)?;
            let owners = model_owner_lines(state);
            let offset = model_window_start(&owners, state.channel_selected(), height);
            let line = owners
                .iter()
                .position(|item| *item == Some(owner))?
                .saturating_sub(offset);
            Some(area.y + (line as u16) + 1)
        }
        // 视觉页：头部两行之后是列表，`A` 的候选贴在列表上方。
        (Pane::Vision, DropdownField::VisionAdd) => Some(area.y + 3),
        (Pane::Suggestion, DropdownField::ResidentSkill) => Some(area.y + 2),
        // 表单页一行一个字段：字段表可能滚动，先减掉窗口起点。
        (Pane::Form(_), DropdownField::Form(index)) => {
            let offset = form_window_offset(state, area);
            Some(area.y + (index.checked_sub(offset)? as u16) + 1)
        }
        _ => None,
    }
}

/// 表单页字段列表占的区域：扣掉底部状态/提示。
pub(super) fn form_rows_area(area: Rect, picker_height: u16) -> Rect {
    Rect {
        height: area
            .height
            .saturating_sub(picker_height + FORM_TAIL_HEIGHT.min(area.height)),
        ..area
    }
}

/// 表单页字段表的窗口起点（可见行号）：聚焦字段始终可见。
pub(super) fn form_window_offset(state: &SettingsState, area: Rect) -> usize {
    let rows = form_rows_area(area, 0);
    window_offset(
        state.form_focus_position(),
        state.form_field_count(),
        rows.height.max(1) as usize,
    )
}
