//! 顶部两行 HUD：固定列宽分段 + 行尾版本号。
//!
//! 阶段一只保留有真实数据来源的分段：模型与工作区来自启动配置，上下文与用量来自
//! 内核的 `turn.token_usage`。MCP、思考强度、队列长度等分段要等对应能力搬到 Rust 后再接，
//! 这里不填占位零值。

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{center, display_width, pad};
use crate::state::AppState;

const PRJ_WIDTH: usize = 14;
const MDL_WIDTH: usize = 30;
const APR_WIDTH: usize = 14;
const QUE_WIDTH: usize = 9;
const NAME_WIDTH: usize = 16;
const CTX_WIDTH: usize = 20;
const IN_WIDTH: usize = 11;
const OUT_WIDTH: usize = 12;
const CA_WIDTH: usize = 13;
const RATE_WIDTH: usize = 15;

const SEPARATOR: &str = "│";

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let [top, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Paragraph::new(line_one(state, area.width)), top);
    frame.render_widget(Paragraph::new(line_two(state, area.width)), bottom);
}

/// 第一行：工作区 │ 模型 │ 审批模式 队列 │ … 版本号。
pub fn line_one(state: &AppState, width: u16) -> Line<'static> {
    let mut spans = vec![
        Span::raw(pad(&state.project, PRJ_WIDTH)),
        separator(),
        Span::raw(pad(&state.model, MDL_WIDTH)),
        separator(),
        Span::raw(pad(state.approval.label(), APR_WIDTH)),
        Span::raw(pad("QUE 0", QUE_WIDTH)),
    ];
    push_tail(&mut spans, width, &state.version);
    Line::from(spans)
}

/// 第二行：项目名（居中）│ 上下文占用 │ IN/OUT/CA │ tok/s │ … 版本号。
pub fn line_two(state: &AppState, width: u16) -> Line<'static> {
    let telemetry = &state.telemetry;
    let mut spans = vec![
        Span::raw(center(&state.project, NAME_WIDTH)),
        separator(),
        Span::raw(pad(
            &context_segment(telemetry.input_tokens, telemetry.context_window, CTX_WIDTH),
            CTX_WIDTH,
        )),
        separator(),
        Span::raw(pad(
            &format!("IN {}", format_tokens(telemetry.input_tokens)),
            IN_WIDTH,
        )),
        Span::raw(pad(
            &format!("OUT {}", format_tokens(telemetry.output_tokens)),
            OUT_WIDTH,
        )),
        Span::raw(pad(
            &format!("CA {}", format_tokens(telemetry.cached_input_tokens)),
            CA_WIDTH,
        )),
        separator(),
        Span::raw(pad(
            &format!("tok/s {}", format_rate(telemetry.rate.value())),
            RATE_WIDTH,
        )),
    ];
    push_tail(&mut spans, width, &state.version);
    Line::from(spans)
}

/// 行尾版本号被弹性留白推到最右端；空间不足时至少留一列间隔。
fn push_tail(spans: &mut Vec<Span<'static>>, width: u16, version: &str) {
    let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let version_width = display_width(version);
    let filler = (width as usize).saturating_sub(used + version_width).max(1);
    spans.push(Span::raw(" ".repeat(filler)));
    spans.push(Span::styled(version.to_string(), Style::new().dim()));
}

fn separator() -> Span<'static> {
    Span::styled(SEPARATOR, Style::new().fg(Color::DarkGray))
}

/// `0% ░░░░ 0/1M`：进度条吸收剩余宽度，总量未知时进度条留空。
fn context_segment(used: u64, window: Option<u64>, width: usize) -> String {
    let total = window.unwrap_or(0);
    // 总量未知时不编造分母与百分比：显示 `--` 而不是把 0 当成真实上限。
    let (head, tail) = match (used.min(total) * 100).checked_div(total) {
        Some(percent) => (
            format!("{percent:>3}% "),
            format!("{}/{}", format_tokens(used), format_tokens(total)),
        ),
        None => (" -- ".to_string(), format!("{}/--", format_tokens(used))),
    };
    let bar_width = width
        .saturating_sub(display_width(&head) + display_width(&tail) + 2)
        .max(1);
    let filled = (used.min(total) * bar_width as u64)
        .checked_div(total)
        .unwrap_or(0) as usize;
    let bar: String = "▓".repeat(filled) + &"░".repeat(bar_width - filled);
    pad(&format!("{head}{bar} {tail}"), width)
}

