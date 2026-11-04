//! `windows_control`：通过 Windows PowerShell 的 UI Automation 定位与操作控件。
//!
//! PowerShell 脚本与 Python 侧逐字一致（`data/windows_uia.ps1`），请求经
//! `OMNICRAWL_UIA_REQUEST_B64` 传入，结果从标准输出解析。
//!
//! `tree` 动作把整个窗口的控件树压成缩进文本返回：语义结构一次到位，比截图省得多，
//! 也比逐节点 JSON 省得多。其余动作在结果里带 `state`（值、勾选态、可用 pattern），
//! 核对效果读它即可，截图改为 `screenshot: true` 显式索取。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine;
use omnicrawl_controllers::json::{python_dumps, python_dumps_compact};
use serde_json::{json, Map, Value};

use super::super::error::ToolError;
use super::super::paths::WorkspacePaths;
use super::args::{
    ensure_allowed_keys, read_action, read_bool, read_bounded_int, read_optional_bounded_int,
    read_optional_text, read_window_handle,
};
use super::screenshot::{snapshot_attachments, DesktopOutcome};

const MAX_CONTROL_RESULTS: i64 = 100;
const MAX_CONTROL_TEXT_CHARS: i64 = 8_192;
const UI_AUTOMATION_TIMEOUT_SECONDS: u64 = 30;

/// 控件树的节点上限：默认 200 已能覆盖绝大多数窗口，上限留给大表格/长列表。
const DEFAULT_TREE_NODES: i64 = 200;
const MAX_TREE_NODES: i64 = 1_000;
const DEFAULT_TREE_DEPTH: i64 = 6;
const MAX_TREE_DEPTH: i64 = 20;
/// 单行长度上限：控件名偶尔带整段正文，不截断会把一条树行撑成一篇文字。
const MAX_TREE_LINE_CHARS: usize = 160;
const MAX_TREE_NAME_CHARS: usize = 80;
const MAX_TREE_VALUE_CHARS: usize = 80;

const CONTROL_TYPES: [&str; 39] = [
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
    // 以下类型由 `tree` 上报，白名单必须能收下它们，否则模型看得到却定位不了。
    "calendar",
    "image",
    "slider",
    "spinner",
    "progressbar",
    "splitbutton",
    "scrollbar",
    "table",
    "datagrid",
    "dataitem",
    "header",
    "headeritem",
    "menubar",
    "toolbar",
    "statusbar",
    "tooltip",
    "thumb",
    "separator",
    "titlebar",
];

const UIA_SCRIPT: &str = include_str!("data/windows_uia.ps1");

const ACTIONS: [&str; 7] = ["list", "tree", "invoke", "set_value", "select", "toggle", "focus"];

const ALLOWED_KEYS: [&str; 11] = [
    "action",
    "window_handle",
    "name",
    "automation_id",
    "class_name",
    "control_type",
    "index",
    "value",
    "max_results",
    "max_depth",
    "screenshot",
];

pub fn windows_control(
    paths: &WorkspacePaths,
    arguments: &Map<String, Value>,
) -> Result<DesktopOutcome, ToolError> {
    let (mut payload, want_screenshot) = operation(arguments)?;
    // 默认不附图：`tree` 的语义结构、其余动作的 `state` 回读都已足够核对，截图是可选的开销。
    let images = if want_screenshot {
        snapshot_attachments(paths, &mut payload)
    } else {
        Vec::new()
    };
    Ok(DesktopOutcome {
        output: python_dumps(&payload, 2),
        images,
    })
}

