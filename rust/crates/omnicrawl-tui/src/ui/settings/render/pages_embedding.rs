use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::draw::{body_text_style, menu_text_style, row_hit};
use super::helpers::{fit, window_offset};
use super::list::render_pane_tail;
use super::pages_decision::decision_window_start_owners;
use super::super::embedding_field_value;
use super::super::hit::HitAction;
use super::super::state::{visible_embedding_fields, EmbeddingChannelKind, SettingsState};

/// 向量模型页：表单态画字段，列表态画向量渠道行。
pub(super) fn render_embedding_models(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    match state.embedding_form() {
        Some(form) => render_embedding_form(frame, area, state, form),
        None => render_embedding_list(frame, area, state, focused),
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

/// 向量渠道列表：两个阶段开关在下、各自一组渠道在各自的开关之下。
///
/// ```text
/// › [ ] 启用嵌入模型
///   [ ] 启用重排序模型
///  嵌入模型
///   [x] OpenAI 向量 [默认]
///       text-embedding-3-small  https://api.openai.com/v1
///  重排序模型
///   [ ] Jina 重排序 [默认]
///       jina-reranker-v2-base-multilingual  https://api.jina.ai/v1
/// ```
pub(super) fn render_embedding_list(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let selected = state.embedding_selected();
    let switch_selected = state.embedding_switch_selected();

    let switches = [
        ("启用嵌入模型", state.embedding_enabled()),
        ("启用重排序模型", state.rerank_enabled()),
    ];
    let mut entries: Vec<(Line<'static>, Option<usize>)> = Vec::new();
    for (row, (label, enabled)) in switches.iter().enumerate() {
        let current = switch_selected && state.embedding_switch_row() == row;
        entries.push((
            Line::styled(
                fit(
                    &format!(
                        "{} [{}] {label}",
                        if current { "›" } else { " " },
                        if *enabled { "x" } else { " " }
                    ),
                    area.width as usize,
                ),
                if current && focused {
                    theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
                } else {
                    menu_text_style()
                },
            ),
            Some(row),
        ));
    }

    // 两组渠道各自一段：分区标题不可选中，行号沿用页面游标（开关区在前，两组随后）。
    let mut absolute = switches.len();
    for kind in [
        EmbeddingChannelKind::Embedding,
        EmbeddingChannelKind::Rerank,
    ] {
        entries.push((
            Line::styled(
                fit(&format!(" {}", kind.section()), area.width as usize),
                theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::BOLD),
            ),
            None,
        ));
        let rows = state.embedding_rows_of(kind);
        if rows.is_empty() {
            entries.push((
                Line::styled(
                    fit(
                        &format!("  尚无{}渠道，按 N 新建第一条。", kind.section()),
                        area.width as usize,
                    ),
                    theme::rich_style(theme::TEXT_MUTED),
                ),
                None,
            ));
        }
        for (index, row) in rows.iter().enumerate() {
            let current = selected == Some((kind, index));
            let line_style = if current && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                menu_text_style()
            };
            let checked = if row.enabled { "x" } else { " " };
            let default_mark = if state.embedding_default_key_of(kind) == row.key {
                " [默认]"
            } else {
                ""
            };
            entries.push((
                Line::styled(
                    format!(
                        "{} [{checked}] {}{default_mark}",
                        if current { "›" } else { " " },
                        row.name
                    ),
                    line_style,
                ),
                Some(absolute),
            ));
            entries.push((
                Line::styled(format!("    {}  {}", row.model, row.base_url), line_style),
                Some(absolute),
            ));
            absolute += 1;
        }
    }

    let cursor = if switch_selected {
        state.embedding_switch_row()
    } else {
        // 分区标题插在中间，游标行号与绘制序号不再一一对应：按归属行号定位窗口。
        selected
            .map(|(kind, index)| {
                switches.len()
                    + match kind {
                        EmbeddingChannelKind::Embedding => index,
                        EmbeddingChannelKind::Rerank => {
                            state.embedding_rows_of(EmbeddingChannelKind::Embedding).len() + index
                        }
                    }
            })
            .unwrap_or(0)
    };
    let owners: Vec<Option<usize>> = entries.iter().map(|(_, owner)| *owner).collect();
    let start = decision_window_start_owners(&owners, cursor, list_height);
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

/// 向量渠道表单：字段一行一个，与决策表单同款（没有枚举候选）。
pub(super) fn render_embedding_form(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    form: super::super::state::EmbeddingFormView<'_>,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, field) in visible_embedding_fields(form.kind).iter().copied().enumerate() {
        state.record_hit(row_hit(area, lines.len()), HitAction::PaneRow(index));
        let is_current = field == form.field;
        let marker = if is_current { "›" } else { " " };
        let editing = is_current && form.input.is_some();
        let value = if editing {
            form.input
                .and_then(|composer| composer.wrapped_lines(area.width.saturating_sub(20)).pop())
                .map(|text| format!("{text}▌"))
                .unwrap_or_default()
        } else {
            embedding_field_value(form.row, field)
        };
        let label = format!("{marker} {}：", field.label());
        lines.push(Line::from(vec![
            Span::styled(label, body_text_style()),
            Span::styled(value, body_text_style()),
        ]));
    }
    if form.is_new {
        lines.push(Line::styled(
            fit("  （新建，Ctrl+S 后写入配置）", area.width as usize),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    let visible = area.height.saturating_sub(4) as usize;
    let offset = window_offset(lines.len(), visible.max(1), 0);
    let shown: Vec<Line<'static>> = lines
        .into_iter()
        .skip(offset)
        .take(visible.max(1))
        .collect();
    Paragraph::new(shown).render(area, frame.buffer_mut());
}
