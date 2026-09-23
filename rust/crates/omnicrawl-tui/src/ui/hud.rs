//! 顶部两行 HUD：内容驱动的分段 + 行尾版本号，窄屏按重要性逐段收缩。
//!
//! 分段贴齐（段内自带前后空格，段间用 `│` 分隔），宽度由内容决定；这与 Python
//! `ui/fullscreen/status/hud.py` 的口径一致——那边同样是「不补固定宽度、超长值由
//! `compact_hud_value` 截断」。窄屏下不靠 ratatui 裁剪，而是先按重要性丢段
//! （版本号最后丢、工作区与模型最先缩短），保证任何宽度下都填满一行且没有半截字符。
//!
//! 阶段一仍然只保留有真实数据来源的分段：模型与工作区来自启动配置，上下文与用量来自
//! 内核的 `turn.token_usage`，MCP 来自启动期发现到的启用 Server 数，队列来自排队预览。

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::fullscreen::status::hud::compact_hud_value;
use super::{display_width, fit};
use crate::state::AppState;

/// 分隔符；Python HUD 用 ⁕，简化页沿用 `│`。
const SEPARATOR: &str = "│";
/// 工具段宽度上限：留出整行宽度给后面还能显示的段。
const PRJ_LIMIT: usize = 40;
const MDL_LIMIT: usize = 18;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let [top, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Paragraph::new(line_one(state, area.width)), top);
    frame.render_widget(Paragraph::new(line_two(state, area.width)), bottom);
}

/// 一个候补分段：越靠后被裁掉得越早。
struct Segment {
    text: String,
    style: Style,
}

impl Segment {
    fn plain(text: String) -> Self {
        Self {
            text,
            style: Style::new(),
        }
    }

    fn styled(text: String, style: Style) -> Self {
        Self { text, style }
    }

    fn width(&self) -> usize {
        display_width(&self.text)
    }
}

/// 第一行：工作区 │ 模型 │ 审批模式 │ QUE │ MCP │ … 版本号。
pub fn line_one(state: &AppState, width: u16) -> Line<'static> {
    let segments = vec![
        Segment::plain(compact_hud_value(&state.project, PRJ_LIMIT)),
        Segment::plain(compact_hud_value(&state.model, MDL_LIMIT)),
        Segment::plain(state.approval.label().to_string()),
        Segment::styled(
            format!("QUE {}", state.pending_inputs.len()),
            Style::new().add_modifier(Modifier::DIM),
        ),
        Segment::styled(
            format!("MCP {}", state.mcp_servers),
            Style::new().add_modifier(Modifier::DIM),
        ),
    ];
    assemble(segments, width, &state.version)
}

/// 第二行：项目名 │ 上下文占用 │ 用量 │ tok/s │ … 版本号。
pub fn line_two(state: &AppState, width: u16) -> Line<'static> {
    let telemetry = &state.telemetry;
    let segments = vec![
        Segment::plain(compact_hud_value(&state.project, MDL_LIMIT)),
        Segment::styled(
            context_segment(telemetry.input_tokens, telemetry.context_window),
            Style::new().add_modifier(Modifier::DIM),
        ),
        Segment::plain(format!("IN {}", format_tokens(telemetry.input_tokens))),
        Segment::plain(format!("OUT {}", format_tokens(telemetry.output_tokens))),
        Segment::plain(format!(
            "CA {}",
            format_tokens(telemetry.cached_input_tokens)
        )),
        Segment::plain(format!("tok/s {}", format_rate(telemetry.rate.value()))),
    ];
    assemble(segments, width, &state.version)
}

/// 把候选分段排成恰好 `width` 列的一行。
///
/// 规则（窄屏收缩顺序）：
/// 1. 逐个尝试加入分段：能放下才加，放不下就跳过该段并继续试后面的短段——
///    这样窄屏掉的是「放不下的那一段」，而不是把后面全丢掉；
/// 2. 行尾版本号优先于任何可选段：只有放得下版本号时才继续加段，
///    否则回退已加入的可选段腾出空间；
/// 3. 第一段永远保留（截断到可用宽度），保证窄屏下仍有工作区/项目标识；
/// 4. 剩余空间补在版本号左侧，行首行尾都没有多余空白。
fn assemble(segments: Vec<Segment>, width: u16, version: &str) -> Line<'static> {
    let width = width as usize;
    let version_width = display_width(version);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    // 版本号与它前面那一格空隙一起占用右侧，先预留出来：可选段只在预算内加入。
    let budget = width.saturating_sub(version_width + 1);
    for (index, segment) in segments.into_iter().enumerate() {
        let separator = if index == 0 {
            String::new()
        } else {
            SEPARATOR.to_string()
        };
        let cost = display_width(&separator) + segment.width();
        if index > 0 && used + cost > budget {
            continue;
        }
        if index == 0 {
            // 第一段永远保留：按剩余预算截断（窄屏下尾部以 … 收尾）。
            let room = budget.saturating_sub(used);
            let text = fit(&segment.text, room);
            used += display_width(&text);
            spans.push(Span::styled(text, segment.style));
            continue;
        }
        if !separator.is_empty() {
            spans.push(separator_span());
        }
        used += cost;
        spans.push(Span::styled(segment.text.clone(), segment.style));
    }

    if used + version_width <= width {
        let filler = width - used - version_width;
        spans.push(Span::raw(" ".repeat(filler)));
        spans.push(Span::styled(version.to_string(), Style::new().dim()));
    }
    // 版本号都放不下时（极窄终端）不再强行追加，避免行宽超过终端。
    let rendered = Line::from(spans);
    let fitted = display_width(&line_text(&rendered));
    if fitted > width {
        return Line::from(fit(&line_text(&rendered), width));
    }
    rendered
}

