//! 渲染辅助：窗口偏移与宽度计算，独立出来便于单测。

/// 让选中项始终可见的窗口起始下标。
pub fn window_offset(selected: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    selected
        .saturating_sub(height - 1)
        .min(len.saturating_sub(height))
}

/// 左栏宽度：期望 30，窄终端按「右栏至少 20 格 + 4 格内边距」收缩。
pub fn left_column_width(total: u16, preferred: u16, minimum: u16, min_right: u16) -> u16 {
    let available = total.saturating_sub(min_right + 4);
    if available >= preferred {
        preferred
    } else {
        available.max(minimum).min(total)
    }
}

/// 按显示宽度截断（复用 `ui::fit`）。
pub fn fit(text: &str, width: usize) -> String {
    crate::ui::fit(text, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_offset_keeps_selection_visible() {
        assert_eq!(window_offset(0, 10, 5), 0);
        assert_eq!(window_offset(3, 10, 5), 0);
        assert_eq!(window_offset(4, 10, 5), 0);
        assert_eq!(window_offset(5, 10, 5), 1);
        assert_eq!(window_offset(9, 10, 5), 5);
        assert_eq!(window_offset(3, 3, 5), 0, "装得下就不滚动");
        assert_eq!(window_offset(3, 10, 0), 0, "零高度不越界");
    }

    #[test]
    fn left_column_shrinks_only_on_narrow_terminals() {
        assert_eq!(left_column_width(120, 30, 24, 20), 30);
        assert_eq!(left_column_width(80, 30, 24, 20), 30);
        assert_eq!(
            left_column_width(54, 30, 24, 20),
            30,
            "刚好放得下 30 + 20 + 4"
        );
        assert_eq!(left_column_width(50, 30, 24, 20), 26);
        assert_eq!(left_column_width(40, 30, 24, 20), 24, "不低于最小宽度");
        assert_eq!(left_column_width(20, 30, 24, 20), 20, "极窄时不超过总宽");
    }
}
