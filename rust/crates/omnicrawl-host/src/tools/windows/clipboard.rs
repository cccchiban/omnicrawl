//! `windows_clipboard`：Unicode 文本的读取、写入与清空。

use std::time::Duration;

use omnicrawl_controllers::json::python_dumps;
use serde_json::{json, Map, Value};

use super::super::error::{ToolError, ToolOutcome};
use super::args::{ensure_allowed_keys, last_error, read_action, read_bounded_int};
use super::ffi;

const MAX_CLIPBOARD_TEXT_CHARS: i64 = 32_768;
const CLIPBOARD_OPEN_RETRIES: usize = 3;
const CLIPBOARD_RETRY_DELAY_SECONDS: f64 = 0.05;
const DEFAULT_READ_CHARS: i64 = 8_000;

const ALLOWED_KEYS: [&str; 3] = ["action", "text", "max_chars"];

pub fn windows_clipboard(arguments: &Map<String, Value>) -> ToolOutcome {
    let payload = operation(arguments)?;
    Ok(python_dumps(&payload, 2))
}

fn operation(arguments: &Map<String, Value>) -> Result<Value, ToolError> {
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let action = read_action(arguments, &["read_text", "write_text", "clear"])?;
    match action.as_str() {
        "read_text" => {
            let max_chars = read_bounded_int(
                arguments,
                "max_chars",
                DEFAULT_READ_CHARS,
                1,
                MAX_CLIPBOARD_TEXT_CHARS,
                true,
            )? as usize;
            let text = read_clipboard_text()?;
            let total_chars = text.chars().count();
            let shown: String = text.chars().take(max_chars).collect();
            Ok(json!({
                "action": "read_text",
                "text": shown,
                "total_chars": total_chars,
                "truncated": total_chars > max_chars,
            }))
        }
        "write_text" => {
            let Some(Value::String(text)) = arguments.get("text") else {
                return Err(ToolError::new("write_text 的 text 必须是字符串。"));
            };
            let text_length = text.chars().count() as i64;
            if text_length > MAX_CLIPBOARD_TEXT_CHARS {
                return Err(ToolError::new(format!(
                    "text 不能超过 {MAX_CLIPBOARD_TEXT_CHARS} 个字符。"
                )));
            }
            write_clipboard_text(text)?;
            // 不回显写入内容，避免密码、令牌被工具结果或会话记录持久化。
            Ok(json!({"action": "write_text", "text_length": text_length}))
        }
        _ => {
            clear_clipboard()?;
            Ok(json!({"action": "clear"}))
        }
    }
}

fn open_clipboard() -> Result<(), ToolError> {
    for attempt in 0..CLIPBOARD_OPEN_RETRIES {
        if unsafe { ffi::OpenClipboard(std::ptr::null_mut()) } != 0 {
            return Ok(());
        }
        if attempt + 1 < CLIPBOARD_OPEN_RETRIES {
            std::thread::sleep(Duration::from_secs_f64(CLIPBOARD_RETRY_DELAY_SECONDS));
        }
    }
    Err(last_error("剪贴板正被其他应用占用，无法打开"))
}

pub fn read_clipboard_text() -> Result<String, ToolError> {
    open_clipboard()?;
    let result = read_clipboard_locked();
    unsafe { ffi::CloseClipboard() };
    result
}

fn read_clipboard_locked() -> Result<String, ToolError> {
    let handle = unsafe { ffi::GetClipboardData(ffi::CF_UNICODETEXT) };
    if handle.is_null() {
        return Ok(String::new());
    }
    let pointer = unsafe { ffi::GlobalLock(handle) } as *const u16;
    if pointer.is_null() {
        return Err(last_error("锁定剪贴板文本失败"));
    }
    let text = unsafe { read_utf16_until_nul(pointer) };
    unsafe { ffi::GlobalUnlock(handle) };
    Ok(text)
}

/// 读取以 NUL 结尾的 UTF-16 字符串；指针来自 GlobalLock，调用方负责解锁。
unsafe fn read_utf16_until_nul(pointer: *const u16) -> String {
    let mut length = 0usize;
    while *pointer.add(length) != 0 {
        length += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length))
}

