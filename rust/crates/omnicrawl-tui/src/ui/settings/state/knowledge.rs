//! 知识库页：开关，以及「清除所有知识库」「打开知识库文件夹」两个操作。
//!
//! 与记忆页同构，只是少了遗忘时间那一档：
//!
//! - **开关**（`[knowledge] enabled`）是配置项，选中即写盘；它决定 `kb_*` 工具组是否进表；
//! - **两个操作**不改配置：清空删的是知识库根目录下的笔记，打开文件夹动的是本机文件管理器。
//!   两者都由宿主执行（界面层不碰文件系统），事件走 [`SettingsEvent::ClearAllKnowledge`]
//!   与 [`SettingsEvent::OpenKnowledgeFolder`]。
//!
//! 清空不可逆，因此与记忆页同样按两次才发出：第一次按 `D` 进入待确认，再按一次才执行。

/// 页面里的一行（顺序即界面顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeRow {
    /// 知识库开关。
    Enabled,
    /// 清除所有知识库。
    ClearAll,
    /// 打开知识库文件夹。
    OpenFolder,
}

impl KnowledgeRow {
    /// 行序。
    pub const ORDER: [KnowledgeRow; 3] = [
        KnowledgeRow::Enabled,
        KnowledgeRow::ClearAll,
        KnowledgeRow::OpenFolder,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Enabled => "知识库",
            Self::ClearAll => "清除所有知识库",
            Self::OpenFolder => "打开知识库文件夹",
        }
    }

    /// 是否为配置项（可以左右换档）；操作行只能按 Enter 触发。
    pub fn is_setting(self) -> bool {
        matches!(self, Self::Enabled)
    }

    pub fn from_index(index: usize) -> KnowledgeRow {
        Self::ORDER[index.min(Self::ORDER.len() - 1)]
    }
}

#[derive(Debug, Default)]
pub struct KnowledgeState {
    /// 知识库是否开启。
    pub(crate) enabled: bool,
    /// 当前选中的行。
    pub(crate) selected: usize,
    /// 是否处于「清除所有知识库」的待确认态。
    pub(crate) confirming_clear: bool,
    /// 最近一次操作的结果文案。
    pub status: String,
}

impl KnowledgeState {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_row(&self) -> KnowledgeRow {
        KnowledgeRow::from_index(self.selected)
    }

    pub fn confirming_clear(&self) -> bool {
        self.confirming_clear
    }

    /// 从配置回填（打开页面时调用）。
    pub fn sync(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.confirming_clear = false;
    }

    /// 上下移动；移动即取消待确认，避免确认的目标与当前选中项不一致。
    pub fn move_selection(&mut self, delta: isize) {
        self.confirming_clear = false;
        let count = KnowledgeRow::ORDER.len() as isize;
        let next = (self.selected as isize + delta).clamp(0, count - 1);
        self.selected = next as usize;
    }

    /// 该行的展示值（右对齐前的文本）。
    pub fn row_value(&self, row: KnowledgeRow) -> String {
        match row {
            KnowledgeRow::Enabled => {
                if self.enabled {
                    "已开启".to_string()
                } else {
                    "已关闭".to_string()
                }
            }
            KnowledgeRow::ClearAll => {
                if self.confirming_clear {
                    "再按 Enter 确认".to_string()
                } else {
                    "不可撤销".to_string()
                }
            }
            KnowledgeRow::OpenFolder => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_lists_three_rows_in_order() {
        let labels: Vec<&str> = KnowledgeRow::ORDER.iter().map(|row| row.label()).collect();
        assert_eq!(labels, vec!["知识库", "清除所有知识库", "打开知识库文件夹"]);
    }

    #[test]
    fn only_the_first_row_is_a_setting() {
        assert!(KnowledgeRow::Enabled.is_setting());
        assert!(!KnowledgeRow::ClearAll.is_setting());
        assert!(!KnowledgeRow::OpenFolder.is_setting());
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut state = KnowledgeState::default();
        state.move_selection(-1);
        assert_eq!(state.selected(), 0);
        for _ in 0..10 {
            state.move_selection(1);
        }
        assert_eq!(state.selected(), KnowledgeRow::ORDER.len() - 1);
    }

    #[test]
    fn moving_the_cursor_cancels_the_pending_clear() {
        let mut state = KnowledgeState::default();
        state.confirming_clear = true;
        state.move_selection(1);
        assert!(!state.confirming_clear());
    }

    #[test]
    fn the_clear_row_warns_that_it_is_pending() {
        let mut state = KnowledgeState::default();
        assert_eq!(state.row_value(KnowledgeRow::ClearAll), "不可撤销");
        state.confirming_clear = true;
        assert_eq!(state.row_value(KnowledgeRow::ClearAll), "再按 Enter 确认");
    }

    #[test]
    fn the_switch_row_shows_the_current_state() {
        let mut state = KnowledgeState::default();
        state.sync(true);
        assert_eq!(state.row_value(KnowledgeRow::Enabled), "已开启");
        state.sync(false);
        assert_eq!(state.row_value(KnowledgeRow::Enabled), "已关闭");
    }
}
