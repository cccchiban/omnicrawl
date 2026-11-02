//! 提问建议页：三个开关，选中即切换并写盘。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出（与决策模型页同一做法）：
//! 这一页只有三行开关，键位与子任务页的总开关行完全同义。

use super::*;

impl SettingsState {
    /// `↑`/`↓` 选行；`←`/`→`/`Enter`/空格 就地切换；`Esc` 回一级菜单。
    pub(super) fn handle_suggestion_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_suggestion(-1);
                None
            }
            KeyCode::Down => {
                self.move_suggestion(1);
                None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_suggestion()
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    pub(super) fn move_suggestion(&mut self, delta: isize) {
        let count = self.suggestion.rows.len() as isize;
        if count == 0 {
            return;
        }
        self.suggestion.selected =
            ((self.suggestion.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 翻转选中行：产出的是**目标状态**而不是「翻转」，宿主拿到就能直接写盘。
    pub(super) fn toggle_suggestion(&mut self) -> Option<SettingsEvent> {
        let row = self.suggestion.rows.get(self.suggestion.selected)?;
        Some(SettingsEvent::Apply(SettingsChange::SuggestionSwitch {
            key: row.key.clone(),
            enabled: !row.enabled,
        }))
    }
}