/// 写入 Unicode 文本（`CF_UNICODETEXT`）。
///
/// 除 `windows_clipboard` 工具外，TUI 的「鼠标拖选即复制」也复用这个入口：
/// 同为进程内 Win32 调用，不另起子进程，中文/emoji 都不会因编码猜错而乱码。
pub fn write_clipboard_text(text: &str) -> Result<(), ToolError> {
    let mut data: Vec<u16> = text.encode_utf16().collect();
    data.push(0);
    let bytes = data.len() * std::mem::size_of::<u16>();
    let handle = unsafe { ffi::GlobalAlloc(ffi::GMEM_MOVEABLE, bytes) };
    if handle.is_null() {
        return Err(last_error("分配剪贴板内存失败"));
    }
    let mut transferred = false;
    let result = (|| -> Result<(), ToolError> {
        let pointer = unsafe { ffi::GlobalLock(handle) } as *mut u16;
        if pointer.is_null() {
            return Err(last_error("锁定剪贴板内存失败"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), pointer, data.len());
            ffi::GlobalUnlock(handle);
        }

        open_clipboard()?;
        let write_result = (|| -> Result<(), ToolError> {
            if unsafe { ffi::EmptyClipboard() } == 0 {
                return Err(last_error("清空剪贴板失败"));
            }
            if unsafe { ffi::SetClipboardData(ffi::CF_UNICODETEXT, handle) }.is_null() {
                return Err(last_error("写入剪贴板失败"));
            }
            Ok(())
        })();
        unsafe { ffi::CloseClipboard() };
        write_result?;
        // SetClipboardData 成功后由系统负责释放 GlobalAlloc 内存。
        transferred = true;
        Ok(())
    })();
    if !transferred {
        unsafe { ffi::GlobalFree(handle) };
    }
    result
}

fn clear_clipboard() -> Result<(), ToolError> {
    open_clipboard()?;
    let result = if unsafe { ffi::EmptyClipboard() } == 0 {
        Err(last_error("清空剪贴板失败"))
    } else {
        Ok(())
    };
    unsafe { ffi::CloseClipboard() };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn arguments_are_validated_before_touching_the_clipboard() {
        let error = windows_clipboard(&arguments(
            json!({"action": "read_text", "max_chars": 99_999}),
        ))
        .expect_err("越界 max_chars 应当被拒绝");
        assert_eq!(error.message, "max_chars 必须在 1 到 32768 之间。");

        let error = windows_clipboard(&arguments(json!({"action": "write_text", "text": 5})))
            .expect_err("非字符串 text 应当被拒绝");
        assert_eq!(error.message, "write_text 的 text 必须是字符串。");

        let long = "字".repeat(32_769);
        let error = windows_clipboard(&arguments(json!({"action": "write_text", "text": long})))
            .expect_err("超长 text 应当被拒绝");
        assert_eq!(error.message, "text 不能超过 32768 个字符。");

        let error = windows_clipboard(&arguments(json!({"action": "peek"})))
            .expect_err("不支持的 action 应当被拒绝");
        assert!(
            error.message.starts_with("action 不支持：peek。"),
            "{}",
            error.message
        );
    }

    #[test]
    fn clipboard_round_trip_restores_previous_text() {
        let previous = read_clipboard_text().unwrap_or_default();
        let written = windows_clipboard(&arguments(
            json!({"action": "write_text", "text": "OmniCrawl 剪贴板往返测试"}),
        ))
        .expect("写入应当成功");
        assert!(written.contains("\"action\": \"write_text\""), "{written}");

        let read =
            windows_clipboard(&arguments(json!({"action": "read_text"}))).expect("读取应当成功");
        let parsed: Value = serde_json::from_str(&read).expect("结果是 JSON");
        assert_eq!(parsed["text"], "OmniCrawl 剪贴板往返测试");
        assert_eq!(parsed["truncated"], false);

        // 恢复原内容，避免影响其他程序。
        let _ = windows_clipboard(&arguments(
            json!({"action": "write_text", "text": previous}),
        ));
    }
}
