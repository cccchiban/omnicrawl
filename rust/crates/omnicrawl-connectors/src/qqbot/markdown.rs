//! QQ Markdown 消息（`msg_type=2`）的适用范围与正文规整。
//!
//! 2026-04 起单聊与群聊的自定义 Markdown 对所有机器人开放，**频道仍需内邀开通**，因此
//! 频道消息一律走纯文本。平台只认文档列出的一套子集：
//!
//! - 标题 `#` / `##`、强调 `**粗**` `_斜_` `~~删除~~`、链接 `[文字](url)` 与 `<url>`、
//!   图片 `![alt #宽 #高](url)`、有序/无序列表、块引用 `>`、分割线 `***`；
//! - **不支持围栏代码块与表格**，出现即被判「无效的 markdown 内容」（40034011）；
//! - **只认一到二级标题**，`###` 及更深会解析失败；
//! - 列表前紧接普通文本时列表不被识别（文档要求用空行隔开）。
//!
//! 早期实现遇到代码块或表格就把**整条消息**降级为纯文本，于是同一段里的 `**粗体**`、
//! 列表全变成原始符号——这是「Markdown 偶尔失效」的主因。现在改成**局部降级**：代码块
//! 转成块引用、表格转成列表项、深标题压到二级、列表前补空行，其余语法照常渲染。
//!
//! 平台要求：填了 `markdown` 就不能再带 `content`，且 `markdown.content` 不能为空。

use super::api::Target;

/// Markdown 正文的长度上限（与纯文本分段共用同一口径）。
pub const MAX_MARKDOWN_CHARS: usize = 1800;

/// 平台对单条 Markdown 的建议上限：规整后超过它就不冒险走 `msg_type=2`。
const MAX_PLATFORM_MARKDOWN_CHARS: usize = 4000;

/// 该目标场景能否发自定义 Markdown：单聊与群聊可以，频道需要内邀（按纯文本发）。
pub fn supports(target: &Target) -> bool {
    !matches!(target, Target::Channel(_))
}

/// 正文里有平台能渲染的语法，或有需要规整的结构（代码块、表格）吗。
///
/// 都没有时返回 false：原文里的符号本来就该原样显示，套上 Markdown 反而多一层解析。
pub fn has_supported(text: &str) -> bool {
    if text.contains("```") {
        return true;
    }
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        is_table_row(trimmed)
            || trimmed.starts_with('#')
            || trimmed.starts_with('>')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("![")
            || trimmed.trim_end() == "***"
            || starts_with_ordered_item(trimmed)
    }) || text.contains("**")
        || text.contains("__")
        || text.contains("~~")
        || text.contains("](")
        || text.matches('`').count() >= 2
}

/// 正文是否以块级语法开头（标题、列表、引用、图片、表格、分割线、围栏）。
///
/// 块级语法必须顶格：连接器给正文段加 `◇ ` 前缀时，若首行是块级元素，前缀会吃掉它
/// 的块属性（`◇ # 结论` 只会显示成普通文字），此时前缀要单独占一行。
pub fn starts_block(text: &str) -> bool {
    let Some(first) = text.lines().next() else {
        return false;
    };
    let trimmed = first.trim_start();
    trimmed.starts_with("```")
        || trimmed.starts_with('#')
        || trimmed.starts_with('>')
        || trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("![")
        || is_table_row(trimmed)
        || trimmed.trim_end() == "***"
        || starts_with_ordered_item(trimmed)
}

/// 该发 Markdown 就返回规整后的正文，否则 `None`（调用方按纯文本发）。
///
/// 长度按**规整前**的正文判定：规整会给代码块补 `> `、给列表补空行，稍微变长是正常的。
/// 规整后若越过平台建议的 4000 字，则退回纯文本（纯文本按 1800 字分段，不会更长）。
pub fn to_markdown(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_MARKDOWN_CHARS {
        return None;
    }
    if !has_supported(trimmed) {
        return None;
    }
    let body = normalize(trimmed);
    if body.is_empty() || body.chars().count() > MAX_PLATFORM_MARKDOWN_CHARS {
        return None;
    }
    Some(body)
}

