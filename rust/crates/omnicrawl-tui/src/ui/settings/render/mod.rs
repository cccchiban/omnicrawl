//! 设置面板的渲染：全屏两区、准星焦点边框、候选下拉与状态行。
//!
//! 视觉对映 Python 的 `SettingsScreen.CSS` 与 `terminal_select_css`：
//! 左右两栏都是圆角框（`round $terminal-border-strong`），焦点所在的一栏换成
//! 准星边框（四角 `⇘ ⇙ ⇗ ⇖`，`$terminal-amber`）；右侧标题在框内缩进 2 格、
//! 加粗；展开的候选列表是白框浮层，高亮项用琥珀底色。

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::Frame;

use super::state::SettingsState;

mod draw;
mod forms;
pub mod helpers;
mod list;
mod overlay;
mod pages_decision;
mod pages_embedding;
mod pages_library;
mod pages_mcp;
mod pages_misc;
mod pages_model;
mod pages_tools;

use draw::{inset_horizontal, paint_hover};
use helpers::left_column_width;
use list::{render_pane, render_rows};

/// 左栏的期望宽度（对映 CSS 的 `width: 30`）。
pub(super) const LEFT_COLUMN_WIDTH: u16 = 30;
/// 左栏的最小宽度（对映 CSS 的 `min-width: 24`）。
pub(super) const LEFT_COLUMN_MIN_WIDTH: u16 = 24;
/// 右栏至少要留出的宽度，窄终端下左栏先收缩。
pub(super) const MIN_RIGHT_WIDTH: u16 = 20;
/// 两栏内边距（对映 CSS 的 `padding: 0 1`）。
pub(super) const COLUMN_PADDING: u16 = 1;
/// 展开的候选浮层最多显示这么多行（对映 CSS 的 `max-height: 12`，另有 2 行边框）。
pub(super) const DROPDOWN_MAX_ROWS: usize = 12;
/// 表单页底部留给「1 空行 + 2 行状态 + 1 行提示」的行数。
pub(super) const FORM_TAIL_HEIGHT: u16 = 4;

/// 菜单/列表项的纯白前景（sRGB，不随终端调色板走）。
///
/// 不用主题里的 `ACCENT_WHITE`（ANSI `white`）：不少终端把 ANSI 白映射成灰阶，只有
/// 「加粗」才被渲染成亮白——于是同一份颜色在标题（加粗）上是白的、在菜单（不加粗）上
/// 是灰的，看起来就像菜单掉了色。直接给 sRGB 纯白，不加粗也足够亮。
pub(super) const MENU_TEXT_COLOR: &str = "#ffffff";

/// 渲染整个设置面板（铺满终端，含底部帮助行）。
pub fn render(frame: &mut Frame, area: Rect, state: &SettingsState) {
    // 命中区每帧重建：鼠标落点必须对应当前画出来的那一帧。
    state.begin_frame();
    // 不再留底部的整行帮助（用户要求删除）：「↑↓ 选择…」这类提示统一画在右侧面板的
    // 下边框上（见 [`draw_box`]），左侧列表不需要提示。
    let main = area;
    let left_width = left_column_width(
        main.width,
        LEFT_COLUMN_WIDTH,
        LEFT_COLUMN_MIN_WIDTH,
        MIN_RIGHT_WIDTH,
    );
    let [left, right] = Layout::horizontal([
        Constraint::Length(left_width.min(main.width)),
        Constraint::Fill(1),
    ])
    .areas(main);

    render_rows(frame, inset_horizontal(left, COLUMN_PADDING), state);
    render_pane(frame, inset_horizontal(right, COLUMN_PADDING), state);
    paint_hover(frame, state);
}

/// 进度条的填充与空白格（单宽字符，保证宽度按列算得准）。
pub(super) const PROGRESS_FILLED: &str = "█";
pub(super) const PROGRESS_EMPTY: &str = "░";

/// 底部 4 行留给状态与提示（与工具页同款）。
pub(super) const MCP_TAIL_RESERVED: u16 = 4;
