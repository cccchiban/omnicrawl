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
