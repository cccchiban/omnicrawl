//! 底部单行 HUD（对映 Python `#bottom-carousel`）。
//!
//! 内容由对映层拼好：`fullscreen/status/hud.rs` 负责分段文本（遥测 + 工作区路径），
//! `fullscreen/status/indicators.rs` 把两段接成一行。本模块只做装配：把
//! [`AppState::hud_text`] 画成贴齐屏幕底缘的一行——左侧内边距与输入框卡片的左边框
//! 对齐（对映 CSS `padding: 0 1 0 0`），超宽按显示宽度以 `…` 收尾
//! （对映 `text-overflow: ellipsis`）。
//!
//! 不再轮播（留言页已移除）：模型与各开关改在欢迎 Logo 下方的会话徽标里显示。
//! 常驻底栏只保留会话态数据，末尾跟一段版本号（`… t/s ⁕ <工作区名> ⁕ v0.4.6`）；
//! 有新版本时版本号旁多一个淡黄色 ↑。工作区名是可点的一段：常色纯白，鼠标悬停
//! 变淡黄，点一下用系统文件管理器打开仓库根目录。
//!
//! [`AppState::hud_text`]: crate::state::AppState::hud_text

use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{display_width, truncate_styled};
use crate::state::AppState;
use crate::ui::fullscreen::status::hud::{WORKSPACE_HOVER_COLOR, WORKSPACE_LINK_COLOR};
use crate::ui::fullscreen::terminal::theme;
use crate::ui::fullscreen::text::StyledText;

/// 底部 HUD 的左内边距：内容与输入框卡片内的文字同列（对映 CSS `padding: 0 1 0 0`
/// 与 `#composer-wrap` 的左边框/内边距）。
const LEFT_PAD: usize = 1;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let hovered = state.hud_workspace_hover();
    let line = tint_workspace(line_of(&state.hud_text, area.width as usize), hovered);
    // 记下工作区名的屏幕矩形：鼠标据此悬停变淡黄、点击打开仓库文件夹。
    // 与 `[ ESC ]` 同一思路——渲染只拿得到 `&AppState`，位置由渲染路径回填。
    state.set_hud_workspace_area(workspace_area(&line, area, hovered));
    frame.render_widget(Paragraph::new(line), area);
}

/// 工作区名的样式：常色纯白，悬停淡黄色。渲染层靠它认出这一段。
fn workspace_color(hovered: bool) -> Color {
    theme::rich_style(if hovered {
        WORKSPACE_HOVER_COLOR
    } else {
        WORKSPACE_LINK_COLOR
    })
    .fg
    .unwrap_or(Color::Reset)
}

