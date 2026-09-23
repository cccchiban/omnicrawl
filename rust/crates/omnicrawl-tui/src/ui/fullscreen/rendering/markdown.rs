//! Markdown → 样式文本（对映 Python 侧 Rich `Markdown` 的渲染）。
//!
//! Python 用 `rich.markdown.Markdown`（RichMarkdown）把 AI 回复与思考内容渲染成带样式的
//! 行；Rust 侧用 `pulldown-cmark` 解析 CommonMark 事件流，再映射到与本项目同族的样式串。
//! Rich 的 Markdown 主题细节（各段的精确色值、标题色相）不复刻，取色沿用主题令牌，
//! 差异记录在 crate README。

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::ui::fullscreen::terminal::theme::REASONING_BACKGROUND;
use crate::ui::fullscreen::text::StyledText;

/// 行内代码：青色前景 + 代码块同款灰底（Rich 默认 `markdown.code` 为青底黑字同族）。
fn inline_code_style() -> String {
    format!("cyan on {REASONING_BACKGROUND}")
}

/// 代码块：整块灰色背景（与思考区视觉一致）。
fn code_block_style() -> String {
    format!("on {REASONING_BACKGROUND}")
}

/// 有序列表序号宽度对齐用不到补白，直接 `N. ` 前缀；无序用 `• `。
const BULLET: &str = "• ";
const LIST_INDENT: &str = "  ";
const RULE: &str = "────────────────";
const QUOTE_PREFIX: &str = "│ ";
const TASK_DONE: &str = "☑ ";
const TASK_TODO: &str = "☐ ";

fn push_newline(rendered: &mut StyledText, at_line_start: &mut bool) {
    if !*at_line_start {
        rendered.push("\n", "");
        *at_line_start = true;
    }
}

fn push_blank_line(rendered: &mut StyledText, at_line_start: &mut bool) {
    rendered.push("\n", "");
    *at_line_start = true;
}

fn current_style(modifiers: &[&'static str], link_depth: usize) -> String {
    let mut parts: Vec<&str> = modifiers.to_vec();
    if link_depth > 0 {
        parts.push("underline");
        parts.push("blue");
    }
    parts.join(" ")
}

fn push_inline_code_block(
    rendered: &mut StyledText,
    text: &str,
    at_line_start: &mut bool,
    depth: usize,
) {
    let style = code_block_style();
    let indent = LIST_INDENT.repeat(depth);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            rendered.push("\n", "");
            *at_line_start = true;
        }
        if !line.is_empty() {
            if !indent.is_empty() {
                rendered.push(&indent, &style);
            }
            rendered.push(line, &style);
            *at_line_start = false;
        }
    }
}

