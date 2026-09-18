//! Python `json.dumps(..., ensure_ascii=False)` 的可用子集。
//!
//! 内核里有多处「把结构原样写成 Python 风格的 JSON 文本」的需求（工具结果信封、审查指令、
//! 参数摘要）。`serde_json` 的紧凑输出不带分隔空格、缩进风格也不同，因此这里手写一个
//! 与 Python 默认参数一致的渲染器。

use serde_json::Value;

/// 渲染为 Python 风格 JSON；`indent` 为 0 时使用默认分隔符（`, ` 与 `: `）。
pub fn python_dumps(value: &Value, indent: usize) -> String {
    render(value, indent, 0)
}

fn render(value: &Value, indent: usize, level: usize) -> String {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                return "{}".to_string();
            }
            if indent == 0 {
                let entries: Vec<String> = map
                    .iter()
                    .map(|(key, item)| {
                        format!("{}: {}", json_string(key), render(item, indent, level))
                    })
                    .collect();
                return format!("{{{}}}", entries.join(", "));
            }
            let pad = " ".repeat(indent * level);
            let child_pad = " ".repeat(indent * (level + 1));
            let entries: Vec<String> = map
                .iter()
                .map(|(key, item)| {
                    format!(
                        "{child_pad}{}: {}",
                        json_string(key),
                        render(item, indent, level + 1)
                    )
                })
                .collect();
            format!("{{\n{}\n{pad}}}", entries.join(",\n"))
        }
        Value::Array(items) => {
            if items.is_empty() {
                return "[]".to_string();
            }
            if indent == 0 {
                let entries: Vec<String> = items
                    .iter()
                    .map(|item| render(item, indent, level))
                    .collect();
                return format!("[{}]", entries.join(", "));
            }
            let pad = " ".repeat(indent * level);
            let child_pad = " ".repeat(indent * (level + 1));
            let entries: Vec<String> = items
                .iter()
                .map(|item| format!("{child_pad}{}", render(item, indent, level + 1)))
                .collect();
            format!("[\n{}\n{pad}]", entries.join(",\n"))
        }
        Value::String(text) => json_string(text),
        other => other.to_string(),
    }
}

fn json_string(text: &str) -> String {
    serde_json::to_string(&Value::String(text.to_string()))
        .unwrap_or_else(|_| format!("\"{text}\""))
}

/// Python `repr()` 的可用子集：字符串用单引号，容器用 `, ` 分隔。
///
/// 用于和 Python 逐字对齐的校验文案（如 `必须是以下值之一：['a', 'b']`）。
pub fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => {
            let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
            format!("'{escaped}'")
        }
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(items) => {
            let entries: Vec<String> = items.iter().map(python_repr).collect();
            format!("[{}]", entries.join(", "))
        }
        Value::Object(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(key, item)| {
                    format!(
                        "{}: {}",
                        python_repr(&Value::String(key.clone())),
                        python_repr(item)
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
    }
}

/// Python `str()` 里数字的写法：整数值带 `.0`，其余走最短表示。
pub fn python_number_text(value: &Value) -> String {
    match value {
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.to_string(),
            None => {
                let float = number.as_f64().unwrap_or_default();
                if float.fract() == 0.0 {
                    format!("{float:.1}")
                } else {
                    format!("{float}")
                }
            }
        },
        other => other.to_string(),
    }
}

/// 渲染为 Python `json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)`：
/// 紧凑分隔符且键按字典序排列（Token 估算用，必须与 Python 逐字节一致）。
pub fn python_dumps_compact_sorted(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        json_string(key),
                        python_dumps_compact_sorted(&map[key])
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        }
        Value::Array(items) => {
            let entries: Vec<String> = items.iter().map(python_dumps_compact_sorted).collect();
            format!("[{}]", entries.join(","))
        }
        other => python_dumps(other, 0),
    }
}

/// 渲染为 Python `json.dumps(value, ensure_ascii=False, sort_keys=True)`：
/// 默认分隔符（`, ` 与 `: `）且键按字典序排列。
pub fn python_dumps_sorted(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let entries: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}: {}", json_string(key), python_dumps_sorted(&map[key])))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        Value::Array(items) => {
            let entries: Vec<String> = items.iter().map(python_dumps_sorted).collect();
            format!("[{}]", entries.join(", "))
        }
        other => python_dumps(other, 0),
    }
}