fn operation(arguments: &Map<String, Value>) -> Result<(Value, bool), ToolError> {
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let action = read_action(arguments, &ACTIONS)?;
    let window_handle = read_window_handle(arguments)?;
    let name = read_optional_text(arguments, "name", 512)?;
    let automation_id = read_optional_text(arguments, "automation_id", 512)?;
    let class_name = read_optional_text(arguments, "class_name", 512)?;
    let control_type = read_control_type(arguments)?;

    let is_tree = action == "tree";
    // tree 从窗口根开始遍历，定位条件与它无关，给了也只会误导。
    if is_tree {
        for key in [
            "name",
            "automation_id",
            "class_name",
            "control_type",
            "index",
            "value",
        ] {
            let provided = match arguments.get(key) {
                None | Some(Value::Null) => false,
                // 归一化会保留空串：空串与「没给」同义，不算定位条件。
                Some(Value::String(text)) => !text.trim().is_empty(),
                Some(_) => true,
            };
            if provided {
                return Err(ToolError::new(
                    "tree 动作遍历整个窗口，不接受 name、automation_id、class_name、control_type、index 或 value 定位条件。",
                ));
            }
        }
    }

    let has_selector =
        name.is_some() || automation_id.is_some() || class_name.is_some() || control_type.is_some();
    if action != "list" && !is_tree && !has_selector {
        return Err(ToolError::new(
            "控件操作必须至少提供 name、automation_id、class_name 或 control_type 之一作为定位条件。",
        ));
    }

    let index = if is_tree {
        None
    } else {
        read_optional_bounded_int(arguments, "index", 0, MAX_CONTROL_RESULTS - 1)?
    };
    let (max_results, max_depth) = match action.as_str() {
        "list" => (
            read_bounded_int(arguments, "max_results", 30, 1, MAX_CONTROL_RESULTS, true)?,
            0,
        ),
        "tree" => (
            read_bounded_int(
                arguments,
                "max_results",
                DEFAULT_TREE_NODES,
                1,
                MAX_TREE_NODES,
                true,
            )?,
            read_bounded_int(
                arguments,
                "max_depth",
                DEFAULT_TREE_DEPTH,
                0,
                MAX_TREE_DEPTH,
                true,
            )?,
        ),
        _ => (0, 0),
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
    } else if arguments.contains_key("value") && !is_tree {
        return Err(ToolError::new("只有 set_value 操作支持 value 参数。"));
    } else {
        None
    };

    let screenshot = read_bool(arguments, "screenshot", false)?;

    let result = run_ui_automation(json!({
        "action": action,
        "window_handle": window_handle,
        "name": name,
        "automation_id": automation_id,
        "class_name": class_name,
        "control_type": control_type,
        "index": index,
        "value": value,
        "max_results": max_results,
        "max_depth": max_depth,
    }))?;
    if is_tree {
        return Ok((render_tree(result, window_handle, max_depth), false));
    }
    Ok((result, screenshot))
}

/// 把控件树压成缩进文本：一行一个控件，只留模型定位与判断要用的字段。
///
/// 逐节点 JSON 一组括号就几十字节，一条树行十几字节，同样一屏差一个数量级。
fn render_tree(result: Value, window_handle: isize, max_depth: i64) -> Value {
    // PowerShell 5.1 把单元素数组压成裸对象：只剩一个控件时 `nodes` 不是数组。
    let nodes: Vec<Value> = match result.get("nodes") {
        Some(Value::Array(items)) => items.clone(),
        Some(value @ Value::Object(_)) => vec![value.clone()],
        _ => Vec::new(),
    };
    let mut lines: Vec<String> = Vec::with_capacity(nodes.len());
    let mut actionable = 0usize;
    for node in &nodes {
        let line = tree_line(node);
        if line.contains("[invoke]")
            || line.contains("[value]")
            || line.contains("[toggle]")
            || line.contains("[select]")
        {
            actionable += 1;
        }
        lines.push(line);
    }
    let truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut text = lines.join("\n");
    if truncated {
        text.push_str("\n...(节点数已达 max_results，树被截断：调小 max_depth 或分段 list)");
    }
    json!({
        "action": "tree",
        "window_handle": format!("{window_handle:#x}"),
        "node_count": lines.len(),
        "max_depth": max_depth,
        "truncated": truncated,
        "tree": text,
        "hint": format!(
            "共 {actionable} 个可操作控件（[invoke]/[value]/[select]/[toggle]）。需要坐标时用 list 取 bounds。"
        ),
    })
}

