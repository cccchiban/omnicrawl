//! `windows_input`：受约束的鼠标与键盘输入（SendInput）。
//!
//! 每次成功动作后附带一张当前画面截图：模型核对点击/输入效果不必再单独截图。

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use omnicrawl_controllers::json::python_dumps;
use serde_json::{json, Map, Value};

use super::super::error::ToolError;
use super::super::paths::WorkspacePaths;
use super::args::{
    ensure_allowed_keys, last_error, read_action, read_bounded_int, read_coordinates,
};
use super::ffi;
use super::screenshot::{snapshot_attachments, DesktopOutcome};

const MAX_INPUT_TEXT_CHARS: i64 = 4_096;
/// 单次按住的最长时长：更久的按住只会把指针卡在按下状态。
const MAX_HOLD_MS: i64 = 10_000;
/// 一次 `actions` 里最多的动作数：够拼「输入网址 + 回车」这类短序列，又不至于变脚本。
const MAX_ACTIONS: usize = 20;

const ALLOWED_KEYS: [&str; 12] = [
    "action",
    "actions",
    "x",
    "y",
    "button",
    "clicks",
    "hold_ms",
    "wheel_delta",
    "key",
    "keys",
    "presses",
    "text",
];

/// 没有按键处于按下状态。
const NO_BUTTON: u8 = 255;
/// 已按下但尚未释放的鼠标键（0/1/2 = left/right/middle）。
///
/// 长按与拖拽要求「按下」和「松开」跨两次工具调用，进程退出时若停在按下状态，
/// 用户的指针会一直粘着：因此 `press` 前先释放残留按键，`release` 幂等。
static PRESSED_BUTTON: AtomicU8 = AtomicU8::new(NO_BUTTON);

pub fn windows_input(
    paths: &WorkspacePaths,
    arguments: &Map<String, Value>,
) -> Result<DesktopOutcome, ToolError> {
    let mut payload = operation(arguments)?;
    let images = if changes_screen(arguments) {
        snapshot_attachments(paths, &mut payload)
    } else {
        Vec::new()
    };
    Ok(DesktopOutcome {
        output: python_dumps(&payload, 2),
        images,
    })
}

fn operation(arguments: &Map<String, Value>) -> Result<Value, ToolError> {
    match arguments.get("actions") {
        None | Some(Value::Null) => execute_plan(plan_action(arguments)?),
        Some(_) => run_actions(arguments),
    }
}

/// 这次调用会不会改变画面：移动指针和按下都不算，所以拖拽中间步骤不截图。
///
/// 拖拽是 press → move → release，每一步都配图只会让上下文堆满同一画面；
/// 收尾的 release 才截一张。
fn changes_screen(arguments: &Map<String, Value>) -> bool {
    const SILENT: [&str; 2] = ["move", "press"];
    let quiet = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(|action| SILENT.contains(&action.trim().to_lowercase().as_str()))
            .unwrap_or(false)
    };
    match arguments.get("actions") {
        Some(Value::Array(items)) => !items.iter().all(|item| quiet(item.get("action"))),
        _ => !quiet(arguments.get("action")),
    }
}