/// 逐行规整：代码块转块引用、表格转列表、标题压到二级、符号列表统一成 `- `。
fn normalize(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<String> = None;
    let mut code: Vec<String> = Vec::new();
    let mut table: Vec<String> = Vec::new();
    for line in text.lines() {
        if fence.is_some() {
            if line.trim_start().starts_with("```") {
                push_code(&mut out, fence.take().unwrap_or_default(), &mut code);
            } else {
                code.push(line.to_string());
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            push_table(&mut out, &mut table);
            fence = Some(line.trim_start().trim_start_matches('`').trim().to_string());
            continue;
        }
        if is_table_row(line.trim_start()) {
            table.push(line.trim().to_string());
            continue;
        }
        push_table(&mut out, &mut table);
        out.push(fix_line(line));
    }
    push_table(&mut out, &mut table);
    if fence.is_some() {
        // 没闭合的围栏：按代码块收尾，不能把 ``` 直接发给平台。
        push_code(&mut out, fence.take().unwrap_or_default(), &mut code);
    }
    join_lines(&out)
}

/// 代码块：围栏换成块引用（平台不支持围栏），语言名单独一行。
fn push_code(out: &mut Vec<String>, language: String, code: &mut Vec<String>) {
    let lines: Vec<String> = std::mem::take(code);
    if lines.iter().all(|line| line.trim().is_empty()) {
        return;
    }
    push_blank(out);
    if !language.is_empty() {
        out.push(format!("> {language}"));
    }
    for line in lines {
        if line.trim().is_empty() {
            out.push(">".to_string());
        } else {
            out.push(format!("> {line}"));
        }
    }
    push_blank(out);
}

/// 表格：丢掉分隔行，其余每行转成一个列表项（平台不支持表格语法）。
fn push_table(out: &mut Vec<String>, table: &mut Vec<String>) {
    let rows: Vec<String> = std::mem::take(table);
    let data: Vec<Vec<String>> = rows
        .iter()
        .filter(|row| !is_table_divider(row))
        .map(|row| split_row(row))
        .filter(|cells| !cells.is_empty())
        .collect();
    if data.is_empty() {
        return;
    }
    push_blank(out);
    for cells in data {
        out.push(format!("- {}", cells.join(" | ")));
    }
    push_blank(out);
}

fn push_blank(out: &mut Vec<String>) {
    if !out.last().map(String::is_empty).unwrap_or(true) {
        out.push(String::new());
    }
}

/// 收尾：列表与块引用前补空行（平台不认紧跟正文的列表）、连续空行折成一个。
fn join_lines(lines: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let needs_blank = is_list_item(line) || line.starts_with('>');
        let glued = out
            .last()
            .map(|last| !last.is_empty() && !is_list_item(last) && !last.starts_with('>'))
            .unwrap_or(false);
        if needs_blank && glued {
            out.push(String::new());
        }
        if line.is_empty() && out.last().map(String::is_empty).unwrap_or(true) {
            continue;
        }
        out.push(line.clone());
    }
    out.join("\n").trim().to_string()
}

fn fix_line(line: &str) -> String {
    let indent: String = line.chars().take_while(|value| *value == ' ').collect();
    let rest = &line[indent.len()..];
    let rest = fix_bullet(rest);
    let rest = fix_heading(&rest);
    let rest = fix_quote(&rest);
    let body = fix_image(&strip_inline_code(&rest));
    if body.trim().is_empty() {
        return String::new();
    }
    if is_list_item(&body) || body.starts_with("> ") {
        // 嵌套层级统一成平台要求的 4 空格（原文本常见 2 或 4 空格缩进）。
        return format!("{}{body}", "    ".repeat((indent.len() + 2) / 4));
    }
    format!("{indent}{body}")
}