/// 悬停时把工作区段换色：只改那一段连续的前景，不重建整行。
///
/// 「连续」是必须的：版本号前的新版本 `↑` 与被悬停的工作区名同色，按颜色全改会把
/// 箭头也染一遍。
fn tint_workspace(mut line: Line<'static>, hovered: bool) -> Line<'static> {
    if !hovered {
        return line;
    }
    let link = workspace_color(false);
    let hover = workspace_color(true);
    let mut running = false;
    for span in line.spans.iter_mut() {
        if span.style.fg == Some(link) {
            span.style = span.style.fg(hover);
            running = true;
        } else if running {
            // 连续段到此结束：后面即使还有同色 span（升级箭头）也不动。
            break;
        }
    }
    line
}

/// 工作区名在本帧那一行里的矩形；这段整段被宽度截掉时为 `None`（那时点不到它）。
///
/// 按**实际画出来的**行量：工作区段独占一种前景色，截断之后剩几格就报几格，
/// 命中区与屏幕上的字永远对齐。
fn workspace_area(line: &Line<'static>, area: Rect, hovered: bool) -> Option<Rect> {
    let color = Some(workspace_color(hovered));
    let mut column = 0usize;
    let mut start = None;
    let mut end = 0usize;
    for span in &line.spans {
        let width = display_width(&span.content);
        if span.style.fg == color {
            start.get_or_insert(column);
            end = column + width;
        } else if start.is_some() {
            // 连续段结束了：后面即使还有同色段（升级箭头）也不属于工作区名。
            break;
        }
        column += width;
    }
    let (start, end) = (start?.max(LEFT_PAD), end.min(usize::from(area.width)));
    if end <= start {
        return None;
    }
    Some(Rect {
        x: area.x.saturating_add(start as u16),
        y: area.y,
        width: (end - start) as u16,
        height: 1,
    })
}

/// HUD 整行 → ratatui 行：左内边距 + 逐字符按显示宽度截断（带省略号）。
///
/// 纯函数，便于单测：给定任意 [`StyledText`] 与列宽都返回不超过该宽度的行。
pub fn line_of(text: &StyledText, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if width == 0 {
        return Line::from(spans);
    }
    spans.push(Span::raw(" ".repeat(LEFT_PAD.min(width))));
    if width <= LEFT_PAD {
        return Line::from(spans);
    }
    spans.extend(truncate_styled(text, width - LEFT_PAD));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::fullscreen::status::indicators::{hud_line_text, HudSource};
    use crate::ui::fullscreen::terminal::theme;

    fn telemetry_line(width: usize) -> Line<'static> {
        line_of(&hud_line_text(&source()), width)
    }

    fn source() -> HudSource {
        HudSource {
            workspace: "D:/work".to_string(),
            input_tokens: 1_200,
            output_tokens: 300,
            cached_input_tokens: 600,
            context_limit: 128_000,
            tokens_per_second: 12.3,
            version: "v0.4.6".to_string(),
            upgrade_available: false,
        }
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn telemetry_line_carries_usage_rate_workspace_and_version() {
        let text = line_text(&telemetry_line(120));
        assert!(text.contains("1.2K/128K 1%"), "{text}");
        assert!(text.contains("t/s"), "{text}");
        assert!(text.contains("⁕ D:/work"), "{text}");
        assert!(!text.contains("工作区"), "工作区前缀已删：{text}");
        assert!(text.contains("D:/work ⁕ v0.4.6"), "{text}");
        // 只查版本号前的箭头：遥测段的 `↑1.2K` 是上行 token 标记，同字形但无关。
        assert!(
            !text.contains("⁕ ↑v0.4.6"),
            "无更新时版本号前不画箭头：{text}"
        );
        // 模型与各开关已移到欢迎 Logo 下的会话徽标，底部不再重复。
        assert!(!text.contains("THINKING"), "{text}");
        assert!(!text.contains("MCP"), "{text}");
        assert!(!text.contains('│'), "底栏用 ⁕ 分隔，不用竖线：{text}");
    }

    #[test]
    fn upgrade_arrow_shows_next_to_the_version() {
        let source = HudSource {
            workspace: "D:/work".to_string(),
            version: "v0.4.6".to_string(),
            upgrade_available: true,
            ..HudSource::default()
        };
        let text = line_text(&line_of(&hud_line_text(&source), 120));
        assert!(text.contains("D:/work ⁕ ↑v0.4.6"), "{text}");
    }

    #[test]
    fn line_starts_with_the_left_padding_and_never_exceeds_width() {
        for width in [8usize, 20, 40, 120, 200] {
            let line = telemetry_line(width);
            let text = line_text(&line);
            assert!(
                display_width(&text) <= width,
                "宽度 {width} 下超宽：{text:?}"
            );
            if width > LEFT_PAD {
                assert!(text.starts_with(' '), "缺少左内边距：{text:?}");
            }
        }
    }

    #[test]
    fn overlong_text_is_ellipsized() {
        let mut text = StyledText::new();
        text.push(&"a".repeat(60), theme::TEXT_PRIMARY);
        let line = line_of(&text, 20);
        let rendered = line_text(&line);
        assert_eq!(display_width(&rendered), 20);
        assert!(rendered.ends_with('…'), "{rendered:?}");
        // 空宽度只保留内边距，不 panic。
        assert!(line_text(&line_of(&text, 0)).is_empty());
    }

    #[test]
    fn styles_survive_the_truncation() {
        let mut text = StyledText::new();
        text.push("问题", theme::ACCENT_RED);
        text.push(" ", theme::TEXT_MUTED);
        text.push("ok", theme::ACCENT_GREEN);
        let line = line_of(&text, 40);
        let styles: Vec<String> = line
            .spans
            .iter()
            .map(|span| format!("{:?}", span.style))
            .collect();
        assert_eq!(line.spans.len(), 4, "内边距 + 三段样式：{styles:?}");
    }

    /// 工作区名的命中区按实际画出的那一行量：整行里只有它是纯白，悬停后变淡黄。
    #[test]
    fn workspace_area_covers_the_white_workspace_span() {
        let area = Rect::new(0, 20, 120, 1);
        let line = line_of(&hud_line_text(&source()), 120);
        let region = workspace_area(&line, area, false).expect("工作区名在屏上");
        assert_eq!(region.y, 20);
        assert_eq!(region.height, 1);
        // 命中区里画出来的正是那一段路径（左右各扣 1 个空格）。
        assert_eq!(u16::from(region.width), 7, "D:/work 共 7 列");
        let text = line_text(&line);
        let start = usize::from(region.x);
        let slice: String = text
            .chars()
            .skip(start)
            .take(usize::from(region.width))
            .collect();
        assert_eq!(slice, "D:/work");
    }

    #[test]
    fn hovered_workspace_area_follows_the_amber_span() {
        let area = Rect::new(0, 20, 120, 1);
        let normal = line_of(&hud_line_text(&source()), 120);
        let hovered = tint_workspace(normal.clone(), true);
        assert_eq!(
            workspace_area(&hovered, area, false),
            None,
            "换色后不再有纯白段"
        );
        let region = workspace_area(&hovered, area, true).expect("淡黄段仍可命中");
        assert_eq!(u16::from(region.width), 7);
    }

    /// 工作区段被窄窗口整段截掉时没有命中区（那时屏幕上点不到它）。
    #[test]
    fn workspace_area_is_absent_when_the_span_is_truncated_away() {
        let area = Rect::new(0, 20, 12, 1);
        let line = line_of(&hud_line_text(&source()), 12);
        assert_eq!(workspace_area(&line, area, false), None);
    }

    /// 有新版本时版本号前的 ↑ 是淡黄色：悬停换色不能连它一起改，命中区也不能扩到那里。
    #[test]
    fn upgrade_arrow_is_not_part_of_the_workspace_link() {
        let area = Rect::new(0, 20, 120, 1);
        let mut upgraded = source();
        upgraded.upgrade_available = true;
        let line = line_of(&hud_line_text(&upgraded), 120);
        let hovered = tint_workspace(line, true);
        let region = workspace_area(&hovered, area, true).expect("工作区名在屏上");
        assert_eq!(u16::from(region.width), 7, "只有 D:/work 这一段");
        let text = line_text(&hovered);
        let start = usize::from(region.x);
        let slice: String = text
            .chars()
            .skip(start)
            .take(usize::from(region.width))
            .collect();
        assert_eq!(slice, "D:/work");
    }
}