/// 一次调用执行多个动作：`actions` 是动作对象数组，按顺序各跑一遍。
///
/// 存在的理由是「输入网址 + 回车」这类成对操作：拆成两次调用会在中间被别的事件
/// 插队（焦点被抢、弹窗出现），也白白多一次模型往返。
fn run_actions(arguments: &Map<String, Value>) -> Result<Value, ToolError> {
    let Some(Value::Array(items)) = arguments.get("actions") else {
        return Err(ToolError::new("actions 必须是动作对象数组。"));
    };
    if arguments.contains_key("action") {
        return Err(ToolError::new(
            "actions 与 action 不能同时提供：顶层 action 只用于单个动作。",
        ));
    }
    // 顶层除 actions 外不再接受别的按键：动作参数一律写在各自的元素里。
    let mut stray: Vec<&str> = arguments
        .keys()
        .map(String::as_str)
        .filter(|key| *key != "actions")
        .collect();
    stray.sort_unstable();
    if !stray.is_empty() {
        return Err(ToolError::new(format!(
            "使用 actions 时顶层不支持参数：{}；请写进各个动作。",
            stray.join("、")
        )));
    }
    if items.len() > MAX_ACTIONS {
        return Err(ToolError::new(format!(
            "actions 最多 {MAX_ACTIONS} 个动作。"
        )));
    }
    // 先把整串动作校验成计划再执行：中间某一步参数写错时不能留下执行了一半的序列。
    let mut plans = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(action) = item.as_object() else {
            return Err(ToolError::new(format!("actions[{index}] 必须是动作对象。")));
        };
        if action.contains_key("actions") {
            return Err(ToolError::new(format!(
                "actions[{index}] 不能再嵌套 actions。"
            )));
        }
        let plan = plan_action(action)
            .map_err(|error| ToolError::new(format!("actions[{index}] 失败：{}", error.message)))?;
        plans.push(plan);
    }
    let mut results = Vec::new();
    for (index, plan) in plans.into_iter().enumerate() {
        let mut payload = execute_plan(plan)?;
        if let Value::Object(map) = &mut payload {
            map.insert("index".to_string(), Value::from(index));
        }
        results.push(payload);
    }
    Ok(json!({"action": "actions", "count": results.len(), "results": results}))
}

/// 一个待执行的动作：解析与校验都在这一步完成，执行阶段不再报参数错误。
enum Plan {
    Move {
        x: i64,
        y: i64,
    },
    Click {
        x: i64,
        y: i64,
        button: String,
        clicks: i64,
        hold_ms: i64,
    },
    Press {
        position: Option<(i64, i64)>,
        button: String,
    },
    Release,
    Scroll {
        position: Option<(i64, i64)>,
        delta: i64,
    },
    Key {
        key: String,
        presses: i64,
    },
    Hotkey {
        keys: Vec<String>,
    },
    TypeText {
        text: String,
        length: i64,
    },
}

const ACTIONS: [&str; 8] = [
    "move",
    "click",
    "press",
    "release",
    "scroll",
    "key",
    "hotkey",
    "type_text",
];

fn plan_action(arguments: &Map<String, Value>) -> Result<Plan, ToolError> {
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let action = read_action(arguments, &ACTIONS)?;
    Ok(match action.as_str() {
        "move" => {
            let (x, y) = read_coordinates(arguments, true)?;
            let (x, y) = (x.expect("必填 x"), y.expect("必填 y"));
            normalised_position(x, y)?;
            Plan::Move { x, y }
        }
        "click" => {
            let (x, y) = read_coordinates(arguments, true)?;
            let button = read_button(arguments)?;
            let clicks = read_bounded_int(arguments, "clicks", 1, 1, 3, true)?;
            let hold_ms = read_bounded_int(arguments, "hold_ms", 0, 0, MAX_HOLD_MS, true)?;
            if hold_ms > 0 && clicks > 1 {
                return Err(ToolError::new("hold_ms 只适用于单次点击（clicks=1）。"));
            }
            let (x, y) = (x.expect("必填 x"), y.expect("必填 y"));
            normalised_position(x, y)?;
            Plan::Click {
                x,
                y,
                button,
                clicks,
                hold_ms,
            }
        }
        "press" => {
            let (x, y) = read_coordinates(arguments, false)?;
            let button = read_button(arguments)?;
            let position = x.zip(y);
            if let Some((x, y)) = position {
                normalised_position(x, y)?;
            }
            Plan::Press { position, button }
        }
        "release" => Plan::Release,
        "scroll" => {
            let (x, y) = read_coordinates(arguments, false)?;
            let delta = read_bounded_int(arguments, "wheel_delta", 0, -12_000, 12_000, false)?;
            let position = x.zip(y);
            if let Some((x, y)) = position {
                normalised_position(x, y)?;
            }
            Plan::Scroll { position, delta }
        }
        "key" => Plan::Key {
            key: normalise_virtual_key(arguments.get("key"))?,
            presses: read_bounded_int(arguments, "presses", 1, 1, 10, true)?,
        },
        "hotkey" => Plan::Hotkey {
            keys: read_hotkey_keys(arguments)?,
        },
        _ => {
            let Some(Value::String(text)) = arguments.get("text") else {
                return Err(ToolError::new("type_text 的 text 必须是非空字符串。"));
            };
            if text.is_empty() {
                return Err(ToolError::new("type_text 的 text 必须是非空字符串。"));
            }
            let length = text.chars().count() as i64;
            if length > MAX_INPUT_TEXT_CHARS {
                return Err(ToolError::new(format!(
                    "text 不能超过 {MAX_INPUT_TEXT_CHARS} 个字符。"
                )));
            }
            Plan::TypeText {
                text: text.clone(),
                length,
            }
        }
    })
}

