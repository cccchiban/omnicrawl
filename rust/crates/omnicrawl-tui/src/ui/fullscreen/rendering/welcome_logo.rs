//! 启动后空会话页在输入框上方显示的 ASCII 品牌 Logo（对映 Python
//! `ui/fullscreen/rendering/welcome_logo.py`）。
//!
//! Logo 来自仓库根 `assets/ASCII.txt` 的前 8 行块字（OmniCrawl）。原文件同时包含
//! 无色与着色两个副本；着色副本使用 RGB(255,255,255) 纯白，这里等价地用 Rich 风格
//! 串 `white` 呈现。块字的行首缩进是字形升部（i 点、l 顶）的定位，必须原样保留，
//! 因此只去掉行尾填充空白（本文件的字面量已按此处理，与 Python 逐字一致）。
//!
//! 本模块只提供静态字形与常量，逐帧动画在 [`super::logo_anim`]；两者都不持有
//! Widget/定时器，因此可以脱离终端做确定性测试。

use crate::ui::fullscreen::terminal::theme::TEXT_MUTED;
use crate::ui::fullscreen::text::StyledText;

/// 8 行块字 Logo。行首缩进与内部空格共同决定各字母的列对齐，不可去除。
const WELCOME_LOGO_LINES: [&str; 8] = [
    "                                      ███                                               ████",
    "                                     ▒▒▒                                               ▒▒███",
    "  ██████  █████████████   ████████   ████   ██████  ████████   ██████   █████ ███ █████ ▒███",
    " ███▒▒███▒▒███▒▒███▒▒███ ▒▒███▒▒███ ▒▒███  ███▒▒███▒▒███▒▒███ ▒▒▒▒▒███ ▒▒███ ▒███▒▒███  ▒███",
    "▒███ ▒███ ▒███ ▒███ ▒███  ▒███ ▒███  ▒███ ▒███ ▒▒▒  ▒███ ▒▒▒   ███████  ▒███ ▒███ ▒███  ▒███",
    "▒███ ▒███ ▒███ ▒███ ▒███  ▒███ ▒███  ▒███ ▒███  ███ ▒███      ███▒▒███  ▒▒███████████   ▒███",
    "▒▒██████  █████▒███ █████ ████ █████ █████▒▒██████  █████    ▒▒████████  ▒▒████▒████    █████",
    " ▒▒▒▒▒▒  ▒▒▒▒▒ ▒▒▒ ▒▒▒▒▒ ▒▒▒▒ ▒▒▒▒▒ ▒▒▒▒▒  ▒▒▒▒▒▒  ▒▒▒▒▒      ▒▒▒▒▒▒▒▒    ▒▒▒▒ ▒▒▒▒    ▒▒▒▒▒",
];

/// 白色字形使用的 Rich 风格串（着色副本的 RGB(255,255,255) 等价表示）。
pub const LOGO_STYLE: &str = "white";

/// 返回 8 行块字 Logo（行尾空白已去除，行首缩进原样保留）。
pub fn welcome_logo_lines() -> Vec<String> {
    WELCOME_LOGO_LINES
        .iter()
        .map(|line| line.to_string())
        .collect()
}

/// 返回用于 TUI 展示的纯白 Logo 文本。
pub fn welcome_logo_text() -> StyledText {
    StyledText::styled(&WELCOME_LOGO_LINES.join("\n"), LOGO_STYLE)
}

/// Logo 下方那行会话徽标：`模型 * THINKING 推理档 APPROVAL 审批模式 MCP 数量`。
///
/// 取值全部来自当前会话/配置（模型、推理强度、审批模式、已启用 MCP 数量），
/// 换配置后下一帧就跟上；标签用弱化灰、取值用白色粗体，与底部遥测的排版语言一致。
pub fn session_badge_text(
    model: &str,
    reasoning_effort: &str,
    approval_mode: &str,
    mcp_count: i64,
) -> StyledText {
    let value_style = format!("{LOGO_STYLE} bold");
    let mut text = StyledText::new();
    text.push(model.trim(), &value_style);
    text.push(" * ", TEXT_MUTED);
    text.push("THINKING ", TEXT_MUTED);
    text.push(&reasoning_effort.trim().to_uppercase(), &value_style);
    text.push(" APPROVAL ", TEXT_MUTED);
    text.push(&approval_code(approval_mode), &value_style);
    text.push(" MCP ", TEXT_MUTED);
    text.push(&mcp_count.max(0).to_string(), &value_style);
    text
}

/// 审批模式 → 徽标里的短码（`MAN` / `REV` / `AUTO`）。
///
/// 传入的既可能是配置里的英文模式名，也可能是界面上显示的中文文案。
fn approval_code(mode: &str) -> String {
    let raw = mode.trim();
    match raw.to_lowercase().as_str() {
        "manual" | "人工确认" => "MAN".to_string(),
        "review" | "模型审查" => "REV".to_string(),
        "auto" | "完全自动批准" => "AUTO".to_string(),
        _ => raw.to_uppercase(),
    }
}

/// 徽标里可点的一段跳到哪个一级设置项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeLink {
    /// 模型名与推理档 → 模型管理。
    ModelManagement,
    /// 审批模式 → 工具设置。
    Tools,
    /// MCP 数量 → MCP 设置。
    Mcp,
}

