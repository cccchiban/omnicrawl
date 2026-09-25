//! Windows 桌面工具执行体：窗口、控件、输入、剪贴板与截图。
//!
//! 语义基准是 `omnicrawl/agent/toolkit/windows_desktop.py`：除控件操作调用 Windows
//! PowerShell 的 UI Automation 外，其余工具直接调用 Win32（声明见 `ffi.rs`）。非 Windows
//! 平台只保留「仅支持 Windows」的错误分支。

#[cfg(windows)]
mod args;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
mod control;
#[cfg(windows)]
mod ffi;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod screenshot;
#[cfg(windows)]
mod window;

#[cfg(not(windows))]
use super::error::{ToolError, ToolOutcome};
#[cfg(not(windows))]
use omnicrawl_controllers::types::ToolImageAttachment;
#[cfg(not(windows))]
use serde_json::{Map, Value};

/// 非 Windows 平台的占位结果类型：字段与 `screenshot::ScreenshotOutcome` 一致。
///
/// 截图的真实现只在 `cfg(windows)` 下编译，但调用方（工具表）不分平台地读
/// `outcome.output` / `outcome.images`，所以这里需要一个同名同形状的类型。
#[cfg(not(windows))]
pub struct ScreenshotOutcome {
    pub output: String,
    pub images: Vec<ToolImageAttachment>,
}

#[cfg(windows)]
pub use clipboard::windows_clipboard;
#[cfg(windows)]
pub use control::windows_control;
#[cfg(windows)]
pub use input::windows_input;
#[cfg(windows)]
pub use screenshot::windows_screenshot;
#[cfg(windows)]
pub use window::windows_window;
#[cfg(not(windows))]
pub fn windows_window(_arguments: &Map<String, Value>) -> ToolOutcome {
    Err(windows_only_error())
}

#[cfg(not(windows))]
pub fn windows_input(_arguments: &Map<String, Value>) -> ToolOutcome {
    Err(windows_only_error())
}

#[cfg(not(windows))]
pub fn windows_clipboard(_arguments: &Map<String, Value>) -> ToolOutcome {
    Err(windows_only_error())
}

#[cfg(not(windows))]
pub fn windows_control(_arguments: &Map<String, Value>) -> ToolOutcome {
    Err(windows_only_error())
}

#[cfg(not(windows))]
pub fn windows_screenshot(
    _paths: &super::paths::WorkspacePaths,
    _arguments: &Map<String, Value>,
) -> Result<ScreenshotOutcome, ToolError> {
    Err(windows_only_error())
}

#[cfg(not(windows))]
fn windows_only_error() -> ToolError {
    ToolError::new("Windows 桌面原生工具仅支持 Windows。")
}
