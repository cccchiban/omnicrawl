//! 记忆功能页的键盘处理。
//!
//! 键位：`↑↓` 选行、`←→`/`Enter`/`空格` 切换或换档（`←` 在操作行上是返回）、`D` 清空
//! （连按两次确认）、`O` 打开目录、`Esc` 返回。
//!
//! 两类行为分开：配置行产出 [`SettingsChange`]（宿主写盘），操作行产出
//! [`SettingsEvent`]（宿主执行不可逆动作）。清空要按两次才真正发出。

use super::*;

impl SettingsState {
    /// 记忆功能页：开关、遗忘时间与两个操作行。
    pub(super) fn handle_memory_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.memory.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.memory.move_selection(1);
                None
            }
            KeyCode::Esc => {
                // 待确认态下 `Esc` 先取消确认，不直接退出页面（避免手滑连带退出）。
                if self.memory.confirming_clear() {
                    self.memory.confirming_clear = false;
                    self.memory.status = "已取消清空。".to_string();
                    return None;
                }
                self.back_to_list();
                None
            }
            // `←`/`→` 是配置行的减/加档；只有操作行上才当返回用。
            // 与 `Esc` 同一条规矩：离开前先取消待确认，别把「已请求清空」带回来。
            KeyCode::Left => {
                if self.memory.confirming_clear() {
                    self.memory.confirming_clear = false;
                    self.memory.status = "已取消清空。".to_string();
                    return None;
                }
                if self.memory.selected_row().is_setting() {
                    return self.cycle_memory_row(-1);
                }
                self.back_to_list();
                None
            }
            KeyCode::Right => self.cycle_memory_row(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_memory_row(),
            KeyCode::Char('d') | KeyCode::Char('D') => self.request_clear_all(),
            KeyCode::Char('o') | KeyCode::Char('O') => {
                Some(SettingsEvent::OpenMemoryFolder)
            }
            _ => None,
        }
    }

    /// `←→`：开关翻面、遗忘时间换档；操作行不响应。
    ///
    /// 改到配置后立刻交给宿主写盘：只在本地翻转草稿会让界面显示的状态与实际配置脱节，
    /// 而且随后按 `Enter` 会按「取反」再提交一次，等于把刚显示的状态又翻回去。
    fn cycle_memory_row(&mut self, delta: isize) -> Option<SettingsEvent> {
        match self.memory.selected_row() {
            MemoryRow::Enabled => {
                let enabled = !self.memory.enabled;
                self.memory.enabled = enabled;
                self.memory.status = if enabled {
                    "记忆功能已开启。".to_string()
                } else {
                    "记忆功能已关闭。".to_string()
                };
                Some(SettingsEvent::Apply(SettingsChange::Feature {
                    key: "memory".to_string(),
                    enabled,
                }))
            }
            MemoryRow::ForgetAfter => {
                let days = self.memory.cycle_forget_after(delta);
                self.memory.status = format!(
                    "记忆遗忘时间已改为{}。",
                    omnicrawl_config::features::memory::forget_after_label(days)
                );
                Some(SettingsEvent::Apply(SettingsChange::MemoryForgetAfter { days }))
            }
            MemoryRow::ClearAll | MemoryRow::OpenFolder => None,
        }
    }

    /// `Enter`/`空格`：配置行交给宿主写盘，操作行触发或进入确认。
    fn activate_memory_row(&mut self) -> Option<SettingsEvent> {
        match self.memory.selected_row() {
            MemoryRow::Enabled => {
                let enabled = !self.memory.enabled;
                self.memory.enabled = enabled;
                Some(SettingsEvent::Apply(SettingsChange::Feature {
                    key: "memory".to_string(),
                    enabled,
                }))
            }
            MemoryRow::ForgetAfter => Some(SettingsEvent::Apply(SettingsChange::MemoryForgetAfter {
                days: self.memory.forget_after_days,
            })),
            MemoryRow::ClearAll => self.request_clear_all(),
            MemoryRow::OpenFolder => Some(SettingsEvent::OpenMemoryFolder),
        }
    }

    /// 清空的二次确认：第一次只转确认态，第二次才真的发出。
    fn request_clear_all(&mut self) -> Option<SettingsEvent> {
        if !self.memory.confirming_clear() {
            self.memory.confirming_clear = true;
            self.memory.status = "消除所有记忆不可撤销：再按 Enter 或 D 确认。".to_string();
            return None;
        }
        self.memory.confirming_clear = false;
        Some(SettingsEvent::ClearAllMemories)
    }
}
