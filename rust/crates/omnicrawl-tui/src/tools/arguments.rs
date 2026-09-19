//! 工具参数的宽松读取：与 Python `workspace/tools.py` 的 `_read_limited_int` 等helper 对齐。
//!
//! 模型给的参数经常是字符串形态（`"200"`）或显式空串，读取规则必须与 Python 一致：
//! 布尔值不当整数、解析失败回落默认值、区间外夹紧。

use serde_json::{Map, Value};

/// 读整数并按区间夹紧；布尔值、非法值与缺失一律回落默认值。
pub fn limited_int(
    arguments: &Map<String, Value>,
    key: &str,
    default: i64,
    minimum: i64,
    maximum: i64,
) -> i64 {
    let Some(value) = arguments.get(key) else {
        return default;
    };
    if value.is_boolean() {
        return default;
    }
    let parsed = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    match parsed {
        Some(number) => number.clamp(minimum, maximum),
        None => default,
    }
}

/// 读可选字符串并去掉首尾空白；非字符串视作未提供。
pub fn optional_text(arguments: &Map<String, Value>, key: &str) -> String {
    match arguments.get(key) {
        Some(Value::String(text)) => text.trim().to_string(),
        _ => String::new(),
    }
}

/// 读字符串本体（不做 trim）；非字符串视作空串。
pub fn raw_text(arguments: &Map<String, Value>, key: &str) -> String {
    match arguments.get(key) {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

pub fn optional_bool(arguments: &Map<String, Value>, key: &str, default: bool) -> bool {
    match arguments.get(key) {
        Some(Value::Bool(value)) => *value,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn limited_int_clamps_and_falls_back() {
        assert_eq!(limited_int(&args(json!({"n": 5})), "n", 1, 1, 500), 5);
        assert_eq!(limited_int(&args(json!({"n": "200"})), "n", 1, 1, 500), 200);
        assert_eq!(limited_int(&args(json!({"n": 9999})), "n", 1, 1, 500), 500);
        assert_eq!(limited_int(&args(json!({"n": 0})), "n", 1, 1, 500), 1);
        assert_eq!(limited_int(&args(json!({"n": "abc"})), "n", 7, 1, 500), 7);
        assert_eq!(limited_int(&args(json!({"n": true})), "n", 7, 1, 500), 7);
        assert_eq!(limited_int(&args(json!({})), "n", 3, 1, 500), 3);
    }

    #[test]
    fn optional_text_trims_and_casts() {
        assert_eq!(optional_text(&args(json!({"t": "  x  "})), "t"), "x");
        assert_eq!(optional_text(&args(json!({"t": 5})), "t"), "");
        assert!(raw_text(&args(json!({"t": "  x  "})), "t").starts_with("  "));
        assert!(optional_bool(&args(json!({"b": true})), "b", false));
        assert!(!optional_bool(&args(json!({"b": "yes"})), "b", false));
    }
}
