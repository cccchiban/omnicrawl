//! 底部单行 HUD（对映 Python `#bottom-carousel`）。
//!
//! 内容由对映层拼好：`fullscreen/status/hud.rs` 负责分段文本（遥测 + 工作区路径），
//! `fullscreen/status/indicators.rs` 把两段接成一行。本模块只做装配：把
//! [`AppState::hud_text`] 画成贴齐屏幕底缘的一行——左侧内边距与输入框卡片的左边框
//! 对齐（对映 CSS `padding: 0 1 0 0`），超宽按显示宽度以 `…` 收尾
//! （对映 `text-overflow: ellipsis`）。
//!
//! 不再轮播（留言页已移除）：模型与各开关改在欢迎 Logo 下方的会话徽标里显示。
//! 常驻底栏只保留会话态数据，末尾跟一段版本号（`工作区 ⁕ v0.4.6`）；有新版本时
//! 版本号旁多一个淡黄色 ↑。
//!
//! [`AppState::hud_text`]: crate::state::AppState::hud_text

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::truncate_styled;
use crate::state::AppState;
use crate::ui::fullscreen::text::StyledText;

/// 底部 HUD 的左内边距：内容与输入框卡片内的文字同列（对映 CSS `padding: 0 1 0 0`
/// 与 `#composer-wrap` 的左边框/内边距）。
const LEFT_PAD: usize = 1;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let line = line_of(&state.hud_text, area.width as usize);
    frame.render_widget(Paragraph::new(line), area);
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
    use crate::ui::display_width;
    use crate::ui::fullscreen::status::indicators::{hud_line_text, HudSource};
    use crate::ui::fullscreen::terminal::theme;

    fn telemetry_line(width: usize) -> Line<'static> {
        let source = HudSource {
            workspace: "D:/work".to_string(),
            input_tokens: 1_200,
            output_tokens: 300,
            cached_input_tokens: 600,
            context_limit: 128_000,
            tokens_per_second: 12.3,
            version: "v0.4.6".to_string(),
            upgrade_available: false,
        };
        line_of(&hud_line_text(&source), width)
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
        assert!(text.contains("⁕ 工作区 D:/work"), "{text}");
        assert!(text.contains("D:/work ⁕ v0.4.6"), "{text}");
        assert!(!text.contains('↑'), "无更新时不画箭头：{text}");
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
}
