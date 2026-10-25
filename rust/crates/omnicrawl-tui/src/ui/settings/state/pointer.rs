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
                // 浮层下拉（上下文页 / 单选页 / 表单页 / 视觉页）与渠道表单的内联候选
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
}
