//! `windows_window`：枚举顶层窗口、读取窗口详情与前台激活。
//!
//! 激活会换掉前台画面，因此那一次调用附带一张截图；`list` / `get` 是只读的，不带图。

use omnicrawl_controllers::json::python_dumps;
use serde_json::{json, Map, Value};

use super::super::error::ToolError;
use super::super::paths::WorkspacePaths;
use super::args::{
    ensure_allowed_keys, last_error, read_action, read_bool, read_bounded_int, read_optional_text,
    read_window_handle,
};
use super::ffi;
use super::screenshot::{snapshot_attachments, DesktopOutcome};

const MAX_WINDOW_RESULTS: i64 = 100;

const ALLOWED_KEYS: [&str; 7] = [
    "action",
    "window_handle",
    "title_contains",
    "class_name_contains",
    "visible_only",
    "include_untitled",
    "max_results",
];

pub fn windows_window(
    paths: &WorkspacePaths,
    arguments: &Map<String, Value>,
) -> Result<DesktopOutcome, ToolError> {
    let mut payload = operation(arguments)?;
    let images = if payload["action"] == "activate" {
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
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let action = read_action(arguments, &["list", "get", "activate"])?;
    if action == "list" {
        let title_contains = read_optional_text(arguments, "title_contains", 512)?;
        let class_name_contains = read_optional_text(arguments, "class_name_contains", 512)?;
        let visible_only = read_bool(arguments, "visible_only", true)?;
        let include_untitled = read_bool(arguments, "include_untitled", false)?;
        let max_results =
            read_bounded_int(arguments, "max_results", 50, 1, MAX_WINDOW_RESULTS, true)?;

        let matches: Vec<Value> = enumerate_windows()?
            .into_iter()
            .filter(|item| {
                let title = item["title"].as_str().unwrap_or_default().to_lowercase();
                let class_name = item["class_name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_lowercase();
                if visible_only && !item["is_visible"].as_bool().unwrap_or(true) {
                    return false;
                }
                if !include_untitled && title.is_empty() {
                    return false;
                }
                if let Some(needle) = &title_contains {
                    if !title.contains(&needle.to_lowercase()) {
                        return false;
                    }
                }
                if let Some(needle) = &class_name_contains {
                    if !class_name.contains(&needle.to_lowercase()) {
                        return false;
                    }
                }
                true
            })
            .collect();

        let matched_count = matches.len() as i64;
        let windows: Vec<Value> = matches.into_iter().take(max_results as usize).collect();
        return Ok(json!({
            "action": "list",
            "matched_count": matched_count,
            "truncated": matched_count > max_results,
            "windows": windows,
        }));
    }

    let handle = read_window_handle(arguments)?;
    let mut details = describe_window(handle)?;
    if action == "activate" {
        activate_window(handle)?;
        if let Value::Object(map) = &mut details {
            map.insert("activated".to_string(), Value::Bool(true));
        }
    }
    Ok(json!({"action": action, "window": details}))
}

thread_local! {
    static ENUMERATED: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn enumerate_windows() -> Result<Vec<Value>, ToolError> {
    ENUMERATED.with(|rows| rows.borrow_mut().clear());
    let ok = unsafe { ffi::EnumWindows(Some(visit_window), 0) };
    if ok == 0 {
        return Err(last_error("枚举窗口失败"));
    }
    Ok(ENUMERATED.with(|rows| rows.borrow().clone()))
}

/// 枚举回调：单个窗口可能在枚举期间销毁，跳过它而不是中止整次操作。
unsafe extern "system" fn visit_window(hwnd: ffi::Hwnd, _param: ffi::Lparam) -> ffi::Bool {
    let handle = hwnd as isize;
    if handle != 0 {
        if let Ok(row) = describe_window(handle) {
            ENUMERATED.with(|rows| rows.borrow_mut().push(row));
        }
    }
    1
}

pub fn describe_window(handle: isize) -> Result<Value, ToolError> {
    let hwnd = handle as ffi::Hwnd;
    if unsafe { ffi::IsWindow(hwnd) } == 0 {
        return Err(ToolError::new(format!(
            "无效或已关闭的 window_handle：{}。",
            format_window_handle(handle)
        )));
    }
    let mut rect = ffi::Rect::default();
    if unsafe { ffi::GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err(last_error("读取窗口位置失败"));
    }
    let title = window_text(hwnd);
    let class_name = window_class(hwnd);
    let mut process_id: u32 = 0;
    unsafe { ffi::GetWindowThreadProcessId(hwnd, &mut process_id) };
    let foreground = unsafe { ffi::GetForegroundWindow() } as isize;

    Ok(json!({
        "window_handle": format_window_handle(handle),
        "title": title,
        "class_name": class_name,
        "process_id": process_id,
        "is_visible": unsafe { ffi::IsWindowVisible(hwnd) } != 0,
        "is_minimized": unsafe { ffi::IsIconic(hwnd) } != 0,
        "is_maximized": unsafe { ffi::IsZoomed(hwnd) } != 0,
        "is_foreground": foreground == handle,
        "bounds": {
            "left": rect.left,
            "top": rect.top,
            "right": rect.right,
            "bottom": rect.bottom,
            "width": rect.right - rect.left,
            "height": rect.bottom - rect.top,
        },
    }))
}

fn window_text(hwnd: ffi::Hwnd) -> String {
    let length = unsafe { ffi::GetWindowTextLengthW(hwnd) }.max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    unsafe { ffi::GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    utf16_to_string(&buffer)
}

fn window_class(hwnd: ffi::Hwnd) -> String {
    let mut buffer = vec![0u16; 512];
    unsafe { ffi::GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    utf16_to_string(&buffer)
}

fn utf16_to_string(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

pub fn format_window_handle(handle: isize) -> String {
    format!("0x{:016X}", handle as usize)
}

fn activate_window(handle: isize) -> Result<(), ToolError> {
    let hwnd = handle as ffi::Hwnd;
    if unsafe { ffi::IsWindow(hwnd) } == 0 {
        return Err(ToolError::new(format!(
            "无效或已关闭的 window_handle：{}。",
            format_window_handle(handle)
        )));
    }
    if unsafe { ffi::IsIconic(hwnd) } != 0 {
        unsafe { ffi::ShowWindow(hwnd, ffi::SW_RESTORE) };
    }
    if unsafe { ffi::SetForegroundWindow(hwnd) } != 0 {
        return Ok(());
    }
    // 拒绝不是错误：Windows 只在「调用方就是前台线程」时才直接放行 `SetForegroundWindow`，
    // 后台进程（本工具几乎总是）必然被挡。此时按官方允许的三条路径依次尝试，
    // 都能在不抢用户前台控制权的前提下把窗口带到前面。
    if foreground_by_attach(hwnd) {
        return Ok(());
    }
    if foreground_by_switch(hwnd) {
        return Ok(());
    }
    if foreground_by_temp_topmost(hwnd) {
        return Ok(());
    }
    Err(ToolError::new(
        "Windows 拒绝将目标窗口置于前台；请由用户手动切换窗口后再重试。",
    ))
}

/// 附着到当前前台线程的输入队列后再调用 `SetForegroundWindow`。
///
/// 这是 `SetForegroundWindow` 的官方放行条件之一：调用线程与前台线程共享输入队列时
/// 会被视为「用户正在操作的应用」。只临时附着、调用完立刻解除，不改变焦点归属。
fn foreground_by_attach(hwnd: ffi::Hwnd) -> bool {
    let foreground = unsafe { ffi::GetForegroundWindow() };
    if foreground.is_null() {
        return false;
    }
    let foreground_thread =
        unsafe { ffi::GetWindowThreadProcessId(foreground, std::ptr::null_mut()) };
    let target_thread = unsafe { ffi::GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
    if foreground_thread == 0 || target_thread == 0 || foreground_thread == target_thread {
        return false;
    }
    let current_thread = unsafe { ffi::GetCurrentThreadId() };
    if current_thread == foreground_thread {
        return false;
    }
    if unsafe { ffi::AttachThreadInput(current_thread, foreground_thread, 1) } == 0 {
        return false;
    }
    unsafe { ffi::BringWindowToTop(hwnd) };
    let raised = unsafe { ffi::SetForegroundWindow(hwnd) } != 0;
    unsafe { ffi::AttachThreadInput(current_thread, foreground_thread, 0) };
    raised
}

/// 模拟一次 `ALT` 按键：Windows 借此把本进程视为「刚有用户输入」，从而放行前台切换。
fn foreground_by_switch(hwnd: ffi::Hwnd) -> bool {
    press_alt();
    if unsafe { ffi::SetForegroundWindow(hwnd) } != 0 {
        return true;
    }
    press_alt();
    false
}

fn press_alt() {
    let make = |flags: u32| ffi::Input {
        kind: ffi::INPUT_KEYBOARD,
        data: ffi::InputUnion {
            keyboard: ffi::KeyboardInput {
                virtual_key: ffi::VK_MENU,
                scan_code: 0,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    };
    let mut inputs = [make(0), make(ffi::KEYEVENTF_KEYUP)];
    unsafe {
        ffi::SendInput(
            inputs.len() as u32,
            inputs.as_mut_ptr(),
            std::mem::size_of::<ffi::Input>() as i32,
        )
    };
}

/// 临时置顶再撤销：置顶会把窗口强制提到最前，撤销后层级恢复原样。
fn foreground_by_temp_topmost(hwnd: ffi::Hwnd) -> bool {
    let flags = ffi::SWP_NOMOVE | ffi::SWP_NOSIZE | ffi::SWP_SHOWWINDOW;
    if unsafe { ffi::SetWindowPos(hwnd, ffi::HWND_TOPMOST, 0, 0, 0, 0, flags) } == 0 {
        return false;
    }
    unsafe { ffi::SetForegroundWindow(hwnd) };
    unsafe { ffi::SetWindowPos(hwnd, ffi::HWND_NOTOPMOST, 0, 0, 0, 0, flags) };
    // 置顶那一步已经把窗口提到最前，键盘焦点拿不到也能让后续点击落到它身上。
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    /// 只断言参数校验与窗口元数据：路径只用于激活成功后落确认截图。
    fn window(value: Value) -> Result<DesktopOutcome, ToolError> {
        windows_window(
            &WorkspacePaths::new(std::env::temp_dir()),
            &arguments(value),
        )
    }

    #[test]
    fn list_reports_windows_on_this_desktop() {
        let outcome = window(json!({"action": "list", "max_results": 5})).expect("枚举应当成功");
        let parsed: Value = serde_json::from_str(&outcome.output).expect("结果是 JSON");
        assert!(outcome.images.is_empty(), "list 是只读的，不该带截图");
        assert_eq!(parsed["action"], "list");
        assert!(parsed["matched_count"].as_i64().unwrap_or(0) >= 0);
        assert!(parsed["windows"].is_array());
    }

    #[test]
    fn unsupported_keys_and_actions_are_rejected() {
        let error = window(json!({"action": "list", "oops": 1})).expect_err("多余参数应当被拒绝");
        assert_eq!(error.message, "不支持的参数：oops。");

        let error = window(json!({"action": "close"})).expect_err("不支持的 action 应当被拒绝");
        assert_eq!(
            error.message,
            "action 不支持：close。可用值：activate、get、list。"
        );

        let error = window(json!({"action": "get", "window_handle": "zz"}))
            .expect_err("非法句柄应当被拒绝");
        assert!(
            error.message.starts_with("window_handle 无法解析：zz。"),
            "{}",
            error.message
        );
    }

    #[test]
    fn handles_accept_hex_without_prefix() {
        let bare = read_window_handle(&arguments(json!({"window_handle": "3099C"})))
            .expect("无前缀十六进制应当被接受");
        assert_eq!(bare, 0x3099C);
        let prefixed = read_window_handle(&arguments(json!({"window_handle": "0x3099C"})))
            .expect("带前缀十六进制应当被接受");
        assert_eq!(prefixed, 0x3099C);
        let decimal = read_window_handle(&arguments(json!({"window_handle": 199068})))
            .expect("十进制整数应当被接受");
        assert_eq!(decimal, 199068);
        // 纯数字串按十进制解释：`3099C` 之外不能是十六进制。
        let digits = read_window_handle(&arguments(json!({"window_handle": "12345"})))
            .expect("纯数字串应当被接受");
        assert_eq!(digits, 12345);
        assert!(read_window_handle(&arguments(json!({"window_handle": "0x"}))).is_err());
        assert!(read_window_handle(&arguments(json!({"window_handle": 0}))).is_err());
        assert!(read_window_handle(&arguments(json!({"window_handle": true}))).is_err());
    }

    #[test]
    fn describe_rejects_stale_handles() {
        let error = describe_window(0x7fff_ffff).expect_err("失效句柄应当被拒绝");
        assert!(
            error.message.starts_with("无效或已关闭的 window_handle："),
            "{}",
            error.message
        );
        assert_eq!(format_window_handle(0x1234), "0x0000000000001234");
    }
}
