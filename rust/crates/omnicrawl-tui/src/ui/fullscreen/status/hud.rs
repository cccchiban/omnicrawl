//! 全屏工作台 HUD 的纯格式化函数（对映 `status/hud.py`）。
//!
//! Python 侧本模块不依赖 Textual：吃数值与字符串，吐 `rich.text.Text`。Rust 侧
//! 保持同样的纯函数形状，输出 [`StyledText`]，样式串、分隔符与空格位置逐字对齐。

use crate::ui::fullscreen::round_half_even;
use crate::ui::fullscreen::terminal::theme::{
    ACCENT_GREEN, BORDER_MUTED, TEXT_MUTED, TEXT_PRIMARY,
};
use crate::ui::fullscreen::text::StyledText;

/// 使用 K/M 缩写压缩 Token 数，同时保留小数量的精确值。
pub fn compact_token_count(value: i64) -> String {
    let value = value.max(0);
    if value < 1_000 {
        return value.to_string();
    }
    if value < 1_000_000 {
        return format!("{:.1}K", value as f64 / 1_000.0).replace(".0K", "K");
    }
    format!("{:.1}M", value as f64 / 1_000_000.0).replace(".0M", "M")
}

/// 生成上下文占用文本：用量/总量 + 百分比（无 CTX 前缀、无进度条）。
///
/// 内容紧排不补固定宽度，`0/1M 0%` 直接跟随分隔符；超限时百分比可超过 100。
pub fn context_usage_text(input_tokens: i64, context_limit: i64) -> StyledText {
    let context_limit = context_limit.max(1);
    let input_tokens = input_tokens.max(0);
    let ratio = input_tokens as f64 / context_limit as f64;
    let percent = round_half_even(ratio * 100.0).min(999);
    let usage = format!(
        "{}/{}",
        compact_token_count(input_tokens),
        compact_token_count(context_limit)
    );
    let mut rendered = StyledText::new();
    rendered.push(&usage, TEXT_PRIMARY);
    rendered.push(" ", TEXT_MUTED);
    rendered.push(&format!("{percent}%"), TEXT_PRIMARY);
    rendered
}

/// 生成第二行遥测（最左段）：上下文占用 ⁕ 输入/输出/缓存+缓存率 ⁕ 速率。
///
/// 字段顺序固定为 上下文占用 ⁕ ↑/↓/† CH% ⁕ t/s，段间用 ⁕ 分隔、内容紧排不补
/// 固定宽度；缓存率 = 缓存命中的输入 token（†）÷ 本次请求总输入 token（↑），
/// CA 是 IN 的子集因此不超过 100%。`tokens_per_second` 是会话累计平均速率，
/// 尚无输出记录时为 0，显示 `-- t/s`。
pub fn token_telemetry_text(
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    context_limit: i64,
    tokens_per_second: f64,
) -> StyledText {
    let input_tokens = input_tokens.max(0);
    let output_tokens = output_tokens.max(0);
    let cached_input_tokens = cached_input_tokens.max(0);
    let tokens_per_second = if tokens_per_second.is_nan() {
        0.0
    } else {
        tokens_per_second.max(0.0)
    };
    let context_limit = context_limit.max(1);
    let cache_percent = if input_tokens > 0 {
        round_half_even(cached_input_tokens as f64 * 100.0 / input_tokens as f64).min(100)
    } else {
        0
    };
    let mut rendered = StyledText::new();
    // CTX 段：用量/总量 + 百分比，行首直接开始，后接 ⁕ 分隔符。
    rendered.append_text(&context_usage_text(input_tokens, context_limit));
    rendered.push(" ", TEXT_MUTED);
    rendered.push("⁕", BORDER_MUTED);
    // 输入/输出/缓存段：↑/↓/† + 值，/ 分隔，缓存占比 CH% 后随 1 空格。
    rendered.push(" ", TEXT_MUTED);
    rendered.push(
        &format!("↑{}", compact_token_count(input_tokens)),
        TEXT_PRIMARY,
    );
    rendered.push("/", TEXT_MUTED);
    rendered.push(
        &format!("↓{}", compact_token_count(output_tokens)),
        TEXT_PRIMARY,
    );
    rendered.push("/", TEXT_MUTED);
    rendered.push(
        &format!("†{}", compact_token_count(cached_input_tokens)),
        TEXT_PRIMARY,
    );
    rendered.push(" ", TEXT_MUTED);
    rendered.push(&format!("CH{cache_percent}%"), TEXT_PRIMARY);
    rendered.push(" ", TEXT_MUTED);
    rendered.push("⁕", BORDER_MUTED);
    // 速率段：-- t/s 或实际速率，段尾 1 空格衔接右段前置 ⁕。
    rendered.push(" ", TEXT_MUTED);
    rendered.push(
        &if tokens_per_second > 0.0 {
            format!("{tokens_per_second:.1}")
        } else {
            "--".to_string()
        },
        TEXT_PRIMARY,
    );
    rendered.push(" t/s", TEXT_MUTED);
    rendered.push(" ", TEXT_MUTED);
    rendered
}

