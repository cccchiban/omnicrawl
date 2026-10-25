//! Win32 FFI：只声明桌面工具实际用到的函数与常量，不引入额外绑定 crate。
//!
//! 语义基准是 `omnicrawl/agent/toolkit/windows_desktop.py` 里通过 `ctypes.WinDLL` 装配的那组 API。

#![allow(non_snake_case)]

use std::os::raw::{c_int, c_void};

pub type Hwnd = *mut c_void;
pub type Bool = c_int;
pub type Hglobal = *mut c_void;
pub type Lparam = isize;
pub type Hdc = *mut c_void;
pub type Hbitmap = *mut c_void;
pub type Hgdiobj = *mut c_void;

pub const SW_RESTORE: c_int = 9;

/// `SetWindowPos` 的 `hWndInsertAfter` 伪句柄：临时置顶再撤销时用。
///
/// 取值等于 `(HWND)(-1)` / `(HWND)(-2)`，必须按目标字宽展开：写成 64 位字面量会在
/// 32 位目标（i686）上触发 `overflowing_literals` 而编译失败。
pub const HWND_TOPMOST: Hwnd = !0usize as Hwnd;
pub const HWND_NOTOPMOST: Hwnd = (!0usize - 1) as Hwnd;
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_SHOWWINDOW: u32 = 0x0040;

/// `VK_MENU`（ALT）：模拟一次按键让 Windows 认为本进程刚收到用户输入。
pub const VK_MENU: u16 = 0x12;

pub const CF_UNICODETEXT: u32 = 13;
pub const CF_DIB: u32 = 8;
pub const CF_DIBV5: u32 = 17;
/// `CF_HDROP`：资源管理器里「复制文件」写的就是它，内容是一串文件路径。
pub const CF_HDROP: u32 = 15;
/// `BITMAPINFOHEADER.biCompression` 的位域压缩（掩码紧跟在头之后）。
pub const BI_BITFIELDS: u32 = 3;
pub const GMEM_MOVEABLE: u32 = 0x0002;

pub const INPUT_MOUSE: u32 = 0;
pub const INPUT_KEYBOARD: u32 = 1;

pub const MOUSEEVENTF_MOVE: u32 = 0x0001;
pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
pub const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
pub const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
pub const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
pub const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
/// 侧键（前进/后退）的按下与松开：具体是哪个键由 `mouse_data` 的 `XBUTTON*` 决定。
pub const MOUSEEVENTF_XDOWN: u32 = 0x0080;
pub const MOUSEEVENTF_XUP: u32 = 0x0100;
pub const XBUTTON1: u32 = 0x0001;
pub const XBUTTON2: u32 = 0x0002;
pub const MOUSEEVENTF_WHEEL: u32 = 0x0800;
pub const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
pub const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;

pub const KEYEVENTF_KEYUP: u32 = 0x0002;
pub const KEYEVENTF_UNICODE: u32 = 0x0004;

pub const SRCCOPY: u32 = 0x00CC_0020;
pub const CAPTUREBLT: u32 = 0x4000_0000;
pub const HALFTONE: c_int = 4;
pub const BI_RGB: u32 = 0;
pub const DIB_RGB_COLORS: u32 = 0;