impl BadgeLink {
    /// 对应的一级项键（与 `settings::ROW_ORDER` 同源）。
    pub fn settings_key(self) -> &'static str {
        match self {
            Self::ModelManagement => "model_management",
            Self::Tools => "tools",
            Self::Mcp => "mcp",
        }
    }
}

/// 徽标里的一段可点区域：字符区间（左闭右开）+ 跳转目标。
///
/// 用字符下标而不是字节，动画帧与静态帧的字符布局一一对应，悬停描色对两者都成立。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadgeSegment {
    pub start: usize,
    pub end: usize,
    pub link: BadgeLink,
}

/// 徽标里可点段的个数（模型 / 推理档 / 审批模式 / MCP 数量）。
pub const BADGE_SEGMENT_COUNT: usize = 4;

/// 从徽标文本里取出可点段。
///
/// 段的位置由 [`session_badge_text`] 的装配顺序决定：
/// `[模型][ * ][THINKING ][档位][ APPROVAL ][模式][ MCP ][数量]`——只有第 0/3/5/7 段可点，
/// 分隔符与标签不参与命中。
pub fn badge_segments(text: &StyledText) -> Vec<BadgeSegment> {
    let mut segments = Vec::with_capacity(BADGE_SEGMENT_COUNT);
    let mut offset = 0usize;
    for (index, span) in text.spans().iter().enumerate() {
        let length = span.text.chars().count();
        let link = match index {
            0 | 3 => Some(BadgeLink::ModelManagement),
            5 => Some(BadgeLink::Tools),
            7 => Some(BadgeLink::Mcp),
            _ => None,
        };
        if let Some(link) = link {
            segments.push(BadgeSegment {
                start: offset,
                end: offset + length,
                link,
            });
        }
        offset += length;
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_keep_glyph_alignment() {
        let lines = welcome_logo_lines();
        assert_eq!(lines.len(), 8);
        assert_eq!(
            lines[0].len() - lines[0].trim_start_matches(' ').len(),
            38,
            "i 点点位决定第一行的缩进，与 Python 字面量一致：{:?}",
            lines[0]
        );
        assert_eq!(
            lines[2].len() - lines[2].trim_start_matches(' ').len(),
            2,
            "左对齐基线行只保留 2 列缩进：{:?}",
            lines[2]
        );
        assert_eq!(
            lines.iter().map(|line| line.chars().count()).max(),
            Some(93),
            "块字整体宽度决定首屏列对齐"
        );
        for line in &lines {
            assert_eq!(line, &line.trim_end(), "不得保留行尾填充空白：{line:?}");
        }
    }

    #[test]
    fn text_is_white_and_joined_by_newline() {
        let text = welcome_logo_text();
        assert_eq!(text.base_style(), "");
        assert_eq!(text.plain(), welcome_logo_lines().join("\n"));
        assert_eq!(text.spans().len(), 1);
        assert_eq!(text.spans()[0].style, LOGO_STYLE);
    }

    #[test]
    fn badge_packs_model_and_live_flags() {
        let badge = session_badge_text("deepseek-4.1-flash", "high", "review", 1);
        assert_eq!(
            badge.plain(),
            "deepseek-4.1-flash * THINKING HIGH APPROVAL REV MCP 1"
        );
        // 标签弱化、取值白色粗体：与底部遥测的排版语言一致。
        assert_eq!(badge.spans()[1].style, TEXT_MUTED, "分隔符与标签用弱化灰");
        assert_eq!(
            badge.spans().last().map(|span| span.style.as_str()),
            Some(format!("{LOGO_STYLE} bold").as_str()),
            "MCP 数量是白色粗体"
        );
    }

    #[test]
    fn badge_maps_every_approval_mode() {
        let badge = |mode: &str| session_badge_text("m", "low", mode, 0).plain();
        assert!(badge("manual").contains("APPROVAL MAN"));
        assert!(badge("review").contains("APPROVAL REV"));
        assert!(badge("auto").contains("APPROVAL AUTO"));
        assert!(badge("人工确认").contains("APPROVAL MAN"));
        assert!(badge("完全自动批准").contains("APPROVAL AUTO"));
        assert!(badge("-1").contains("MCP 0"), "负数按 0 显示");
    }

    #[test]
    fn badge_segments_cover_only_the_four_values() {
        let text = session_badge_text("deepseek-4.1-flash", "high", "review", 1);
        let segments = badge_segments(&text);
        assert_eq!(segments.len(), BADGE_SEGMENT_COUNT);
        let pieces: Vec<String> = segments
            .iter()
            .map(|segment| {
                text.plain()
                    .chars()
                    .skip(segment.start)
                    .take(segment.end - segment.start)
                    .collect()
            })
            .collect();
        assert_eq!(
            pieces,
            vec![
                "deepseek-4.1-flash".to_string(),
                "HIGH".to_string(),
                "REV".to_string(),
                "1".to_string()
            ]
        );
        assert_eq!(
            segments[0].link,
            BadgeLink::ModelManagement,
            "模型名 → 模型管理"
        );
        assert_eq!(segments[1].link, BadgeLink::ModelManagement, "推理档 → 模型管理");
        assert_eq!(segments[2].link, BadgeLink::Tools, "审批模式 → 工具设置");
        assert_eq!(segments[3].link, BadgeLink::Mcp, "MCP 数量 → MCP 设置");
        assert_eq!(BadgeLink::Mcp.settings_key(), "mcp");
    }
}
