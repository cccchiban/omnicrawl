//! `/sessions` 的输入框上方可选菜单（对映 Python `input/sessions_menu.py`）。
//!
//! `/sessions` 不再把会话列表追加进对话区，而是以可导航的预选列表展示在输入框
//! 上方：上下键选择、Enter 把 `\/resume <session_id>` 填回输入框（用户再按 Enter
//! 提交即可恢复），Esc 收起。与 Python 的分工一致：本模块只是纯状态机与渲染，
//! 会话列表的读取在宿主侧完成。
//!
//! 与 Python 的差异：候选行把时间、条数与标题压成一行展示，不再按终端宽度动态
//! 截断标题（宿主已经把标题裁到合理长度）。

use crate::ui::fullscreen::terminal::theme::{ACCENT_AMBER, TEXT_MUTED, TEXT_SECONDARY};
use crate::ui::fullscreen::text::StyledText;

/// 菜单可见候选行数（对齐斜杠命令菜单的可见上限）。
pub const SESSIONS_MENU_VISIBLE_OPTIONS: usize = 8;

/// 会话菜单里的一条可选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMenuItem {
    pub session_id: String,
    /// 形如 `09-25 14:30` 的本地时间。
    pub updated_at: String,
    pub message_count: u64,
    /// 已归一化的标题（空标题由宿主替换成「未命名会话」）。
    pub title: String,
    pub is_current: bool,
}

/// 会话选择菜单的状态机。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionsMenu {
    items: Vec<SessionMenuItem>,
    selection: usize,
}

impl SessionsMenu {
    pub fn new() -> Self {
        Self::default()
    }

    /// 菜单是否正在显示。
    pub fn is_open(&self) -> bool {
        !self.items.is_empty()
    }

    pub fn items(&self) -> &[SessionMenuItem] {
        &self.items
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    /// 打开菜单并展示给定条目。
    pub fn open(&mut self, items: Vec<SessionMenuItem>) {
        self.items = items;
        self.selection = 0;
    }

    /// 收起菜单并清空状态。
    pub fn close(&mut self) {
        self.items.clear();
        self.selection = 0;
    }

    /// 可见行数（供输入区高度计算）。
    pub fn visible_rows(&self) -> usize {
        self.items.len().min(SESSIONS_MENU_VISIBLE_OPTIONS)
    }

    /// 上下键循环移动选中项。
    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as isize;
        self.selection = (self.selection as isize + delta).rem_euclid(len) as usize;
    }

    pub fn selected(&self) -> Option<&SessionMenuItem> {
        self.items.get(self.selection)
    }

    /// 可见窗口 `[start, end)`：选中项始终可见，与命令菜单同一套起点计算。
    pub fn visible_window(&self) -> (usize, usize) {
        let limit = SESSIONS_MENU_VISIBLE_OPTIONS as isize;
        let len = self.items.len() as isize;
        if len <= limit {
            return (0, self.items.len());
        }
        let start = (self.selection as isize - limit + 1).min(len - limit).max(0) as usize;
        (start, start + SESSIONS_MENU_VISIBLE_OPTIONS)
    }

    /// 渲染可见候选：选中项 `› ` + 粗体琥珀，当前会话带 `*` 标记，
    /// 时间 / 条数 / 标题用弱化色。
    pub fn render(&self) -> StyledText {
        let (start, end) = self.visible_window();
        let end = end.min(self.items.len());
        let mut lines = StyledText::new();
        for (offset, item) in self.items[start..end].iter().enumerate() {
            let selected = start + offset == self.selection;
            let style = if selected {
                format!("bold {ACCENT_AMBER}")
            } else {
                TEXT_SECONDARY.to_string()
            };
            let short_id: String = item.session_id.chars().take(8).collect();
            let marker = if selected { "› " } else { "  " };
            let current = if item.is_current { "*" } else { " " };
            lines.push(
                &format!("{marker}{current} {short_id}  "),
                &style,
            );
            lines.push(
                &format!(
                    "{}  {}条  {}",
                    item.updated_at, item.message_count, item.title
                ),
                TEXT_MUTED,
            );
            if offset + 1 < end - start {
                lines.push("\n", "");
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize) -> SessionMenuItem {
        SessionMenuItem {
            session_id: format!("session-{index:04}"),
            updated_at: "09-25 14:30".to_string(),
            message_count: index as u64,
            title: format!("会话 {index}"),
            is_current: index == 0,
        }
    }

    fn menu(count: usize) -> SessionsMenu {
        let mut menu = SessionsMenu::new();
        menu.open((0..count).map(item).collect());
        menu
    }

    #[test]
    fn selection_wraps_and_clamps_visible_window() {
        let mut menu = menu(20);
        assert_eq!(menu.visible_rows(), SESSIONS_MENU_VISIBLE_OPTIONS);
        assert_eq!(menu.visible_window(), (0, 8));
        menu.move_selection(-1);
        assert_eq!(menu.selection(), 19, "向上越界回绕到末项");
        for _ in 0..12 {
            menu.move_selection(1);
        }
        assert_eq!(menu.selection(), 11);
        assert_eq!(menu.visible_window(), (4, 12));
    }

    #[test]
    fn open_and_close_toggle_state() {
        let mut menu = menu(3);
        assert!(menu.is_open());
        assert_eq!(menu.selected().map(|entry| entry.message_count), Some(0));
        menu.close();
        assert!(!menu.is_open());
        assert_eq!(menu.visible_rows(), 0);
        assert!(menu.selected().is_none());
    }

    #[test]
    fn render_marks_selection_and_current_session() {
        let menu = menu(3);
        let plain = menu.render().plain();
        assert!(plain.contains("› * session-"), "{plain}");
        assert!(plain.contains("会话 0"), "{plain}");
        assert!(plain.contains("09-25 14:30"), "{plain}");
    }
}
