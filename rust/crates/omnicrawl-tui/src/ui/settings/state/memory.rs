//! 记忆功能页：开关、遗忘时间，以及「消除所有记忆」「打开记忆文件夹」两个操作。
//!
//! 这一页混了三类东西，因此不是单纯的 `Choice` 页：
//!
//! - **开关**（`[memory] enabled`）与**遗忘时间**（`[memory] forget_after_days`）是配置项，
//!   选中即写盘，与其余设置页一致；
//! - **两个操作**不改配置：清空记忆动的是三类作用域的存储，打开文件夹动的是本机文件管理器。
//!   两者都由宿主执行（界面层不碰文件系统，也不碰记忆存储），事件走
//!   [`SettingsEvent::ClearAllMemories`] 与 [`SettingsEvent::OpenMemoryFolder`]。
//!
//! 清空不可逆，因此与「删除会话」同样按两次才发出：第一次按 `D` 进入待确认，再按一次才执行；
//! `Esc` 或移动选择取消待确认。

/// 页面里的一行（顺序即界面顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRow {
    /// 记忆功能开关。
    Enabled,
    /// 遗忘时间。
    ForgetAfter,
    /// 消除所有记忆。
    ClearAll,
    /// 打开记忆文件夹。
    OpenFolder,
}

impl MemoryRow {
    /// 行序。
    pub const ORDER: [MemoryRow; 4] = [
        MemoryRow::Enabled,
        MemoryRow::ForgetAfter,
        MemoryRow::ClearAll,
        MemoryRow::OpenFolder,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Enabled => "记忆功能",
            Self::ForgetAfter => "记忆遗忘时间",
            Self::ClearAll => "消除所有记忆",
            Self::OpenFolder => "打开记忆文件夹",
        }
    }

    /// 是否为配置项（可以左右换档）；操作行只能按 Enter 触发。
    pub fn is_setting(self) -> bool {
        matches!(self, Self::Enabled | Self::ForgetAfter)
    }

    pub fn from_index(index: usize) -> MemoryRow {
        Self::ORDER[index.min(Self::ORDER.len() - 1)]
    }
}

#[derive(Debug, Default)]
pub struct MemoryState {
    /// 记忆功能是否开启。
    pub(crate) enabled: bool,
    /// 遗忘时间（天）。
    pub(crate) forget_after_days: i64,
    /// 当前选中的行。
    pub(crate) selected: usize,
    /// 是否处于「消除所有记忆」的待确认态。
    pub(crate) confirming_clear: bool,
    /// 最近一次操作的结果文案。
    pub status: String,
}

impl MemoryState {
    /// 记忆功能是否开启。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn forget_after_days(&self) -> i64 {
        self.forget_after_days
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// 当前选中的行。
    pub fn selected_row(&self) -> MemoryRow {
        MemoryRow::from_index(self.selected)
    }

    pub fn confirming_clear(&self) -> bool {
        self.confirming_clear
    }

    /// 从配置回填（打开页面时调用）。
    pub fn sync(&mut self, enabled: bool, forget_after_days: i64) {
        self.enabled = enabled;
        self.forget_after_days = forget_after_days;
        self.confirming_clear = false;
    }

    /// 上下移动；移动即取消待确认，避免确认的目标与当前选中项不一致。
    pub fn move_selection(&mut self, delta: isize) {
        self.confirming_clear = false;
        let count = MemoryRow::ORDER.len() as isize;
        let next = (self.selected as isize + delta).clamp(0, count - 1);
        self.selected = next as usize;
    }

    /// 在遗忘时间的四个候选里换档：`delta` 为 ±1。
    pub fn cycle_forget_after(&mut self, delta: isize) -> i64 {
        let choices = omnicrawl_config::features::memory::FORGET_AFTER_DAYS_CHOICES;
        let current = choices
            .iter()
            .position(|choice| *choice == self.forget_after_days)
            .unwrap_or(choices.len() - 1) as isize;
        let next = (current + delta).clamp(0, choices.len() as isize - 1) as usize;
        self.forget_after_days = choices[next];
        self.forget_after_days
    }

    /// 该行的展示值（右对齐前的文本）。
    pub fn row_value(&self, row: MemoryRow) -> String {
        match row {
            MemoryRow::Enabled => {
                if self.enabled {
                    "已开启".to_string()
                } else {
                    "已关闭".to_string()
                }
            }
            MemoryRow::ForgetAfter => {
                omnicrawl_config::features::memory::forget_after_label(self.forget_after_days)
            }
            MemoryRow::ClearAll => {
                if self.confirming_clear {
                    "再按 Enter 确认".to_string()
                } else {
                    "不可撤销".to_string()
                }
            }
            MemoryRow::OpenFolder => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_lists_four_rows_in_order() {
        let labels: Vec<&str> = MemoryRow::ORDER.iter().map(|row| row.label()).collect();
        assert_eq!(
            labels,
            vec!["记忆功能", "记忆遗忘时间", "消除所有记忆", "打开记忆文件夹"]
        );
    }

    #[test]
    fn only_the_first_two_rows_are_settings() {
        assert!(MemoryRow::Enabled.is_setting());
        assert!(MemoryRow::ForgetAfter.is_setting());
        assert!(!MemoryRow::ClearAll.is_setting());
        assert!(!MemoryRow::OpenFolder.is_setting());
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut state = MemoryState::default();
        state.move_selection(-1);
        assert_eq!(state.selected(), 0);
        for _ in 0..10 {
            state.move_selection(1);
        }
        assert_eq!(state.selected(), MemoryRow::ORDER.len() - 1);
    }

    #[test]
    fn forget_after_cycles_through_the_four_choices() {
        let mut state = MemoryState::default();
        state.sync(true, 1);
        assert_eq!(state.cycle_forget_after(1), 10);
        assert_eq!(state.cycle_forget_after(1), 30);
        assert_eq!(state.cycle_forget_after(1), 365);
        // 到头就停住，不回绕。
        assert_eq!(state.cycle_forget_after(1), 365);
        assert_eq!(state.cycle_forget_after(-1), 30);
    }

    #[test]
    fn moving_the_cursor_cancels_the_pending_clear() {
        let mut state = MemoryState::default();
        state.confirming_clear = true;
        state.move_selection(1);
        assert!(!state.confirming_clear());
    }

    #[test]
    fn the_clear_row_warns_that_it_is_pending() {
        let mut state = MemoryState::default();
        assert_eq!(state.row_value(MemoryRow::ClearAll), "不可撤销");
        state.confirming_clear = true;
        assert_eq!(state.row_value(MemoryRow::ClearAll), "再按 Enter 确认");
    }

    #[test]
    fn the_forget_row_shows_a_human_label() {
        let mut state = MemoryState::default();
        state.sync(true, 30);
        assert_eq!(state.row_value(MemoryRow::ForgetAfter), "一月");
        state.sync(true, 1);
        assert_eq!(state.row_value(MemoryRow::ForgetAfter), "一天");
    }
}
