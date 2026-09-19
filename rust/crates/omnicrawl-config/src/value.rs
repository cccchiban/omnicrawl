//! TOML 值与 Python 语义之间的桥接。
//!
//! 配置解析大量使用 Python 的 truthiness 与 `str()`：`str(raw.get("x") or "")`
//! 会把 `0`、`false`、`""`、空列表、空对象一律折成空串。这里把这层语义显式写出来。

use crate::toml::{Table, Value};

/// Python truthiness：`""`、`0`、`0.0`、`false`、空数组、空表都是假。
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.is_empty(),
        Value::Integer(number) => *number != 0,
        Value::Float(number) => *number != 0.0,
        Value::Boolean(flag) => *flag,
        Value::Datetime(_) => true,
        Value::Array(items) => !items.is_empty(),
        Value::Table(table) => !table.is_empty(),
    }
}

/// Python `str(value or "")`：falsy 成空串，字符串原样，其余走 `repr`。
pub fn python_str(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(value) if !truthy(value) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(value) => python_repr(value),
    }
}

/// Python `repr` 的可用子集（配置里只会出现标量、列表与对象）。
pub fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => python_string_repr(text),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) => crate::toml::float_repr(*number),
        Value::Boolean(flag) => {
            if *flag {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Datetime(datetime) => datetime.to_string(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(python_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Table(table) => python_dict_repr(table),
    }
}

fn python_dict_repr(table: &Table) -> String {
    let parts: Vec<String> = table
        .iter()
        .map(|(key, value)| format!("{}: {}", python_string_repr(key), python_repr(value)))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

fn python_string_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch => out.push(ch),
        }
    }
    out.push(quote);
    out
}

/// TOML 值 → JSON 值（内核能力解析等复用 JSON 形态的入口）。
pub fn toml_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(text) => serde_json::Value::String(text.clone()),
        Value::Integer(number) => serde_json::Value::from(*number),
        Value::Float(number) => serde_json::Value::from(*number),
        Value::Boolean(flag) => serde_json::Value::Bool(*flag),
        Value::Datetime(datetime) => serde_json::Value::String(datetime.to_string()),
        Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect::<Vec<_>>())
        }
        Value::Table(table) => toml_to_json_object(table),
    }
}

/// TOML 表 → JSON 对象。
pub fn toml_to_json_object(table: &Table) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, item) in table {
        map.insert(key.clone(), toml_to_json(item));
    }
    serde_json::Value::Object(map)
}

/// `serde_json::Value` → TOML 值；`null` 按 `_strip_none` 语义丢弃（调用方已过滤键）。
pub fn json_to_toml(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Boolean(false),
        serde_json::Value::Bool(flag) => Value::Boolean(*flag),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::Integer(integer)
            } else {
                Value::Float(number.as_f64().unwrap_or_default())
            }
        }
        serde_json::Value::String(text) => Value::String(text.clone()),
        serde_json::Value::Array(items) => {
            Value::Array(items.iter().map(json_to_toml).collect::<Vec<Value>>())
        }
        serde_json::Value::Object(_) => Value::Table(json_object_to_table(value)),
    }
}

/// JSON 对象 → TOML 表；`null` 键按 `_strip_none` 语义丢弃。
pub fn json_object_to_table(value: &serde_json::Value) -> Table {
    let mut table = Table::new();
    if let Some(map) = value.as_object() {
        for (key, item) in map {
            if item.is_null() {
                continue;
            }
            table.insert(key.clone(), json_to_toml(item));
        }
    }
    table
}
