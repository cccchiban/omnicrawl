//! 提问建议页：上方一个开关，下方 Skill 列表（选中回车看描述详情）。
//!
//! 开关选中即写盘；Skill 列表只是给用户看「本机装了哪些 Skill、各自能做什么」，
//! 详情把描述按面板宽度折行铺开，一次显示 [`SUGGESTION_DETAIL_ROWS`] 行，其余靠 ↑↓/滚轮翻。

use super::*;

/// 开关行在整页行序里的下标（Skill 行从它之后开始）。
const SWITCH_ROW: usize = 0;

impl SettingsState {
    /// `↑`/`↓` 选行；开关行上 `←`/`→`/`Enter`/空格 切换；Skill 行上 `Enter` 看详情；`Esc` 返回。
    pub(super) fn handle_suggestion_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.suggestion.detail.is_some() {
            return self.handle_suggestion_detail_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_suggestion(-1);
                None
            }
            KeyCode::Down => {
                self.move_suggestion(1);
                None
            }
            KeyCode::Left | KeyCode::Right if self.suggestion_on_switch() => {
                self.toggle_suggestion()
            }
            KeyCode::Char(' ') if self.suggestion_on_switch() => self.toggle_suggestion(),
            KeyCode::Enter => {
                if self.suggestion_on_switch() {
                    return self.toggle_suggestion();
                }
                self.open_suggestion_detail();
                None
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    /// 详情态：`↑`/`↓` 滚动描述，`Esc` 回列表。
    fn handle_suggestion_detail_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => self.scroll_suggestion_detail(-1),
            KeyCode::Down => self.scroll_suggestion_detail(1),
            KeyCode::Esc | KeyCode::Left => self.close_suggestion_detail(),
            _ => {}
        }
        None
    }

    pub(super) fn move_suggestion(&mut self, delta: isize) {
        let count = self.suggestion_row_count() as isize;
        if count == 0 {
            return;
        }
        self.suggestion.selected =
            ((self.suggestion.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 选中行是不是开关行。
    pub(super) fn suggestion_on_switch(&self) -> bool {
        self.suggestion.selected == SWITCH_ROW
    }

    /// 整页的行数：开关行 + Skill 列表。
    pub fn suggestion_row_count(&self) -> usize {
        SWITCH_ROW + 1 + self.suggestion.skills.len()
    }

    /// 翻转开关：产出的是**目标状态**而不是「翻转」，宿主拿到就能直接写盘。
    pub(super) fn toggle_suggestion(&mut self) -> Option<SettingsEvent> {
        Some(SettingsEvent::Apply(SettingsChange::SuggestionSwitch {
            enabled: !self.suggestion.enabled,
        }))
    }

    /// 打开选中 Skill 的详情（滚动位置归零）。
    pub(super) fn open_suggestion_detail(&mut self) {
        let Some(index) = self.suggestion.selected.checked_sub(SWITCH_ROW + 1) else {
            return;
        };
        if index >= self.suggestion.skills.len() {
            return;
        }
        self.suggestion.detail = Some((index, 0));
    }

    pub(super) fn close_suggestion_detail(&mut self) {
        self.suggestion.detail = None;
    }

    /// 详情滚动：按最后渲染出的总行数夹住，滚到末行就停。
    pub fn scroll_suggestion_detail(&mut self, delta: isize) {
        let total = self.suggestion_detail_total.get();
        let limit = total.saturating_sub(SUGGESTION_DETAIL_ROWS);
        let Some((_, scroll)) = self.suggestion.detail.as_mut() else {
            return;
        };
        *scroll = (*scroll as isize + delta).clamp(0, limit as isize) as usize;
    }

    /// 已打开的详情对应哪个 Skill。
    pub fn suggestion_detail_skill(&self) -> Option<&SuggestionSkillRow> {
        let (index, _) = self.suggestion.detail?;
        self.suggestion.skills.get(index)
    }

    /// 详情当前的滚动起点。
    pub fn suggestion_detail_scroll(&self) -> usize {
        self.suggestion
            .detail
            .map(|(_, scroll)| scroll)
            .unwrap_or(0)
    }

    /// 渲染层回报本次详情正文折行后的总行数（决定滚到底停在哪一行）。
    pub fn set_suggestion_detail_total(&self, total: usize) {
        self.suggestion_detail_total.set(total);
    }
}