/// 一条树行：`缩进 control_type "name" #automation_id 标记...`。
fn tree_line(node: &Value) -> String {
    let depth = node.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize;
    let control_type = node
        .get("control_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut line = "  ".repeat(depth.min(MAX_TREE_DEPTH as usize));
    line.push_str(control_type);

    let name = node.get("name").and_then(Value::as_str).unwrap_or("").trim();
    if !name.is_empty() {
        line.push_str(&format!(" {}", quote(&truncate(name, MAX_TREE_NAME_CHARS))));
    }
    let automation_id = node
        .get("automation_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !automation_id.is_empty() {
        line.push_str(&format!(" #{automation_id}"));
    }

    let mut marks: Vec<String> = Vec::new();
    match node.get("patterns") {
        Some(Value::Array(patterns)) => {
            for pattern in patterns.iter().filter_map(Value::as_str) {
                marks.push(format!("[{pattern}]"));
            }
        }
        // PowerShell 5.1 会把单元素数组序列化成裸字符串。
        Some(Value::String(pattern)) => marks.push(format!("[{pattern}]")),
        _ => {}
    }
    if let Some(value) = node.get("value").and_then(Value::as_str) {
        if !value.trim().is_empty() {
            marks.push(format!(
                "={}",
                quote(&truncate(value.trim(), MAX_TREE_VALUE_CHARS))
            ));
        }
    }
    if let Some(checked) = node.get("checked").and_then(Value::as_str) {
        marks.push(format!("checked={checked}"));
    }
    if node.get("is_enabled").and_then(Value::as_bool) == Some(false) {
        marks.push("disabled".to_string());
    }
    if node.get("is_offscreen").and_then(Value::as_bool) == Some(true) {
        marks.push("offscreen".to_string());
    }
    if !marks.is_empty() {
        line.push_str(&format!(" {}", marks.join(" ")));
    }
    truncate(&line, MAX_TREE_LINE_CHARS)
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "'"))
}

