//! 设置面板的渲染：全屏两区、准星焦点边框、候选下拉与状态行。
//!
//! 视觉对映 Python 的 `SettingsScreen.CSS` 与 `terminal_select_css`：
//! 左右两栏都是圆角框（`round $terminal-border-strong`），焦点所在的一栏换成
//! 准星边框（四角 `⇘ ⇙ ⇗ ⇖`，`$terminal-amber`）；右侧标题在框内缩进 2 格、
//! 加粗；折叠态下拉是 3 行细边框，聚焦/展开时换成白色粗边框；展开的候选列表
//! 是白框浮层，高亮项用琥珀底色。

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use ratatui::Frame;

use super::hit::HitAction;
use super::state::{
    ChannelField, ChannelFormView, ContextField, DropdownField, Focus, Pane, SettingsState,
    ToolSwitchRow,
};
use super::{channel_field_value, row_label};
use crate::ui::fullscreen::terminal::theme;
use helpers::{fit, left_column_width, window_offset};

/// 左栏的期望宽度（对映 CSS 的 `width: 30`）。
const LEFT_COLUMN_WIDTH: u16 = 30;
/// 左栏的最小宽度（对映 CSS 的 `min-width: 24`）。
const LEFT_COLUMN_MIN_WIDTH: u16 = 24;
/// 右栏至少要留出的宽度，窄终端下左栏先收缩。
const MIN_RIGHT_WIDTH: u16 = 20;
/// 两栏内边距（对映 CSS 的 `padding: 0 1`）。
const COLUMN_PADDING: u16 = 1;
/// 展开的候选浮层最多显示这么多行（对映 CSS 的 `max-height: 12`，另有 2 行边框）。
const DROPDOWN_MAX_ROWS: usize = 12;
/// 上下文页一个字段占的行数：1 空行 + 1 标签行 + 3 行下拉框。
const FIELD_BLOCK_HEIGHT: u16 = 5;
/// 表单页底部留给「1 空行 + 2 行状态 + 1 行提示」的行数。
const FORM_TAIL_HEIGHT: u16 = 4;

/// 渲染整个设置面板（铺满终端，含底部帮助行）。
pub fn render(frame: &mut Frame, area: Rect, state: &SettingsState) {
    // 命中区每帧重建：鼠标落点必须对应当前画出来的那一帧。
    state.begin_frame();
    let [main, help] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let left_width = left_column_width(
        main.width,
        LEFT_COLUMN_WIDTH,
        LEFT_COLUMN_MIN_WIDTH,
        MIN_RIGHT_WIDTH,
    );
    let [left, right] = Layout::horizontal([
        Constraint::Length(left_width.min(main.width)),
        Constraint::Fill(1),
    ])
    .areas(main);

    render_rows(frame, inset_horizontal(left, COLUMN_PADDING), state);
    render_pane(frame, inset_horizontal(right, COLUMN_PADDING), state);
    render_help(frame, help, state);
    paint_hover(frame, state);
}

/// 列表里第 `row` 行（相对窗口起点）的命中区。
///
/// 各页的行高都是 1，只是起点不同；调用方传「已经画到第几行」即可，不必再算一次布局。
fn row_hit(area: Rect, row: usize) -> Rect {
    Rect {
        x: area.x,
        y: area.y + row as u16,
        width: area.width,
        height: 1,
    }
}

/// 悬停加亮：把鼠标所在行的整行加上下划线。
///
/// 直接改渲染结果而不是在十几个面板渲染函数里各写一遍「这行是不是被悬停」——
/// 行区域取的是同一帧记下来的命中区，因此不会指错行；颜色不动，避免与选中态的
/// 琥珀色、面板自己的取色打架。
fn paint_hover(frame: &mut Frame, state: &SettingsState) {
    let Some(area) = state.hover_area() else {
        return;
    };
    let buffer = frame.buffer_mut();
    let bottom = area.y.saturating_add(area.height).min(buffer.area.height);
    let right = area.x.saturating_add(area.width).min(buffer.area.width);
    for y in area.y..bottom {
        for x in area.x..right {
            let cell = &mut buffer[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::UNDERLINED));
        }
    }
}

