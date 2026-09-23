//! 输入框：单行起步、按显示宽度软折行，超过五行后在编辑器内滚动。

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::state::{AppState, COMPOSER_MAX_LINES};

const PROMPT: &str = "› ";
const CONTINUATION: &str = "  ";

/// 提示符占的显示列数；`PROMPT` 含多字节字符，不能按字节数算。
fn prompt_width() -> u16 {
    super::display_width(PROMPT) as u16
}

/// 输入框占用的行数：与框内换行结果一致，最短一行。
pub fn height(state: &AppState, width: u16) -> u16 {
    let body = width.saturating_sub(prompt_width()).max(1);
    let (lines, _) = state.composer.visible_lines(body);
    (lines.len().max(1) as u16).min(COMPOSER_MAX_LINES as u16)
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

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let body_width = area.width.saturating_sub(prompt_width()).max(1);
    let (lines, cursor_row) = state.composer.visible_lines(body_width);

    let mut rendered: Vec<Line<'static>> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let prefix = if index == 0 { PROMPT } else { CONTINUATION };
        rendered.push(Line::from(vec![
            Span::styled(prefix, Style::new().fg(Color::Magenta)),
            Span::raw(line.clone()),
        ]));
    }
    frame.render_widget(Paragraph::new(rendered), area);

    let (_, column) = state.composer.cursor_position(body_width);
    let x = area.x + prompt_width() + column as u16;
    let y = area.y + cursor_row as u16;
    if x < area.x + area.width && y < area.y + area.height {
        frame.set_cursor_position((x, y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;

    #[test]
    fn height_follows_wrapped_lines() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        assert_eq!(height(&state, 40), 1, "空输入只有一行");
        state.composer.insert("短");
        assert_eq!(height(&state, 40), 1);
        // 每行正文宽 4 列（宽度 6 减提示符），五个全角字正好三行。
        state.composer.insert("中文测试");
        assert_eq!(height(&state, 6), 3);
        for _ in 0..10 {
            state.composer.newline();
            state.composer.insert("行");
        }
        assert_eq!(height(&state, 40), COMPOSER_MAX_LINES as u16);
    }
}