/// 把 Markdown 渲染成样式文本；换行只表达逻辑行，折行交给渲染层。
pub fn render_markdown(markdown: &str) -> StyledText {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);

    let mut rendered = StyledText::new();
    let mut modifiers: Vec<&'static str> = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut counters: Vec<u64> = Vec::new();
    let mut link_depth = 0usize;
    let mut in_code_block = false;
    let mut pending_prefix: Option<String> = None;
    let mut quote_depth = 0usize;
    let mut at_line_start = true;

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => push_newline(&mut rendered, &mut at_line_start),
                Tag::Heading { .. } => {
                    push_newline(&mut rendered, &mut at_line_start);
                    modifiers.push("bold");
                }
                Tag::CodeBlock(_) => {
                    push_newline(&mut rendered, &mut at_line_start);
                    in_code_block = true;
                }
                Tag::List(start) => {
                    push_newline(&mut rendered, &mut at_line_start);
                    lists.push(start);
                    counters.push(start.unwrap_or(1));
                }
                Tag::Item => {
                    push_newline(&mut rendered, &mut at_line_start);
                    let indent = LIST_INDENT.repeat(lists.len().saturating_sub(1));
                    let prefix = match lists.last() {
                        Some(Some(_)) => {
                            let number = counters.last().copied().unwrap_or(1);
                            if let Some(counter) = counters.last_mut() {
                                *counter = number + 1;
                            }
                            format!("{indent}{number}. ")
                        }
                        _ => format!("{indent}{BULLET}"),
                    };
                    pending_prefix = Some(prefix);
                }
                Tag::BlockQuote(_) => {
                    push_newline(&mut rendered, &mut at_line_start);
                    quote_depth += 1;
                }
                Tag::Emphasis => modifiers.push("italic"),
                Tag::Strong => modifiers.push("bold"),
                Tag::Strikethrough => modifiers.push("strike"),
                Tag::Link { .. } => link_depth += 1,
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => push_blank_line(&mut rendered, &mut at_line_start),
                TagEnd::Heading(_) => {
                    push_newline(&mut rendered, &mut at_line_start);
                    push_blank_line(&mut rendered, &mut at_line_start);
                    modifiers.pop();
                }
                TagEnd::CodeBlock => {
                    push_newline(&mut rendered, &mut at_line_start);
                    push_blank_line(&mut rendered, &mut at_line_start);
                    in_code_block = false;
                }
                TagEnd::Item => push_newline(&mut rendered, &mut at_line_start),
                TagEnd::List(_) => {
                    lists.pop();
                    counters.pop();
                    if lists.is_empty() {
                        push_blank_line(&mut rendered, &mut at_line_start);
                    }
                }
                TagEnd::BlockQuote(_) => {
                    push_newline(&mut rendered, &mut at_line_start);
                    push_blank_line(&mut rendered, &mut at_line_start);
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    modifiers.pop();
                }
                TagEnd::Link => link_depth = link_depth.saturating_sub(1),
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    push_inline_code_block(&mut rendered, &text, &mut at_line_start, quote_depth);
                    continue;
                }
                let mut style = current_style(&modifiers, link_depth);
                if quote_depth > 0 && at_line_start {
                    rendered.push(&QUOTE_PREFIX.repeat(quote_depth), "dim");
                }
                if let Some(prefix) = pending_prefix.take() {
                    style = current_style(&modifiers, link_depth);
                    rendered.push(&prefix, &style);
                }
                rendered.push(&text, &style);
                at_line_start = false;
            }
            Event::Code(text) => {
                if let Some(prefix) = pending_prefix.take() {
                    rendered.push(&prefix, "");
                }
                rendered.push(&text, &inline_code_style());
                at_line_start = false;
            }
            Event::TaskListMarker(checked) => {
                let indent = LIST_INDENT.repeat(lists.len().saturating_sub(1));
                pending_prefix = Some(format!(
                    "{indent}{}",
                    if checked { TASK_DONE } else { TASK_TODO }
                ));
            }
            Event::SoftBreak => {
                rendered.push(" ", "");
                at_line_start = false;
            }
            Event::HardBreak => push_newline(&mut rendered, &mut at_line_start),
            Event::Rule => {
                push_newline(&mut rendered, &mut at_line_start);
                rendered.push(RULE, "dim");
                push_blank_line(&mut rendered, &mut at_line_start);
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                rendered.push(&text, "dim");
                at_line_start = false;
            }
            _ => {}
        }
    }

    push_newline(&mut rendered, &mut at_line_start);
    rendered
}

