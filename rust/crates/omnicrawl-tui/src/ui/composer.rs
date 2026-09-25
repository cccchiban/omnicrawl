//! 输入卡：白色圆角边框 + 单行起步按显示宽度软折行（对映 Python `#composer-wrap`）。
//!
//! 版式（CSS `#composer-wrap { height: 3; margin: 1 0 0 0; padding: 0 2;
//! border: round $terminal-white }` + `#composer { padding: 0 1 }`）：
//! 上方留一行与消息区分隔，卡片本体是「上边框 + 内容 + 下边框」，内容行左右各缩进
//! 边框 1 + 卡片内边距 2 + 编辑器内边距 1 = 4 格。空输入时显示占位文案
//! `› 输入消息或 / 命令`（Python 把 `› ` 写在占位符里，正文不再带提示符）。

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use super::fullscreen::terminal::theme;
use crate::state::{AppState, COMPOSER_MAX_LINES};

/// 占位文案：`› ` 属于占位符本身（对映 Python `placeholder="› 输入消息或 / 命令"`）。
const PLACEHOLDER: &str = "› 输入消息或 / 命令";
/// 卡片上方与消息区之间的空行（CSS `margin: 1 0 0 0`）。
const MARGIN_TOP: u16 = 1;
/// 圆角边框占的行/列数。
const BORDER: u16 = 1;
/// 卡片内边距（CSS `padding: 0 2`）。
const WRAP_PAD: u16 = 2;
/// 编辑器内边距（CSS `#composer { padding: 0 1 }`）。
const TEXT_PAD: u16 = 1;
/// 正文左（右）侧的总缩进。
const TEXT_OFFSET: u16 = BORDER + WRAP_PAD + TEXT_PAD;

/// 输入卡占用的行数：上方空行 + 上下边框 + 框内换行行数（上限 [`COMPOSER_MAX_LINES`]）。
pub fn height(state: &AppState, width: u16) -> u16 {
    let body = body_width(width);
    let (lines, _) = state.composer.visible_lines(body);
    let content = (lines.len().max(1) as u16).min(COMPOSER_MAX_LINES as u16);
    MARGIN_TOP + 2 * BORDER + content
}

/// 正文可用列宽：扣掉左右边框与两侧内边距。
pub fn body_width(width: u16) -> u16 {
    width.saturating_sub(2 * TEXT_OFFSET).max(1)
}

/// 命令菜单占用的行数：每项一行，超出可见上限时随选择位滚动（对映 `_resize_composer_to_text`
/// 的 `menu_rows = min(len(matches), COMMAND_MENU_VISIBLE_OPTIONS)`）。
pub fn menu_height(state: &AppState) -> u16 {
    state.composer.menu().visible_rows() as u16
}

/// 渲染命令菜单：选中项 `› ` + 琥珀色，其余次级色，描述段弱化。
///
/// 菜单行与 Python 一样不软折行（超宽部分裁断，对应 Textual 的 `no_wrap` + `ellipsis`）。
pub fn render_menu(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let rendered = state.composer.menu().render();
    let lines: Vec<Line<'static>> = rendered
        .split_lines()
        .iter()
        .map(|line| Line::from(line.to_spans()))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// 渲染输入卡本体：圆角白框 + 框内正文（空输入时是占位文案）+ 光标。
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
    let (lines, cursor_row) = state.composer.visible_lines(body);
    let text = Rect {
        x: inner.x + WRAP_PAD + TEXT_PAD,
        y: inner.y,
        width: inner.width.saturating_sub(2 * (WRAP_PAD + TEXT_PAD)),
        height: inner.height,
    };
    if text.width == 0 {
        return;
    }

    if state.composer.is_empty() {
        // 占位文案：弱化色，不带光标（对映 TextArea 的 placeholder 行为）。
        frame.render_widget(
            Paragraph::new(Line::styled(
                PLACEHOLDER,
                theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::DIM),
            )),
            text,
        );
        return;
    }

    let rendered: Vec<Line<'static>> = lines.iter().map(|line| Line::raw(line.clone())).collect();
    frame.render_widget(Paragraph::new(rendered), text);

    let (_, column) = state.composer.cursor_position(body);
    let x = text.x + column as u16;
    let y = text.y + cursor_row as u16;
    if x < text.x + text.width && y < text.y + text.height {
        frame.set_cursor_position((x, y));
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
        assert_eq!(body_width(4 + 2 * TEXT_OFFSET), 4);
        assert_eq!(height(&state, 4 + 2 * TEXT_OFFSET), MARGIN_TOP + 2 + 2);

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
}
