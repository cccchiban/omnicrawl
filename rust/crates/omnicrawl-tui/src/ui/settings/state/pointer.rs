//! 指针命中：一帧的命中区登记、悬停与点击分发。
//!
//! 界面每帧重建命中表，鼠标语义（悬停高亮、点击进页）只在这里判定。

use super::*;

impl SettingsState {
    /// 开始一帧渲染：丢掉上一帧记录的可点区域。
    pub fn begin_frame(&self) {
        self.hits.borrow_mut().clear();
    }

    /// 记下一块可点区域（只在渲染时调用；空区域不记）。
    pub fn record_hit(&self, area: Rect, action: HitAction) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.hits.borrow_mut().push(HitRegion { area, action });
    }

    /// 按落点反查动作；**后画的优先**——展开的候选浮层画在面板之上，应当先命中它。
    pub fn hit_at(&self, column: u16, row: u16) -> Option<HitAction> {
        self.hits
            .borrow()
            .iter()
            .rev()
            .find(|region| {
                column >= region.area.x
                    && column < region.area.x + region.area.width
                    && row >= region.area.y
                    && row < region.area.y + region.area.height
            })
            .map(|region| region.action)
    }

    /// 记录鼠标悬停的行；`None` 表示光标已离开可点区域。
    pub fn set_hover(&mut self, action: Option<HitAction>) {
        self.hover = action;
    }

    pub fn hover(&self) -> Option<HitAction> {
        self.hover
    }

    /// 悬停行在当前帧里的区域（用于描边加亮）；该行本帧没画出来时为 `None`。
    pub fn hover_area(&self) -> Option<Rect> {
        let hover = self.hover?;
        self.hits
            .borrow()
            .iter()
            .rev()
            .find(|region| region.action == hover)
            .map(|region| region.area)
    }

    /// 左键点击分派。
    ///
    /// - 左栏：单击即切页（Python 的列表点击是选中并立即进入，且切页不写配置）；
    /// - 右栏：先把该页的选中行 / 字段移到落点，**再点当前行**才等同 `Enter`
    ///   （工具开关、枚举循环这类行被误触一次就是配置变更，所以不做单击即改）；
    /// - 候选浮层：单击即选中该候选并确认（浮层本来就是为这一次选择而展开的）。
    pub fn click(&mut self, action: HitAction) -> Option<SettingsEvent> {
        match action {
            HitAction::Row(index) => {
                self.select_row(index);
                self.handle_list_key(KeyCode::Enter)
            }
            HitAction::PaneRow(index) => {
                if self.select_pane_row(index) {
                    return self.handle_key(KeyCode::Enter);
                }
                None
            }
            HitAction::Option(index) => {
                // 浮层下拉（模型管理页 / 表单页 / 视觉页）与渠道表单的内联候选
                // 是两套结构：前者记在 `self.dropdown`，后者记在渠道表单里。
                if self.dropdown.is_some() {
                    let count = self.dropdown_options().len();
                    if count == 0 {
                        return None;
                    }
                    if let Some(dropdown) = self.dropdown.as_mut() {
                        dropdown.selected = index.min(count - 1);
                    }
                } else if let Some(form) = self.model_management.form.as_mut() {
                    let count = form.dropdown.as_ref().map(|d| d.options.len()).unwrap_or(0);
                    if count == 0 {
                        return None;
                    }
                    let dropdown = form.dropdown.as_mut()?;
                    dropdown.selected = index.min(count - 1);
                } else {
                    return None;
                }
                self.handle_key(KeyCode::Enter)
            }
        }
    }

    /// 滚轮：等价于按一次 `↑` / `↓`。
    ///
    /// 各页的可见窗口本来就跟着选中项走（`window_offset`），因此滚动只需移动选中项：
    /// 按方向键走哪条分支，滚轮就走哪条，不另开一套滚动偏移——否则键盘与滚轮会各滚各的。
    /// 正在编辑文本时不动：那里 `↑`/`↓` 是挪光标，把光标滚走不是用户想要的。
    pub fn scroll(&mut self, delta: isize) {
        if delta == 0 || self.editing_text() {
            return;
        }
        let key = if delta < 0 {
            KeyCode::Up
        } else {
            KeyCode::Down
        };
        // 一格滚轮最多折算 `SCROLL_MAX_STEPS` 行：长列表里一次猛滚不该跳过整屏。
        let repeats = delta.unsigned_abs().min(SCROLL_MAX_STEPS);
        for _ in 0..repeats {
            self.handle_key(key);
        }
    }

    /// 当前是否正在某个文本字段里编辑。
    fn editing_text(&self) -> bool {
        match self.pane {
            Pane::Tts => self.tts.input.is_some(),
            Pane::Form(_) => self.form().is_some_and(|form| form.input().is_some()),
            Pane::ModelManagement => self
                .model_management
                .form
                .as_ref()
                .is_some_and(|form| form.input.is_some()),
            Pane::Mcp => self
                .mcp
                .editor
                .as_ref()
                .is_some_and(|editor| editor.input.is_some()),
            _ => false,
        }
    }
}