fn separator_span() -> Span<'static> {
    Span::styled(SEPARATOR, Style::new().fg(Color::DarkGray))
}

fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.to_string())
        .collect()
}

/// `50% ▓▓░░ 0.5M/1M`：上下文占用；总量未知时显示 `--` 而不是编造分母。
fn context_segment(used: u64, window: Option<u64>) -> String {
    let total = window.unwrap_or(0);
    match (used.min(total) * 100).checked_div(total) {
        Some(percent) => format!(
            "{percent}% {}/{}",
            format_tokens(used.min(total)),
            format_tokens(total)
        ),
        None => format!("-- {}/--", format_tokens(used)),
    }
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
        line_text(line)
    }

    #[test]
    fn lines_fill_exact_terminal_width_at_every_size() {
        for width in [24u16, 32, 40, 60, 80, 110, 200] {
            let state = state();
            for (name, line) in [
                ("第一行", line_one(&state, width)),
                ("第二行", line_two(&state, width)),
            ] {
                let text = text_of(&line);
                assert!(
                    display_width(&text) <= width as usize,
                    "{name}在宽度 {width} 下超宽：{text:?}"
                );
                assert!(
                    display_width(&text) >= width as usize - 1,
                    "{name}在宽度 {width} 下没有填满（应有版本号前的弹性留白）：{text:?}"
                );
            }
        }
    }

    #[test]
    fn wide_rows_end_with_the_version() {
        let state = state();
        for width in [80u16, 130, 200] {
            for line in [line_one(&state, width), line_two(&state, width)] {
                let text = text_of(&line);
                assert_eq!(display_width(&text), width as usize, "{text:?}");
                assert!(text.ends_with(&state.version), "版本号应在行尾：{text:?}");
            }
        }
    }

    #[test]
    fn narrow_rows_drop_optional_segments_but_keep_the_identity() {
        let mut state = state();
        state.pending_inputs.push_back("排队".to_string());
        state.mcp_servers = 3;

        let narrow = text_of(&line_one(&state, 24));
        assert!(
            narrow.starts_with("omnicrawl"),
            "窄屏仍要保留工作区标识：{narrow:?}"
        );
        assert!(
            !narrow.contains("MCP") && !narrow.contains("QUE"),
            "窄屏先丢尾部可选段：{narrow:?}"
        );

        let wide = text_of(&line_one(&state, 110));
        assert!(wide.contains("QUE 1"), "{wide:?}");
        assert!(wide.contains("MCP 3"), "{wide:?}");
    }

    #[test]
    fn very_narrow_terminal_keeps_the_version_inside_the_row() {
        let state = state();
        let text = text_of(&line_one(&state, 8));
        assert_eq!(display_width(&text), 8, "{text:?}");
        assert!(text.ends_with(&state.version), "版本号仍要在行尾：{text:?}");

        // 比版本号还窄的终端：不追加版本号，整行按预算截断，绝不超宽。
        let tiny = text_of(&line_one(&state, 4));
        assert!(display_width(&tiny) <= 4, "{tiny:?}");
    }

    #[test]
    fn long_fields_are_compacted_with_head_and_tail() {
        let mut state = state();
        state.project = format!("{}/{}", "a".repeat(40), "b".repeat(40));
        state.model = "m".repeat(60);
        let text = text_of(&line_one(&state, 160));
        assert!(text.contains('…'), "超长字段应保留首尾并在中间省略：{text}");
        assert_eq!(display_width(&text), 160);
    }

    #[test]
    fn context_segment_reports_usage_or_unknown_limit() {
        let mut state = state();
        state.telemetry = Telemetry {
            input_tokens: 500_000,
            output_tokens: 2_400,
            cached_input_tokens: 7_100,
            context_window: Some(1_000_000),
            rate: crate::state::RateEstimator::default(),
        };
        let text = text_of(&line_two(&state, 120));
        assert!(text.contains("50% 500K/1M"), "{text}");
        assert!(text.contains("IN 500K"), "{text}");
        assert!(text.contains("OUT 2.4K"), "{text}");
        assert!(text.contains("CA 7.1K"), "{text}");
        assert!(text.contains("tok/s --"), "{text}");

        state.telemetry.context_window = None;
        let unknown = text_of(&line_two(&state, 120));
        assert!(
            unknown.contains("-- 500K/--"),
            "总量未知时不编造分母：{unknown}"
        );
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
}
