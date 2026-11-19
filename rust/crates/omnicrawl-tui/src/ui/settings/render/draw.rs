use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType};
use ratatui::Frame;

use crate::ui::fullscreen::terminal::theme;

use super::super::state::SettingsState;
use super::{MENU_TEXT_COLOR, PROGRESS_EMPTY, PROGRESS_FILLED};

pub(super) fn menu_text_style() -> Style {
    theme::rich_style(MENU_TEXT_COLOR)
}

/// 设置页正文（字段标签与取值）：与菜单同一份纯白。
///
/// 标签原先用 `TEXT_MUTED`（dim 修饰符）弱化，取值用 `TEXT_PRIMARY`（`default`）——两者都
/// 交给终端调色板，深色主题下常被渲染成灰。用户要求整页可读，因此正文统一纯白，
/// 主次改由琥珀高亮、加粗标题与缩进承担。提示行、空态、省略号仍保持弱化。
pub(super) fn body_text_style() -> Style {
    theme::rich_style(MENU_TEXT_COLOR)
}

/// 左右两栏各自的内边距（对映 CSS 的 `padding: 0 1`）。
pub(super) fn inset_horizontal(area: Rect, amount: u16) -> Rect {
    if area.width <= amount * 2 {
        return area;
    }
    Rect {
        x: area.x + amount,
        y: area.y,
        width: area.width - amount * 2,
        height: area.height,
    }
}

/// 框内区域：去掉边框，再按 `padding` 内缩。
pub(super) fn box_inner(area: Rect, padding: u16) -> Rect {
    let inset = 1 + padding;
    if area.width <= inset * 2 || area.height <= inset * 2 {
        return Rect {
            x: area.x,
            y: area.y,
            width: 0,
            height: 0,
        };
    }
    Rect {
        x: area.x + inset,
        y: area.y + inset,
        width: area.width - inset * 2,
        height: area.height - inset * 2,
    }
}

/// 画一栏的圆角框；聚焦时四角换成指向框内的准星箭头。
pub(super) fn draw_box(frame: &mut Frame, area: Rect, focused: bool, hint: Option<&str>) {
    let style = if focused {
        theme::rich_style(theme::ACCENT_AMBER)
    } else {
        theme::rich_style(theme::BORDER_STRONG)
    };
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(style);
    // 操作提示画在**下边框上**（`------- ↑↓ 选择 ←→/Enter/空格 切换 --------`），
    // 灰色弱化：不再单占框内一行，也不再有页面底部那行全局帮助。
    if let Some(hint) = hint.filter(|hint| !hint.trim().is_empty()) {
        block = block.title_bottom(Line::styled(
            format!(" {hint} "),
            theme::rich_style(theme::TEXT_MUTED),
        ));
    }
    frame.render_widget(block, area);
    if focused && area.width >= 2 && area.height >= 2 {
        paint_crosshair(frame.buffer_mut(), area, style);
    }
}

/// 把四角换成准星箭头：左上 ⇘、右上 ⇙、左下 ⇗、右下 ⇖（都指向框内）。
pub(super) fn paint_crosshair(buffer: &mut Buffer, area: Rect, style: Style) {
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    for (x, y, glyph) in [
        (area.x, area.y, "⇘"),
        (right, area.y, "⇙"),
        (area.x, bottom, "⇗"),
        (right, bottom, "⇖"),
    ] {
        buffer.set_string(x, y, glyph, style);
    }
}

/// 列表里第 `row` 行（相对窗口起点）的命中区。
///
/// 各页的行高都是 1，只是起点不同；调用方传「已经画到第几行」即可，不必再算一次布局。
pub(super) fn row_hit(area: Rect, row: usize) -> Rect {
    Rect {
        x: area.x,
        y: area.y + row as u16,
        width: area.width,
        height: 1,
    }
}