/// 左右两栏各自的内边距（对映 CSS 的 `padding: 0 1`）。
fn inset_horizontal(area: Rect, amount: u16) -> Rect {
    if area.width <= amount * 2 {
        return area;
    }
    Rect {
        x: area.x + amount,
        y: area.y,
        width: area.width - amount * 2,
        height: area.height,
    }
}

/// 框内区域：去掉边框，再按 `padding` 内缩。
fn box_inner(area: Rect, padding: u16) -> Rect {
    let inset = 1 + padding;
    if area.width <= inset * 2 || area.height <= inset * 2 {
        return Rect {
            x: area.x,
            y: area.y,
            width: 0,
            height: 0,
        };
    }
    Rect {
        x: area.x + inset,
        y: area.y + inset,
        width: area.width - inset * 2,
        height: area.height - inset * 2,
    }
}

/// 画一栏的圆角框；聚焦时四角换成指向框内的准星箭头。
fn draw_box(frame: &mut Frame, area: Rect, focused: bool) {
    let style = if focused {
        theme::rich_style(theme::ACCENT_AMBER)
    } else {
        theme::rich_style(theme::BORDER_STRONG)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style);
    frame.render_widget(block, area);
    if focused && area.width >= 2 && area.height >= 2 {
        paint_crosshair(frame.buffer_mut(), area, style);
    }
}

/// 把四角换成准星箭头：左上 ⇘、右上 ⇙、左下 ⇗、右下 ⇖（都指向框内）。
fn paint_crosshair(buffer: &mut Buffer, area: Rect, style: Style) {
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    for (x, y, glyph) in [
        (area.x, area.y, "⇘"),
        (right, area.y, "⇙"),
        (area.x, bottom, "⇗"),
        (right, bottom, "⇖"),
    ] {
        buffer.set_string(x, y, glyph, style);
    }
}

fn render_rows(frame: &mut Frame, area: Rect, state: &SettingsState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = state.focus() == Focus::List;
    draw_box(frame, area, focused);
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
                theme::rich_style(theme::TEXT_SECONDARY)
            };
            Line::styled(fit(&row_label(key), inner.width as usize), style)
        })
        .collect();
    Paragraph::new(lines).render(inner, frame.buffer_mut());
}

fn render_pane(frame: &mut Frame, area: Rect, state: &SettingsState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = state.focus() == Focus::Pane;
    draw_box(frame, area, focused);
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
        Pane::Context => render_context(frame, body, state, focused),
        Pane::Tools => render_tools(frame, body, state, focused),
        Pane::Mcp => render_mcp(frame, body, state, focused),
        Pane::Subagents => render_subagents(frame, body, state, focused),
        Pane::Vision => render_vision(frame, body, state, focused),
        Pane::Channels => render_channels(frame, body, state, focused),
        Pane::Choice(_) => render_choice(frame, body, state, focused),
        Pane::Form(_) => render_form(frame, body, state, focused),
        Pane::Tts => render_tts(frame, body, state, focused),
        Pane::ConfigChat => render_config_chat(frame, body, state),
        Pane::Pending => render_pending(frame, body),
    }
    render_dropdown_overlay(frame, body, state, focused);
}