pub const SM_XVIRTUALSCREEN: c_int = 76;
pub const SM_YVIRTUALSCREEN: c_int = 77;
pub const SM_CXVIRTUALSCREEN: c_int = 78;
pub const SM_CYVIRTUALSCREEN: c_int = 79;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MouseInput {
    pub dx: i32,
    pub dy: i32,
    pub mouse_data: u32,
    pub flags: u32,
    pub time: u32,
    pub extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KeyboardInput {
    pub virtual_key: u16,
    pub scan_code: u16,
    pub flags: u32,
    pub time: u32,
    pub extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HardwareInput {
    pub message: u32,
    pub param_low: u16,
    pub param_high: u16,
    pub param: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BitmapInfoHeader {
    pub size: u32,
    pub width: i32,
    pub height: i32,
    pub planes: u16,
    pub bit_count: u16,
    pub compression: u32,
    pub size_image: u32,
    pub x_pels_per_meter: i32,
    pub y_pels_per_meter: i32,
    pub clr_used: u32,
    pub clr_important: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BitmapInfo {
    pub header: BitmapInfoHeader,
    pub colors: [u32; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union InputUnion {
    pub mouse: MouseInput,
    pub keyboard: KeyboardInput,
    pub hardware: HardwareInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Input {
    pub kind: u32,
    pub data: InputUnion,
}

#[link(name = "user32")]
unsafe extern "system" {
    pub fn EnumWindows(
        callback: Option<unsafe extern "system" fn(Hwnd, Lparam) -> Bool>,
        param: Lparam,
    ) -> Bool;
    pub fn IsWindow(hwnd: Hwnd) -> Bool;
    pub fn IsWindowVisible(hwnd: Hwnd) -> Bool;
    pub fn IsIconic(hwnd: Hwnd) -> Bool;
    pub fn IsZoomed(hwnd: Hwnd) -> Bool;
    pub fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> Bool;
    pub fn GetWindowTextLengthW(hwnd: Hwnd) -> c_int;
    pub fn GetWindowTextW(hwnd: Hwnd, buffer: *mut u16, max_count: c_int) -> c_int;
    pub fn GetClassNameW(hwnd: Hwnd, buffer: *mut u16, max_count: c_int) -> c_int;
    pub fn GetWindowThreadProcessId(hwnd: Hwnd, process_id: *mut u32) -> u32;
    pub fn GetForegroundWindow() -> Hwnd;
    pub fn GetCurrentThreadId() -> u32;
    pub fn AttachThreadInput(attach: u32, attach_to: u32, should_attach: Bool) -> Bool;
    pub fn BringWindowToTop(hwnd: Hwnd) -> Bool;
    pub fn SetWindowPos(
        hwnd: Hwnd,
        insert_after: Hwnd,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
        flags: u32,
    ) -> Bool;
    pub fn ShowWindow(hwnd: Hwnd, command: c_int) -> Bool;
    pub fn SetForegroundWindow(hwnd: Hwnd) -> Bool;
    pub fn SendInput(count: u32, inputs: *const Input, size: c_int) -> u32;
    pub fn OpenClipboard(hwnd: Hwnd) -> Bool;
    pub fn CloseClipboard() -> Bool;
    pub fn EmptyClipboard() -> Bool;
    pub fn GetClipboardData(format: u32) -> Hglobal;
    pub fn SetClipboardData(format: u32, data: Hglobal) -> Hglobal;
    pub fn GetSystemMetrics(index: c_int) -> c_int;
    pub fn GetDC(hwnd: Hwnd) -> Hdc;
    pub fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> c_int;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GlobalAlloc(flags: u32, bytes: usize) -> Hglobal;
    pub fn GlobalLock(handle: Hglobal) -> *mut c_void;
    pub fn GlobalSize(handle: Hglobal) -> usize;
    pub fn GlobalUnlock(handle: Hglobal) -> Bool;
    pub fn GlobalFree(handle: Hglobal) -> Hglobal;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    pub fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
    pub fn CreateCompatibleBitmap(hdc: Hdc, width: c_int, height: c_int) -> Hbitmap;
    pub fn SelectObject(hdc: Hdc, object: Hgdiobj) -> Hgdiobj;
    pub fn BitBlt(
        dest: Hdc,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
        source: Hdc,
        source_x: c_int,
        source_y: c_int,
        rop: u32,
    ) -> Bool;
    pub fn StretchBlt(
        dest: Hdc,
        x: c_int,
        y: c_int,
        width: c_int,
        height: c_int,
        source: Hdc,
        source_x: c_int,
        source_y: c_int,
        source_width: c_int,
        source_height: c_int,
        rop: u32,
    ) -> Bool;
    pub fn SetStretchBltMode(hdc: Hdc, mode: c_int) -> c_int;
    pub fn DeleteObject(object: Hgdiobj) -> Bool;
    pub fn DeleteDC(hdc: Hdc) -> Bool;
    pub fn GetDIBits(
        hdc: Hdc,
        bitmap: Hbitmap,
        start: u32,
        lines: u32,
        bits: *mut c_void,
        info: *mut BitmapInfo,
        usage: u32,
    ) -> c_int;
}