/// 行内代码 `x` 平台不渲染反引号，去掉保留内容。
fn strip_inline_code(line: &str) -> String {
    if !line.contains('`') || line.matches('`').count() % 2 != 0 {
        return line.to_string();
    }
    line.replace('`', "")
}

/// 块引用补一个空格，兼容 `>` 后直接写字的写法。
fn fix_quote(line: &str) -> String {
    let fences: String = line.chars().take_while(|value| *value == '>').collect();
    if fences.is_empty() {
        return line.to_string();
    }
    let body = &line[fences.len()..];
    if body.is_empty() || body.starts_with(' ') {
        return line.to_string();
    }
    format!("{fences} {body}")
}

/// `* 项目` 与 `- 项目` 同义，但平台只把 `-` 当列表，统一成 `-`。
fn fix_bullet(line: &str) -> String {
    match line.strip_prefix("* ") {
        Some(rest) => format!("- {rest}"),
        None => line.to_string(),
    }
}

/// 标题压到二级（平台只有一到二级），缺空格时补上；`#!/bin/sh` 这类不当标题。
fn fix_heading(line: &str) -> String {
    let hashes = line.chars().take_while(|value| *value == '#').count();
    if hashes == 0 || hashes > 6 {
        return line.to_string();
    }
    let body = &line[hashes..];
    match body.chars().next() {
        None => String::new(),
        Some(value) if value.is_whitespace() => {
            let text = body.trim_start();
            if text.is_empty() {
                return String::new();
            }
            format!("{} {text}", "#".repeat(hashes.min(2)))
        }
        // 紧跟文字（中文或字母）也按标题处理；`#1`、`#!/bin/sh` 这类留原样。
        Some(value) if is_word(value) && !value.is_ascii_digit() => {
            format!("{} {}", "#".repeat(hashes.min(2)), body.trim_start())
        }
        Some(_) => line.to_string(),
    }
}

/// 只认公网可达的图片地址；本地路径发过去会被平台判转存失败，退化成文字。
fn fix_image(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("![") {
        out.push_str(&rest[..start]);
        let token_start = start + 2;
        let Some(alt_close) = rest[token_start..].find("](") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let alt_close = token_start + alt_close;
        let Some(url_end) = rest[alt_close + 2..].find(')') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let url_end = alt_close + 2 + url_end;
        let alt = rest[token_start..alt_close]
            .split('#')
            .next()
            .unwrap_or("")
            .trim();
        let url = rest[alt_close + 2..url_end].trim();
        if is_public_url(url) {
            out.push_str(&rest[start..url_end + 1]);
        } else if alt.is_empty() {
            out.push_str(url);
        } else {
            out.push_str(alt);
        }
        rest = &rest[url_end + 1..];
    }
    out.push_str(rest);
    out
}

fn is_public_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("qqbot-media://")
}

fn is_word(value: char) -> bool {
    value.is_alphanumeric() || !value.is_ascii()
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_table_divider(row: &str) -> bool {
    let cells = split_row(row);
    !cells.is_empty()
        && cells
            .iter()
            .all(|cell| cell.chars().all(|value| matches!(value, '-' | ':')))
}

fn split_row(row: &str) -> Vec<String> {
    row.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .filter(|cell| !cell.is_empty())
        .collect()
}

fn is_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- ") || starts_with_ordered_item(trimmed)
}

