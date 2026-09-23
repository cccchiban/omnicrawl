//! 渲染层：两行 HUD、消息流、任务清单、待决面板、状态行与输入框。
//!
//! 消息区在渲染前先按真实列宽软折行，滚动窗口因此按显示行精确计算，
//! 不依赖终端自己的换行结果。
//!
//! `fullscreen/` 是 Python `omnicrawl/ui/fullscreen/` 的逐层对映实现（按目录
//! 对齐、行为与视觉对齐），上述模块是该对映层完成后要退役的早期简化页面。

pub mod composer;
pub mod config_chat;
pub mod conversation;
pub mod file_picker;
pub mod fullscreen;
pub mod hud;
pub mod panels;
pub mod queue;
pub mod settings;
pub mod splash;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::Frame;

use crate::state::AppState;

/// HUD 固定两行：一行遥测、一行项目与上下文占用。
pub const HUD_HEIGHT: u16 = 2;

/// 一帧的各区几何：渲染与鼠标命中判定共用同一套布局计算，两者永远一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiAreas {
    pub hud: Rect,
    pub conversation: Rect,
    pub todos: Rect,
    pub panel: Rect,
    pub queue: Rect,
    pub status: Rect,
    /// 输入框上方的命令菜单（对映 Python `#composer-wrap` 里的菜单行预算）。
    pub menu: Rect,
    pub composer: Rect,
}

/// 按当前状态把整屏切成固定条带；消息区吃剩余高度。
pub fn layout(area: Rect, state: &AppState) -> UiAreas {
    let width = area.width;
    let [hud, conversation, todos, panel, queue, status, menu, composer] = Layout::vertical([
        Constraint::Length(HUD_HEIGHT),
        // 消息区吃掉除固定条带外的全部高度；用 Fill 而不是 Min，避免多余空间落到布局末尾。
        Constraint::Fill(1),
        Constraint::Length(panels::todo_height(state)),
        Constraint::Length(panels::panel_height(state, width)),
        Constraint::Length(queue::height(state)),
        Constraint::Length(panels::status_height(state)),
        Constraint::Length(composer::menu_height(state)),
        Constraint::Length(composer::height(state, width)),
    ])
    .areas(area);
    UiAreas {
        hud,
        conversation,
        todos,
        panel,
        queue,
        status,
        menu,
        composer,
    }
}

/// 渲染一帧：设置面板打开时铺满整屏（对映 Python 的 `SettingsScreen` 模态页）。
///
/// 文件选择弹层盖在设置面板之上（TTS 页选参考音频时用它）；弹层自己在中间画对话框，
/// 底下的界面照旧渲染，退出弹层时不需要重排布局。
pub fn render(
    frame: &mut Frame,
    state: &AppState,
    settings: Option<&settings::SettingsState>,
    picker: Option<&file_picker::FilePickerState>,
    chat: Option<&config_chat::ConfigChatState>,
) {
    if let Some(chat) = chat {
        config_chat::render(frame, frame.area(), chat);
        return;
    }
    if let Some(settings) = settings {
        settings::render::render(frame, frame.area(), settings);
        if let Some(picker) = picker {
            file_picker::render(frame, frame.area(), picker);
        }
        return;
    }
    let area = frame.area();
    let width = area.width;
    let regions = layout(area, state);

    hud::render(frame, regions.hud, state);
    conversation::render(frame, regions.conversation, state);
    panels::render_todos(frame, regions.todos, state, width);
    panels::render_panel(frame, regions.panel, state, width);
    queue::render(frame, regions.queue, state);
    panels::render_status(frame, regions.status, state);
    composer::render_menu(frame, regions.menu, state);
    composer::render(frame, regions.composer, state);
}

/// 按显示列宽折行；CJK 与 emoji 各占自己的列宽，阶段一按列断行，不做单词级避断。
pub fn wrap_display(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut current));
            used = 0;
            continue;
        }
        let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cell > width && used > 0 {
            lines.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += cell;
    }
    lines.push(current);
    lines
}

/// 截到指定显示宽度，超出时以 `…` 收尾。
pub fn fit(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cell > width.saturating_sub(1) {
            break;
        }
        result.push(ch);
        used += cell;
    }
    result.push('…');
    result
}

pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

/// 左对齐补空格到指定显示宽度（先截断）。
pub fn pad(text: &str, width: usize) -> String {
    let fitted = fit(text, width);
    let used = display_width(&fitted);
    format!("{fitted}{}", " ".repeat(width.saturating_sub(used)))
}

/// 居中对齐到指定显示宽度。
pub fn center(text: &str, width: usize) -> String {
    let fitted = fit(text, width);
    let used = display_width(&fitted);
    let left = width.saturating_sub(used) / 2;
    format!(
        "{}{fitted}{}",
        " ".repeat(left),
        " ".repeat(width.saturating_sub(used + left))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_display_counts_cjk_columns() {
        assert_eq!(wrap_display("中文测试", 4), vec!["中文", "测试"]);
        assert_eq!(wrap_display("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_display("第一行\n第二行", 20), vec!["第一行", "第二行"]);
        assert_eq!(wrap_display("", 5), vec![""]);
    }

    #[test]
    fn fit_keeps_display_width_and_marks_truncation() {
        assert_eq!(fit("abc", 5), "abc");
        assert_eq!(fit("中文字", 4), "中…");
        assert_eq!(display_width(&fit("中文测试中文", 6)), 5);
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn pad_and_center_reach_requested_width() {
        assert_eq!(pad("abc", 6), "abc   ");
        assert_eq!(display_width(&pad("中文", 6)), 6);
        assert_eq!(display_width(&center("中文", 6)), 6);
        assert_eq!(center("ab", 6), "  ab  ");
    }
}
