//! `windows_control`：通过 Windows PowerShell 的 UI Automation 定位与操作控件。
//!
//! PowerShell 脚本与 Python 侧逐字一致（`data/windows_uia.ps1`），请求经
//! `OMNICRAWL_UIA_REQUEST_B64` 传入，结果从标准输出解析。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine;
use omnicrawl_controllers::json::{python_dumps, python_dumps_compact};
use serde_json::{json, Map, Value};

use super::super::error::{ToolError, ToolOutcome};
use super::args::{
    ensure_allowed_keys, read_action, read_bounded_int, read_optional_bounded_int,
    read_optional_text, read_window_handle,
};

const MAX_CONTROL_RESULTS: i64 = 100;
const MAX_CONTROL_TEXT_CHARS: i64 = 8_192;
const UI_AUTOMATION_TIMEOUT_SECONDS: u64 = 30;

const CONTROL_TYPES: [&str; 20] = [
    "button",
    "checkbox",
    "combobox",
    "edit",
    "hyperlink",
    "list",
    "listitem",
    "menu",
    "menuitem",
    "radiobutton",
    "tab",
    "tabitem",
    "text",
    "tree",
    "treeitem",
    "window",
    "pane",
    "document",
    "custom",
    "group",
];

const UIA_SCRIPT: &str = include_str!("data/windows_uia.ps1");

const ALLOWED_KEYS: [&str; 9] = [
    "action",
    "window_handle",
    "name",
    "automation_id",
    "class_name",
    "control_type",
    "index",
    "value",
    "max_results",
];

pub fn windows_control(arguments: &Map<String, Value>) -> ToolOutcome {
    let payload = operation(arguments)?;
    Ok(python_dumps(&payload, 2))
}

fn operation(arguments: &Map<String, Value>) -> Result<Value, ToolError> {
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let action = read_action(
        arguments,
        &["list", "invoke", "set_value", "select", "toggle", "focus"],
    )?;
    let window_handle = read_window_handle(arguments)?;
    let name = read_optional_text(arguments, "name", 512)?;
    let automation_id = read_optional_text(arguments, "automation_id", 512)?;
    let class_name = read_optional_text(arguments, "class_name", 512)?;
    let control_type = read_control_type(arguments)?;

    let has_selector =
        name.is_some() || automation_id.is_some() || class_name.is_some() || control_type.is_some();
    if action != "list" && !has_selector {
        return Err(ToolError::new(
            "控件操作必须至少提供 name、automation_id、class_name 或 control_type 之一作为定位条件。",
        ));
    }

    let index = read_optional_bounded_int(arguments, "index", 0, MAX_CONTROL_RESULTS - 1)?;
    let max_results = if action == "list" {
        read_bounded_int(arguments, "max_results", 30, 1, MAX_CONTROL_RESULTS, true)?
    } else {
        0
    };

    let value = if action == "set_value" {
        let Some(Value::String(text)) = arguments.get("value") else {
            return Err(ToolError::new("set_value 的 value 必须是字符串。"));
        };
        if text.chars().count() as i64 > MAX_CONTROL_TEXT_CHARS {
            return Err(ToolError::new(format!(
                "value 不能超过 {MAX_CONTROL_TEXT_CHARS} 个字符。"
            )));
        }
        Some(Value::String(text.clone()))
    } else if arguments.contains_key("value") {
        return Err(ToolError::new("只有 set_value 操作支持 value 参数。"));
    } else {
        None
    };

    run_ui_automation(json!({
        "action": action,
        "window_handle": window_handle,
        "name": name,
        "automation_id": automation_id,
        "class_name": class_name,
        "control_type": control_type,
        "index": index,
        "value": value,
        "max_results": max_results,
    }))
}

fn read_control_type(arguments: &Map<String, Value>) -> Result<Option<String>, ToolError> {
    let Some(raw) = read_optional_text(arguments, "control_type", 64)? else {
        return Ok(None);
    };
    // listitem/radiobutton 等名称不含分隔符；兼容模型常见的 list_item 写法。
    let normalized = raw.to_lowercase().replace('_', "");
    if !CONTROL_TYPES.contains(&normalized.as_str()) {
        let mut sorted = CONTROL_TYPES.to_vec();
        sorted.sort_unstable();
        let original = arguments
            .get("control_type")
            .map(|value| super::args::argument_text(Some(value)))
            .unwrap_or_default();
        return Err(ToolError::new(format!(
            "control_type 不支持：{original}。可用值：{}。",
            sorted.join(", ")
        )));
    }
    Ok(Some(normalized))
}