/// 渠道页：表单态画字段，列表态画渠道行。
fn render_channels(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    match state.channel_form() {
        Some(form) => render_channel_form(frame, area, state, form, focused),
        None => render_channel_list(frame, area, state, focused),
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

/// 渠道列表：`› 名称（provider · 模型）  已启用（当前）`。
fn render_channel_list(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.channel_rows();
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let offset = window_offset(state.channel_selected(), rows.len(), list_height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset).take(list_height) {
        state.record_hit(
            row_hit(
                Rect {
                    x: area.x + 1,
                    width: area.width.saturating_sub(1),
                    ..area
                },
                index - offset,
            ),
            HitAction::PaneRow(index),
        );
        let selected = index == state.channel_selected();
        let marker = if selected { "›" } else { " " };
        let current = if row.key == state.channel_default_key() {
            "（当前）"
        } else {
            ""
        };
        let text = format!(
            "{marker} {}  {} · {}{current}  {}",
            row.name,
            row.provider,
            row.model_id,
            if row.enabled {
                "已启用"
            } else {
                "已关闭"
            }
        );
        let style = if selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            theme::rich_style(theme::TEXT_SECONDARY)
        };
        lines.push(Line::styled(text, style));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            fit("（配置里还没有渠道；按 N 新建一条）", area.width as usize),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    Paragraph::new(lines).render(
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            height: area.height.min(list_height as u16),
            ..area
        },
        frame.buffer_mut(),
    );
}

/// 渠道表单：一行一个字段；枚举字段带 `▼`，编辑中的文本字段带光标块。
///
/// 带 `state` 是为了登记命中区（表单自己只是只读视图，不携带任何状态）。
fn render_channel_form(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    form: ChannelFormView<'_>,
    focused: bool,
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
                let suffix = if matches!(field, ChannelField::Provider | ChannelField::Protocol) {
                    " ▼"
                } else {
                    ""
                };
                format!("{}{suffix}", channel_field_value(form.row, field))
            }
        };
        let label = format!("{marker} {}：", field.label());
        let label_style = if is_current && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            theme::rich_style(theme::TEXT_MUTED)
        };
        lines.push(Line::from(vec![
            Span::styled(label, label_style),
            Span::styled(value, theme::rich_style(theme::TEXT_PRIMARY)),
        ]));
    }
    // 展开的候选排在表单下面（内联列表，不做 Python 的浮层下拉框）。
    if let Some(dropdown) = form.dropdown {
        lines.push(Line::raw(""));
        for (index, option) in dropdown.options.iter().enumerate() {
            state.record_hit(row_hit(area, lines.len()), HitAction::Option(index));
            let highlighted = index == dropdown.selected;
            let style = if highlighted {
                Style::default().bg(Color::Yellow)
            } else {
                theme::rich_style(theme::TEXT_PRIMARY)
            };
            lines.push(Line::styled(
                fit(&format!("  {option}"), area.width as usize),
                style,
            ));
        }
    }
    if form.is_new {
        lines.push(Line::styled(
            "（新渠道，Ctrl+S 保存后写盘）".to_string(),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    let visible = area.height.saturating_sub(4) as usize;
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
/// 浮层紧贴被展开的那个字段：上下文页贴 3 行下拉框的下沿、单选页贴下拉框下沿、
/// 表单页贴那一行字段的下沿。
fn dropdown_overlay_y(
    pane: Pane,
    field: DropdownField,
    area: Rect,
    state: &SettingsState,
) -> Option<u16> {
    match (pane, field) {
        (Pane::Context, DropdownField::Context(context_field)) => {
            let index = match context_field {
                ContextField::Window => 0,
                ContextField::Compaction => 1,
            };
            // 上下文页每个字段占「1 空行 + 1 标签行 + 3 行下拉框」。
            Some(area.y + 2 + index * FIELD_BLOCK_HEIGHT + 3)
        }
        (Pane::Choice(_), DropdownField::Choice(_)) => Some(area.y + 3),
        // 视觉页：头部两行之后是列表，`A` 的候选贴在列表上方。
        (Pane::Vision, DropdownField::VisionAdd) => Some(area.y + 3),
        // 表单页一行一个字段：字段表可能滚动，先减掉窗口起点。
        (Pane::Form(_), DropdownField::Form(index)) => {
            let offset = form_window_offset(state, area);
            Some(area.y + (index.checked_sub(offset)? as u16) + 1)
        }
        _ => None,
    }
}

/// 表单页字段表的窗口起点：聚焦字段始终可见。
fn form_window_offset(state: &SettingsState, area: Rect) -> usize {
    window_offset(
        state.form_focused(),
        state.form_field_count(),
        form_visible_rows(area) as usize,
    )
}

/// 表单页能显示的字段行数：底部 4 行留给状态与提示。
fn form_visible_rows(area: Rect) -> u16 {
    area.height.saturating_sub(FORM_TAIL_HEIGHT).max(1)
}

/// 单选页：一个下拉框（面板不重复标题，与 Python 的 `SelectPane` 一致）+ 状态行。
fn render_choice(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    // `SelectCurrent:focus { border: tall $terminal-white }`：聚焦即白色粗边框。
    let (style, kind) = if focused {
        (theme::rich_style(theme::ACCENT_WHITE), BorderType::Thick)
    } else {
        (theme::rich_style(theme::BORDER_SUBTLE), BorderType::Plain)
    };
    let box_area = Rect {
        height: 3.min(area.height),
        ..area
    };
    state.record_hit(box_area, HitAction::PaneRow(0));
    render_field_box(frame, box_area, &state.choice_value(), style, kind);

    // `#select-pane-status { height: 2; margin-top: 2; color: $terminal-text-secondary }`：
    // 下拉框只有 1 行内容框（3 行含边框），下面空 2 行再写状态。
    let offset = 4.min(area.height);
    let tail = Rect {
        y: area.y + offset,
        height: area.height.saturating_sub(offset),
        ..area
    };
    render_pane_tail(
        frame,
        tail,
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::TEXT_SECONDARY),
    );
}

/// 表单页：一行一个字段（`› 标签：值 ▼`），候选展开时用公共浮层。
fn render_form(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.form_rows();
    let visible = form_visible_rows(area) as usize;
    let offset = form_window_offset(state, area);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, row)| {
            state.record_hit(row_hit(area, index - offset), HitAction::PaneRow(index));
            let marker = if row.focused { "›" } else { " " };
            let value = if row.has_menu {
                format!("{} ▼", row.value)
            } else {
                row.value.clone()
            };
            let label_style = if row.focused && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                theme::rich_style(theme::TEXT_MUTED)
            };
            Line::from(vec![
                Span::styled(format!("{marker} {}：", row.label), label_style),
                Span::styled(value, theme::rich_style(theme::TEXT_PRIMARY)),
            ])
        })
        .collect();
    Paragraph::new(lines).render(
        Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            height: area.height.min((visible as u16).max(1)),
            ..area
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

fn render_context(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let expanded = state.dropdown();
    let mut y = area.y;
    let entries = [
        (ContextField::Window, "上下文长度"),
        (ContextField::Compaction, "上下文阈值"),
    ];
    for (index, (field, label)) in entries.into_iter().enumerate() {
        let block_height = FIELD_BLOCK_HEIGHT.min((area.y + area.height).saturating_sub(y));
        if block_height < 2 {
            break;
        }
        // `.context-field-label { margin-top: 1 }`：标签上方留一行空行。
        y += 1;
        let label_line = Line::styled(
            fit(label, area.width as usize),
            theme::rich_style(theme::TEXT_MUTED),
        );
        Paragraph::new(label_line).render(
            Rect {
                y,
                height: 1,
                ..area
            },
            frame.buffer_mut(),
        );
        y += 1;

        let box_area = Rect {
            y,
            height: 3.min((area.y + area.height).saturating_sub(y)),
            ..area
        };
        let active = expanded.map(|dropdown| dropdown.field) == Some(DropdownField::Context(field));
        let is_focused = focused && state.context_field() == field;
        let style = if is_focused || active {
            theme::rich_style(theme::ACCENT_WHITE)
        } else {
            theme::rich_style(theme::BORDER_SUBTLE)
        };
        let kind = if is_focused || active {
            BorderType::Thick
        } else {
            BorderType::Plain
        };
        let value = match field {
            ContextField::Window => format!("{}K", state.context_window_tokens() / 1000),
            ContextField::Compaction => format!("{}%", state.compaction_percent()),
        };
        render_field_box(frame, box_area, &value, style, kind);
        // 字段的可点区域盖住标签行 + 下拉框，点哪都能选中这个字段。
        state.record_hit(
            Rect {
                y: y.saturating_sub(1),
                height: 4.min((area.y + area.height).saturating_sub(y.saturating_sub(1))),
                ..area
            },
            HitAction::PaneRow(index),
        );
        y += 3;
    }
    let tail = Rect {
        y,
        height: (area.y + area.height).saturating_sub(y),
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

/// 折叠态下拉框：3 行边框 + 值（`padding: 0 1`）。
fn render_field_box(frame: &mut Frame, area: Rect, value: &str, style: Style, kind: BorderType) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::bordered().border_type(kind).border_style(style);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let line = Line::styled(
        fit(&format!(" {value}"), inner.width as usize),
        theme::rich_style(theme::TEXT_PRIMARY),
    );
    Paragraph::new(line).render(Rect { height: 1, ..inner }, frame.buffer_mut());
}

fn render_tools(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.tool_rows();
    // 底部 3 行留给状态（1 空行 + 2 行文本）与提示（1 行）。
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let offset = window_offset(state.tool_selected(), rows.len(), list_height);
    // `.tool-pane-row { padding: 0 1 }`：整行再缩进 1 格。
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
            Line::from(tool_row_spans(
                row,
                index == state.tool_selected() && focused,
            ))
        })
        .collect();
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

/// MCP 设置页：全局行 / Server 列表 / 编辑器三个子模式。
fn render_mcp(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    if state.mcp_editor_title().is_some() {
        render_mcp_editor(frame, area, state, focused);
    } else if state.mcp_servers_view() {
        render_mcp_servers(frame, area, state, focused);
    } else {
        render_mcp_globals(frame, area, state, focused);
    }
}

/// 底部 4 行留给状态与提示（与工具页同款）。
const MCP_TAIL_RESERVED: u16 = 4;

fn render_mcp_tail(frame: &mut Frame, area: Rect, state: &SettingsState) {
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

fn render_mcp_globals(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
                theme::rich_style(theme::TEXT_SECONDARY)
            };
            Line::styled(fit(&text, list_area.width as usize), style)
        })
        .collect();
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
    render_mcp_tail(frame, area, state);
}