/// 对映 `_UniformGrayMarkdown`：保留 Markdown 结构，把全部前景统一为思考区灰阶。
pub fn uniform_gray(markdown: &str) -> StyledText {
    let body = render_markdown(markdown);
    let mut rendered = StyledText::new();
    for span in body.spans() {
        if span.text.is_empty() {
            continue;
        }
        let style = if span.style.is_empty() {
            "bright_black".to_string()
        } else {
            format!("{} bright_black", span.style)
        };
        rendered.push(&span.text, &style);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paragraph_becomes_one_line() {
        let rendered = render_markdown("你好，世界");
        assert_eq!(rendered.plain().trim_end(), "你好，世界");
    }

    #[test]
    fn heading_is_bold_with_blank_line() {
        let rendered = render_markdown("# 标题");
        assert_eq!(rendered.plain(), "标题\n\n");
        assert_eq!(rendered.spans()[0].style, "bold");
    }

    #[test]
    fn strong_and_emphasis_carry_modifiers() {
        let rendered = render_markdown("普通 **粗** 与 *斜* 结束");
        assert_eq!(rendered.plain().trim_end(), "普通 粗 与 斜 结束");
        let styles: Vec<&str> = rendered
            .spans()
            .iter()
            .map(|span| span.style.as_str())
            .collect();
        assert!(styles.contains(&"bold"));
        assert!(styles.contains(&"italic"));
    }

    #[test]
    fn inline_code_uses_code_background() {
        let rendered = render_markdown("调用 `kb_search` 工具");
        assert!(rendered.plain().contains("kb_search"));
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style == inline_code_style()));
    }

    #[test]
    fn fenced_code_block_keeps_lines_with_background() {
        let rendered = render_markdown("```rust\nlet a = 1;\nlet b = 2;\n```");
        let plain = rendered.plain();
        assert!(plain.contains("let a = 1;"));
        assert!(plain.contains("let b = 2;"));
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style == code_block_style()));
    }

    #[test]
    fn bullet_and_ordered_lists_get_prefixes() {
        let bullet = render_markdown("- 第一项\n- 第二项");
        assert!(bullet.plain().contains("• 第一项"));
        assert!(bullet.plain().contains("• 第二项"));

        let ordered = render_markdown("1. 甲\n2. 乙");
        assert!(ordered.plain().contains("1. 甲"));
        assert!(ordered.plain().contains("2. 乙"));
    }

    #[test]
    fn nested_list_indents_by_depth() {
        let rendered = render_markdown("- 外层\n  - 内层");
        assert!(rendered.plain().contains("• 外层"));
        assert!(rendered.plain().contains("  • 内层"));
    }

    #[test]
    fn task_list_markers_use_checkbox_glyphs() {
        let rendered = render_markdown("- [x] 完成项\n- [ ] 未完成项");
        let plain = rendered.plain();
        assert!(plain.contains("☑ 完成项"));
        assert!(plain.contains("☐ 未完成项"));
    }

    #[test]
    fn soft_break_becomes_space_hard_break_becomes_newline() {
        let soft = render_markdown("第一行\n第二行");
        assert_eq!(soft.plain().trim_end(), "第一行 第二行");
        let hard = render_markdown("第一行  \n第二行");
        assert_eq!(hard.plain().trim_end(), "第一行\n第二行");
    }

    #[test]
    fn links_are_underlined_and_blue() {
        let rendered = render_markdown("见 [文档](https://example.com)");
        assert!(rendered.plain().contains("文档"));
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style.contains("underline") && span.style.contains("blue")));
    }

    #[test]
    fn rule_renders_a_horizontal_line() {
        let rendered = render_markdown("上面\n\n---\n\n下面");
        assert!(rendered.plain().contains(RULE));
        assert!(rendered.plain().contains("上面"));
        assert!(rendered.plain().contains("下面"));
    }

    #[test]
    fn blockquote_lines_carry_prefix() {
        let rendered = render_markdown("> 引用内容");
        assert!(rendered.plain().contains("│ 引用内容"));
    }

    #[test]
    fn uniform_gray_overrides_foreground_but_keeps_structure() {
        let rendered = uniform_gray("# 标题\n\n正文");
        assert!(rendered.plain().contains("标题"));
        assert!(rendered.plain().contains("正文"));
        assert!(rendered
            .spans()
            .iter()
            .all(|span| span.style.contains("bright_black")));
        // 结构样式（粗体）保留，只是叠加灰阶前景。
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style.contains("bold")));
    }

    #[test]
    fn empty_markdown_renders_nothing() {
        assert_eq!(render_markdown("").plain(), "");
        assert_eq!(uniform_gray("").plain(), "");
    }
}
