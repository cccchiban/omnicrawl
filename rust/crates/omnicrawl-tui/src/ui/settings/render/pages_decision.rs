use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::display_width;
use crate::ui::fullscreen::terminal::theme;

use super::draw::{menu_text_style, progress_bar_text, progress_phase, row_hit};
use super::forms::render_decision_form;
use super::helpers::fit;
use super::list::render_pane_tail;
use super::super::hit::HitAction;
use super::super::state::{
    SettingsState, DECISION_LOCAL_SECTION, DECISION_PROGRESS_BAR_WIDTH, DECISION_SWITCH_SECTION,
};

/// 结构化决策模型页：表单态画字段，列表态画决策渠道行。
pub(super) fn render_decision_models(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    match state.decision_form() {
        Some(form) => render_decision_form(frame, area, state, form),
        None => render_decision_list(frame, area, state, focused),
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

/// 决策渠道列表：渠道两行行式（与渠道页同款），下方接「功能开关」分区单行开关。
///
/// ```text
/// › [x] Jev 主渠道 [默认]
///     jev  jev-latest  https://api.typesafe.ai
///
///  功能开关
///   [x] 工具调用审查使用决策模型
/// ```
pub(super) fn render_decision_list(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.decision_rows();
    let switches = state.decision_switch_rows();
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let selected = state.decision_selected();

    // 先把整页排成「行 + 归属行号（分区标题无归属）」的序列，再按选中行的跨度开窗口——
    // 渠道占 2 行、开关占 1 行，窗口长度按行算会让选中行半截露在框外。
    let mut entries: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    if rows.is_empty() {
        entries.push((
            Line::styled(
                fit("尚无决策渠道，按 N 新建第一条渠道。", area.width as usize),
                theme::rich_style(theme::TEXT_MUTED),
            ),
            None,
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        let line_style = if index == selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            menu_text_style()
        };
        let checked = if row.enabled { "x" } else { " " };
        let default_mark = if state.decision_default_key() == row.key {
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
                format!("    {}  {}  {}", row.mode, row.model, row.base_url),
                line_style,
            ),
            Some(index),
        ));
    }
    if !switches.is_empty() {
        entries.push((
            Line::styled(
                fit(&format!(" {DECISION_SWITCH_SECTION}"), area.width as usize),
                theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::BOLD),
            ),
            None,
        ));
        for (offset, row) in switches.iter().enumerate() {
            let absolute = rows.len() + offset;
            let line_style = if absolute == selected && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                menu_text_style()
            };
            entries.push((
                Line::styled(
                    fit(
                        &format!(
                            "{} [{}] {}",
                            if absolute == selected { "›" } else { " " },
                            if row.enabled { "x" } else { " " },
                            row.label
                        ),
                        area.width as usize,
                    ),
                    line_style,
                ),
                Some(absolute),
            ));
        }
    }

    // 自部署分区：尺寸/设备/已下载状态 + 环境与服务状态 + 动作行。
    let local_rows = state.decision_local_rows();
    if !local_rows.is_empty() {
        let base = rows.len() + switches.len();
        entries.push((
            Line::styled(
                fit(&format!(" {DECISION_LOCAL_SECTION}"), area.width as usize),
                theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::BOLD),
            ),
            None,
        ));
        for (offset, row) in local_rows.iter().enumerate() {
            let absolute = base + offset;
            let line_style = if absolute == selected && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else if row.action {
                theme::rich_style(theme::ACCENT_WHITE)
            } else {
                menu_text_style()
            };
            let text = if row.value.is_empty() {
                format!(
                    "{} {}",
                    if absolute == selected { "›" } else { " " },
                    row.label
                )
            } else {
                format!(
                    "{} {}：{}",
                    if absolute == selected { "›" } else { " " },
                    row.label,
                    row.value
                )
            };
            entries.push((
                Line::styled(fit(&text, area.width as usize), line_style),
                Some(absolute),
            ));
            // 进度条独占一行画在进度行下方：行宽受限时按比例吃满可用列。
            if row.bar {
                let detail = if row.progress_detail.is_empty() {
                    String::new()
                } else {
                    format!(" {}", row.progress_detail)
                };
                let bar_width = (area.width as usize)
                    .saturating_sub(display_width(&detail))
                    .min(DECISION_PROGRESS_BAR_WIDTH);
                let bar = progress_bar_text(row.progress, bar_width, progress_phase());
                entries.push((
                    Line::styled(
                        fit(&format!("    {bar}{detail}"), area.width as usize),
                        theme::rich_style(theme::TEXT_MUTED),
                    ),
                    Some(absolute),
                ));
            }
        }
    }

    let start = decision_window_start(&entries, selected, list_height);
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
pub(super) fn decision_window_start(
    entries: &[(Line<'static>, Option<usize>)],
    selected: usize,
    height: usize,
) -> usize {
    let owners: Vec<Option<usize>> = entries.iter().map(|(_, owner)| *owner).collect();
    decision_window_start_owners(&owners, selected, height)
}

/// [`decision_window_start`] 的按归属行号版本：模型管理页只需要行号，不必造出整行。
pub(super) fn decision_window_start_owners(owners: &[Option<usize>], selected: usize, height: usize) -> usize {
    if owners.is_empty() || height == 0 {
        return 0;
    }
    // 选中条目在行序列里的起止（含多行条目）。
    let first = owners
        .iter()
        .position(|owner| *owner == Some(selected))
        .unwrap_or(0);
    let last = owners
        .iter()
        .rposition(|owner| *owner == Some(selected))
        .unwrap_or(first);
    let needed = last - first + 1;
    if needed >= height {
        return first;
    }
    // 选中条目之后能塞下的行数：优先让它下方的内容也可见。
    let tail = height - needed;
    first.saturating_sub(tail.min(first))
}
