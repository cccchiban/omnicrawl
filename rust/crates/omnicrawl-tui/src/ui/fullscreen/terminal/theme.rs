//! OmniCrawl 全屏 TUI 的终端原生透明主题令牌（对映 `terminal/theme.py`）。
//!
//! Python 侧令牌有两套消费方：Rich 文本（`TEXT_*` / `ACCENT_*`，风格串）与
//! Textual CSS 变量（`terminal-*`，注册进 `TERMINAL_THEME`）。Rust 侧没有 CSS
//! 层，令牌保留 Rich 风格串，由 [`rich_style`] 解析成 ratatui 样式，保证两侧
//! 同名同值；CSS 变量表与 `terminal_select_css` / `terminal_css` 属于 Textual
//! 专属机制，不逐行照搬。

use ratatui::style::{Color, Modifier, Style};

pub const THEME_NAME: &str = "omnicrawl-terminal";

pub const TRANSPARENT: &str = "transparent";
pub const TERMINAL_FOREGROUND: &str = "ansi_default";
pub const TERMINAL_BACKGROUND: &str = "ansi_default";

// Rich 使用 default/标准色名，Textual CSS 使用 ansi_default/ansi_*；
// 两种表示最终都交给终端自己的前景色与 ANSI 调色板解析。
pub const TEXT_PRIMARY: &str = "default";
pub const TEXT_SECONDARY: &str = "default";
pub const TEXT_MUTED: &str = "dim";
pub const TEXT_FAINT: &str = "dim";
pub const TEXT_ON_ACCENT: &str = "default";

pub const BORDER_SUBTLE: &str = "dim";
pub const BORDER_STRONG: &str = "default";
pub const SCROLLBAR: &str = "default";
/// 顶部 HUD 分隔符使用的灰色（ANSI 亮黑）：比 dim 更清晰可见，又比默认前景克制。
pub const BORDER_MUTED: &str = "bright_black";

pub const ACCENT_GREEN: &str = "green";
pub const ACCENT_GREEN_SOFT: &str = TRANSPARENT;
pub const ACCENT_BLUE: &str = "blue";
pub const ACCENT_BLUE_SOFT: &str = TRANSPARENT;
pub const ACCENT_AMBER: &str = "yellow";
pub const ACCENT_AMBER_SOFT: &str = TRANSPARENT;
pub const ACCENT_RED: &str = "red";
pub const ACCENT_PURPLE: &str = "magenta";

/// 用户消息左侧细竖条强调色（青色）。
pub const ACCENT_CYAN: &str = "cyan";
/// 工具进行中的边框色（白色）：与成功/失败语义色区分开，不引入新色相。
pub const ACCENT_WHITE: &str = "white";

/// 用户消息背景：Python 侧是 26% 透明紫，由 Textual 与终端底色混合。
pub const USER_BACKGROUND: &str = "rgba(170, 80, 210, 0.26)";
/// 工具调用与输出不叠加色块，保持终端原生透明背景。
pub const TOOL_BACKGROUND: &str = TRANSPARENT;
pub const TOOL_FOCUS_BACKGROUND: &str = TRANSPARENT;
pub const TOOL_TEXT: &str = "bright_black";

/// 思考区沿用代码块同款灰色背景与灰色前景（字体由终端自身提供）。
pub const REASONING_BACKGROUND: &str = "#272822";
pub const REASONING_TEXT: &str = "bright_black";

/// 解析 Rich 风格串；无法识别的词元忽略（与 Rich 丢弃未知样式名一致）。
///
/// `default` / `ansi_default` 表示终端默认前景，即不设置颜色；`dim` 是样式位
/// 而非颜色名，映射到 ratatui 的暗色修饰。
/// Rich 风格串 → ratatui 样式。
///
/// 支持词法：修饰符（bold/dim/italic/underline/reverse/strike）、颜色名、`#rrggbb` 与
/// `rgba(r,g,b,a)`，以及 Rich 的 **`on <颜色>`** 背景语法（`"on #272822"`、
/// `"cyan on #272822"`）。最后一个很关键：对映层的行内代码与代码块正是用它表达
/// 「暗底」，若把它当成前景色解析，代码块就变成暗底暗字（彻底看不见）。
pub fn rich_style(spec: &str) -> Style {
    let mut style = Style::default();
    // 上一个词是 `on`：下一个颜色落到背景上。
    let mut background = false;
    for token in spec.split_whitespace() {
        if token == "on" {
            background = true;
            continue;
        }
        let color = match token {
            "bold" => {
                style = style.add_modifier(Modifier::BOLD);
                continue;
            }
            "dim" => {
                style = style.add_modifier(Modifier::DIM);
                continue;
            }
            "italic" => {
                style = style.add_modifier(Modifier::ITALIC);
                continue;
            }
            "underline" => {
                style = style.add_modifier(Modifier::UNDERLINED);
                continue;
            }
            "reverse" => {
                style = style.add_modifier(Modifier::REVERSED);
                continue;
            }
            "strike" | "strikethrough" => {
                style = style.add_modifier(Modifier::CROSSED_OUT);
                continue;
            }
            // `default` 类词只是在背景位置（`on default`）才表达「背景回到终端默认」；
            // 单独出现时保持原有语义（什么都不改）。
            "default" | "ansi_default" | "transparent" | "none" => {
                if background {
                    Some(Color::Reset)
                } else {
                    background = false;
                    continue;
                }
            }
            "black" | "ansi_black" => Some(Color::Black),
            "red" | "ansi_red" => Some(Color::Red),
            "green" | "ansi_green" => Some(Color::Green),
            "yellow" | "ansi_yellow" => Some(Color::Yellow),
            "blue" | "ansi_blue" => Some(Color::Blue),
            "magenta" | "ansi_magenta" => Some(Color::Magenta),
            "cyan" | "ansi_cyan" => Some(Color::Cyan),
            "white" | "ansi_white" => Some(Color::White),
            "bright_black" | "ansi_bright_black" => Some(Color::DarkGray),
            "bright_white" | "ansi_bright_white" => Some(Color::Gray),
            other => parse_color(other),
        };
        match color {
            Some(color) if background => style = style.bg(color),
            Some(color) => style = style.fg(color),
            // 认不出的词直接忽略，且不消费背景标记（`on` 后接未知词时不误当前景）。
            None => continue,
        }
        background = false;
    }
    style
}

