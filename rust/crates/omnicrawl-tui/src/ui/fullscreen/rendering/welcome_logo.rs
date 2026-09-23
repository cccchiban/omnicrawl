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
}