fn run_ui_automation(request: Value) -> Result<Value, ToolError> {
    let Some(executable) = find_powershell() else {
        return Err(ToolError::new(
            "未找到 Windows PowerShell，无法加载 Windows UI Automation。",
        ));
    };
    let encoded_request =
        base64::engine::general_purpose::STANDARD.encode(python_dumps_compact(&request).as_bytes());

    let child = Command::new(&executable)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            UIA_SCRIPT,
        ])
        .env("OMNICRAWL_UIA_REQUEST_B64", encoded_request)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ToolError::new(format!("启动 Windows UI Automation 失败：{error}")))?;

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let output = match receiver.recv_timeout(Duration::from_secs(UI_AUTOMATION_TIMEOUT_SECONDS)) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(ToolError::new(format!(
                "Windows UI Automation 执行失败：{error}"
            )))
        }
        Err(_) => {
            return Err(ToolError::new(format!(
                "UI Automation 操作超过 {UI_AUTOMATION_TIMEOUT_SECONDS} 秒，已终止。"
            )))
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw_output = stdout.trim().trim_start_matches('\u{feff}');
    let payload: Value = match serde_json::from_str(raw_output) {
        Ok(value) => value,
        Err(_) => {
            let detail = output_preview(if stderr.trim().is_empty() {
                raw_output
            } else {
                &stderr
            });
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(" 诊断：{detail}")
            };
            return Err(ToolError::new(format!(
                "UI Automation 未返回有效 JSON。{suffix}"
            )));
        }
    };
    let Some(payload) = payload.as_object() else {
        return Err(ToolError::new("UI Automation 返回格式无效。"));
    };
    if !payload.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let message = payload
            .get("error")
            .map(|value| super::args::argument_text(Some(value)))
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| "未知 UI Automation 错误。".to_string());
        return Err(ToolError::new(format!("UI 控件操作失败：{message}")));
    }
    if !output.status.success() {
        let detail = output_preview(&stderr);
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(" 诊断：{detail}")
        };
        return Err(ToolError::new(format!(
            "UI Automation 进程异常退出。{suffix}"
        )));
    }
    match payload.get("result") {
        Some(Value::Object(map)) => Ok(Value::Object(map.clone())),
        _ => Err(ToolError::new("UI Automation 未返回结构化结果。")),
    }
}

fn find_powershell() -> Option<PathBuf> {
    // UIAutomationClient 在 Windows PowerShell 5.1 中长期随系统提供；PowerShell 7 作为回退。
    ["powershell", "pwsh"].iter().find_map(|name| which(name))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(format!("{name}.exe"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn output_preview(value: &str) -> String {
    const MAXIMUM: usize = 1_000;
    let text = value.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAXIMUM {
        return text.to_string();
    }
    let head: String = chars[..MAXIMUM - 3].iter().collect();
    format!("{head}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn arguments_are_validated_before_calling_powershell() {
        let error = windows_control(&arguments(json!({"action": "list"})))
            .expect_err("缺 window_handle 应当被拒绝");
        assert_eq!(
            error.message,
            "window_handle 必须是十进制整数或 0x 开头的十六进制字符串。"
        );

        let error = windows_control(&arguments(
            json!({"action": "list", "window_handle": 42, "control_type": "slider"}),
        ))
        .expect_err("不支持的控件类型应当被拒绝");
        assert!(
            error
                .message
                .starts_with("control_type 不支持：slider。可用值："),
            "{}",
            error.message
        );

        let error = windows_control(&arguments(json!({"action": "invoke", "window_handle": 42})))
            .expect_err("缺定位条件应当被拒绝");
        assert_eq!(
            error.message,
            "控件操作必须至少提供 name、automation_id、class_name 或 control_type 之一作为定位条件。"
        );

        let error = windows_control(&arguments(
            json!({"action": "list", "window_handle": 42, "value": "x"}),
        ))
        .expect_err("非 set_value 不支持 value");
        assert_eq!(error.message, "只有 set_value 操作支持 value 参数。");

        let error = windows_control(&arguments(
            json!({"action": "set_value", "window_handle": 42, "name": "a", "value": 7}),
        ))
        .expect_err("非字符串 value 应当被拒绝");
        assert_eq!(error.message, "set_value 的 value 必须是字符串。");
    }

    #[test]
    fn control_types_are_normalised_without_separators() {
        let normalized = read_control_type(&arguments(json!({"control_type": "list_item"})))
            .expect("下划线写法应当被接受");
        assert_eq!(normalized.as_deref(), Some("listitem"));
        assert!(read_control_type(&arguments(json!({"control_type": ""})))
            .expect("空串视为未提供")
            .is_none());
    }

    #[test]
    fn output_preview_truncates_long_diagnostics() {
        let long = "x".repeat(1_200);
        let preview = output_preview(&long);
        assert_eq!(preview.chars().count(), 1_000);
        assert!(preview.ends_with("..."));
    }
}
