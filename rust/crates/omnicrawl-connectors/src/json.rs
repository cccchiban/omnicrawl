//! Python `json.dumps` 等价的序列化（`ensure_ascii=False`、默认分隔符 `, ` 与 `: `）。
//!
//! 连接器把负载字符串交给平台接口，Python 侧用 `json.dumps` 生成，卡片与提示文案的
//! 逐字节形状属于对照语义的一部分；`serde_json` 的紧凑写法（无空格）与之不同，所以
//! 这里单独实现一份。浮点写法沿用 `serde_json` 的短写法（Python 会写成 `1e+20`），
//! 这一点与内核既有差异清单一致。

use serde_json::Value;

/// `json.dumps(value, ensure_ascii=False)`。
pub fn dumps(value: &Value) -> String {
    let mut buffer = String::new();
    write_value(value, &mut buffer, None);
    buffer
}

/// `json.dumps(value, ensure_ascii=False, indent=2)`。
pub fn dumps_indent(value: &Value) -> String {
    let mut buffer = String::new();
    write_value(value, &mut buffer, Some(0));
    buffer
}

fn write_value(value: &Value, buffer: &mut String, indent: Option<usize>) {
    match value {
        Value::Null => buffer.push_str("null"),
        Value::Bool(true) => buffer.push_str("true"),
        Value::Bool(false) => buffer.push_str("false"),
        Value::Number(number) => buffer.push_str(&number.to_string()),
        Value::String(text) => write_string(text, buffer),
        Value::Array(items) => write_array(items, buffer, indent),
        Value::Object(map) => write_object(map, buffer, indent),
    }
}

fn write_array(items: &[Value], buffer: &mut String, indent: Option<usize>) {
    if items.is_empty() {
        buffer.push_str("[]");
        return;
    }
    buffer.push('[');
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            buffer.push(',');
            if indent.is_none() {
                buffer.push(' ');
            }
        }
        if let Some(level) = indent {
            buffer.push('\n');
            buffer.push_str(&" ".repeat(level + 2));
        }
        write_value(item, buffer, indent.map(|level| level + 2));
    }
    if let Some(level) = indent {
        buffer.push('\n');
        buffer.push_str(&" ".repeat(level));
    }
    buffer.push(']');
}

fn write_object(map: &serde_json::Map<String, Value>, buffer: &mut String, indent: Option<usize>) {
    if map.is_empty() {
        buffer.push_str("{}");
        return;
    }
    buffer.push('{');
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 {
            buffer.push(',');
            if indent.is_none() {
                buffer.push(' ');
            }
        }
        if let Some(level) = indent {
            buffer.push('\n');
            buffer.push_str(&" ".repeat(level + 2));
        }
        write_string(key, buffer);
        buffer.push_str(": ");
        write_value(value, buffer, indent.map(|level| level + 2));
    }
    if let Some(level) = indent {
        buffer.push('\n');
        buffer.push_str(&" ".repeat(level));
    }
    buffer.push('}');
}

fn write_string(text: &str, buffer: &mut String) {
    buffer.push('"');
    for character in text.chars() {
        match character {
            '"' => buffer.push_str("\\\""),
            '\\' => buffer.push_str("\\\\"),
            '\n' => buffer.push_str("\\n"),
            '\r' => buffer.push_str("\\r"),
            '\t' => buffer.push_str("\\t"),
            '\u{8}' => buffer.push_str("\\b"),
            '\u{c}' => buffer.push_str("\\f"),
            other if (other as u32) < 0x20 => {
                buffer.push_str(&format!("\\u{:04x}", other as u32));
            }
            other => buffer.push(other),
        }
    }
    buffer.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn separators_match_python_defaults() {
        assert_eq!(dumps(&json!({"text": "一"})), "{\"text\": \"一\"}");
        assert_eq!(dumps(&json!([1, 2])), "[1, 2]");
        assert_eq!(dumps(&json!({})), "{}");
        assert_eq!(dumps(&json!([])), "[]");
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(dumps(&json!("a\nb")), "\"a\\nb\"");
        assert_eq!(dumps(&json!("\u{1}")), "\"\\u0001\"");
    }

    #[test]
    fn indented_output_puts_keys_on_own_lines() {
        let rendered = dumps_indent(&json!({"a": 1, "b": [2]}));
        let lines: Vec<&str> = rendered.split('\n').collect();
        assert_eq!(lines.len(), 6, "{rendered}");
        assert_eq!(lines[0], "{");
        assert_eq!(lines[1].trim_start(), "\"a\": 1,");
        assert_eq!(lines[2].trim_start(), "\"b\": [");
        assert_eq!(lines[3].trim_start(), "2");
        assert_eq!(lines[4].trim_start(), "]");
        assert_eq!(lines[5], "}");
        assert_eq!(
            lines[3].chars().take_while(|value| *value == ' ').count(),
            4
        );
    }
}