fn render_mcp_servers(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
                theme::rich_style(theme::TEXT_SECONDARY)
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

fn render_mcp_editor(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
            theme::rich_style(theme::TEXT_SECONDARY)
        };
        lines.push(Line::styled(fit(&text, list_area.width as usize), style));
    }
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
    render_mcp_tail(frame, area, state);
}

/// 视觉设置页：模型原生视觉行 + 代理开关行 + 故障转移列表 + 状态与提示。
fn render_vision(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
            theme::rich_style(theme::TEXT_SECONDARY)
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
fn render_subagents(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
            theme::rich_style(theme::TEXT_SECONDARY)
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

/// 工具开关行的文本：`› 名称：已启用（未注册）`。
fn tool_row_spans(row: &ToolSwitchRow, selected: bool) -> Vec<Span<'static>> {
    let state_text = if row.enabled {
        "已启用"
    } else {
        "已关闭"
    };
    let suffix = if row.registered {
        ""
    } else {
        "（未注册）"
    };
    let text = format!(
        "{} {}：{state_text}{suffix}",
        if selected { "›" } else { " " },
        row.label
    );
    let style = if selected {
        theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
    } else {
        theme::rich_style(theme::TEXT_SECONDARY)
    };
    vec![Span::styled(text, style)]
}

/// TTS 页：行（`› 标签：值`）；动作用行以琥珀强调。
fn render_tts(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.tts_rows();
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    let offset = window_offset(state.tts_focused(), rows.len(), list_height);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset).take(list_height) {
        state.record_hit(row_hit(area, lines.len()), HitAction::PaneRow(index));
        let selected = index == state.tts_focused();
        let marker = if selected { "›" } else { " " };
        let style = if selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else if row.action {
            theme::rich_style(theme::ACCENT_WHITE)
        } else {
            theme::rich_style(theme::TEXT_SECONDARY)
        };
        let text = if row.value.is_empty() {
            format!("{marker} {}", row.label)
        } else {
            format!("{marker} {}：{}", row.label, row.value)
        };
        lines.push(Line::styled(fit(&text, area.width as usize), style));
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

/// 未知一级项：一级表与面板表不同步时的兜底文案（正常构建不会出现）。
fn render_pending(frame: &mut Frame, area: Rect) {
    let text = "该设置项没有对应的面板。";
    let line = Line::styled(
        fit(text, area.width as usize),
        theme::rich_style(theme::TEXT_MUTED),
    );
    Paragraph::new(line).render(Rect { height: 1, ..area }, frame.buffer_mut());
}

/// 「通过对话修改设置」：本页只是一个入口，说明用途并把按键写清楚。
///
/// 真正的对话在 [`crate::ui::config_chat`] 的独立弹层里：设置面板是一整屏模态页，把会话
/// 塞进右侧会挤掉其余一级项的可读宽度（与 Python 把 `config_chat` 做成独立页面同因）。
fn render_config_chat(frame: &mut Frame, area: Rect, state: &SettingsState) {
    let lines: Vec<Line<'static>> = vec![
        Line::styled(
            fit(
                "用一句话改配置：模型、上下文、工具开关、TTS 等。",
                area.width as usize,
            ),
            theme::rich_style(theme::TEXT_PRIMARY),
        ),
        Line::styled(
            fit(
                "按 Enter 打开配置对话（本地路由器，不经过模型）。",
                area.width as usize,
            ),
            theme::rich_style(theme::TEXT_SECONDARY),
        ),
    ];
    Paragraph::new(lines).render(
        Rect {
            height: area.height.min(2),
            ..area
        },
        frame.buffer_mut(),
    );
    render_pane_tail(
        frame,
        Rect {
            y: area.y + area.height.saturating_sub(2),
            height: area.height.min(2),
            ..area
        },
        state.status(),
        state.pane_hint(),
        theme::rich_style(theme::TEXT_MUTED),
    );
}

/// 面板底部的状态行（1 空行 + 最多 2 行文本）与提示行（1 行）。
fn render_pane_tail(frame: &mut Frame, area: Rect, status: &str, hint: &str, hint_style: Style) {
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
    let hint_y = status_area.y + status_height;
    if !hint.is_empty() && hint_y < area.y + area.height {
        let line = Line::styled(fit(hint, width), hint_style);
        Paragraph::new(line).render(
            Rect {
                y: hint_y,
                height: 1,
                ..area
            },
            frame.buffer_mut(),
        );
    }
}

/// 展开的候选浮层：白框 + 琥珀高亮，浮在被覆盖的内容之上。
fn render_dropdown_overlay(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
                theme::rich_style(theme::TEXT_PRIMARY)
            };
            Line::styled(fit(&format!(" {label}"), inner.width as usize), style)
        })
        .collect();
    Paragraph::new(lines).render(inner, frame.buffer_mut());
}