fn execute_plan(plan: Plan) -> Result<Value, ToolError> {
    match plan {
        Plan::Move { x, y } => {
            send_inputs(&[absolute_mouse_input(x, y, ffi::MOUSEEVENTF_MOVE)?])?;
            Ok(json!({"action": "move", "x": x, "y": y}))
        }
        Plan::Click {
            x,
            y,
            button,
            clicks,
            hold_ms,
        } => {
            let (down, up) = buttonflags(&button)?;
            let data = button_data(&button);
            let mut inputs = vec![absolute_mouse_input(x, y, ffi::MOUSEEVENTF_MOVE)?];
            if hold_ms > 0 {
                // 按住必须跨两次提交：同一次 SendInput 里插入的停留不会让目标窗口
                // 收到独立的「按住」状态。
                inputs.push(mouse_wheel(down, data));
                send_inputs(&inputs)?;
                std::thread::sleep(Duration::from_millis(hold_ms as u64));
                send_inputs(&[mouse_wheel(up, data)])?;
                return Ok(json!({
                    "action": "click",
                    "x": x,
                    "y": y,
                    "button": button,
                    "clicks": clicks,
                    "hold_ms": hold_ms,
                }));
            }
            for _ in 0..clicks {
                inputs.push(mouse_wheel(down, data));
                inputs.push(mouse_wheel(up, data));
            }
            send_inputs(&inputs)?;
            Ok(json!({"action": "click", "x": x, "y": y, "button": button, "clicks": clicks}))
        }
        // 按下后不立即松开：长按、拖拽与滑块的起点。
        Plan::Press { position, button } => {
            let (down, _) = buttonflags(&button)?;
            // 上一次 press 没配对的 release：先松手，避免指针被判定成拖拽中。
            release_pressed()?;
            let mut inputs = Vec::new();
            if let Some((x, y)) = position {
                inputs.push(absolute_mouse_input(x, y, ffi::MOUSEEVENTF_MOVE)?);
            }
            inputs.push(mouse_wheel(down, button_data(&button)));
            send_inputs(&inputs)?;
            PRESSED_BUTTON.store(pressed_code(&button)?, Ordering::SeqCst);
            let mut payload = json!({"action": "press", "button": button});
            if let Some((x, y)) = position {
                payload["x"] = Value::from(x);
                payload["y"] = Value::from(y);
            }
            Ok(payload)
        }
        // 松开：幂等，未按下时也返回成功（拖拽收尾重复调用不该失败）。
        Plan::Release => {
            let released = release_pressed()?;
            Ok(json!({"action": "release", "button": released}))
        }
        Plan::Scroll { position, delta } => {
            let mut inputs = Vec::new();
            if let Some((x, y)) = position {
                inputs.push(absolute_mouse_input(x, y, ffi::MOUSEEVENTF_MOVE)?);
            }
            inputs.push(mouse_input(0, 0, ffi::MOUSEEVENTF_WHEEL, delta as u32));
            send_inputs(&inputs)?;
            let mut payload = json!({"action": "scroll", "wheel_delta": delta});
            if let Some((x, y)) = position {
                payload["x"] = Value::from(x);
                payload["y"] = Value::from(y);
            }
            Ok(payload)
        }
        Plan::Key { key, presses } => {
            send_virtual_keys(std::slice::from_ref(&key), presses)?;
            Ok(json!({"action": "key", "key": key, "presses": presses}))
        }
        Plan::Hotkey { keys } => {
            send_virtual_keys(&keys, 1)?;
            Ok(json!({"action": "hotkey", "keys": keys}))
        }
        Plan::TypeText { text, length } => {
            send_unicode_text(&text)?;
            // 不回显文本，避免输入密码、令牌等内容被工具结果或会话记录持久化。
            Ok(json!({"action": "type_text", "text_length": length}))
        }
    }
}

