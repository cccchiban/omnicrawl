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
use super::picker::window_bounds;
use super::state::{
    ChannelField, ChannelFormView, ContextField, DecisionField, DecisionFormView, DropdownField,
    Focus, Pane, SettingsState, ToolSwitchRow, DECISION_LOCAL_SECTION, DECISION_SWITCH_SECTION,
    TOOLS_APPROVAL_LABEL, TOOLS_APPROVAL_ROW,
};
use super::{channel_field_value, decision_field_value, row_label};
use crate::ui::fullscreen::terminal::theme;
use omnicrawl_config::models::channels::{protocol_label, provider_label};
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
/// 压缩页内嵌选择器的搜索/提示行占 1 行。
const PICKER_PROMPT_HEIGHT: u16 = 1;
/// 压缩页内嵌选择器的列框高度：2 行边框 + 1 行列标题 + 最多 4 行内容。
const PICKER_BOX_HEIGHT: u16 = 7;

/// 渲染整个设置面板（铺满终端，含底部帮助行）。
pub fn render(frame: &mut Frame, area: Rect, state: &SettingsState) {
    // 命中区每帧重建：鼠标落点必须对应当前画出来的那一帧。
    state.begin_frame();
    // 不再留底部的整行帮助（用户要求删除）：「↑↓ 选择…」这类提示统一画在右侧面板的
    // 下边框上（见 [`draw_box`]），左侧列表不需要提示。
    let main = area;
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
    // 悬停不再画下划线（用户要求），改成黄色字体。
    let hover = theme::rich_style(theme::ACCENT_AMBER).fg;
    let buffer = frame.buffer_mut();
    let bottom = area.y.saturating_add(area.height).min(buffer.area.height);
    let right = area.x.saturating_add(area.width).min(buffer.area.width);
    for y in area.y..bottom {
        for x in area.x..right {
            let cell = &mut buffer[(x, y)];
            if let Some(color) = hover {
                cell.set_fg(color);
            }
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
fn draw_box(frame: &mut Frame, area: Rect, focused: bool, hint: Option<&str>) {
    let style = if focused {
        theme::rich_style(theme::ACCENT_AMBER)
    } else {
        theme::rich_style(theme::BORDER_STRONG)
    };
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style);
    // 操作提示画在**下边框上**（`------- ↑↓ 选择 ←→/Enter/空格 切换 --------`），
    // 灰色弱化：不再单占框内一行，也不再有页面底部那行全局帮助。
    if let Some(hint) = hint.filter(|hint| !hint.trim().is_empty()) {
        block = block.title_bottom(Line::styled(
            format!(" {hint} "),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
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
        Pane::Context => render_context(frame, body, state, focused),
        Pane::Tools => render_tools(frame, body, state, focused),
        Pane::Mcp => render_mcp(frame, body, state, focused),
        Pane::Subagents => render_subagents(frame, body, state, focused),
        Pane::Vision => render_vision(frame, body, state, focused),
        Pane::Channels => render_channels(frame, body, state, focused),
        Pane::DecisionModels => render_decision_models(frame, body, state, focused),
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

/// 渠道列表：对映 Python `ChannelManagerPane._row_text` 的两行行式。
///
/// ```text
/// › [x] 渠道名 [默认]
///     OpenAI / Chat Completions  gpt-5.2  https://api.example.com/v1
/// ```
///
/// 游标 `›` 只在选中行出现，启用态用 `[x]` / `[ ]` 勾选框表达（与 Python 一致），
/// 默认渠道后追加 ` [默认]`；第二行缩进 4 格，给出「请求方式 / 请求协议」的中文标签、
/// 模型 ID 与 Base URL。选中行沿用 Python 的琥珀色加粗。
fn render_channel_list(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let rows = state.channel_rows();
    let reserved = 4u16.min(area.height);
    let list_height = area.height.saturating_sub(reserved) as usize;
    // 一条渠道占两行（Python 的 `.channel-row { height: 2 }`）。
    let visible_rows = (list_height / 2).max(1);
    let offset = window_offset(state.channel_selected(), rows.len(), visible_rows);
    let default_key = state.channel_default_key();
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(offset).take(visible_rows) {
        let top = lines.len() as u16;
        // 命中区覆盖两行：鼠标落在标题行或明细行都算选中这条渠道。
        state.record_hit(
            Rect {
                x: area.x + 1,
                y: area.y + top,
                width: area.width.saturating_sub(1),
                height: 2,
            },
            HitAction::PaneRow(index),
        );
        let selected = index == state.channel_selected();
        let cursor = if selected { "›" } else { " " };
        let checked = if row.enabled { "x" } else { " " };
        let default_mark = if row.key == default_key {
            " [默认]"
        } else {
            ""
        };
        let style = if selected && focused {
            theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
        } else {
            theme::rich_style(theme::TEXT_SECONDARY)
        };
        lines.push(Line::styled(
            format!("{cursor} [{checked}] {}{default_mark}", row.name),
            style,
        ));
        lines.push(Line::styled(
            format!(
                "    {} / {}  {}  {}",
                provider_label(&row.provider),
                protocol_label(&row.protocol),
                row.model_id,
                row.base_url
            ),
            style,
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            fit("尚无渠道，按 N 新建第一条渠道。", area.width as usize),
            theme::rich_style(theme::TEXT_MUTED),
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
}

/// 结构化决策模型页：表单态画字段，列表态画决策渠道行。
fn render_decision_models(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
///     jev  jev-latest  https://jevtypesafeai.com/api
///
///  功能开关
///   [x] 工具调用审查使用决策模型
/// ```
fn render_decision_list(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
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
            theme::rich_style(theme::TEXT_SECONDARY)
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
                theme::rich_style(theme::TEXT_SECONDARY)
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
                theme::rich_style(theme::TEXT_SECONDARY)
            };
            let text = if row.value.is_empty() {
                format!("{} {}", if absolute == selected { "›" } else { " " }, row.label)
            } else {
                format!(
                    "{} {}：{}",
                    if absolute == selected { "›" } else { " " },
                    row.label,
                    row.value
                )
            };
            entries.push((Line::styled(fit(&text, area.width as usize), line_style), Some(absolute)));
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
fn decision_window_start(
    entries: &[(Line<'static>, Option<usize>)],
    selected: usize,
    height: usize,
) -> usize {
    if entries.is_empty() || height == 0 {
        return 0;
    }
    // 选中条目在行序列里的起止（含多行条目）。
    let first = entries
        .iter()
        .position(|(_, owner)| *owner == Some(selected))
        .unwrap_or(0);
    let last = entries
        .iter()
        .rposition(|(_, owner)| *owner == Some(selected))
        .unwrap_or(first);
    let needed = last - first + 1;
    if needed >= height {
        return first;
    }
    // 选中条目之后能塞下的行数：优先让它下方的内容也可见。
    let tail = height - needed;
    first.saturating_sub(tail.min(first))
}

/// 决策渠道表单：一行一个字段；枚举字段带 `▼`，编辑中的文本字段带光标块。
fn render_decision_form(
    frame: &mut Frame,
    area: Rect,
    state: &SettingsState,
    form: DecisionFormView<'_>,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, field) in DecisionField::ORDER.iter().copied().enumerate() {
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
                let suffix = if field == DecisionField::Mode { " ▼" } else { "" };
                format!("{}{suffix}", decision_field_value(form.row, field))
            }
        };
        let label = format!("{marker} {}：", field.label());
        let label_style = theme::rich_style(theme::TEXT_MUTED);
        lines.push(Line::from(vec![
            Span::styled(label, label_style),
            Span::styled(value, theme::rich_style(theme::TEXT_PRIMARY)),
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
                    theme::rich_style(theme::TEXT_PRIMARY)
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

/// 渠道表单：一行一个字段；枚举字段带 `▼`，编辑中的文本字段带光标块。
///
/// 带 `state` 是为了登记命中区（表单自己只是只读视图，不携带任何状态）。
fn render_channel_form(
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
        let label_style = theme::rich_style(theme::TEXT_MUTED);
        lines.push(Line::from(vec![
            Span::styled(label, label_style),
            Span::styled(value, theme::rich_style(theme::TEXT_PRIMARY)),
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
                    theme::rich_style(theme::TEXT_PRIMARY)
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

/// 表单页字段列表占的区域：扣掉底部状态/提示与（压缩页的）模型选择器。
fn form_rows_area(area: Rect, picker_height: u16) -> Rect {
    Rect {
        height: area
            .height
            .saturating_sub(picker_height + FORM_TAIL_HEIGHT.min(area.height)),
        ..area
    }
}

/// 表单页字段表的窗口起点：聚焦字段始终可见。
fn form_window_offset(state: &SettingsState, area: Rect) -> usize {
    let rows = form_rows_area(area, picker_block_height(area, state));
    window_offset(
        state.form_focused(),
        state.form_field_count(),
        rows.height.max(1) as usize,
    )
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
    // 压缩页的内嵌选择器先占一块：剩下多少行给字段列表（窗口计算与命中区必须同源）。
    let picker_height = picker_block_height(area, state);
    let rows_area = form_rows_area(area, picker_height);
    let rows = state.form_rows();
    let visible = rows_area.height.max(1) as usize;
    let offset = window_offset(state.form_focused(), state.form_field_count(), visible);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, row)| {
            state.record_hit(row_hit(rows_area, index - offset), HitAction::PaneRow(index));
            let marker = if row.focused { "›" } else { " " };
            let value = if row.has_menu {
                format!("{} ▼", row.value)
            } else {
                row.value.clone()
            };
            let label_style = theme::rich_style(theme::TEXT_MUTED);
            Line::from(vec![
                Span::styled(format!("{marker} {}：", row.label), label_style),
                Span::styled(value, theme::rich_style(theme::TEXT_PRIMARY)),
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
    if picker_height > 0 {
        let picker_area = Rect {
            y: rows_area.y + rows_area.height,
            height: picker_height,
            ..area
        };
        render_model_picker(frame, picker_area, state, focused);
    }

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

/// 压缩页里内嵌选择器占的高度；不是压缩页（或没有选择器）时为 0。
///
/// 终端太矮时先把选择器压到最小（只留提示行 + 边界），字段列表反而优先。
fn picker_block_height(area: Rect, state: &SettingsState) -> u16 {
    if state.model_picker().is_none() {
        return 0;
    }
    let wanted = PICKER_PROMPT_HEIGHT + PICKER_BOX_HEIGHT;
    let available = area.height.saturating_sub(FORM_TAIL_HEIGHT + 1);
    wanted.min(available)
}

/// 内嵌双列模型选择器：一行搜索/提示 + 两个并列的列框（对映 Python 内嵌的 `ModelPickerPane`）。
fn render_model_picker(frame: &mut Frame, area: Rect, state: &SettingsState, focused: bool) {
    let Some(picker) = state.model_picker() else {
        return;
    };
    if area.width < 8 || area.height == 0 {
        return;
    }
    let prompt_area = Rect {
        height: 1.min(area.height),
        ..area
    };
    let prompt = if picker.searching() {
        format!("搜索：{}▌", picker.search_text())
    } else if picker.query().is_empty() {
        "按 / 搜索" .to_string()
    } else {
        format!("搜索：{}（Enter 清除）", picker.query())
    };
    let prompt_style = if picker.searching() || !picker.query().is_empty() {
        theme::rich_style(theme::ACCENT_WHITE)
    } else {
        theme::rich_style(theme::TEXT_MUTED)
    };
    Paragraph::new(Line::styled(
        fit(&prompt, prompt_area.width as usize),
        prompt_style,
    ))
    .render(prompt_area, frame.buffer_mut());

    let boxes_area = Rect {
        y: area.y + prompt_area.height,
        height: area.height.saturating_sub(prompt_area.height),
        ..area
    };
    if boxes_area.height < 3 || boxes_area.width < 8 {
        return;
    }
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(boxes_area);
    let active_column = picker.column();
    render_picker_column(
        frame,
        left,
        "渠道选择",
        picker
            .channel_indices(state.channel_rows())
            .iter()
            .map(|index| {
                let channel = &state.channel_rows()[*index];
                if channel.name.trim().is_empty() {
                    channel.key.clone()
                } else {
                    channel.name.clone()
                }
            })
            .collect(),
        clamp_position(picker.column_position(0), picker.channel_indices(state.channel_rows()).len()),
        picker.current_channel(state.channel_rows()),
        focused && picker.focused() && active_column == 0,
    );
    render_picker_column(
        frame,
        right,
        "模型",
        picker
            .model_indices()
            .iter()
            .map(|index| picker.models()[*index].clone())
            .collect(),
        clamp_position(picker.column_position(1), picker.model_indices().len()),
        picker
            .current_model()
            .and_then(|current| picker.models().iter().position(|model| model == current)),
        focused && picker.focused() && active_column == 1,
    );
}

/// 一列的列框：圆角边框 + 列标题 + 窗口内条目（带前后省略行）。
fn render_picker_column(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<String>,
    selected: Option<usize>,
    current: Option<usize>,
    active: bool,
) {
    if area.width == 0 || area.height < 3 {
        return;
    }
    let border_style = if active {
        theme::rich_style(theme::ACCENT_GREEN)
    } else {
        theme::rich_style(theme::BORDER_STRONG)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style);
    let inner = block.inner(area);
    block.render(area, frame.buffer_mut());
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    // 第一行是列标题（对映 Python 列框里的 `model-column-title`）。
    Paragraph::new(Line::styled(
        fit(title, inner.width as usize),
        theme::rich_style(theme::TEXT_SECONDARY).add_modifier(Modifier::BOLD),
    ))
    .render(
        Rect {
            height: 1,
            ..inner
        },
        frame.buffer_mut(),
    );
    let list_area = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if list_area.height == 0 {
        return;
    }
    let content_rows = list_area.height as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if items.is_empty() {
        lines.push(Line::styled(
            fit("（空）", list_area.width as usize),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    } else {
        let selected = selected.unwrap_or(0);
        // 条目多于可视行数时，前后各留一行省略提示（与 Python 的 window_size 语义一致）。
        let window_size = if items.len() <= content_rows {
            content_rows
        } else {
            content_rows.saturating_sub(2).max(1)
        };
        let (start, end) = window_bounds(items.len(), selected, window_size);
        if start > 0 {
            lines.push(Line::styled(
                fit(&format!("... 前面 {start} 个"), list_area.width as usize),
                theme::rich_style(theme::TEXT_MUTED),
            ));
        }
        for (index, label) in items.iter().enumerate().take(end).skip(start) {
            let marker = if current == Some(index) {
                "●"
            } else if index == selected {
                "›"
            } else {
                " "
            };
            let style = if index == selected {
                theme::rich_style(theme::ACCENT_GREEN).add_modifier(Modifier::BOLD)
            } else if current == Some(index) {
                theme::rich_style(theme::TEXT_PRIMARY)
            } else {
                theme::rich_style(theme::TEXT_SECONDARY)
            };
            lines.push(Line::styled(
                fit(&format!("{marker} {label}"), list_area.width as usize),
                style,
            ));
        }
        if end < items.len() {
            lines.push(Line::styled(
                fit(&format!("... 后面 {} 个", items.len() - end), list_area.width as usize),
                theme::rich_style(theme::TEXT_MUTED),
            ));
        }
    }
    lines.truncate(content_rows);
    Paragraph::new(lines).render(list_area, frame.buffer_mut());
}

/// 把选中位置夹到合法范围；列表为空时返回 `None`。
fn clamp_position(position: usize, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(position.min(len - 1))
    }
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
            approval_row_text(state, state.tool_selected() == TOOLS_APPROVAL_ROW && focused),
            if state.tool_selected() == TOOLS_APPROVAL_ROW && focused {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else {
                theme::rich_style(theme::TEXT_SECONDARY)
            },
        )]));
    }
    let skip = offset.saturating_sub(1);
    lines.extend(
        rows.iter()
            .enumerate()
            .skip(skip)
            .take(list_height.saturating_sub(lines.len()))
            .map(|(index, row)| {
                state.record_hit(
                    row_hit(list_area, index + 1 - offset),
                    HitAction::PaneRow(index + 1),
                );
                Line::from(tool_row_spans(
                    row,
                    index + 1 == state.tool_selected() && focused,
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

/// 工具调用审查行的文本：`› 工具调用审查：自动审查`。
fn approval_row_text(state: &SettingsState, selected: bool) -> String {
    format!(
        "{} {}：{}",
        if selected { "›" } else { " " },
        TOOLS_APPROVAL_LABEL,
        state.tool_approval_value()
    )
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

/// 面板底部的状态行（1 空行 + 最多 2 行文本）。
///
/// 键位提示已画在面板**下边框**上（见 [`draw_box`]），这里不再重复；保留尾部两行预算
/// （[`FORM_TAIL_HEIGHT`]）不变，多出来的那行留空——它同时是状态行可折到 2 行的依据。
fn render_pane_tail(frame: &mut Frame, area: Rect, status: &str, _hint: &str, _hint_style: Style) {
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
