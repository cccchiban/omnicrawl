//! `windows_input`：受约束的鼠标与键盘输入（SendInput）。

use omnicrawl_controllers::json::python_dumps;
use serde_json::{json, Map, Value};

use super::super::error::{ToolError, ToolOutcome};
use super::args::{
    ensure_allowed_keys, last_error, read_action, read_bounded_int, read_coordinates,
};
use super::ffi;

const MAX_INPUT_TEXT_CHARS: i64 = 4_096;

const ALLOWED_KEYS: [&str; 10] = [
    "action",
    "x",
    "y",
    "button",
    "clicks",
    "wheel_delta",
    "key",
    "keys",
    "presses",
    "text",
];

pub fn windows_input(arguments: &Map<String, Value>) -> ToolOutcome {
    let payload = operation(arguments)?;
    Ok(python_dumps(&payload, 2))
}

fn operation(arguments: &Map<String, Value>) -> Result<Value, ToolError> {
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let action = read_action(
        arguments,
        &["move", "click", "scroll", "key", "hotkey", "type_text"],
    )?;
    match action.as_str() {
        "move" => {
            let (x, y) = read_coordinates(arguments, true)?;
            let (x, y) = (x.expect("必填 x"), y.expect("必填 y"));
            send_inputs(&[absolute_mouse_input(x, y, ffi::MOUSEEVENTF_MOVE)?])?;
            Ok(json!({"action": "move", "x": x, "y": y}))
        }
        "click" => {
            let (x, y) = read_coordinates(arguments, true)?;
            let (x, y) = (x.expect("必填 x"), y.expect("必填 y"));
            let button = match arguments.get("button") {
                None | Some(Value::Null) => "left".to_string(),
                Some(value) => super::args::argument_text(Some(value))
                    .trim()
                    .to_lowercase(),
            };
            let (down, up) = match button.as_str() {
                "left" => (ffi::MOUSEEVENTF_LEFTDOWN, ffi::MOUSEEVENTF_LEFTUP),
                "right" => (ffi::MOUSEEVENTF_RIGHTDOWN, ffi::MOUSEEVENTF_RIGHTUP),
                "middle" => (ffi::MOUSEEVENTF_MIDDLEDOWN, ffi::MOUSEEVENTF_MIDDLEUP),
                _ => return Err(ToolError::new("button 仅支持 left、right 或 middle。")),
            };
            let clicks = read_bounded_int(arguments, "clicks", 1, 1, 3, true)?;
            let mut inputs = vec![absolute_mouse_input(x, y, ffi::MOUSEEVENTF_MOVE)?];
            for _ in 0..clicks {
                inputs.push(mouse_wheel(down, 0));
                inputs.push(mouse_wheel(up, 0));
            }
            send_inputs(&inputs)?;
            Ok(json!({"action": "click", "x": x, "y": y, "button": button, "clicks": clicks}))
        }
        "scroll" => {
            let (x, y) = read_coordinates(arguments, false)?;
            let delta = read_bounded_int(arguments, "wheel_delta", 0, -12_000, 12_000, false)?;
            let mut inputs = Vec::new();
            if let (Some(x), Some(y)) = (x, y) {
                inputs.push(absolute_mouse_input(x, y, ffi::MOUSEEVENTF_MOVE)?);
            }
            inputs.push(mouse_input(0, 0, ffi::MOUSEEVENTF_WHEEL, delta as u32));
            send_inputs(&inputs)?;
            let mut payload = json!({"action": "scroll", "wheel_delta": delta});
            if let (Some(x), Some(y)) = (x, y) {
                payload["x"] = Value::from(x);
                payload["y"] = Value::from(y);
            }
            Ok(payload)
        }
        "key" => {
            let key = normalise_virtual_key(arguments.get("key"))?;
            let presses = read_bounded_int(arguments, "presses", 1, 1, 10, true)?;
            send_virtual_keys(std::slice::from_ref(&key), presses)?;
            Ok(json!({"action": "key", "key": key, "presses": presses}))
        }
        "hotkey" => {
            let Some(Value::Array(raw_keys)) = arguments.get("keys") else {
                return Err(ToolError::new(
                    "hotkey 的 keys 必须是包含 2 到 5 个键名的数组。",
                ));
            };
            if raw_keys.len() < 2 || raw_keys.len() > 5 {
                return Err(ToolError::new(
                    "hotkey 的 keys 必须是包含 2 到 5 个键名的数组。",
                ));
            }
            let mut keys = Vec::new();
            for raw in raw_keys {
                keys.push(normalise_virtual_key(Some(raw))?);
            }
            let mut unique: Vec<&String> = keys.iter().collect();
            unique.sort();
            unique.dedup();
            if unique.len() != keys.len() {
                return Err(ToolError::new("hotkey 的 keys 不能包含重复键名。"));
            }
            send_virtual_keys(&keys, 1)?;
            Ok(json!({"action": "hotkey", "keys": keys}))
        }
        _ => {
            let Some(Value::String(text)) = arguments.get("text") else {
                return Err(ToolError::new("type_text 的 text 必须是非空字符串。"));
            };
            if text.is_empty() {
                return Err(ToolError::new("type_text 的 text 必须是非空字符串。"));
            }
            let text_length = text.chars().count() as i64;
            if text_length > MAX_INPUT_TEXT_CHARS {
                return Err(ToolError::new(format!(
                    "text 不能超过 {MAX_INPUT_TEXT_CHARS} 个字符。"
                )));
            }
            send_unicode_text(text)?;
            // 不回显文本，避免输入密码、令牌等内容被工具结果或会话记录持久化。
            Ok(json!({"action": "type_text", "text_length": text_length}))
        }
    }
}

