//! JSON 取值助手：把 Python 的取值习惯（按属性或键、真值判定、str 化）固定下来。

use serde_json::Value;

/// Python 真值语义：`None`/空串/空容器/0/`False` 为假。
pub(crate) fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
    }
}

/// 数值/布尔的 str 化按 JSON 写法（Python `str()` 会给出 `True`，见 README 的已知差异）。
pub(crate) fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Python `json.dumps(value, ensure_ascii=False)` 的等价写法：分隔符带空格、非 ASCII 原样输出、
/// 键保持插入序（workspace 开了 `serde_json/preserve_order`）。
///
/// 浮点写法与 Python `repr` 不同（`1e20` vs `1e+20`），差异见 README 的「已知差异」。
pub(crate) fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => out.push_str(&Value::String(text.clone()).to_string()),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(entries) => {
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(out, &Value::String(key.clone()));
                out.push_str(": ");
                write_value(out, item);
            }
            out.push('}');
        }
    }
}