fn render_help(frame: &mut Frame, area: Rect, state: &SettingsState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // `#settings-help { padding: 0 2 }`：整行缩进 2 格。
    let text = fit(state.help_text(), area.width.saturating_sub(2) as usize);
    let line = Line::styled(format!("  {text}"), theme::rich_style(theme::TEXT_MUTED));
    Paragraph::new(line).render(area, frame.buffer_mut());
}

/// 渲染辅助：窗口偏移与宽度计算，独立出来便于单测。
pub mod helpers {
    /// 让选中项始终可见的窗口起始下标。
    pub fn window_offset(selected: usize, len: usize, height: usize) -> usize {
        if height == 0 || len <= height {
            return 0;
        }
        selected
            .saturating_sub(height - 1)
            .min(len.saturating_sub(height))
    }

    /// 左栏宽度：期望 30，窄终端按「右栏至少 20 格 + 4 格内边距」收缩。
    pub fn left_column_width(total: u16, preferred: u16, minimum: u16, min_right: u16) -> u16 {
        let available = total.saturating_sub(min_right + 4);
        if available >= preferred {
            preferred
        } else {
            available.max(minimum).min(total)
        }
    }

    /// 按显示宽度截断（复用 `ui::fit`）。
    pub fn fit(text: &str, width: usize) -> String {
        crate::ui::fit(text, width)
    }
}

#[cfg(test)]
mod tests {
    use super::helpers::*;

    #[test]
    fn window_offset_keeps_selection_visible() {
        assert_eq!(window_offset(0, 10, 5), 0);
        assert_eq!(window_offset(3, 10, 5), 0);
        assert_eq!(window_offset(4, 10, 5), 0);
        assert_eq!(window_offset(5, 10, 5), 1);
        assert_eq!(window_offset(9, 10, 5), 5);
        assert_eq!(window_offset(3, 3, 5), 0, "装得下就不滚动");
        assert_eq!(window_offset(3, 10, 0), 0, "零高度不越界");
    }

    #[test]
    fn left_column_shrinks_only_on_narrow_terminals() {
        assert_eq!(left_column_width(120, 30, 24, 20), 30);
        assert_eq!(left_column_width(80, 30, 24, 20), 30);
        assert_eq!(
            left_column_width(54, 30, 24, 20),
            30,
            "刚好放得下 30 + 20 + 4"
        );
        assert_eq!(left_column_width(50, 30, 24, 20), 26);
        assert_eq!(left_column_width(40, 30, 24, 20), 24, "不低于最小宽度");
        assert_eq!(left_column_width(20, 30, 24, 20), 20, "极窄时不超过总宽");
    }
}
