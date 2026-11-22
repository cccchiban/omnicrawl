use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::{body_text_style, menu_text_style, row_hit};
use super::forms::dropdown_overlay_y;
use super::helpers::{fit, window_offset};
use super::list::render_pane_tail;
use super::super::hit::HitAction;
use super::super::state::{SettingsState, SUGGESTION_DETAIL_ROWS};
use super::DROPDOWN_MAX_ROWS;

/// 展开的候选浮层：白框 + 琥珀高亮，浮在被覆盖的内容之上。
pub(super) fn render_dropdown_overlay(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let Some(dropdown) = state.dropdown() else {
        return;
    };
    if !focused || area.width == 0 || area.height == 0 {
        return;
    }
    let options = state.dropdown_options();
    if options.is_empty() {
        return;
    }
    // 浮层紧贴「被展开的那个字段」的下沿；面板与该字段对不上时不画。
    let Some(overlay_y) = dropdown_overlay_y(state.pane(), dropdown.field, area, state) else {
        return;
    };
    let bottom = area.y + area.height;
    if overlay_y + 2 > bottom {
        return;
    }
    let rows = options.len().min(DROPDOWN_MAX_ROWS) as u16;
    let overlay = Rect {
        x: area.x,
        y: overlay_y,
        width: area.width,
        height: (rows + 2).min(bottom - overlay_y),
    };
    let block = Block::bordered().border_style(theme::rich_style(theme::ACCENT_WHITE));
    let inner = block.inner(overlay);
    // 浮层浮在面板内容之上：先擦掉自己覆盖的区域，否则被盖住的那几行会从候选项右侧
    // 透出来（「Exa API Key：（未配置）」与候选挤在同一行）。
    frame.render_widget(Clear, overlay);
    frame.render_widget(block, overlay);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let visible = inner.height as usize;
    let offset = window_offset(dropdown.selected, options.len(), visible);
    let lines: Vec<Line<'static>> = options
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, (label, _))| {
            state.record_hit(row_hit(inner, index - offset), HitAction::Option(index));
            // 高亮项：琥珀底色 + 终端默认前景（对映 `.option-list--option-highlighted`）。
            let style = if index == dropdown.selected {
                Style::default()
                    .fg(theme::rich_style(theme::TEXT_ON_ACCENT)
                        .fg
                        .unwrap_or(Color::Reset))
                    .bg(Color::Yellow)
            } else {
                menu_text_style()
            };
            Line::styled(fit(&format!(" {label}"), inner.width as usize), style)
        })
        .collect();
    Paragraph::new(lines).render(inner, frame.buffer_mut());
}

/// Skills 管理页：建议开关、常驻选择与本机 Skill 描述列表。
///
/// ```text
/// › 建议 Skill：开启
///   常驻 Skill：无
///
///   Skills
///      agent-browser-cli            user
///   ›  kimi-docx                    user
/// ```
pub(super) fn render_suggestion(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    if state.suggestion_detail_open() {
        render_suggestion_detail(frame, area, state, focused);
        return;
    }
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let list_area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        height: area.height.min(list_height.max(1) as u16),
        ..area
    };
    let width = list_area.width as usize;
    let skills = state.suggestion_skills();
    let selected = state.suggestion_selected();
    let mut lines: Vec<Line<'static>> = Vec::new();
    // 开关行与「Skills」分区标题固定在上方，滚动只作用在 Skill 行上。
    let value = if state.suggestion_enabled() {
        "开启"
    } else {
        "关闭"
    };
    state.record_hit(row_hit(list_area, 0), HitAction::PaneRow(0));
    lines.push(Line::styled(
        fit(
            &format!(
                "{} {}：{value}",
                if selected == 0 { "›" } else { " " },
                omnicrawl_config::features::suggestion::SUGGESTION_SKILLS_LABEL
            ),
            width,
        ),
        if selected == 0 && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        },
    ));
    if list_area.height > 1 {
        state.record_hit(row_hit(list_area, 1), HitAction::PaneRow(1));
    }
    lines.push(Line::styled(
        fit(&format!("{} 常驻 Skill：{}",
            if selected == 1 { "›" } else { " " },
            state.resident_skill_name().unwrap_or("无")), width),
        if selected == 1 && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        },
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        fit("  Skills", width),
        theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::BOLD),
    ));
    let body_height = list_height.saturating_sub(lines.len());
    let selected_skill = selected.saturating_sub(2);
    let offset = window_offset(selected_skill, skills.len(), body_height);
    for (index, row) in skills.iter().enumerate().skip(offset).take(body_height) {
        let current = index + 2 == selected;
        state.record_hit(
            row_hit(list_area, lines.len()),
            HitAction::PaneRow(index + 2),
        );
        lines.push(Line::styled(
            fit(
                &format!(
                    "{}  {}  {}",
                    if current { "›" } else { " " },
                    row.name,
                    row.scope
                ),
                width,
            ),
            if current && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                menu_text_style()
            },
        ));
    }
    if skills.is_empty() && body_height > 0 {
        lines.push(Line::styled(
            fit("    未发现 Skill", width),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
    render_pane_tail(
        frame,
        Rect {
            y: area.y + area.height.saturating_sub(reserved),
            height: reserved,
            ..area
        },
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}

/// Skill 详情：名称与作用域一行，描述折行铺开，只露 [`SUGGESTION_DETAIL_ROWS`] 行，其余靠 ↑↓ 滚。
pub(super) fn render_suggestion_detail(frame: &mut Frame, area: Rect, state: &SettingsState, _focused: bool) {
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let list_area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        height: area.height.min(list_height.max(1) as u16),
        ..area
    };
    let width = list_area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(skill) = state.suggestion_detail_skill() {
        lines.push(Line::styled(
            fit(&format!("{}  {}", skill.name, skill.scope), width),
            theme::rich_style(theme::ACCENT_WHITE).add_modifier(Modifier::BOLD),
        ));
        let description = if skill.description.is_empty() {
            "（没有描述）"
        } else {
            skill.description.as_str()
        };
        let wrapped = crate::ui::wrap_display(description, width);
        state.set_suggestion_detail_total(wrapped.len());
        let start = state
            .suggestion_detail_scroll()
            .min(wrapped.len().saturating_sub(1));
        for row in wrapped.iter().skip(start).take(SUGGESTION_DETAIL_ROWS) {
            lines.push(Line::styled(fit(row, width), body_text_style()));
        }
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
    render_pane_tail(
        frame,
        Rect {
            y: area.y + area.height.saturating_sub(reserved),
            height: reserved,
            ..area
        },
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}

/// 未知一级项：一级表与面板表不同步时的兜底文案（正常构建不会出现）。
pub(super) fn render_pending(frame: &mut Frame, area: Rect) {
    let text = "该设置项没有对应的面板。";
    let line = Line::styled(
        fit(text, area.width as usize),
        theme::rich_style(theme::TEXT_MUTED),
    );
    Paragraph::new(line).render(Rect { height: 1, ..area }, frame.buffer_mut());
}
