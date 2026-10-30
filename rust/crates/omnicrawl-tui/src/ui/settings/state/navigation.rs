//! 导航：一级项与二级面板之间的移动。
//!
//! 「选中项 → 面板 → 焦点」三层状态的推进只在这里发生。

use super::*;

impl SettingsState {


    /// 左栏：把选中项移到第 N 项；复用键盘的逐项移动，保证切页时被清掉的
    /// 状态（收起下拉、上下文字段回到第一个）与按 `↑`/`↓` 完全一致。
    pub(super) fn select_row(&mut self, index: usize) {
        let count = ROW_ORDER.len();
        if index >= count || index == self.selected {
            return;
        }
        let mut guard = 0usize;
        while self.selected != index && guard <= count {
            let direction = if index > self.selected { 1 } else { -1 };
            self.move_selection(direction);
            guard += 1;
        }
    }


    /// 右栏：把当前页的选中行 / 字段移到第 N 个可点行。
    ///
    /// 返回 `true` 表示落点就是当前行（调用方可继续走 `Enter` 语义）。
    pub(super) fn select_pane_row(&mut self, index: usize) -> bool {
        self.dropdown = None;
        self.focus = Focus::Pane;
        match self.pane {
            Pane::Tools => {
                let target = index.min(self.tool_row_count().saturating_sub(1));
                let already = self.tools.selected == target;
                self.tools.selected = target;
                already
            }
            Pane::Subagents => {
                let target = index.min(self.subagents.rows.len().saturating_sub(1));
                let already = self.subagents.selected == target;
                self.subagents.selected = target;
                already
            }
            Pane::Vision => {
                let target = index.min(self.vision.models.len().saturating_sub(1));
                let already = self.vision.selected == target;
                self.vision.selected = target;
                already
            }
            Pane::Tts => {
                // 点击给的是屏幕行号，按当前可见行换算成配置行号（Gemini 下没有语速行）。
                let rows = self.tts.visible_rows();
                let Some(target) = rows.get(index).copied() else {
                    return false;
                };
                let already = self.tts.focused == target;
                self.tts.focused = target;
                already
            }
            Pane::ModelManagement => {
                // 列表态选渠道或参数行，表单态选字段（字段顺序就是 `ChannelField::ORDER`）。
                if let Some(form) = self.model_management.form.as_mut() {
                    let Some(field) = ChannelField::ORDER.get(index).copied() else {
                        return false;
                    };
                    let already = form.field == field;
                    form.field = field;
                    already
                } else {
                    let target = index.min(self.model_management.row_count().saturating_sub(1));
                    let already = self.model_management.selected == target;
                    self.model_management.selected = target;
                    already
                }
            }
            Pane::DecisionModels => {
                // 与渠道页同形：列表态选渠道或开关，表单态选字段。
                if let Some(form) = self.decision_models.form.as_mut() {
                    let Some(field) = DecisionField::ORDER.get(index).copied() else {
                        return false;
                    };
                    let already = form.field == field;
                    form.field = field;
                    already
                } else {
                    let target = index.min(self.decision_row_count().saturating_sub(1));
                    let already = self.decision_models.selected == target;
                    self.decision_models.selected = target;
                    already
                }
            }
            Pane::EmbeddingModels => {
                if let Some(form) = self.embedding_models.form.as_mut() {
                    let Some(field) = EmbeddingField::ORDER.get(index).copied() else {
                        return false;
                    };
                    let already = form.field == field;
                    form.field = field;
                    already
                } else {
                    let target = index.min(self.embedding_models.total_rows().saturating_sub(1));
                    let already = self.embedding_models.selected == target;
                    self.embedding_models.selected = target;
                    already
                }
            }
            Pane::Mcp => {
                if let Some(editor) = self.mcp.editor.as_mut() {
                    let already = editor.focused == index;
                    editor.focused = index;
                    already
                } else if self.mcp.servers_view {
                    let target = index.min(self.mcp.servers.len().saturating_sub(1));
                    let already = self.mcp.server_selected == target;
                    self.mcp.server_selected = target;
                    already
                } else {
                    let target = index.min(MCP_ROW_COUNT - 1);
                    let already = self.mcp.selected == target;
                    self.mcp.selected = target;
                    already
                }
            }
            Pane::Choice(_) => {
                // 单选页只有一个字段：点它就是「把候选展开」，于是直接走 Enter。
                self.focus = Focus::Pane;
                true
            }
            Pane::Form(_) => {
                let Some(form) = self.form_mut() else {
                    return false;
                };
                // 渲染给的是可见行号（隐藏字段不占行），要映射回字段下标；隐藏行不可聚焦。
                let Some(target) = form.visible_indices().get(index).copied() else {
                    return false;
                };
                let already = form.focused() == target;
                form.set_focused(target);
                already
            }
            Pane::Sessions => {
                let target = index.min(self.sessions.rows().len().saturating_sub(1));
                let already = self.sessions.selected() == target;
                if !already {
                    self.sessions.select(target);
                }
                already
            }
            Pane::Pending => false,
        }
    }


    /// 按一级项的键直接落到那一页（欢迎页徽标等鼠标入口用）；未知键不动。
    ///
    /// 与点左栏同义：选中项移过去、右侧挂上该页、焦点进右栏。
    pub fn select_key(&mut self, key: &str) {
        let Some(index) = ROW_ORDER.iter().position(|item| *item == key) else {
            return;
        };
        self.select_row(index);
        self.enter_pane();
    }


    pub fn selected(&self) -> usize {
        self.selected
    }


    pub fn selected_key(&self) -> &'static str {
        ROW_ORDER[self.selected]
    }


    pub fn focus(&self) -> Focus {
        self.focus
    }


    pub fn pane(&self) -> Pane {
        self.pane
    }


    pub fn title(&self) -> String {
        row_label(self.selected_key())
    }


    pub fn context_window_tokens(&self) -> i64 {
        self.model_management.window_tokens
    }


    pub fn compaction_percent(&self) -> i64 {
        self.model_management.percent
    }


    pub(super) fn move_selection(&mut self, delta: isize) {
        let count = ROW_ORDER.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(count)) as usize;
        let next = Pane::for_row(self.selected_key());
        if next != self.pane {
            self.pane = next;
            // 换面板等价于重新进入：收起下拉，选中行回到第一个。
            self.dropdown = None;
            self.model_management.selected = 0;
        }
    }


    /// 模型管理页列表态的行移动：渠道行与「模型与参数」分区行一起循环。
    pub(super) fn move_model_row(&mut self, delta: isize) {
        let count = self.model_management.row_count() as isize;
        if count <= 0 {
            return;
        }
        self.model_management.selected =
            ((self.model_management.selected as isize + delta).rem_euclid(count)) as usize;
        self.dropdown = None;
    }


    pub(super) fn enter_pane(&mut self) {
        self.focus = Focus::Pane;
        self.dropdown = None;
    }


    pub(super) fn back_to_list(&mut self) {
        self.focus = Focus::List;
        self.dropdown = None;
    }
}