/// 解析 `#rrggbb` 与 `rgba(r, g, b, a)`。
///
/// ratatui 没有 alpha 通道：rgba 只取 RGB 分量，与 Python 侧「终端支持真彩色时
/// 与底色混合、不支持时保留颜色语义」的降级路径不同（见 crate README 已知差异）。
pub fn parse_color(spec: &str) -> Option<Color> {
    let spec = spec.trim();
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() == 6 {
            let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(red, green, blue));
        }
        return None;
    }
    let inner = spec.strip_prefix("rgba(")?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return None;
    }
    let red = parts[0].parse::<u8>().ok()?;
    let green = parts[1].parse::<u8>().ok()?;
    let blue = parts[2].parse::<u8>().ok()?;
    Some(Color::Rgb(red, green, blue))
}

/// 用户消息背景色（rgba 只取 RGB 分量，见 [`parse_color`]）。
pub fn user_background() -> Color {
    parse_color(USER_BACKGROUND).unwrap_or(Color::Reset)
}

/// 思考区背景色。
pub fn reasoning_background() -> Color {
    parse_color(REASONING_BACKGROUND).unwrap_or(Color::Reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_style_maps_modifiers_and_colors() {
        assert_eq!(
            rich_style("dim"),
            Style::default().add_modifier(Modifier::DIM)
        );
        assert_eq!(
            rich_style("default bold"),
            Style::default().add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            rich_style("bright_black"),
            Style::default().fg(Color::DarkGray)
        );
        assert_eq!(
            rich_style("green bold"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(rich_style("default"), Style::default());
        assert_eq!(rich_style("unknown_token"), Style::default());
    }

    #[test]
    fn rich_style_supports_the_on_keyword_for_backgrounds() {
        let dark = Color::Rgb(0x27, 0x28, 0x22);
        // 纯背景（对映层代码块的写法）：不能落到前景上，否则就是暗底暗字。
        assert_eq!(rich_style("on #272822"), Style::default().bg(dark));
        assert_eq!(
            rich_style("cyan on #272822"),
            Style::default().fg(Color::Cyan).bg(dark)
        );
        assert_eq!(
            rich_style("bright_black on #272822 italic"),
            Style::default()
                .fg(Color::DarkGray)
                .bg(dark)
                .add_modifier(Modifier::ITALIC)
        );
        // `on default` 回到默认背景；`on` 后接认不出的词不误当前景。
        assert_eq!(rich_style("on default"), Style::default().bg(Color::Reset));
        assert_eq!(rich_style("on nope"), Style::default());
    }

    #[test]
    fn parse_color_handles_hex_and_rgba() {
        assert_eq!(parse_color("#272822"), Some(Color::Rgb(0x27, 0x28, 0x22)));
        assert_eq!(
            parse_color("rgba(170, 80, 210, 0.26)"),
            Some(Color::Rgb(170, 80, 210))
        );
        assert_eq!(parse_color("rgb(1,2,3)"), None);
        assert_eq!(parse_color("#27282"), None);
        assert_eq!(parse_color("dim"), None);
    }

    #[test]
    fn token_colors_resolve() {
        assert_eq!(user_background(), Color::Rgb(170, 80, 210));
        assert_eq!(reasoning_background(), Color::Rgb(0x27, 0x28, 0x22));
        assert_eq!(rich_style(TOOL_TEXT), Style::default().fg(Color::DarkGray));
    }
}
