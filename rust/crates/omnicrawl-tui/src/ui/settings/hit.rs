//! 设置面板的鼠标命中：渲染时把「可点区域 → 动作」记进状态，事件处理时按落点反查。
//!
//! 为什么不另写一套几何计算：设置面板一页一种布局（单行列表、字段框、浮层下拉、
//! 表单字段），几何散在 `render.rs` 的十几个函数里；把命中区域**渲染时**记下来，
//! 与 `ui::layout` 的「渲染与命中共用同一套区域计算」是同一条思路，但省掉了把
//! 每页布局参数再描述一遍的重复，也不会因为两处几何不同步而点错行。

use ratatui::layout::Rect;

/// 一次点击或悬停对应的语义动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitAction {
    /// 左栏一级项（点击 = 切到该页）。
    Row(usize),
    /// 右栏自上而下的第 N 个可点行 / 字段（语义由当前页决定）。
    PaneRow(usize),
    /// 展开的候选浮层里的第 N 项。
    Option(usize),
}

/// 一块可点区域（行号是屏幕绝对行，命中判定不用再做换算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitRegion {
    pub area: Rect,
    pub action: HitAction,
}
