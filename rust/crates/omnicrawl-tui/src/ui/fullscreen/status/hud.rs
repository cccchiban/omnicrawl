//! 全屏工作台 HUD 的纯格式化函数（对映 `status/hud.py`）。
//!
//! Python 侧本模块不依赖 Textual：吃数值与字符串，吐 `rich.text.Text`。Rust 侧
//! 保持同样的纯函数形状，输出 [`StyledText`]，样式串、分隔符与空格位置逐字对齐。

use unicode_width::UnicodeWidthChar;

use crate::ui::fullscreen::random::Rng;
use crate::ui::fullscreen::round_half_even;
use crate::ui::fullscreen::terminal::theme::{
    ACCENT_GREEN, BORDER_MUTED, TEXT_MUTED, TEXT_PRIMARY,
};
use crate::ui::fullscreen::text::StyledText;

/// 底部轮播留言页的候选文本文件（与代码同目录）。
pub const CAROUSEL_MESSAGES_FILE: &str = "carousel_messages.txt";

/// Python 侧从包资源在运行期读取同一文件；Rust 侧编译期内嵌，保证脱离宿主
/// 单文件分发时留言页仍有内容（代价是不能在运行期编辑，见 crate README）。
const CAROUSEL_MESSAGES: &str = include_str!("carousel_messages.txt");

