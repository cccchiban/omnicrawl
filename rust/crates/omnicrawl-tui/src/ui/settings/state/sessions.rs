//! 会话管理页的键盘处理。
//!
//! 键位：↑↓ 选择、`D` 删除（连按两次确认）、`R` 刷新、`Esc` 返回。
//! 与配置页不同，这一页的删除不是写配置，而是产出 [`SettingsEvent::DeleteSession`]
//! 交给宿主走内核链路；列表与状态由宿主回填。

use super::*;

impl SettingsState {
    /// 会话管理页：列表选中、删除确认与刷新。
    pub(super) fn handle_sessions_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.sessions.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.sessions.move_selection(1);
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                self.back_to_list();
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                // 第一次按只转入确认态，第二次才真正发出删除事件。
                self.sessions
                    .request_delete()
                    .map(|session_id| SettingsEvent::DeleteSession { session_id })
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                // `Y` 只作为确认键：没有待确认目标时什么也不做。
                self.sessions
                    .confirming()
                    .map(|pending| pending.session_id.clone())
                    .map(|session_id| SettingsEvent::DeleteSession { session_id })
            }
            KeyCode::Char('r') | KeyCode::Char('R') => Some(SettingsEvent::RefreshSessions),
            _ => None,
        }
    }
}
