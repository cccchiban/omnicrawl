//! 知识库页的键盘处理。
//!
//! 键位与记忆页一致：`↑↓` 选行、`←→`/`Enter`/`空格` 切换、`D` 清除（连按两次确认）、
//! `O` 打开目录、`Esc` 返回。
//!
//! 两类行为分开：配置行产出 [`SettingsChange`]（宿主写盘），操作行产出
//! [`SettingsEvent`]（宿主执行不可逆动作）。清除要按两次才真正发出。

use super::*;

impl SettingsState {
    /// 知识库页：开关与两个操作行。
    pub(super) fn handle_knowledge_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.knowledge.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.knowledge.move_selection(1);
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                // 待确认态下 `Esc` 先取消确认，不直接退出页面（避免手滑连带退出）。
                if self.knowledge.confirming_clear() {
                    self.knowledge.confirming_clear = false;
                    self.knowledge.status = "已取消清除。".to_string();
                    return None;
                }
                self.back_to_list();
                None
            }
            KeyCode::Right => {
                self.toggle_knowledge_row();
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_knowledge_row(),
            KeyCode::Char('d') | KeyCode::Char('D') => self.request_clear_knowledge(),
            KeyCode::Char('o') | KeyCode::Char('O') => Some(SettingsEvent::OpenKnowledgeFolder),
            _ => None,
        }
    }

    /// `←→`：开关翻面；操作行不响应。
    fn toggle_knowledge_row(&mut self) {
        if self.knowledge.selected_row() == KnowledgeRow::Enabled {
            let enabled = !self.knowledge.enabled;
            self.knowledge.enabled = enabled;
            self.knowledge.status = if enabled {
                "知识库已开启。".to_string()
            } else {
                "知识库已关闭。".to_string()
            };
        }
    }

    /// `Enter`/`空格`：开关交给宿主写盘，操作行触发或进入确认。
    fn activate_knowledge_row(&mut self) -> Option<SettingsEvent> {
        match self.knowledge.selected_row() {
            KnowledgeRow::Enabled => {
                let enabled = !self.knowledge.enabled;
                self.knowledge.enabled = enabled;
                Some(SettingsEvent::Apply(SettingsChange::Feature {
                    key: "knowledge".to_string(),
                    enabled,
                }))
            }
            KnowledgeRow::ClearAll => self.request_clear_knowledge(),
            KnowledgeRow::OpenFolder => Some(SettingsEvent::OpenKnowledgeFolder),
        }
    }

    /// 清除的二次确认：第一次只转确认态，第二次才真的发出。
    fn request_clear_knowledge(&mut self) -> Option<SettingsEvent> {
        if !self.knowledge.confirming_clear() {
            self.knowledge.confirming_clear = true;
            self.knowledge.status = "清除所有知识库不可撤销：再按 Enter 或 D 确认。".to_string();
            return None;
        }
        self.knowledge.confirming_clear = false;
        Some(SettingsEvent::ClearAllKnowledge)
    }
}