fn truncate(text: &str, maximum: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= maximum {
        return text.to_string();
    }
    let head: String = chars[..maximum.saturating_sub(1)].iter().collect();
    format!("{head}…")
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

    /// 参数校验一律在启动 UI Automation 之前失败，路径只用于成功时落确认截图。
    fn control(value: Value) -> Result<DesktopOutcome, ToolError> {
        windows_control(
            &WorkspacePaths::new(std::env::temp_dir()),
            &arguments(value),
        )
    }

    #[test]
    fn arguments_are_validated_before_calling_powershell() {
        let error = control(json!({"action": "list"})).expect_err("缺 window_handle 应当被拒绝");
        assert_eq!(
            error.message,
            "window_handle 缺失：先用 windows_window 的 list 取得句柄。"
        );

        let error =
            control(json!({"action": "list", "window_handle": 42, "control_type": "slider"}))
                .expect_err("不支持的控件类型应当被拒绝");
        assert!(
            error
                .message
                .starts_with("control_type 不支持：slider。可用值："),
            "{}",
            error.message
        );

        let error = control(json!({"action": "invoke", "window_handle": 42}))
            .expect_err("缺定位条件应当被拒绝");
        assert_eq!(
            error.message,
            "控件操作必须至少提供 name、automation_id、class_name 或 control_type 之一作为定位条件。"
        );

        let error = control(json!({"action": "list", "window_handle": 42, "value": "x"}))
            .expect_err("非 set_value 不支持 value");
        assert_eq!(error.message, "只有 set_value 操作支持 value 参数。");

        let error =
            control(json!({"action": "set_value", "window_handle": 42, "name": "a", "value": 7}))
                .expect_err("非字符串 value 应当被拒绝");
        assert_eq!(error.message, "set_value 的 value 必须是字符串。");
    }

    /// tree 走窗口根的整树遍历：定位条件与 value 都会误导，直接拒绝。
    #[test]
    fn tree_rejects_selector_arguments() {
        let error = control(json!({"action": "tree", "window_handle": 42, "name": "确定"}))
            .expect_err("tree 不接受 name");
        assert!(
            error.message.starts_with("tree 动作遍历整个窗口，不接受"),
            "{}",
            error.message
        );

        let error = control(json!({"action": "tree", "window_handle": 42, "index": 0}))
            .expect_err("tree 不接受 index");
        assert!(error.message.starts_with("tree 动作遍历整个窗口"), "{}", error.message);
    }

    /// 一条树行要同时表达层级、类型、名称、自动化 ID 与可操作 pattern。
    #[test]
    fn tree_lines_carry_structure_and_patterns() {
        let node = json!({
            "depth": 2,
            "control_type": "button",
            "name": "保存",
            "automation_id": "btnSave",
            "patterns": ["invoke"],
            "is_enabled": true,
            "is_offscreen": false,
        });
        assert_eq!(tree_line(&node), "    button \"保存\" #btnSave [invoke]");
    }

    /// 值、勾选态、禁用与离屏都要在行内可见：这是「不截图也能核对」的基础。
    #[test]
    fn tree_lines_reveal_state_marks() {
        let node = json!({
            "depth": 0,
            "control_type": "edit",
            "name": "地址栏",
            "value": "https://example.com",
            "checked": "on",
            "is_enabled": false,
            "is_offscreen": true,
        });
        let line = tree_line(&node);
        assert!(line.starts_with("edit \"地址栏\""), "{line}");
        assert!(line.contains("=\"https://example.com\""), "{line}");
        assert!(line.contains("checked=on"), "{line}");
        assert!(line.contains("disabled"), "{line}");
        assert!(line.contains("offscreen"), "{line}");
    }

    /// 长控件名与整段正文会撑爆一行；截断必须按字符算，不能切出半个汉字。
    #[test]
    fn tree_lines_are_truncated_by_characters() {
        let long_name = "保存".repeat(100);
        let node = json!({"depth": 0, "control_type": "text", "name": long_name});
        let line = tree_line(&node);
        assert!(line.chars().count() <= MAX_TREE_LINE_CHARS, "{line}");
        assert!(line.contains('…'), "{line}");
        // 名称部分按 MAX_TREE_NAME_CHARS 收口，中文字符没有被切半。
        assert_eq!(line.matches('"').count(), 2, "{line}");

        // 名称本身不长、但值很长时，值按 MAX_TREE_VALUE_CHARS 收口。
        let node = json!({
            "depth": 0,
            "control_type": "edit",
            "name": "输入框",
            "value": "x".repeat(200),
            "checked": "on",
            "is_enabled": false,
            "is_offscreen": true,
        });
        let line = tree_line(&node);
        assert!(line.chars().count() <= MAX_TREE_LINE_CHARS, "{line}");
        assert!(line.contains('…'), "{line}");
        assert!(line.ends_with("checked=on disabled offscreen"), "{line}");
    }

    /// 树结果给模型的是文本而不是逐节点 JSON：字段名只出现一次。
    #[test]
    fn tree_result_is_a_compact_text_block() {
        let result = json!({
            "action": "tree",
            "truncated": false,
            "nodes": [
                {"depth": 0, "control_type": "window", "name": "记事本"},
                {"depth": 1, "control_type": "edit", "name": "文本编辑器", "patterns": ["value"]},
                {"depth": 1, "control_type": "button", "name": "关闭", "patterns": ["invoke"]},
            ],
        });
        let rendered = render_tree(result, 0x1234, 6);
        assert_eq!(rendered["action"], "tree");
        assert_eq!(rendered["node_count"], 3);
        assert_eq!(rendered["window_handle"], "0x1234");
        let tree = rendered["tree"].as_str().expect("tree 文本");
        assert_eq!(
            tree,
            "window \"记事本\"\n  edit \"文本编辑器\" [value]\n  button \"关闭\" [invoke]"
        );
        assert!(rendered["hint"]
            .as_str()
            .expect("hint")
            .starts_with("共 2 个可操作控件"));
    }

    /// PowerShell 5.1 会把单元素数组序列化成裸对象/裸字符串：树只剩一个节点时也要能读。
    #[test]
    fn tree_result_tolerates_single_element_arrays() {
        let result = json!({
            "action": "tree",
            "truncated": false,
            "nodes": {"depth": 0, "control_type": "window", "name": "弹窗", "patterns": "invoke"},
        });
        let rendered = render_tree(result, 0x10, 6);
        assert_eq!(rendered["node_count"], 1);
        assert_eq!(
            rendered["tree"].as_str().expect("tree 文本"),
            "window \"弹窗\" [invoke]"
        );
    }

    #[test]
    fn tree_depth_is_bounded() {
        let error = control(json!({"action": "tree", "window_handle": 42, "max_depth": 99}))
            .expect_err("超深遍历应当被拒绝");
        assert_eq!(
            error.message,
            format!("max_depth 必须在 0 到 {MAX_TREE_DEPTH} 之间。")
        );

        let error = control(json!({"action": "tree", "window_handle": 42, "screenshot": "yes"}))
            .expect_err("screenshot 必须是布尔值");
        assert_eq!(error.message, "screenshot 必须是布尔值。");
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