fn mouse_input(dx: i32, dy: i32, flags: u32, mouse_data: u32) -> ffi::Input {
    ffi::Input {
        kind: ffi::INPUT_MOUSE,
        data: ffi::InputUnion {
            mouse: ffi::MouseInput {
                dx,
                dy,
                mouse_data,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn mouse_wheel(flags: u32, mouse_data: u32) -> ffi::Input {
    mouse_input(0, 0, flags, mouse_data)
}

fn keyboard_input(virtual_key: u16, scan_code: u16, flags: u32) -> ffi::Input {
    ffi::Input {
        kind: ffi::INPUT_KEYBOARD,
        data: ffi::InputUnion {
            keyboard: ffi::KeyboardInput {
                virtual_key,
                scan_code,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn virtual_desktop_metrics() -> (i64, i64, i64, i64) {
    let left = unsafe { ffi::GetSystemMetrics(ffi::SM_XVIRTUALSCREEN) } as i64;
    let top = unsafe { ffi::GetSystemMetrics(ffi::SM_YVIRTUALSCREEN) } as i64;
    let width = unsafe { ffi::GetSystemMetrics(ffi::SM_CXVIRTUALSCREEN) } as i64;
    let height = unsafe { ffi::GetSystemMetrics(ffi::SM_CYVIRTUALSCREEN) } as i64;
    (left, top, width, height)
}

/// 物理像素坐标 → SendInput 的 0..65535 归一化坐标。
fn absolute_mouse_input(x: i64, y: i64, flags: u32) -> Result<ffi::Input, ToolError> {
    let (left, top, width, height) = virtual_desktop_metrics();
    if width <= 1 || height <= 1 {
        return Err(ToolError::new("无法读取有效的虚拟桌面尺寸。"));
    }
    if !(left <= x && x < left + width && top <= y && y < top + height) {
        return Err(ToolError::new(format!(
            "坐标超出虚拟桌面范围：x={x}、y={y}，范围为 [{left}, {}] × [{top}, {}]。",
            left + width - 1,
            top + height - 1
        )));
    }
    let normalized_x = python_round((x - left) as f64 * 65_535.0 / (width - 1) as f64) as i32;
    let normalized_y = python_round((y - top) as f64 * 65_535.0 / (height - 1) as f64) as i32;
    Ok(mouse_input(
        normalized_x,
        normalized_y,
        flags | ffi::MOUSEEVENTF_ABSOLUTE | ffi::MOUSEEVENTF_VIRTUALDESK,
        0,
    ))
}

/// Python `round()` 是银行家舍入（.5 取偶），与 Rust 默认的 half-away-from-zero 不同。
fn python_round(value: f64) -> f64 {
    if (value - value.trunc()).abs() == 0.5 {
        let floor = value.floor();
        return if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        };
    }
    value.round()
}

fn send_virtual_keys(keys: &[String], presses: i64) -> Result<(), ToolError> {
    let codes: Vec<u32> = keys
        .iter()
        .map(|key| virtual_key_code(key).unwrap_or(0))
        .collect();
    let mut inputs = Vec::new();
    if codes.len() == 1 {
        for _ in 0..presses {
            inputs.push(keyboard_input(codes[0] as u16, 0, 0));
            inputs.push(keyboard_input(codes[0] as u16, 0, ffi::KEYEVENTF_KEYUP));
        }
    } else {
        for code in &codes {
            inputs.push(keyboard_input(*code as u16, 0, 0));
        }
        for code in codes.iter().rev() {
            inputs.push(keyboard_input(*code as u16, 0, ffi::KEYEVENTF_KEYUP));
        }
    }
    send_inputs(&inputs)
}

fn send_unicode_text(text: &str) -> Result<(), ToolError> {
    let mut inputs = Vec::new();
    for unit in text.encode_utf16() {
        inputs.push(keyboard_input(0, unit, ffi::KEYEVENTF_UNICODE));
        inputs.push(keyboard_input(
            0,
            unit,
            ffi::KEYEVENTF_UNICODE | ffi::KEYEVENTF_KEYUP,
        ));
    }
    send_inputs(&inputs)
}

fn send_inputs(inputs: &[ffi::Input]) -> Result<(), ToolError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe {
        ffi::SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<ffi::Input>() as i32,
        )
    };
    if sent as usize != inputs.len() {
        return Err(last_error(&format!(
            "SendInput 仅提交了 {sent}/{} 个输入事件",
            inputs.len()
        )));
    }
    Ok(())
}

const KEY_NAME_ALIASES: [(&str, &str); 9] = [
    ("control", "ctrl"),
    ("escape", "esc"),
    ("return", "enter"),
    ("pgup", "page_up"),
    ("pgdn", "page_down"),
    ("del", "delete"),
    ("ins", "insert"),
    ("windows", "win"),
    ("lwin", "win"),
];

fn normalise_virtual_key(value: Option<&Value>) -> Result<String, ToolError> {
    let Some(Value::String(raw)) = value else {
        return Err(ToolError::new("键名必须是字符串。"));
    };
    let mut key = raw.trim().to_lowercase().replace(['-', ' '], "_");
    if let Some((_, alias)) = KEY_NAME_ALIASES.iter().find(|(name, _)| *name == key) {
        key = (*alias).to_string();
    }
    let mut chars = key.chars();
    if let (Some(single), None) = (chars.next(), chars.next()) {
        if single.is_alphabetic() {
            key = key.to_uppercase();
        }
    }
    if virtual_key_code(&key).is_none() {
        return Err(ToolError::new(format!("不支持的键名：{raw}。")));
    }
    Ok(key)
}

/// Python `VIRTUAL_KEY_CODES` 的等价查表（含数字、字母与 F1–F24）。
fn virtual_key_code(key: &str) -> Option<u32> {
    let named = match key {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" => 0x0D,
        "shift" => 0x10,
        "ctrl" => 0x11,
        "alt" => 0x12,
        "pause" => 0x13,
        "caps_lock" => 0x14,
        "esc" => 0x1B,
        "space" => 0x20,
        "page_up" => 0x21,
        "page_down" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "print_screen" => 0x2C,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "win" => 0x5B,
        "rwin" => 0x5C,
        "apps" => 0x5D,
        "num_lock" => 0x90,
        "scroll_lock" => 0x91,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    if key.len() == 1 {
        let single = key.chars().next().unwrap_or_default();
        if single.is_ascii_digit() {
            return Some(0x30 + single.to_digit(10).unwrap_or(0));
        }
        if single.is_ascii_uppercase() {
            return Some(single as u32);
        }
    }
    if let Some(rest) = key.strip_prefix('f') {
        if let Ok(number) = rest.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Some(0x6F + number);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn keys_are_normalised_and_validated() {
        assert_eq!(
            normalise_virtual_key(Some(&json!("Control"))).unwrap(),
            "ctrl"
        );
        assert_eq!(
            normalise_virtual_key(Some(&json!(" page-down "))).unwrap(),
            "page_down"
        );
        assert_eq!(normalise_virtual_key(Some(&json!("a"))).unwrap(), "A");
        assert_eq!(virtual_key_code("A"), Some(0x41));
        assert_eq!(virtual_key_code("7"), Some(0x37));
        assert_eq!(virtual_key_code("f24"), Some(0x87));
        assert_eq!(virtual_key_code("f25"), None);

        let error =
            normalise_virtual_key(Some(&json!("whatever"))).expect_err("未知键名应当被拒绝");
        assert_eq!(error.message, "不支持的键名：whatever。");
    }

    #[test]
    fn input_arguments_are_validated_before_any_event_is_sent() {
        let error = windows_input(&arguments(json!({"action": "hotkey", "keys": ["ctrl"]})))
            .expect_err("按键过少应当被拒绝");
        assert_eq!(
            error.message,
            "hotkey 的 keys 必须是包含 2 到 5 个键名的数组。"
        );

        let error = windows_input(&arguments(
            json!({"action": "hotkey", "keys": ["ctrl", "ctrl"]}),
        ))
        .expect_err("重复键名应当被拒绝");
        assert_eq!(error.message, "hotkey 的 keys 不能包含重复键名。");

        let error = windows_input(&arguments(json!({"action": "click", "x": 1})))
            .expect_err("只给 x 应当被拒绝");
        assert_eq!(error.message, "x 与 y 必须同时提供。");

        let error =
            windows_input(&arguments(json!({"action": "click"}))).expect_err("缺坐标应当被拒绝");
        assert_eq!(error.message, "该操作必须同时提供 x 与 y。");

        let error = windows_input(&arguments(
            json!({"action": "click", "x": 1, "y": 1, "button": "side"}),
        ))
        .expect_err("不支持的按键应当被拒绝");
        assert_eq!(error.message, "button 仅支持 left、right 或 middle。");

        let error = windows_input(&arguments(json!({"action": "type_text", "text": ""})))
            .expect_err("空文本应当被拒绝");
        assert_eq!(error.message, "type_text 的 text 必须是非空字符串。");

        let error = windows_input(&arguments(json!({"action": "scroll", "wheel_delta": 0})))
            .expect_err("滚轮增量不能为 0");
        assert_eq!(
            error.message,
            "wheel_delta 必须在 -12000 到 12000，且不能为 0 之间。"
        );

        let error = windows_input(&arguments(json!({"action": "move", "x": 99_999, "y": 0})))
            .expect_err("越界坐标应当被拒绝");
        assert!(
            error.message.starts_with("坐标超出虚拟桌面范围："),
            "{}",
            error.message
        );
    }
}