/// 悬停加亮：把鼠标所在行的整行加上下划线。
///
/// 直接改渲染结果而不是在十几个面板渲染函数里各写一遍「这行是不是被悬停」——
/// 行区域取的是同一帧记下来的命中区，因此不会指错行；颜色不动，避免与选中态的
/// 琥珀色、面板自己的取色打架。
pub(super) fn paint_hover(frame: &mut Frame, state: &SettingsState) {
    let Some(area) = state.hover_area() else {
        return;
    };
    // 悬停不再画下划线（用户要求），改成黄色字体。
    let hover = theme::rich_style(theme::ACCENT_AMBER).fg;
    let buffer = frame.buffer_mut();
    let bottom = area.y.saturating_add(area.height).min(buffer.area.height);
    let right = area.x.saturating_add(area.width).min(buffer.area.width);
    for y in area.y..bottom {
        for x in area.x..right {
            let cell = &mut buffer[(x, y)];
            if let Some(color) = hover {
                cell.set_fg(color);
            }
        }
    }
}

/// 画一条进度条：比例已知时左满右空，未知时画沿轨道循环滑动的滑块。
///
/// `phase` 是滑块进度（列），由 [`progress_phase`] 从墙上时钟算出，
/// 因此界面不必自己保存动画状态，纯函数也便于测试。
pub(super) fn progress_bar_text(ratio: Option<f64>, width: usize, phase: usize) -> String {
    if width == 0 {
        return String::new();
    }
    match ratio {
        Some(ratio) => {
            let filled = ((ratio * width as f64).round() as usize).min(width);
            format!(
                "{}{}",
                PROGRESS_FILLED.repeat(filled),
                PROGRESS_EMPTY.repeat(width - filled)
            )
        }
        None => {
            // 循环滑动：滑块宽约 1/4，左端从 `-slider` 推进到 `width`，滑出去后重新进入。
            let slider = (width / 4).max(1);
            let offset = phase % (width + slider);
            let mut text = String::new();
            for column in 0..width {
                let inside = column < offset && column + slider >= offset;
                text.push_str(if inside {
                    PROGRESS_FILLED
                } else {
                    PROGRESS_EMPTY
                });
            }
            text
        }
    }
}

/// 滑块当前所在的列（每 120ms 挪一列：够明显，又不至于闪烁）。
pub(super) fn progress_phase() -> usize {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    (millis / 120) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_fills_by_ratio() {
        assert_eq!(progress_bar_text(Some(0.5), 4, 0), "██░░");
        assert_eq!(progress_bar_text(Some(0.0), 4, 0), "░░░░");
        assert_eq!(progress_bar_text(Some(1.0), 4, 0), "████");
        assert_eq!(progress_bar_text(Some(0.0), 0, 0), "", "零宽没有条");
        // 比例越界时不能画超宽。
        assert_eq!(progress_bar_text(Some(1.5), 3, 0), "███");
    }

    #[test]
    fn unknown_progress_slides_and_wraps() {
        let width = 8;
        let slider = (width / 4).max(1);
        let cycle = width + slider;
        // 一个周期内：滑块始终只占 slider 列，且从左侧滑出、右侧滑入。
        for phase in 0..cycle {
            let text = progress_bar_text(None, width, phase);
            assert_eq!(text.chars().count(), width, "宽度恒等于轨道宽");
            let filled = text.matches(PROGRESS_FILLED).count();
            assert!(filled <= slider + 1, "phase {phase} 填充过宽：{text}");
        }
        // 起点与周期末尾位置不同：确实在动。
        assert_ne!(
            progress_bar_text(None, width, 0),
            progress_bar_text(None, width, slider)
        );
        assert_eq!(
            progress_bar_text(None, width, 0),
            progress_bar_text(None, width, cycle),
            "滑过一个周期回到原位"
        );
    }

    #[test]
    fn progress_phase_advances_with_the_clock() {
        let first = progress_phase();
        assert!(first > 0, "墙上时钟换算出的相位应为正");
    }
}
