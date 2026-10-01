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

/// 单次尝试读取剪贴板文本；被占用等失败一律返回 `None`，**不重试也不睡眠**。
///
/// 给 TUI 的粘贴识别用：那条路径每收到一批按键就要比对一次剪贴板，而
/// [`read_clipboard_text`] 的退避重试（3 次 × 50ms）会把界面卡住近 100ms。
/// 对映 Python 侧 `_read_windows_clipboard_text`：读不到就当识别不了，退回普通按键流。
pub fn try_read_clipboard_text() -> Option<String> {
    if unsafe { ffi::OpenClipboard(std::ptr::null_mut()) } == 0 {
        return None;
    }
    let result = read_clipboard_locked().ok();
    unsafe { ffi::CloseClipboard() };
    result
}

fn read_clipboard_locked() -> Result<String, ToolError> {
    let handle = unsafe { ffi::GetClipboardData(ffi::CF_UNICODETEXT) };
    if handle.is_null() {
        // 资源管理器里「复制文件」只写 `CF_HDROP`，不写文本；不认它就等于取不到可比对的路径，
        // 粘贴识别只能靠按键形状兜底（大路径会被控制台按批切开，拼不出完整路径）。
        return Ok(read_clipboard_file_path_locked().unwrap_or_default());
    }
    let pointer = unsafe { ffi::GlobalLock(handle) } as *const u16;
    if pointer.is_null() {
        return Err(last_error("锁定剪贴板文本失败"));
    }
    let text = unsafe { read_utf16_until_nul(pointer) };
    unsafe { ffi::GlobalUnlock(handle) };
    Ok(text)
}

/// 读 `CF_HDROP` 里第一个文件的完整路径；不是文件列表时返回 `None`。
///
/// 取一个就够：粘贴识别只需一个可与按键流比对的完整路径。
fn read_clipboard_file_path_locked() -> Option<String> {
    let handle = unsafe { ffi::GetClipboardData(ffi::CF_HDROP) };
    if handle.is_null() {
        return None;
    }
    let pointer = unsafe { ffi::GlobalLock(handle) } as *const u8;
    if pointer.is_null() {
        return None;
    }
    let size = unsafe { ffi::GlobalSize(handle) };
    let bytes = unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec();
    unsafe { ffi::GlobalUnlock(handle) };
    // DROPFILES：`pFiles` 是文件名的字节偏移，`fWide` 表示名字是 UTF-16。
    if bytes.len() < 20 {
        return None;
    }
    let offset = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let wide = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) != 0;
    if offset >= bytes.len() {
        return None;
    }
    if wide {
        let rest = &bytes[offset..];
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect();
        if units.is_empty() {
            return None;
        }
        String::from_utf16(&units).ok()
    } else {
        let rest = &bytes[offset..];
        let text: Vec<u8> = rest.iter().take_while(|byte| **byte != 0).copied().collect();
        if text.is_empty() {
            return None;
        }
        Some(text.iter().map(|byte| char::from(*byte)).collect())
    }
}

/// 读剪贴板的位图（`CF_DIB` / `CF_DIBV5`）并编码成 PNG 字节；里面没有图片时返回 `None`。
///
/// 给 TUI 的「Ctrl+V 粘贴图片」用：浏览器、截图工具与「复制图片」都走 `CF_DIB`
/// （`CF_BITMAP` 是 GDI 句柄，取像素要再经 `GetDIBits`，这里不做——按 DIB 拷贝更直接）。
/// 仍是单次尝试、不重试不睡眠：这条路径在按键热路径上调用。
pub fn try_read_clipboard_image_png() -> Option<Vec<u8>> {
    if unsafe { ffi::OpenClipboard(std::ptr::null_mut()) } == 0 {
        return None;
    }
    let result = read_clipboard_image_locked();
    unsafe { ffi::CloseClipboard() };
    result
}

