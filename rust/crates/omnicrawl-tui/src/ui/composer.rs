//! 输入卡：白色圆角边框 + 单行起步按显示宽度软折行（对映 Python `#composer-wrap`）。
//!
//! 版式：上方留一行与消息区分隔（CSS `margin: 1 0 0 0`），卡片本体是「上边框 + 内容 +
//! 下边框」。**内容紧贴左右边框**——按用户要求去掉了 CSS 的 `padding: 0 2` 与
//! `#composer { padding: 0 1 }`，只留 1 格边框本身。空输入时显示占位文案
//! `› 输入消息或 / 命令`（Python 把 `› ` 写在占位符里，正文不再带提示符）。
//!
//! 内容超过 5 行（[`COMPOSER_MAX_LINES`]）时框内随光标滚动，右侧画一条与消息区同款的
//! 细线滚动条；光标由我们自己画成白色粗块（`█` 风格的反白格），不依赖终端那个会闪烁的细光标。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use super::fullscreen::terminal::theme;
use crate::state::{AppState, COMPOSER_MAX_LINES};

/// 占位文案：`› ` 属于占位符本身（对映 Python `placeholder="› 输入消息或 / 命令"`）。
const PLACEHOLDER: &str = "› 输入消息或 / 命令";
/// 卡片上方与消息区之间的空行（CSS `margin: 1 0 0 0`）。
const MARGIN_TOP: u16 = 0;
/// 圆角边框占的行/列数，也是正文左侧的总缩进（内容紧贴边框）。
const BORDER: u16 = 1;
/// 正文左（右）侧的总缩进。
const TEXT_OFFSET: u16 = BORDER;
/// 右侧留给细线滚动条的列数。
const SCROLLBAR_WIDTH: u16 = 1;

/// 输入卡占用的行数：上方空行 + 上下边框 + 框内换行行数（上限 [`COMPOSER_MAX_LINES`]）。
pub fn height(state: &AppState, width: u16) -> u16 {
    let body = body_width(width);
    let (lines, _) = state.composer.visible_lines(body);
    let content = (lines.len().max(1) as u16).min(COMPOSER_MAX_LINES as u16);
    MARGIN_TOP + 2 * BORDER + content
}

/// 正文可用列宽：扣掉左右边框与右侧滚动条列。
pub fn body_width(width: u16) -> u16 {
    width
        .saturating_sub(2 * TEXT_OFFSET)
        .saturating_sub(SCROLLBAR_WIDTH)
        .max(1)
}

/// 输入框上方菜单占用的行数：`/sessions` 会话菜单优先，其次才是命令菜单。
///
/// 两套菜单不会同时打开（会话菜单由提交 `/sessions` 触发，此时命令菜单已收起），
/// 每项一行，超出可见上限时随选择位滚动（对映 `_resize_composer_to_text`
/// 的 `menu_rows = min(len(matches), COMMAND_MENU_VISIBLE_OPTIONS)`）。
pub fn menu_height(state: &AppState) -> u16 {
    if state.sessions_menu.is_open() {
        state.sessions_menu.visible_rows() as u16
    } else {
        state.composer.menu().visible_rows() as u16
    }
}

/// 渲染输入框上方的菜单：`/sessions` 会话列表优先，否则是斜杠命令菜单。
///
/// 选中项 `› ` + 琥珀色，其余次级色，描述段弱化；菜单行与 Python 一样不软折行
/// （超宽部分裁断，对应 Textual 的 `no_wrap` + `ellipsis`）。
pub fn render_menu(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let rendered = if state.sessions_menu.is_open() {
        state.sessions_menu.render()
    } else {
        state.composer.menu().render()
    };
    let lines: Vec<Line<'static>> = rendered
        .split_lines()
        .iter()
        .map(|line| Line::from(line.to_spans()))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// 渲染输入卡本体：圆角白框 + 框内正文（空输入时是占位文案）+ 滚动条 + 粗光标。
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height <= MARGIN_TOP || area.width == 0 {
        return;
    }
    let card = card_area(area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::rich_style(theme::ACCENT_WHITE));
    let inner = block.inner(card);
    frame.render_widget(block, card);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let body = body_width(area.width);
    let (lines, cursor_row, start, total) = state.composer.visible_window(body);
    let text = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width.saturating_sub(SCROLLBAR_WIDTH),
        height: inner.height,
    };
    if text.width == 0 {
        return;
    }
    render_scrollbar(frame, inner, total, text.height as usize, start);

    if state.composer.is_empty() {
        // 占位文案：弱化色；**光标照样画**（用户要求空白时也看得到插入点，
        // 与 TextArea 在 placeholder 上仍显示光标的行为一致）。
        frame.render_widget(
            Paragraph::new(Line::styled(
                PLACEHOLDER,
                theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::DIM),
            )),
            text,
        );
        let line = lines.first().cloned().unwrap_or_default();
        paint_block_cursor(frame.buffer_mut(), text, &line, 0, 0);
        return;
    }

    let rendered: Vec<Line<'static>> = lines.iter().map(|line| Line::raw(line.clone())).collect();
    frame.render_widget(Paragraph::new(rendered), text);

    let (_, column) = state.composer.cursor_position(body);
    paint_block_cursor(
        frame.buffer_mut(),
        text,
        &lines[cursor_row.min(lines.len().saturating_sub(1))],
        cursor_row as u16,
        column,
    );
}

