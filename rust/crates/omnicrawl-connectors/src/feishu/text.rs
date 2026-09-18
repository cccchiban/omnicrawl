//! 飞书文本处理：内部标签清理、脱敏、分段与正文定型。
//!
//! 语义基准是 Python `omnicrawl/connectors/fsapp.py` 的 `_split_text` / `_clean_text` /
//! `_display_text` / `_resolve_final_text` / `_split_segment_for_card` / `_parse_json` /
//! `_text_value`。正则改写为手写扫描（本 crate 不引入 regex），边界与 Python 的
//! 非贪婪匹配一致。

use serde_json::Value;

use omnicrawl_session::redact_sensitive_text;

/// 飞书单条文本消息的保守上限。
pub const MAX_TEXT_CHARS: usize = 3000;

/// 单条卡片正文上限，超出部分改用文本消息补发。
pub const SEGMENT_MAX_CHARS: usize = 6000;

/// 需要从展示文本中剔除的内部标签。
const DISPLAY_TAGS: [&str; 4] = ["thinking", "summary", "tool_use", "file_content"];

/// 把文本整理为待发送片段：空文本返回空列表，超长文本按安全上限分段。
///
/// 在 [`MAX_TEXT_CHARS`] 上限内优先按段落/列表项边界切分，保留可读性。
pub fn split_text(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let trimmed = text.trim_end_matches('\n');
    let cleaned = if trimmed.is_empty() { text } else { trimmed };
    if cleaned.chars().count() <= MAX_TEXT_CHARS {
        return vec![cleaned.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in cleaned.split('\n') {
        let boundary = line.is_empty() || line.starts_with(['-', '*', '#', '>']);
        let candidate = if current.is_empty() {
            line.to_string()
        } else {
            format!("{current}\n{line}")
        };
        if (candidate.chars().count() > MAX_TEXT_CHARS
            || (boundary && !current.is_empty() && current.chars().count() >= MAX_TEXT_CHARS / 2))
            && !current.is_empty()
        {
            parts.push(current.clone());
            current.clear();
        }
        if current.is_empty() {
            current = line.to_string();
        } else {
            current.push('\n');
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// 清理内部展示标签、脱敏敏感值，并折叠空行与行尾空格。
pub fn clean_text(text: &str) -> String {
    let stripped = strip_display_tags(text).trim().to_string();
    let redacted = redact_sensitive_text(&stripped);
    let collapsed = collapse_blank_lines(&redacted);
    collapse_trailing_spaces(&collapsed)
}

/// 清理文本并给空响应提供可读兜底。
pub fn display_text(text: &str) -> String {
    let cleaned = clean_text(text);
    if cleaned.is_empty() {
        "（任务完成，无文本输出）".to_string()
    } else {
        cleaned
    }
}

/// 正文定型：最终回答更完整（忽略空白后以流式片段为前缀）时改用它，否则保留已展示内容。
pub fn resolve_final_text(streamed: &str, reply: &str) -> String {
    let streamed_text = clean_text(streamed);
    let reply_text = clean_text(reply);
    if streamed_text.is_empty() {
        return reply_text;
    }
    if reply_text.is_empty() {
        return streamed_text;
    }
    let squeezed_streamed: String = streamed_text.split_whitespace().collect();
    let squeezed_reply: String = reply_text.split_whitespace().collect();
    if squeezed_reply.starts_with(&squeezed_streamed) {
        return reply_text;
    }
    streamed_text
}

/// 把长正文切成「单条消息正文」与「需要文本消息补发的剩余部分」。
pub fn split_segment_for_card(text: &str) -> (String, String) {
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= SEGMENT_MAX_CHARS {
        return (text.to_string(), String::new());
    }
    let mut head: String = characters[..SEGMENT_MAX_CHARS].iter().collect();
    for boundary in ["\n\n", "\n"] {
        if let Some(index) = head.rfind(boundary) {
            if head[..index].chars().count() > SEGMENT_MAX_CHARS / 2 {
                head = head[..index].to_string();
                break;
            }
        }
    }
    let rest: String = characters[head.chars().count()..].iter().collect();
    let tail = rest.trim_start_matches('\n').to_string();
    (head, tail)
}

/// 解析 JSON 对象字段（非对象、非字符串或解析失败都返回空对象）。
pub fn parse_json_object(value: &Value) -> Value {
    match value {
        Value::Object(_) => value.clone(),
        Value::String(text) if !text.trim().is_empty() => match serde_json::from_str::<Value>(text)
        {
            Ok(parsed @ Value::Object(_)) => parsed,
            _ => Value::Object(serde_json::Map::new()),
        },
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// 读取飞书文本字段：对象取 `text` 或 `content`，其余按字符串处理。
pub fn text_value(value: &Value) -> String {
    match value {
        Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("content"))
            .map(value_as_text)
            .unwrap_or_default()
            .trim()
            .to_string(),
        other => value_as_text(other).trim().to_string(),
    }
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 剔除 `<thinking>…</thinking>` 等内部标签段；无闭合标签时保留原文（与正则行为一致）。
fn strip_display_tags(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    loop {
        let Some((start, content_start)) = earliest_tag(rest, false) else {
            result.push_str(rest);
            break;
        };
        result.push_str(&rest[..start]);
        let Some(offset) = earliest_tag(&rest[content_start..], true) else {
            result.push_str(&rest[start..]);
            break;
        };
        rest = &rest[content_start + offset.0 + offset.1..];
    }
    result
}

/// 找最早出现的标签（`closing=true` 时找闭合标签），返回（起点, 标签长度）。
fn earliest_tag(text: &str, closing: bool) -> Option<(usize, usize)> {
    let mut found: Option<(usize, usize)> = None;
    for tag in DISPLAY_TAGS {
        let needle = if closing {
            format!("</{tag}>")
        } else {
            format!("<{tag}>")
        };
        if let Some(index) = text.find(&needle) {
            if found.map(|(current, _)| index < current).unwrap_or(true) {
                found = Some((index, needle.len()));
            }
        }
    }
    found
}

/// 3 个及以上连续换行折叠为 2 个。
fn collapse_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut newline_run = 0_usize;
    for character in text.chars() {
        if character == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                result.push(character);
            }
        } else {
            newline_run = 0;
            result.push(character);
        }
    }
    result
}

/// 行尾空格与制表符清理（保留其余空白结构）。
fn collapse_trailing_spaces(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            result.push('\n');
        }
        result.push_str(line.trim_end_matches([' ', '\t']));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_tags_are_removed() {
        let text = "前<thinking>推理</thinking>后<tool_use>x</tool_use>";
        assert_eq!(clean_text(text), "前后");
    }

    #[test]
    fn unclosed_tag_keeps_text() {
        let text = "保留<thinking>未闭合";
        assert_eq!(clean_text(text), "保留<thinking>未闭合");
    }

    #[test]
    fn blank_lines_collapse_to_two() {
        assert_eq!(clean_text("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(clean_text("a  \nb\t\n"), "a\nb");
    }

    #[test]
    fn display_text_falls_back_on_empty() {
        assert_eq!(display_text("   "), "（任务完成，无文本输出）");
        assert_eq!(display_text("正文"), "正文");
    }

    #[test]
    fn final_text_prefers_more_complete_reply() {
        assert_eq!(resolve_final_text("前半", "前半后半"), "前半后半");
        assert_eq!(resolve_final_text("前半 后半", "前半"), "前半 后半");
        assert_eq!(resolve_final_text("", "回复"), "回复");
        assert_eq!(resolve_final_text("流式", ""), "流式");
    }

    #[test]
    fn long_segment_splits_at_paragraph_boundary() {
        let text = format!("{}\n\n{}", "a".repeat(4000), "b".repeat(3000));
        let (head, tail) = split_segment_for_card(&text);
        assert_eq!(head.chars().count(), 4000);
        assert_eq!(tail.chars().count(), 3000);
    }

    #[test]
    fn short_text_is_single_part() {
        assert_eq!(split_text(""), Vec::<String>::new());
        assert_eq!(split_text("一行\n"), vec!["一行".to_string()]);
    }

    #[test]
    fn long_text_splits_within_limit() {
        let text = format!("{}\n{}", "x".repeat(2000), "y".repeat(2000));
        let parts = split_text(&text);
        assert!(parts.len() >= 2);
        assert!(parts
            .iter()
            .all(|part| part.chars().count() <= MAX_TEXT_CHARS));
    }
}
