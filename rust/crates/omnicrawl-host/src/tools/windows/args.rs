//! 桌面工具共用的参数读取与错误文案：与 Python 的 `_read_*` 助手逐字对齐。

use omnicrawl_controllers::json::python_number_text;
use serde_json::{Map, Value};

use super::super::error::ToolError;

/// 最近一次 Win32 调用的错误码。
pub fn last_win32_error() -> u32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32
}

/// 与 Python `_raise_last_error` 同形：有错误码时带上系统消息，否则只有前缀。
pub fn last_error(prefix: &str) -> ToolError {
    let code = last_win32_error();
    if code == 0 {
        return ToolError::new(format!("{prefix}。"));
    }
    ToolError::new(format!(
        "{prefix}：{}（错误码 {code}）。",
        system_message(code)
    ))
}

fn system_message(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32)
        .to_string()
        .replace(&format!(" (os error {code})"), "")
        .trim()
        .to_string()
}

pub fn ensure_allowed_keys(
    arguments: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), ToolError> {
    let mut unsupported: Vec<&str> = arguments
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    unsupported.sort_unstable();
    if unsupported.is_empty() {
        return Ok(());
    }
    Err(ToolError::new(format!(
        "不支持的参数：{}。",
        unsupported.join("、")
    )))
}

pub fn read_action(arguments: &Map<String, Value>, allowed: &[&str]) -> Result<String, ToolError> {
    let Some(Value::String(raw)) = arguments.get("action") else {
        return Err(ToolError::new("action 必须是字符串。"));
    };
    let action = raw.trim().to_lowercase();
    if !allowed.contains(&action.as_str()) {
        let mut sorted = allowed.to_vec();
        sorted.sort_unstable();
        return Err(ToolError::new(format!(
            "action 不支持：{raw}。可用值：{}。",
            sorted.join("、")
        )));
    }
    Ok(action)
}

pub fn read_optional_text(
    arguments: &Map<String, Value>,
    key: &str,
    maximum: usize,
) -> Result<Option<String>, ToolError> {
    let value = match arguments.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let Value::String(text) = value else {
        return Err(ToolError::new(format!("{key} 必须是字符串。")));
    };
    let normalized = text.trim();
    if normalized.is_empty() {
        return Ok(None);
    }
    if normalized.chars().count() > maximum {
        return Err(ToolError::new(format!("{key} 不能超过 {maximum} 个字符。")));
    }
    Ok(Some(normalized.to_string()))
}

pub fn read_bool(
    arguments: &Map<String, Value>,
    key: &str,
    default: bool,
) -> Result<bool, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(ToolError::new(format!("{key} 必须是布尔值。"))),
    }
}

pub fn read_bounded_int(
    arguments: &Map<String, Value>,
    key: &str,
    default: i64,
    minimum: i64,
    maximum: i64,
    allow_zero: bool,
) -> Result<i64, ToolError> {
    let raw = match arguments.get(key) {
        None | Some(Value::Null) => Value::from(default),
        Some(value) => value.clone(),
    };
    let value = match &raw {
        Value::Bool(_) => return Err(ToolError::new(format!("{key} 必须是整数。"))),
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.trunc() as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    let Some(value) = value else {
        return Err(ToolError::new(format!("{key} 必须是整数。")));
    };
    if value < minimum || value > maximum || (!allow_zero && value == 0) {
        let mut range = format!("{minimum} 到 {maximum}");
        if !allow_zero {
            range.push_str("，且不能为 0");
        }
        return Err(ToolError::new(format!("{key} 必须在 {range} 之间。")));
    }
    Ok(value)
}

#[allow(dead_code)] // 控件操作（windows_control）与截图参数解析共用这一层助手。
pub fn read_optional_bounded_int(
    arguments: &Map<String, Value>,
    key: &str,
    minimum: i64,
    maximum: i64,
) -> Result<Option<i64>, ToolError> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => read_bounded_int(arguments, key, minimum, minimum, maximum, true).map(Some),
    }
}

/// `window_handle`：接受十进制整数或 `0x` 前缀的十六进制字符串。
pub fn read_window_handle(arguments: &Map<String, Value>) -> Result<isize, ToolError> {
    const INVALID: &str = "window_handle 必须是十进制整数或 0x 开头的十六进制字符串。";
    let value = arguments.get("window_handle");
    let handle = match value {
        None | Some(Value::Null) | Some(Value::Bool(_)) => return Err(ToolError::new(INVALID)),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.trunc() as i64)),
        Some(Value::String(text)) => parse_handle_text(text.trim()),
        _ => None,
    };
    let Some(handle) = handle else {
        return Err(ToolError::new(INVALID));
    };
    if handle <= 0 {
        return Err(ToolError::new("window_handle 必须是正整数。"));
    }
    Ok(handle as isize)
}

fn parse_handle_text(text: &str) -> Option<i64> {
    let stripped = text.trim();
    match stripped
        .strip_prefix("0x")
        .or_else(|| stripped.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => stripped.parse::<i64>().ok(),
    }
}

/// `x`/`y`：必须成对出现；`required` 为真时缺一不可。
pub fn read_coordinates(
    arguments: &Map<String, Value>,
    required: bool,
) -> Result<(Option<i64>, Option<i64>), ToolError> {
    let has_x = arguments.get("x").is_some_and(|value| !value.is_null());
    let has_y = arguments.get("y").is_some_and(|value| !value.is_null());
    if has_x != has_y {
        return Err(ToolError::new("x 与 y 必须同时提供。"));
    }
    if !has_x {
        if required {
            return Err(ToolError::new("该操作必须同时提供 x 与 y。"));
        }
        return Ok((None, None));
    }
    let x = read_bounded_int(arguments, "x", 0, -100_000, 100_000, true)?;
    let y = read_bounded_int(arguments, "y", 0, -100_000, 100_000, true)?;
    Ok((Some(x), Some(y)))
}

/// `str(value or "")` 的可用子集：数字与布尔也能读成文本。
pub fn argument_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => if *flag { "True" } else { "False" }.to_string(),
        Some(Value::Number(_)) => python_number_text(value.expect("数字")),
        Some(other) => value_to_text(other),
    }
}

fn value_to_text(value: &Value) -> String {
    omnicrawl_controllers::json::python_repr(value)
}