fn read_hotkey_keys(arguments: &Map<String, Value>) -> Result<Vec<String>, ToolError> {
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
    Ok(keys)
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

/// 读取 `button`：缺省为左键，接受 left、right、middle 以及侧键 back、forward。
fn read_button(arguments: &Map<String, Value>) -> Result<String, ToolError> {
    let button = match arguments.get("button") {
        None | Some(Value::Null) => "left".to_string(),
        Some(value) => super::args::argument_text(Some(value))
            .trim()
            .to_lowercase(),
    };
    // 先校验再发事件：不支持的按键名不应产生任何副作用。
    buttonflags(&button)?;
    Ok(button)
}

/// 按键名 → (down, up) 事件标志。
fn buttonflags(button: &str) -> Result<(u32, u32), ToolError> {
    match button {
        "left" => Ok((ffi::MOUSEEVENTF_LEFTDOWN, ffi::MOUSEEVENTF_LEFTUP)),
        "right" => Ok((ffi::MOUSEEVENTF_RIGHTDOWN, ffi::MOUSEEVENTF_RIGHTUP)),
        "middle" => Ok((ffi::MOUSEEVENTF_MIDDLEDOWN, ffi::MOUSEEVENTF_MIDDLEUP)),
        "back" => Ok((ffi::MOUSEEVENTF_XDOWN, ffi::MOUSEEVENTF_XUP)),
        "forward" => Ok((ffi::MOUSEEVENTF_XDOWN, ffi::MOUSEEVENTF_XUP)),
        _ => Err(ToolError::new(
            "button 仅支持 left、right、middle、back 或 forward。",
        )),
    }
}

/// 侧键要带 `XBUTTON1`/`XBUTTON2` 的 `mouse_data`；普通键这一位为 0。
fn button_data(button: &str) -> u32 {
    match button {
        "back" => ffi::XBUTTON1,
        "forward" => ffi::XBUTTON2,
        _ => 0,
    }
}

/// 按键名 → 按下标记（0/1/2/3/4 = left/right/middle/back/forward）。
fn pressed_code(button: &str) -> Result<u8, ToolError> {
    match button {
        "left" => Ok(0),
        "right" => Ok(1),
        "middle" => Ok(2),
        "back" => Ok(3),
        "forward" => Ok(4),
        _ => Err(ToolError::new(
            "button 仅支持 left、right、middle、back 或 forward。",
        )),
    }
}

/// 释放当前按下的键；返回被释放的键名，未按下时返回 `None`。
///
/// 幂等：拖拽收尾的重复 `release`、以及 `press` 前的残留清理都走这里。
fn release_pressed() -> Result<Option<String>, ToolError> {
    let code = PRESSED_BUTTON.swap(NO_BUTTON, Ordering::SeqCst);
    let button = match code {
        0 => "left",
        1 => "right",
        2 => "middle",
        3 => "back",
        4 => "forward",
        _ => return Ok(None),
    };
    let (_, up) = buttonflags(button)?;
    send_inputs(&[mouse_wheel(up, button_data(button))])?;
    Ok(Some(button.to_string()))
}

/// 进程退出前兜底释放：宁可多松一次，也不要把指针留在按下状态。
pub fn release_on_exit() {
    if PRESSED_BUTTON.load(Ordering::SeqCst) != NO_BUTTON {
        let _ = release_pressed();
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
    let (normalized_x, normalized_y) = normalised_position(x, y)?;
    Ok(mouse_input(
        normalized_x,
        normalized_y,
        flags | ffi::MOUSEEVENTF_ABSOLUTE | ffi::MOUSEEVENTF_VIRTUALDESK,
        0,
    ))
}

/// 坐标校验与归一化：规划阶段先调一次，越界不必等到发事件才发现。
fn normalised_position(x: i64, y: i64) -> Result<(i32, i32), ToolError> {
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
    Ok((
        python_round((x - left) as f64 * 65_535.0 / (width - 1) as f64) as i32,
        python_round((y - top) as f64 * 65_535.0 / (height - 1) as f64) as i32,
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

    /// 参数校验一律在发事件之前失败，路径只用于在成功时落确认截图。
    fn input(value: Value) -> Result<DesktopOutcome, ToolError> {
        windows_input(
            &WorkspacePaths::new(std::env::temp_dir()),
            &arguments(value),
        )
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
        let error =
            input(json!({"action": "hotkey", "keys": ["ctrl"]})).expect_err("按键过少应当被拒绝");
        assert_eq!(
            error.message,
            "hotkey 的 keys 必须是包含 2 到 5 个键名的数组。"
        );

        let error = input(json!({"action": "hotkey", "keys": ["ctrl", "ctrl"]}))
            .expect_err("重复键名应当被拒绝");
        assert_eq!(error.message, "hotkey 的 keys 不能包含重复键名。");

        let error = input(json!({"action": "click", "x": 1})).expect_err("只给 x 应当被拒绝");
        assert_eq!(error.message, "x 与 y 必须同时提供。");

        let error = input(json!({"action": "click"})).expect_err("缺坐标应当被拒绝");
        assert_eq!(error.message, "该操作必须同时提供 x 与 y。");

        let error = input(json!({"action": "click", "x": 1, "y": 1, "button": "side"}))
            .expect_err("不支持的按键应当被拒绝");
        assert_eq!(
            error.message,
            "button 仅支持 left、right、middle、back 或 forward。"
        );

        let error =
            input(json!({"action": "type_text", "text": ""})).expect_err("空文本应当被拒绝");
        assert_eq!(error.message, "type_text 的 text 必须是非空字符串。");

        let error =
            input(json!({"action": "scroll", "wheel_delta": 0})).expect_err("滚轮增量不能为 0");
        assert_eq!(
            error.message,
            "wheel_delta 必须在 -12000 到 12000，且不能为 0 之间。"
        );

        let error =
            input(json!({"action": "move", "x": 99_999, "y": 0})).expect_err("越界坐标应当被拒绝");
        assert!(
            error.message.starts_with("坐标超出虚拟桌面范围："),
            "{}",
            error.message
        );
    }

    #[test]
    fn hold_and_drag_arguments_are_validated() {
        // 一键多击与 hold_ms 组合没有明确定义，必须在发事件前拒绝。
        let error =
            input(json!({"action": "click", "x": 10, "y": 10, "clicks": 2, "hold_ms": 100}))
                .expect_err("多击不能带按住时长");
        assert_eq!(error.message, "hold_ms 只适用于单次点击（clicks=1）。");

        let error = input(json!({"action": "click", "x": 10, "y": 10, "hold_ms": 99_999}))
            .expect_err("按住时长应当有上限");
        assert!(error.message.contains("hold_ms"), "{}", error.message);

        let error = input(json!({"action": "press", "x": 10, "y": 10, "button": "side"}))
            .expect_err("不支持的按键应当被拒绝");
        assert_eq!(
            error.message,
            "button 仅支持 left、right、middle、back 或 forward。"
        );

        // press / release 的坐标是可选的：拖拽时先按下再移动。
        let error = input(json!({"action": "press", "x": 10})).expect_err("只给 x 应当被拒绝");
        assert_eq!(error.message, "x 与 y 必须同时提供。");
    }

    #[test]
    fn side_buttons_map_to_xbutton_flags_and_data() {
        // 前进/后退是 XBUTTON：标志相同，靠 mouse_data 区分，缺了它两个键无法分辨。
        assert_eq!(
            buttonflags("back").unwrap(),
            (ffi::MOUSEEVENTF_XDOWN, ffi::MOUSEEVENTF_XUP)
        );
        assert_eq!(button_data("back"), ffi::XBUTTON1);
        assert_eq!(button_data("forward"), ffi::XBUTTON2);
        assert_eq!(button_data("left"), 0);
        assert_eq!(pressed_code("back").unwrap(), 3);
        assert_eq!(pressed_code("forward").unwrap(), 4);
    }

    #[test]
    fn actions_are_validated_before_any_event_is_sent() {
        let error = input(json!({"actions": "type_text"})).expect_err("actions 必须是数组");
        assert_eq!(error.message, "actions 必须是动作对象数组。");

        let error = input(json!({"actions": ["key"]})).expect_err("元素必须是对象");
        assert_eq!(error.message, "actions[0] 必须是动作对象。");

        // 顶层 action 与 actions 语义冲突，无法判断谁优先，直接拒绝。
        let error = input(json!({"action": "key", "actions": [{"action": "key", "key": "enter"}]}))
            .expect_err("顶层 action 不能与 actions 并存");
        assert_eq!(
            error.message,
            "actions 与 action 不能同时提供：顶层 action 只用于单个动作。"
        );

        let error = input(json!({"actions": [{"action": "key", "key": "enter"}], "text": "泄露"}))
            .expect_err("动作参数不能写到顶层");
        assert_eq!(
            error.message,
            "使用 actions 时顶层不支持参数：text；请写进各个动作。"
        );

        // 逐个动作校验：错误必须指到具体下标，否则模型不知道改哪一条。
        let error =
            input(json!({"actions": [{"action": "key", "key": "enter"}, {"action": "zoom"}]}))
                .expect_err("非法动作应当被拒绝");
        assert!(
            error
                .message
                .starts_with("actions[1] 失败：action 不支持：zoom"),
            "{}",
            error.message
        );

        let error =
            input(json!({"actions": [{"actions": []}]})).expect_err("嵌套 actions 应当被拒绝");
        assert_eq!(error.message, "actions[0] 不能再嵌套 actions。");
    }

    #[test]
    fn later_step_errors_abort_the_whole_sequence_before_any_event() {
        // 第一步与第二步都合法时才有意义；第二步越界必须在发出第一步之前就失败。
        let error = input(json!({
            "actions": [
                {"action": "type_text", "text": "https://example.com"},
                {"action": "click", "x": 99_999, "y": 0}
            ]
        }))
        .expect_err("越界坐标应当在执行前被拒绝");
        assert!(
            error
                .message
                .starts_with("actions[1] 失败：坐标超出虚拟桌面范围："),
            "{}",
            error.message
        );
    }

    #[test]
    fn only_actions_that_change_the_screen_ask_for_a_snapshot() {
        // 指针移动与按住不带确认截图；拖拽由收尾的 release 出一张。
        assert!(!changes_screen(&arguments(
            json!({"action": "move", "x": 1, "y": 1})
        )));
        assert!(!changes_screen(&arguments(
            json!({"action": "press", "x": 1, "y": 1})
        )));
        assert!(changes_screen(&arguments(json!({"action": "release"}))));
        assert!(changes_screen(&arguments(
            json!({"action": "click", "x": 1, "y": 1})
        )));
        assert!(!changes_screen(&arguments(json!({"actions": [
            {"action": "press", "x": 1, "y": 1},
            {"action": "move", "x": 9, "y": 9},
        ]}))));
        assert!(changes_screen(&arguments(json!({"actions": [
            {"action": "type_text", "text": "a"},
            {"action": "key", "key": "enter"},
        ]}))));
    }

    #[test]
    fn actions_count_is_capped() {
        let items: Vec<Value> = (0..21)
            .map(|_| json!({"action": "key", "key": "enter"}))
            .collect();
        let error = input(json!({ "actions": items })).expect_err("动作数量应当有上限");
        assert_eq!(error.message, "actions 最多 20 个动作。");
    }
}