/// 内容超出可见行数时的细线滚动条（与消息区同款：透明轨道 + `█` 滑块）。
fn render_scrollbar(frame: &mut Frame, inner: Rect, total: usize, height: usize, start: usize) {
    if height == 0 || total <= height || inner.width == 0 || inner.height == 0 {
        return;
    }
    let column = Rect {
        x: inner.x + inner.width.saturating_sub(SCROLLBAR_WIDTH),
        width: SCROLLBAR_WIDTH.min(inner.width),
        ..inner
    };
    if column.width == 0 {
        return;
    }
    let max_offset = total - height;
    let mut scrollbar = ScrollbarState::new(max_offset).position(start.min(max_offset));
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(None)
        .thumb_symbol("█")
        .style(theme::rich_style(theme::SCROLLBAR));
    frame.render_stateful_widget(bar, column, &mut scrollbar);
}

/// 画白色粗光标：光标所在那一格反白（白底黑字），宽字符连着占位格一起填。
///
/// 用自绘块替代终端光标，既够粗也不会闪（`Terminal::draw` 不设光标时终端光标保持隐藏）。
fn paint_block_cursor(buffer: &mut Buffer, text: Rect, line: &str, row: u16, column: usize) {
    let y = text.y + row;
    if y >= text.y + text.height {
        return;
    }
    let x = text.x + column as u16;
    if x >= text.x + text.width {
        return;
    }
    // 光标所在列对应的字符宽度：宽字符要连右侧占位格一起染色，否则只白一半。
    let mut walked = 0usize;
    let mut cells = 1u16;
    for ch in line.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if walked >= column {
            cells = (width.max(1) as u16).min(text.width - (x - text.x));
            break;
        }
        walked += width;
    }
    let style = Style::new().bg(Color::White).fg(Color::Black);
    for offset in 0..cells.max(1) {
        let cell_x = x + offset;
        if cell_x >= text.x + text.width {
            break;
        }
        buffer[(cell_x, y)].set_style(style);
    }
}

/// 卡片本体区域：扣掉卡片上方的空行。
fn card_area(area: Rect) -> Rect {
    Rect {
        y: area.y + MARGIN_TOP,
        height: area.height.saturating_sub(MARGIN_TOP),
        ..area
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;

    fn state() -> AppState {
        AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual)
    }

    #[test]
    fn height_covers_margin_border_and_wrapped_lines() {
        let state = state();
        // 宽终端下空输入：上方空行 + 上下边框 + 一行占位 = 4 行。
        assert_eq!(height(&state, 40), MARGIN_TOP + 2 + 1);

        let mut state = state;
        state.composer.insert("中文测试");
        assert_eq!(height(&state, 40), MARGIN_TOP + 2 + 1);

        // 收窄到正文只剩 4 列：四个全角字折成两行，卡片跟着长高。
        assert_eq!(body_width(4 + 2 * TEXT_OFFSET + SCROLLBAR_WIDTH), 4);
        assert_eq!(
            height(&state, 4 + 2 * TEXT_OFFSET + SCROLLBAR_WIDTH),
            MARGIN_TOP + 2 + 2
        );

        // 超过上限后在编辑器内滚动：卡片不再继续长高。
        for _ in 0..10 {
            state.composer.newline();
            state.composer.insert("行");
        }
        assert_eq!(
            height(&state, 40),
            MARGIN_TOP + 2 + COMPOSER_MAX_LINES as u16
        );
    }

    #[test]
    fn body_starts_right_after_the_border() {
        // 内容紧贴左右边框：正文可用宽度 = 总宽 - 2 格边框 - 1 格滚动条。
        assert_eq!(TEXT_OFFSET, 1);
        assert_eq!(body_width(40), 40 - 2 - SCROLLBAR_WIDTH);
    }
}