fn read_clipboard_image_locked() -> Option<Vec<u8>> {
    // DIBV5 带 alpha，优先；旧程序只给 CF_DIB 时退回它。
    for format in [ffi::CF_DIBV5, ffi::CF_DIB] {
        let handle = unsafe { ffi::GetClipboardData(format) };
        if handle.is_null() {
            continue;
        }
        let size = unsafe { ffi::GlobalSize(handle) };
        if size == 0 {
            continue;
        }
        let pointer = unsafe { ffi::GlobalLock(handle) } as *const u8;
        if pointer.is_null() {
            continue;
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec();
        unsafe { ffi::GlobalUnlock(handle) };
        if let Some(png) = dib_to_png(&bytes) {
            return Some(png);
        }
    }
    None
}

/// 剪贴板 DIB → PNG（RGBA）。
///
/// 剪贴板里的位图是「文件头之后」的那段：`BITMAPINFOHEADER`（或 V5）跟着调色板与像素。
/// 高度为正表示自底向上存储，为负表示自顶向下；`biBitCount` 只处理 24 / 32 两种——
/// 屏幕上复制的图基本都是它们，其余（索引色、16 位）交给调用方当作「不是图片」。
fn dib_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    const HEADER_SIZE: usize = 40;
    if bytes.len() < HEADER_SIZE {
        return None;
    }
    let read_u32 = |offset: usize| -> u32 {
        u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    };
    let read_i32 = |offset: usize| -> i32 {
        i32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])
    };
    let header_size = read_u32(0) as usize;
    if header_size < HEADER_SIZE || header_size > bytes.len() {
        return None;
    }
    let width = read_i32(4);
    let raw_height = read_i32(8);
    let bit_count = u16::from_le_bytes([bytes[14], bytes[15]]);
    let compression = read_u32(16);
    // BI_RGB 是无压缩格式；BI_BITFIELDS 的 32 位也很常见（掩码即 BGRA 通道序）。
    if compression != ffi::BI_RGB && compression != ffi::BI_BITFIELDS {
        return None;
    }
    if bit_count != 24 && bit_count != 32 {
        return None;
    }
    if compression == ffi::BI_BITFIELDS && bit_count != 32 {
        return None;
    }
    // V5 头（>= 108）自带掩码；40 字节头用 BI_BITFIELDS 时，三个掩码紧跟在头后面。
    // 掩码只接受标准 BGRA 布局，别的通道序（如 ARGB）按「不是图片」处理而不是猜错颜色。
    let mut pixels_offset = header_size;
    if compression == ffi::BI_BITFIELDS {
        if header_size >= 108 {
            if read_u32(40) != 0x00FF_0000
                || read_u32(44) != 0x0000_FF00
                || read_u32(48) != 0x0000_00FF
            {
                return None;
            }
        } else {
            if bytes.len() < header_size + 12 {
                return None;
            }
            if read_u32(header_size) != 0x00FF_0000
                || read_u32(header_size + 4) != 0x0000_FF00
                || read_u32(header_size + 8) != 0x0000_00FF
            {
                return None;
            }
            pixels_offset += 12;
        }
    }
    if width <= 0 || raw_height == 0 {
        return None;
    }
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();
    let channels = (bit_count / 8) as usize;
    let stride = ((width as usize * channels + 3) / 4) * 4;
    let pixels_bytes = stride.checked_mul(height as usize)?;
    if bytes.len() < pixels_offset.checked_add(pixels_bytes)? {
        return None;
    }
    let pixels = &bytes[pixels_offset..pixels_offset + pixels_bytes];

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        // 自底向上：最后一行是图像顶部。
        let source_row = if top_down {
            row
        } else {
            height as usize - 1 - row
        };
        let line = &pixels[source_row * stride..source_row * stride + width as usize * channels];
        for pixel in line.chunks_exact(channels) {
            rgba.push(pixel[2]);
            rgba.push(pixel[1]);
            rgba.push(pixel[0]);
            // 24 位没有 alpha 通道，按不透明处理；32 位里 0 常见于「未填 alpha」的截图工具，
            // 照原样保留会让整张图看起来全透明，因此 0 也当不透明。
            rgba.push(if channels == 4 && pixel[3] != 0 { pixel[3] } else { 255 });
        }
    }

    let mut output = Vec::new();
    let mut encoder = png::Encoder::new(&mut output, width as u32, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&rgba).ok()?;
    drop(writer);
    Some(output)
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

    /// 造一张 32 位 DIB：`header_size` 决定是否带 V5 头，`top_down` 决定高度符号。
    fn dib(width: i32, height: i32, pixels_bgra: &[[u8; 4]], top_down: bool, v5: bool) -> Vec<u8> {
        let header_size: u32 = if v5 { 124 } else { 40 };
        let mut bytes = vec![0u8; header_size as usize];
        bytes[0..4].copy_from_slice(&header_size.to_le_bytes());
        bytes[4..8].copy_from_slice(&width.to_le_bytes());
        let signed_height = if top_down { -height } else { height };
        bytes[8..12].copy_from_slice(&signed_height.to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&32u16.to_le_bytes());
        bytes[16..20].copy_from_slice(&ffi::BI_RGB.to_le_bytes());
        // 自底向上存储时，第一行是图像底部。
        let ordered: Vec<[u8; 4]> = if top_down {
            pixels_bgra.to_vec()
        } else {
            pixels_bgra.iter().rev().copied().collect()
        };
        for pixel in ordered {
            bytes.extend_from_slice(&pixel);
        }
        bytes
    }

    /// 用 `png` 解码回来（host 依赖里没有 `image`，`png` 本来就是编码用的那一个）。
    fn decode_png(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("PNG 应当可读");
        let mut buffer = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buffer).expect("PNG 应当可解码");
        buffer.truncate(info.buffer_size());
        (info.width, info.height, buffer)
    }

    fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * width + x) * 4) as usize;
        [
            rgba[offset],
            rgba[offset + 1],
            rgba[offset + 2],
            rgba[offset + 3],
        ]
    }

    #[test]
    fn dib_is_decoded_to_png_with_upright_pixels() {
        // DIB 像素是 BGRA 字节序：`[0,0,255,255]` 是红色，`[255,0,0,255]` 是蓝色。
        let red_bgra = [0u8, 0, 255, 255];
        let blue_bgra = [255u8, 0, 0, 255];
        let red_rgba = [255u8, 0, 0, 255];
        let blue_rgba = [0u8, 0, 255, 255];
        for top_down in [false, true] {
            for v5 in [false, true] {
                let raw = dib(2, 2, &[red_bgra, red_bgra, blue_bgra, blue_bgra], top_down, v5);
                let png = dib_to_png(&raw)
                    .unwrap_or_else(|| panic!("top_down={top_down} v5={v5} 应当能解码"));
                let (width, height, rgba) = decode_png(&png);
                assert_eq!((width, height), (2, 2));
                assert_eq!(pixel(&rgba, width, 0, 0), red_rgba, "第一行是图像顶部");
                assert_eq!(pixel(&rgba, width, 0, 1), blue_rgba, "第二行是图像底部");
            }
        }
    }

    #[test]
    fn non_image_dib_shapes_are_rejected() {
        // 16 位色深：通道序与 8 位每通道不同，交给调用方当「不是图片」。
        let mut raw = dib(2, 2, &[[0, 0, 255, 255]; 4], true, false);
        raw[14..16].copy_from_slice(&16u16.to_le_bytes());
        assert!(dib_to_png(&raw).is_none());

        // 头尺寸超出缓冲：截断的数据不能崩，只能返回 None。
        let raw = dib(2, 2, &[[0, 0, 255, 255]; 4], true, false);
        assert!(dib_to_png(&raw[..20]).is_none());
        assert!(dib_to_png(&raw[..raw.len() - 2]).is_none());

        // 像素区不足（声称 8×8 却只有 2×2 的数据）。
        let raw = dib(8, 8, &[[0, 0, 255, 255]; 4], true, false);
        assert!(dib_to_png(&raw).is_none());
    }

    #[test]
    fn bitfields_masks_after_a_40_byte_header_are_skipped() {
        let mut raw = dib(2, 2, &[[0, 0, 255, 255]; 4], true, false);
        raw[16..20].copy_from_slice(&ffi::BI_BITFIELDS.to_le_bytes());
        // 40 字节头之后插三个掩码（R / G / B），像素区随之后移。
        let mut with_masks = raw[..40].to_vec();
        with_masks.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
        with_masks.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
        with_masks.extend_from_slice(&0x0000_00FFu32.to_le_bytes());
        with_masks.extend_from_slice(&raw[40..]);
        let png = dib_to_png(&with_masks).expect("掩码正确时应当能解码");
        let (width, _, rgba) = decode_png(&png);
        assert_eq!(pixel(&rgba, width, 0, 0), [255, 0, 0, 255], "BGRA 的纯红解出 RGBA 纯红");

        // 非 BGRA 掩码（ARGB）：宁可当「不是图片」，也不要把颜色猜错。
        let mut argb = with_masks.clone();
        argb[40..44].copy_from_slice(&0xFF00_0000u32.to_le_bytes());
        assert!(dib_to_png(&argb).is_none());
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