fn format_tokens(value: u64) -> String {
    let scale = |divisor: f64, suffix: &str| {
        let text = format!("{:.1}", value as f64 / divisor);
        let text = text.strip_suffix(".0").unwrap_or(&text).to_string();
        format!("{text}{suffix}")
    };
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => scale(1_000.0, "K"),
        _ => scale(1_000_000.0, "M"),
    }
}

fn format_rate(value: Option<f64>) -> String {
    match value {
        Some(rate) => format!("{rate:.1}"),
        None => "--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;
    use crate::state::Telemetry;

    fn state() -> AppState {
        AppState::new(
            "omnicrawl".to_string(),
            "deepseek-v4-flash".to_string(),
            ApprovalMode::Manual,
        )
    }

    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn lines_fill_exact_terminal_width_with_version_at_the_end() {
        // 行内容自然宽约 96 列；更窄的终端由 ratatui 裁剪，不做弹性收缩，因此只断言宽屏。
        for width in [110u16, 130, 200] {
            let state = state();
            for line in [line_one(&state, width), line_two(&state, width)] {
                let text = text_of(&line);
                assert_eq!(
                    display_width(&text),
                    width as usize,
                    "宽度 {width} 时行宽不符：{text:?}"
                );
                assert!(text.ends_with(&state.version), "版本号应在行尾：{text:?}");
            }
        }
    }

    #[test]
    fn long_fields_are_truncated_without_shifting_later_segments() {
        let mut state = state();
        state.model = "m".repeat(60);
        let text = text_of(&line_one(&state, 120));
        assert!(text.contains('…'), "超长模型名应被截断：{text}");
        // 竖线位置由固定段宽决定，模型名变长不影响审批段起点。
        let first_bar = text.find('│').expect("应有分隔竖线");
        assert_eq!(first_bar, PRJ_WIDTH);
    }

    #[test]
    fn context_segment_keeps_fixed_width_and_absorbs_bar() {
        let segment = context_segment(0, Some(1_000_000), CTX_WIDTH);
        assert_eq!(display_width(&segment), CTX_WIDTH, "实际：{segment:?}");
        assert!(segment.contains("0/1M"), "实际：{segment:?}");

        let half = context_segment(500_000, Some(1_000_000), CTX_WIDTH);
        assert!(half.contains("50%"), "实际：{half:?}");
        assert!(half.contains('▓'), "半程应有填充：{half:?}");
        assert_eq!(display_width(&half), CTX_WIDTH);

        let unknown = context_segment(1234, None, CTX_WIDTH);
        assert!(unknown.contains("--"), "总量未知时不编造分母：{unknown:?}");
        assert!(unknown.contains("1.2K/--"), "实际：{unknown:?}");
        assert_eq!(display_width(&unknown), CTX_WIDTH);
    }

    #[test]
    fn token_and_rate_formatting_matches_hud_conventions() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1K");
        assert_eq!(format_tokens(18_600), "18.6K");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(2_400_000), "2.4M");
        assert_eq!(format_rate(None), "--");
        assert_eq!(format_rate(Some(12.34)), "12.3");
    }

    #[test]
    fn line_two_reports_usage_from_telemetry() {
        let mut state = state();
        state.telemetry = Telemetry {
            input_tokens: 18_600,
            output_tokens: 2_400,
            cached_input_tokens: 7_100,
            context_window: Some(1_000_000),
            rate: crate::state::RateEstimator::default(),
        };
        let text = text_of(&line_two(&state, 120));
        assert!(text.contains("IN 18.6K"), "{text}");
        assert!(text.contains("OUT 2.4K"), "{text}");
        assert!(text.contains("CA 7.1K"), "{text}");
        assert!(text.contains("tok/s --"), "{text}");
    }
}
