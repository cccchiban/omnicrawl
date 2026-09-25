//! 渲染层：消息流、任务清单、待决面板、输入卡与底部单行轮播 HUD。
//!
//! 消息区在渲染前先按真实列宽软折行，滚动窗口因此按显示行精确计算，
//! 不依赖终端自己的换行结果。
//!
//! `fullscreen/` 是 Python `omnicrawl/ui/fullscreen/` 的逐层对映实现（按目录
//! 对齐、行为与视觉对齐），上述模块是该对映层完成后要退役的早期简化页面。
//! 本层已按 Python 当前版式对齐：底部单行轮播 HUD、悬浮圆角输入卡、`user：` 标签
//! 消息、无边框工具卡（`●` 状态点 + 缩进正文）、会话流内的运行状态行。

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
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::time::Instant;

use crate::state::AppState;
use crate::ui::fullscreen::terminal::theme;
use crate::ui::fullscreen::text::StyledText;

/// 底部单行轮播 HUD 占一行（对映 CSS `#bottom-carousel { height: 1 }`）。
pub const HUD_HEIGHT: u16 = 1;

/// 输入框上方那行瞬时提示（拖选复制等）占一行：不进会话流，几秒后自散。
pub const NOTICE_LINE_HEIGHT: u16 = 1;

/// 一帧的各区几何：渲染与鼠标命中判定共用同一套布局计算，两者永远一致。
///
/// 与 Python 的 `#shell` 一致，自上而下是：会话区（吃剩余高度）→ 计划区 → 待决面板 →
/// 排队预览 → 命令菜单 → 输入卡 → 底部轮播 HUD。运行状态行不再单占条带：
/// 它作为会话流里的一条临时消息渲染（对映 `.runtime-status-message`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiAreas {
    pub hud: Rect,
    pub conversation: Rect,
    pub todos: Rect,
    pub panel: Rect,
    pub queue: Rect,
    /// 输入框上方的命令菜单（对映 Python `#composer-wrap` 里的菜单行预算）。
    /// 输入框上方（命令菜单之上）的瞬时提示行。
    pub notice: Rect,
    pub menu: Rect,
    pub composer: Rect,
}

/// 按当前状态把整屏切成固定条带；消息区吃剩余高度。
pub fn layout(area: Rect, state: &AppState) -> UiAreas {
    let width = area.width;
    let [conversation, todos, panel, queue, notice, menu, composer, hud] = Layout::vertical([
        // 消息区吃掉除固定条带外的全部高度；用 Fill 而不是 Min，避免多余空间落到布局末尾。
        Constraint::Fill(1),
        Constraint::Length(panels::todo_height(state)),
        Constraint::Length(panels::panel_height(state, width)),
        Constraint::Length(queue::height(state)),
        Constraint::Length(notice_line_height(state)),
        Constraint::Length(composer::menu_height(state)),
        Constraint::Length(composer::height(state, width)),
        Constraint::Length(HUD_HEIGHT),
    ])
    .areas(area);
    UiAreas {
        hud,
        conversation,
        todos,
        panel,
        queue,
        notice,
        menu,
        composer,
    }
}

/// 瞬时提示行的高度：有未见过的提示就占一行。
fn notice_line_height(state: &AppState) -> u16 {
    if state.notice_line_text(Instant::now()).is_some() {
        NOTICE_LINE_HEIGHT
    } else {
        0
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

    conversation::render(frame, regions.conversation, state);
    panels::render_todos(frame, regions.todos, state, width);
    panels::render_panel(frame, regions.panel, state, width);
    queue::render(frame, regions.queue, state);
    render_notice_line(frame, regions.notice, state);
    composer::render_menu(frame, regions.menu, state);
    composer::render(frame, regions.composer, state);
    hud::render(frame, regions.hud, state);
}

/// 输入框上方那行瞬时提示（拖选复制等）：不进会话流，也不占消息区。
fn render_notice_line(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(text) = state.notice_line_text(Instant::now()) else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let styled = StyledText::styled(&format!("  · {text}"), theme::TEXT_MUTED);
    frame.render_widget(
        Paragraph::new(hud::line_of(&styled, usize::from(area.width))),
        area,
    );
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

/// 按显示宽度截断富文本，保留原有样式；被截断时以弱化色的 `…` 收尾。
///
/// 消息流行与底部轮播行都按显示列宽切，不能按字节或字符数算——CJK 与 emoji
/// 各占自己的列宽（对映 Python 的 `text-overflow: ellipsis`）。
pub fn truncate_styled(text: &StyledText, width: usize) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    // 先摊平成「字符 + 样式串」，再逐字符累加宽度决定截断点。
    let cells: Vec<(char, &str)> = text
        .spans()
        .iter()
        .flat_map(|span| span.text.chars().map(move |ch| (ch, span.style.as_str())))
        .collect();
    let total: usize = cells
        .iter()
        .map(|(ch, _)| unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0))
        .sum();
    let truncated = total > width;
    // 截断时留一格给省略号。
    let budget = if truncated { width - 1 } else { width };
    let mut used = 0usize;
    let mut kept: Vec<(char, &str)> = Vec::new();
    for (ch, style) in cells {
        let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cell > budget {
            break;
        }
        used += cell;
        kept.push((ch, style));
    }
    // 同一样式的相邻字符合并成一个 span，避免一行内出现大量碎片。
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut index = 0usize;
    while index < kept.len() {
        let style = kept[index].1;
        let mut chunk = String::new();
        while index < kept.len() && kept[index].1 == style {
            chunk.push(kept[index].0);
            index += 1;
        }
        spans.push(Span::styled(chunk, theme::rich_style(style)));
    }
    if truncated {
        spans.push(Span::styled("…", theme::rich_style(theme::TEXT_MUTED)));
    }
    spans
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