fn starts_with_ordered_item(line: &str) -> bool {
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return false;
    }
    line[digits.len()..].starts_with(". ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_fall_back_to_plain_text() {
        assert!(supports(&Target::User("F6A8".to_string())));
        assert!(supports(&Target::Group("A1B2".to_string())));
        assert!(!supports(&Target::Channel("8888".to_string())));
    }

    #[test]
    fn headings_and_bold_are_sent_as_markdown() {
        let text = "# 结论\n\n- 第一点\n- **重点**";
        assert_eq!(to_markdown(text).as_deref(), Some(text));
        assert!(has_supported(text));
    }

    #[test]
    fn plain_prose_stays_plain_text() {
        assert_eq!(to_markdown("目录里有 a.py 与 b.py。"), None);
        assert!(!has_supported("目录里有 a.py 与 b.py。"));
    }

    #[test]
    fn fenced_code_turns_into_a_quote_block() {
        let text = "**说明**\n```rust\nfn main() {}\n```\n完成。";
        let body = to_markdown(text).expect("代码块应就地降级而不是整条放弃");
        assert!(!body.contains("```"), "{body}");
        assert!(body.contains("> rust"), "{body}");
        assert!(body.contains("> fn main() {}"), "{body}");
        // 同一段里的其余语法照常保留。
        assert!(body.contains("**说明**"), "{body}");
    }

    #[test]
    fn tables_turn_into_list_items() {
        let text = "**对比**\n| 项 | 值 |\n|---|---|\n| a | 1 |\n| b | 2 |";
        let body = to_markdown(text).expect("表格应就地降级");
        assert!(!body.contains("---"), "{body}");
        assert!(body.contains("- 项 | 值"), "{body}");
        assert!(body.contains("- a | 1"), "{body}");
        assert!(body.contains("**对比**"), "{body}");
    }

    #[test]
    fn deep_headings_are_clamped_to_level_two() {
        let body = to_markdown("### 三级\n#### 四级").expect("标题应保留");
        assert_eq!(body, "## 三级\n## 四级");
        // 无空格的标题补空格，`#!/bin/sh` 不当标题。
        assert_eq!(to_markdown("#结论").as_deref(), Some("# 结论"));
        assert_eq!(to_markdown("#!/bin/sh **x**").as_deref(), Some("#!/bin/sh **x**"));
    }

    #[test]
    fn lists_get_a_blank_line_and_star_becomes_dash() {
        let body = to_markdown("结论如下\n* 一\n* 二").expect("列表应保留");
        assert_eq!(body, "结论如下\n\n- 一\n- 二");
    }

    #[test]
    fn local_images_degrade_to_text() {
        let body = to_markdown("![截图](shots/a.png) 与 ![图](https://x/y.png)").expect("应保留");
        assert!(body.contains("截图"), "{body}");
        assert!(!body.contains("shots/a.png"), "{body}");
        assert!(body.contains("![图](https://x/y.png)"), "{body}");
    }

    #[test]
    fn empty_and_overlong_text_is_rejected() {
        assert_eq!(to_markdown("   "), None);
        let long = format!("# 标题\n{}", "字".repeat(MAX_MARKDOWN_CHARS));
        assert_eq!(to_markdown(&long), None);
    }

    #[test]
    fn inline_code_loses_its_backticks() {
        let body = to_markdown("看 `README.md` 这个文件").expect("行内代码应保留内容");
        assert_eq!(body, "看 README.md 这个文件");
    }

    #[test]
    fn collapsed_quotes_and_nested_lists_are_normalized() {
        let body = to_markdown(">引用一行\n1. 一级\n  - 二级").expect("应保留");
        assert!(body.starts_with("> 引用一行"), "{body}");
        assert!(body.contains("\n    - 二级"), "{body}");
    }

    #[test]
    fn block_syntax_at_the_first_line_is_detected() {
        assert!(starts_block("# 标题\n正文"));
        assert!(starts_block("- 一\n- 二"));
        assert!(starts_block("> 引用"));
        assert!(starts_block("```rust\nfn main() {}\n```"));
        assert!(!starts_block("先看目录。\n- 一"));
        assert!(!starts_block(""));
    }

    #[test]
    fn links_and_images_count_as_supported() {
        assert!(has_supported("看这个[文档](https://doc.qq.com)"));
        assert!(has_supported("![图 #100px #100px](https://x/y.png)"));
        assert!(has_supported("1. 第一步"));
        assert!(!has_supported("1.没有空格"));
    }
}