/// 生成顶部右段状态卡片中的 FIFO 排队数量文本。
pub fn pending_queue_text(pending_count: i64) -> StyledText {
    let pending_count = pending_count.max(0);
    let mut rendered = StyledText::new();
    rendered.push("QUE ", TEXT_MUTED);
    rendered.push(&pending_count.to_string(), &format!("{TEXT_PRIMARY} bold"));
    rendered
}

/// 保留既有调用接口，以终端 ANSI 主强调色渲染品牌文字。
pub fn gradient_text(text: &str) -> StyledText {
    StyledText::styled(text, &format!("{ACCENT_GREEN} bold"))
}

/// 压缩 HUD 字段，避免长模型名把整行挤乱。
///
/// 终端按字符截断；超长时保留首尾可读片段，中间用省略号。
pub fn compact_hud_value(value: &str, max_chars: usize) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "-".to_string();
    }
    let limit = max_chars.max(4);
    let length = text.chars().count();
    if length <= limit {
        return text;
    }
    if limit <= 4 {
        return format!("{}…", take_first(&text, limit - 1));
    }
    let head = ((limit - 1) / 2).max(1);
    let tail = (limit - 1 - head).max(1);
    format!("{}…{}", take_first(&text, head), take_last(&text, tail))
}

fn take_first(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn take_last(text: &str, count: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(count);
    chars[start..].iter().collect()
}

// 顶部两行字段段不使用固定宽度：各段内容紧排（段内自带前后 1 空格，段间自然
// 2 空格），数字位数增长时后续字段自然顺移。分隔符为 ⁕，行首与行尾不再有闭合
// 竖线；超长值由 compact_hud_value 截断。

/// 渲染第一行左段：项目绝对路径（灰色，行首直接开始）。
///
/// 直接显示完整路径不做 basename 截断，用灰色弱化视觉；超长路径由
/// [`compact_hud_value`] 截断（保留首尾）。尾部 1 空格保持字段间距。
pub fn context_summary_text(workspace: &str) -> StyledText {
    let mut rendered = StyledText::new();
    rendered.push(&compact_hud_value(workspace, 40), TEXT_MUTED);
    rendered.push(" ", TEXT_MUTED);
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_token_count_uses_k_and_m_suffixes() {
        assert_eq!(compact_token_count(0), "0");
        assert_eq!(compact_token_count(999), "999");
        assert_eq!(compact_token_count(1_000), "1K");
        assert_eq!(compact_token_count(1_234), "1.2K");
        assert_eq!(compact_token_count(1_000_000), "1M");
        assert_eq!(compact_token_count(1_500_000), "1.5M");
        assert_eq!(compact_token_count(-5), "0");
    }

    #[test]
    fn context_usage_text_marks_overflow_and_never_exceeds_999() {
        let rendered = context_usage_text(0, 1_000_000);
        assert_eq!(rendered.plain(), "0/1M 0%");
        let over = context_usage_text(1_500_000, 1_000_000);
        assert_eq!(over.plain(), "1.5M/1M 150%");
        let clamped = context_usage_text(9_990_000, 1);
        assert!(clamped.plain().ends_with("999%"));
    }

    #[test]
    fn token_telemetry_text_orders_segments() {
        let rendered = token_telemetry_text(1_200, 300, 600, 128_000, 12.34);
        assert_eq!(
            rendered.plain(),
            "1.2K/128K 1% ⁕ ↑1.2K/↓300/†600 CH50% ⁕ 12.3 t/s "
        );
        let idle = token_telemetry_text(0, 0, 0, 128_000, 0.0);
        assert!(idle.plain().contains("CH0%"));
        assert!(idle.plain().contains("-- t/s"));
    }

    #[test]
    fn token_telemetry_text_caps_cache_percent() {
        let rendered = token_telemetry_text(100, 0, 500, 1_000, 0.0);
        assert!(rendered.plain().contains("CH100%"));
    }

    #[test]
    fn pending_queue_text_prefixes_count() {
        let rendered = pending_queue_text(4);
        assert_eq!(rendered.plain(), "QUE 4");
        assert_eq!(rendered.spans()[1].style, "default bold");
    }

    #[test]
    fn gradient_text_uses_accent_green_bold() {
        let rendered = gradient_text("OmniCrawl");
        assert_eq!(rendered.plain(), "OmniCrawl");
        assert_eq!(rendered.spans()[0].style, "green bold");
    }

    #[test]
    fn compact_hud_value_keeps_head_and_tail() {
        assert_eq!(compact_hud_value("", 10), "-");
        assert_eq!(compact_hud_value("  deepseek\n v4 ", 18), "deepseek v4");
        assert_eq!(compact_hud_value("abcdefghij", 6), "ab…hij");
        assert_eq!(compact_hud_value("abcdefghij", 4), "abc…");
        assert_eq!(compact_hud_value("abcdefghij", 1), "abc…");
    }

    #[test]
    fn context_summary_text_truncates_long_paths() {
        let rendered = context_summary_text("D:/very/long/workspace/path/that/exceeds/the/limit");
        assert_eq!(rendered.plain().chars().last(), Some(' '));
        assert!(rendered.plain().contains('…'));
    }

    #[test]
    fn round_half_even_matches_python_round() {
        assert_eq!(round_half_even(0.5), 0);
        assert_eq!(round_half_even(1.5), 2);
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(29.999), 30);
    }
}
