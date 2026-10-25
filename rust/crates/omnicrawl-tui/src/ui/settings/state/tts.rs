//! TTS 页的键盘处理与行激活。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    /// TTS 页：输入态 → 普通态两层。输入态优先，其余键位照行表走。
    pub(super) fn handle_tts_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.tts.input.is_some() {
            match key {
                KeyCode::Esc => self.tts.input = None,
                KeyCode::Enter => self.tts.commit_input(),
                KeyCode::Backspace => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.backspace();
                    }
                }
                KeyCode::Delete => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.delete();
                    }
                }
                KeyCode::Left => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_left();
                    }
                }
                KeyCode::Right => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_right();
                    }
                }
                KeyCode::Home => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_home();
                    }
                }
                KeyCode::End => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.move_end();
                    }
                }
                KeyCode::Char(ch) => {
                    if let Some((_, composer)) = self.tts.input.as_mut() {
                        composer.insert(&ch.to_string());
                    }
                }
                _ => {}
            }
            return None;
        }
        match key {
            KeyCode::Up => {
                self.tts.move_focus(-1);
                None
            }
            KeyCode::Down => {
                self.tts.move_focus(1);
                None
            }
            KeyCode::Left => {
                self.tts.cycle(self.tts.focused, -1);
                None
            }
            KeyCode::Right => {
                self.tts.cycle(self.tts.focused, 1);
                None
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_tts_row(),
            _ => None,
        }
    }

    /// `Enter` 触发选中行动作；切换类行就地翻，文本行进输入态，动作用产出事件。
    pub(super) fn activate_tts_row(&mut self) -> Option<SettingsEvent> {
        let row = self.tts.focused;
        match row {
            TTS_ROW_ENABLED
            | TTS_ROW_PROVIDER
            | TTS_ROW_AUTO_PLAY
            | TTS_ROW_SPEAK_REPLIES
            | TTS_ROW_VOICE
            | TTS_ROW_SPEED => {
                self.tts.cycle(row, 1);
                None
            }
            TTS_ROW_SAVE => Some(SettingsEvent::Apply(SettingsChange::Tts(TtsChange::Save(
                self.tts.draft(),
            )))),
            // 其余可见行（地址/模型/指令/API Key）是文本行：进输入态编辑。
            _ => {
                self.tts.begin_input(row);
                None
            }
        }
    }
}