/// 读取轮播候选文本：逐行去首尾空白并丢弃空行。
pub fn load_carousel_message_lines() -> Vec<String> {
    CAROUSEL_MESSAGES
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

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

/// 解密扫描特效：进度 0~EROSION_FRACTION 为乱码侵蚀旧文本，
/// 之后把乱码从左到右逐步"吐出"为清晰的新文本。
pub const EROSION_FRACTION: f64 = 0.45;
/// 解密扫描波前右侧的乱码区中，每字符以该概率闪现真实目标字符。
pub const SHIMMER_CHANCE: f64 = 0.16;
/// 乱码字符集：随机符号 + 数字 + 大小写字母（纯 ASCII，按字节索引抽样）。
const GARBLE_BYTES: &[u8] =
    b"#@%&*+=<>/\\?^$!~|0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// 判断字符是否为终端双宽（CJK 全角/宽字符）。
fn is_wide_char(ch: char) -> bool {
    UnicodeWidthChar::width(ch).unwrap_or(0) == 2
}

/// 生成替换乱码字符；双宽字符用两个单宽乱码保持终端宽度。
fn garble_cells(original: char, style: &str, rand: &mut Rng) -> Vec<(char, String)> {
    let count = if is_wide_char(original) { 2 } else { 1 };
    (0..count)
        .map(|_| {
            let index = rand.index(GARBLE_BYTES.len());
            (GARBLE_BYTES[index] as char, style.to_string())
        })
        .collect()
}

/// 生成底部轮播切换时的「解密扫描特效」的一帧。
///
/// 进度 `0.0` 完整显示旧文本、`1.0` 完整显示新文本。前半段乱码波从左到右侵蚀
/// 旧文本；后半段扫描波从左到右把乱码逐步蜕变成清晰的新文本——波前左侧已解密、
/// 波前右侧仍是闪烁乱码（偶发闪现真实字符）。`rand` 传入固定种子时输出可复现。
pub fn decrypt_frame(
    old_text: &StyledText,
    new_text: &StyledText,
    progress: f64,
    rand: &mut Rng,
) -> StyledText {
    let progress = progress.clamp(0.0, 1.0);
    let old_plain: Vec<char> = old_text.plain().chars().collect();
    let new_plain: Vec<char> = new_text.plain().chars().collect();
    let old_styles = old_text.char_styles();
    let new_styles = new_text.char_styles();
    let garble_style = TEXT_MUTED;
    let mut cells: Vec<(char, String)> = Vec::new();
    if progress < EROSION_FRACTION {
        // 侵蚀阶段：乱码波从左到右吃掉旧文本，波前左侧已乱码、右侧完好。
        let front = if progress > 0.0 && !old_plain.is_empty() {
            old_plain
                .len()
                .min((old_plain.len() as f64 * progress / EROSION_FRACTION + 0.999) as usize)
        } else {
            0
        };
        for (index, ch) in old_plain.iter().enumerate() {
            if index < front {
                cells.extend(garble_cells(*ch, garble_style, rand));
            } else {
                cells.push((*ch, old_styles[index].clone()));
            }
        }
    } else {
        // 解密阶段：扫描波从左到右把乱码吐出为清晰新文本。
        let reveal = (progress - EROSION_FRACTION) / (1.0 - EROSION_FRACTION);
        let front = new_plain
            .len()
            .min((new_plain.len() as f64 * reveal) as usize);
        for (index, ch) in new_plain.iter().enumerate() {
            if index < front {
                cells.push((*ch, new_styles[index].clone()));
            } else if index == front {
                cells.extend(garble_cells(*ch, garble_style, rand));
            } else if rand.random() < SHIMMER_CHANCE {
                cells.push((*ch, new_styles[index].clone()));
            } else {
                cells.extend(garble_cells(*ch, garble_style, rand));
            }
        }
    }
    let mut rendered = StyledText::new();
    for (ch, style) in cells {
        rendered.push(&ch.to_string(), &style);
    }
    rendered
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

/// 生成第二行右段：模型、推理强度、审批模式、MCP 数量、排队数。
///
/// 行首自带 “⁕ ” 前置分隔符（衔接第二行左段的 t/s），MDL 为裸值段（无前缀
/// 标签），THK 保留标签且值区紧凑；MDL/THK 之间用 ⁕ 分隔，APR/MCP/QUE 段间
/// 不使用竖线（段内自带空格，段间自然 2 空格）。
pub fn status_summary_text(
    approval_mode: &str,
    mcp_enabled_count: i64,
    pending_count: i64,
    model: &str,
    reasoning_effort: &str,
) -> StyledText {
    let raw_approval = approval_mode.trim();
    let approval = match raw_approval.to_lowercase().as_str() {
        "manual" | "人工确认" => "MAN".to_string(),
        "auto" | "完全自动批准" => "AUTO".to_string(),
        "review" | "模型审查" => "REV".to_string(),
        _ => raw_approval.to_uppercase(),
    };
    let bold_primary = format!("{TEXT_PRIMARY} bold");
    let mut rendered = StyledText::new();
    // 行首 ⁕ 前置分隔符：衔接第二行左段（t/s），随本组件恒显示。
    rendered.push("⁕", BORDER_MUTED);
    rendered.push(" ", TEXT_MUTED);
    // MDL 段：无标签裸值，直接跟随分隔符；超长模型名截断。
    rendered.push(&compact_hud_value(model, 18), &bold_primary);
    rendered.push(" ", TEXT_MUTED);
    rendered.push("⁕", BORDER_MUTED);
    // THK 段：保留标签，内容紧排使 THK 与右侧 APR 段紧贴。
    rendered.push(" THK ", TEXT_MUTED);
    rendered.push(
        &compact_hud_value(&reasoning_effort.to_uppercase(), 5),
        &bold_primary,
    );
    rendered.push(" ", TEXT_MUTED);
    // APR 段：无竖线，紧贴 THK 段（段间自然 2 空格）。
    rendered.push(" APR ", TEXT_MUTED);
    rendered.push(&compact_hud_value(&approval, 8), &bold_primary);
    rendered.push(" ", TEXT_MUTED);
    // MCP 段：段内自带前后空格，内容紧排。
    rendered.push(" MCP ", TEXT_MUTED);
    rendered.push(&mcp_enabled_count.max(0).to_string(), &bold_primary);
    rendered.push(" ", TEXT_MUTED);
    // QUE 段：段内自带前后空格，内容紧排。
    rendered.push(" QUE ", TEXT_MUTED);
    rendered.push(&pending_count.max(0).to_string(), &bold_primary);
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
    fn status_summary_text_maps_approval_modes() {
        let manual = status_summary_text("manual", 2, 1, "deepseek-v4", "high");
        assert!(manual.plain().contains("MAN"));
        assert!(manual.plain().contains("THK HIGH"));
        assert!(manual.plain().contains("MCP 2"));
        assert!(manual.plain().contains("QUE 1"));
        let chinese = status_summary_text("人工确认", 0, 0, "", "");
        assert!(chinese.plain().contains("MAN"));
        let auto = status_summary_text("完全自动批准", 0, 0, "", "");
        assert!(auto.plain().contains("AUTO"));
        let review = status_summary_text("模型审查", 0, 0, "", "");
        assert!(review.plain().contains("REV"));
        let custom = status_summary_text("manual-x", 0, 0, "", "");
        assert!(custom.plain().contains("MANUAL-X"));
    }

    #[test]
    fn decrypt_frame_hits_both_ends() {
        let old = StyledText::styled("旧的一行文本", "dim");
        let new = StyledText::styled("新的一行文本", "default");
        let mut rand = Rng::new(3);
        assert_eq!(
            decrypt_frame(&old, &new, 0.0, &mut rand).plain(),
            "旧的一行文本"
        );
        assert_eq!(
            decrypt_frame(&old, &new, 1.0, &mut rand).plain(),
            "新的一行文本"
        );
    }

    #[test]
    fn decrypt_frame_fills_display_width_with_garble() {
        // 侵蚀阶段乱码替换旧文本：宽字符换成两个单宽乱码，总显示宽度不变。
        let old = StyledText::styled("中文字", "dim");
        let new = StyledText::styled("abc", "default");
        let mut rand = Rng::new(5);
        let frame = decrypt_frame(&old, &new, 0.3, &mut rand);
        let garbled = frame
            .plain()
            .chars()
            .filter(|ch| ch.is_ascii() && GARBLE_BYTES.contains(&(*ch as u8)))
            .count();
        assert_eq!(garbled, 4);
        assert!(frame.plain().ends_with('字'));
        assert_eq!(frame.display_width(), 6);
    }

    #[test]
    fn load_carousel_message_lines_reads_bundled_text() {
        let lines = load_carousel_message_lines();
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| !line.trim().is_empty()));
        assert_eq!(CAROUSEL_MESSAGES_FILE, "carousel_messages.txt");
    }

    #[test]
    fn round_half_even_matches_python_round() {
        assert_eq!(round_half_even(0.5), 0);
        assert_eq!(round_half_even(1.5), 2);
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(29.999), 30);
    }
}
