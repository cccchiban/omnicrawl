//! `windows_screenshot`：截取虚拟桌面、屏幕区域或指定窗口，并作为视觉附件交给模型。
//!
//! 语义基准是 `omnicrawl/agent/toolkit/windows_desktop.py`：GDI 抓物理像素 → 按
//! `max_dimension` 等比缩放（`HALFTONE`）→ PNG 落盘；若 PNG 超过视觉模型的安全内联大小，
//! 继续按 0.75 缩小，直到不超过限制或达到最小边长。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine;
use omnicrawl_controllers::json::python_dumps;
use omnicrawl_controllers::types::ToolImageAttachment;
use serde_json::{json, Map, Value};

use super::super::error::ToolError;
use super::super::paths::WorkspacePaths;
use super::args::{
    ensure_allowed_keys, last_error, read_bounded_int, read_coordinates, read_window_handle,
};
use super::ffi;
use super::window::describe_window;

pub const DEFAULT_MAX_DIMENSION: i64 = 2_048;
const MIN_MODEL_IMAGE_DIMENSION: i64 = 256;
const MAX_SCREENSHOT_DIMENSION: i64 = 8_192;
const MAX_SCREENSHOT_SOURCE_PIXELS: i64 = 100_000_000;
const MAX_MODEL_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

const TARGETS: [&str; 3] = ["desktop", "region", "window"];

const ALLOWED_KEYS: [&str; 7] = [
    "target",
    "window_handle",
    "x",
    "y",
    "width",
    "height",
    "max_dimension",
];

/// 一次截图的产物：给模型的 JSON 文本与视觉附件。
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenshotOutcome {
    pub output: String,
    pub images: Vec<ToolImageAttachment>,
}

pub fn windows_screenshot(
    paths: &WorkspacePaths,
    arguments: &Map<String, Value>,
) -> Result<ScreenshotOutcome, ToolError> {
    let payload = operation(paths, arguments)?;
    let path = PathBuf::from(payload["_output_path"].as_str().unwrap_or_default());
    let bytes = std::fs::read(&path)
        .map_err(|error| ToolError::new(format!("读取截图文件失败：{error}")))?;
    let mut visible = payload.clone();
    if let Value::Object(map) = &mut visible {
        map.remove("_output_path");
    }
    Ok(ScreenshotOutcome {
        output: python_dumps(&visible, 2),
        images: vec![ToolImageAttachment {
            media_type: "image/png".to_string(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            filename: path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            detail: String::new(),
        }],
    })
}

fn operation(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> Result<Value, ToolError> {
    ensure_allowed_keys(arguments, &ALLOWED_KEYS)?;
    let target = read_target(arguments)?;
    let max_dimension = read_bounded_int(
        arguments,
        "max_dimension",
        DEFAULT_MAX_DIMENSION,
        MIN_MODEL_IMAGE_DIMENSION,
        MAX_SCREENSHOT_DIMENSION,
        true,
    )?;
    let desktop = virtual_desktop_bounds()?;

    let source_bounds = match target.as_str() {
        "desktop" => {
            reject_present_keys(arguments, &["window_handle", "x", "y", "width", "height"])?;
            desktop.clone()
        }
        "region" => {
            reject_present_keys(arguments, &["window_handle"])?;
            let (x, y) = read_coordinates(arguments, true)?;
            let (x, y) = (x.expect("必填 x"), y.expect("必填 y"));
            let width = read_bounded_int(arguments, "width", 0, 1, 100_000, true)?;
            let height = read_bounded_int(arguments, "height", 0, 1, 100_000, true)?;
            bounds_from_xywh(x, y, width, height)?
        }
        _ => {
            reject_present_keys(arguments, &["x", "y", "width", "height"])?;
            let handle = read_window_handle(arguments)?;
            let window = describe_window(handle)?;
            if window["is_minimized"].as_bool().unwrap_or(false) {
                return Err(ToolError::new("目标窗口已最小化；请先恢复窗口后再截图。"));
            }
            let bounds = &window["bounds"];
            let window_bounds = bounds_from_xywh(
                bounds["left"].as_i64().unwrap_or(0),
                bounds["top"].as_i64().unwrap_or(0),
                bounds["width"].as_i64().unwrap_or(0),
                bounds["height"].as_i64().unwrap_or(0),
            )?;
            // GetWindowRect 可能包含落在虚拟桌面外的 DWM 边框（最大化窗口常见）：
            // 取可见交集，完全不可见才失败。
            match intersect_bounds(&window_bounds, &desktop) {
                Some(bounds) => bounds,
                None => {
                    return Err(ToolError::new(
                        "目标窗口当前完全位于虚拟桌面之外，无法截图。",
                    ))
                }
            }
        }
    };

    validate_screenshot_bounds(&source_bounds, &desktop)?;
    let source_width = source_bounds["width"].as_i64().unwrap_or(0);
    let source_height = source_bounds["height"].as_i64().unwrap_or(0);
    if source_width * source_height > MAX_SCREENSHOT_SOURCE_PIXELS {
        return Err(ToolError::new(format!(
            "截图源区域不能超过 {MAX_SCREENSHOT_SOURCE_PIXELS} 像素。"
        )));
    }

    let output_path = new_screenshot_path(paths, &target)?;
    let (image_width, image_height, byte_count) =
        match capture_png(&source_bounds, &output_path, max_dimension) {
            Ok(result) => result,
            Err(error) => {
                let _ = std::fs::remove_file(&output_path);
                return Err(error);
            }
        };

    Ok(json!({
        "target": target,
        "path": display_path(paths, &output_path),
        "source_bounds": {
            "left": source_bounds["left"],
            "top": source_bounds["top"],
            "right": source_bounds["right"],
            "bottom": source_bounds["bottom"],
            "width": source_width,
            "height": source_height,
        },
        "image": {
            "media_type": "image/png",
            "width": image_width,
            "height": image_height,
            "bytes": byte_count,
            "scaled": image_width != source_width || image_height != source_height,
        },
        "_output_path": output_path.to_string_lossy(),
    }))
}

fn read_target(arguments: &Map<String, Value>) -> Result<String, ToolError> {
    let raw = match arguments.get("target") {
        None | Some(Value::Null) => "desktop".to_string(),
        Some(value) => super::args::argument_text(Some(value)),
    };
    let value = raw.trim().to_lowercase();
    if !TARGETS.contains(&value.as_str()) {
        let mut sorted = TARGETS.to_vec();
        sorted.sort_unstable();
        return Err(ToolError::new(format!(
            "target 不支持：{raw}。可用值：{}。",
            sorted.join("、")
        )));
    }
    Ok(value)
}

fn reject_present_keys(
    arguments: &Map<String, Value>,
    forbidden: &[&str],
) -> Result<(), ToolError> {
    let mut present: Vec<&str> = forbidden
        .iter()
        .copied()
        .filter(|key| arguments.get(*key).is_some_and(|value| !value.is_null()))
        .collect();
    present.sort_unstable();
    if present.is_empty() {
        return Ok(());
    }
    Err(ToolError::new(format!(
        "当前截图目标不支持参数：{}。",
        present.join("、")
    )))
}

fn virtual_desktop_bounds() -> Result<Map<String, Value>, ToolError> {
    let left = unsafe { ffi::GetSystemMetrics(ffi::SM_XVIRTUALSCREEN) } as i64;
    let top = unsafe { ffi::GetSystemMetrics(ffi::SM_YVIRTUALSCREEN) } as i64;
    let width = unsafe { ffi::GetSystemMetrics(ffi::SM_CXVIRTUALSCREEN) } as i64;
    let height = unsafe { ffi::GetSystemMetrics(ffi::SM_CYVIRTUALSCREEN) } as i64;
    bounds_from_xywh(left, top, width, height)
}

fn bounds_from_xywh(
    x: i64,
    y: i64,
    width: i64,
    height: i64,
) -> Result<Map<String, Value>, ToolError> {
    if width <= 0 || height <= 0 {
        return Err(ToolError::new("截图宽度和高度必须为正整数。"));
    }
    let mut bounds = Map::new();
    bounds.insert("left".to_string(), Value::from(x));
    bounds.insert("top".to_string(), Value::from(y));
    bounds.insert("right".to_string(), Value::from(x + width));
    bounds.insert("bottom".to_string(), Value::from(y + height));
    bounds.insert("width".to_string(), Value::from(width));
    bounds.insert("height".to_string(), Value::from(height));
    Ok(bounds)
}

fn intersect_bounds(
    first: &Map<String, Value>,
    second: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let number = |map: &Map<String, Value>, key: &str| map[key].as_i64().unwrap_or(0);
    let left = number(first, "left").max(number(second, "left"));
    let top = number(first, "top").max(number(second, "top"));
    let right = number(first, "right").min(number(second, "right"));
    let bottom = number(first, "bottom").min(number(second, "bottom"));
    if right <= left || bottom <= top {
        return None;
    }
    bounds_from_xywh(left, top, right - left, bottom - top).ok()
}

fn validate_screenshot_bounds(
    source: &Map<String, Value>,
    desktop: &Map<String, Value>,
) -> Result<(), ToolError> {
    let number = |map: &Map<String, Value>, key: &str| map[key].as_i64().unwrap_or(0);
    if number(source, "width") <= 0 || number(source, "height") <= 0 {
        return Err(ToolError::new("截图区域没有有效尺寸。"));
    }
    if number(source, "left") < number(desktop, "left")
        || number(source, "top") < number(desktop, "top")
        || number(source, "right") > number(desktop, "right")
        || number(source, "bottom") > number(desktop, "bottom")
    {
        return Err(ToolError::new(format!(
            "截图区域必须完全位于虚拟桌面内；请求区域为 [{}, {}, {}, {}]，虚拟桌面为 [{}, {}, {}, {}]。",
            number(source, "left"),
            number(source, "top"),
            number(source, "right"),
            number(source, "bottom"),
            number(desktop, "left"),
            number(desktop, "top"),
            number(desktop, "right"),
            number(desktop, "bottom"),
        )));
    }
    Ok(())
}

fn fit_dimensions(width: i64, height: i64, max_dimension: i64) -> (i64, i64) {
    let largest = width.max(height);
    if largest <= max_dimension {
        return (width, height);
    }
    let scale = max_dimension as f64 / largest as f64;
    (
        python_round(width as f64 * scale).max(1.0) as i64,
        python_round(height as f64 * scale).max(1.0) as i64,
    )
}

/// Python `round()` 的银行家舍入；与缩放后的尺寸计算保持一致。
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

fn new_screenshot_path(paths: &WorkspacePaths, target: &str) -> Result<PathBuf, ToolError> {
    let directory = paths
        .root()
        .join(".omnicrawl")
        .join(".agent_tmp")
        .join("images");
    std::fs::create_dir_all(&directory)
        .map_err(|error| ToolError::new(format!("创建截图目录失败：{error}")))?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S_%6fZ").to_string();
    Ok(directory.join(format!("windows_{target}_{stamp}_{}.png", unique_suffix())))
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.subsec_nanos())
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:08x}", nanos ^ sequence.wrapping_mul(2_654_435_761))
}

fn display_path(paths: &WorkspacePaths, path: &Path) -> String {
    match path.strip_prefix(paths.root()) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

/// 抓屏、缩放、编码，必要时继续缩小直到不超过内联大小限制。
fn capture_png(
    source_bounds: &Map<String, Value>,
    output_path: &Path,
    max_dimension: i64,
) -> Result<(i64, i64, u64), ToolError> {
    let number = |key: &str| source_bounds[key].as_i64().unwrap_or(0);
    let mut width = number("width");
    let mut height = number("height");
    let bitmap = capture_screen_bitmap(number("left"), number("top"), width, height)?;
    let mut bitmap = BitmapGuard::new(bitmap);
    let mut error: Option<ToolError> = None;

    let mut result = None;
    let (target_width, target_height) = fit_dimensions(width, height, max_dimension);
    if (target_width, target_height) != (width, height) {
        let resized = resize_bitmap(bitmap.raw(), width, height, target_width, target_height);
        match resized {
            Ok(resized) => {
                bitmap.replace(resized);
                width = target_width;
                height = target_height;
            }
            Err(failure) => error = Some(failure),
        }
    }

    while error.is_none() {
        match save_bitmap_as_png(bitmap.raw(), output_path, width, height) {
            Ok(byte_count) => {
                if byte_count <= MAX_MODEL_IMAGE_BYTES {
                    result = Some((width, height, byte_count));
                    break;
                }
                if width.max(height) <= MIN_MODEL_IMAGE_DIMENSION {
                    error = Some(ToolError::new(
                        "截图 PNG 超过视觉模型的安全内联大小限制，且无法继续缩小。",
                    ));
                    break;
                }
                let next_width = (python_round(width as f64 * 0.75) as i64).max(1);
                let next_height = (python_round(height as f64 * 0.75) as i64).max(1);
                if (next_width, next_height) == (width, height) {
                    error = Some(ToolError::new("截图无法继续缩小到模型可接受大小。"));
                    break;
                }
                match resize_bitmap(bitmap.raw(), width, height, next_width, next_height) {
                    Ok(resized) => {
                        bitmap.replace(resized);
                        width = next_width;
                        height = next_height;
                    }
                    Err(failure) => {
                        error = Some(failure);
                        break;
                    }
                }
            }
            Err(failure) => {
                error = Some(failure);
                break;
            }
        }
    }
    drop(bitmap);
    match (result, error) {
        (Some(value), _) => Ok(value),
        (None, Some(failure)) => Err(failure),
        (None, None) => Err(ToolError::new("截图失败。")),
    }
}

/// 位图句柄守卫：无论成功失败都要 DeleteObject。
struct BitmapGuard(Option<ffi::Hbitmap>);

impl BitmapGuard {
    fn new(bitmap: ffi::Hbitmap) -> Self {
        Self(Some(bitmap))
    }

    fn raw(&self) -> ffi::Hbitmap {
        self.0.expect("位图句柄存在")
    }

    fn replace(&mut self, bitmap: ffi::Hbitmap) {
        if let Some(previous) = self.0.replace(bitmap) {
            unsafe { ffi::DeleteObject(previous) };
        }
    }
}

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if let Some(bitmap) = self.0.take() {
            unsafe { ffi::DeleteObject(bitmap) };
        }
    }
}

fn capture_screen_bitmap(
    x: i64,
    y: i64,
    width: i64,
    height: i64,
) -> Result<ffi::Hbitmap, ToolError> {
    let screen_dc = unsafe { ffi::GetDC(std::ptr::null_mut()) };
    if screen_dc.is_null() {
        return Err(last_error("获取屏幕设备上下文失败"));
    }
    let mut memory_dc: ffi::Hdc = std::ptr::null_mut();
    let mut bitmap: ffi::Hbitmap = std::ptr::null_mut();
    let mut previous: ffi::Hgdiobj = std::ptr::null_mut();
    let outcome = (|| -> Result<ffi::Hbitmap, ToolError> {
        memory_dc = unsafe { ffi::CreateCompatibleDC(screen_dc) };
        if memory_dc.is_null() {
            return Err(last_error("创建截图内存设备上下文失败"));
        }
        bitmap = unsafe { ffi::CreateCompatibleBitmap(screen_dc, width as i32, height as i32) };
        if bitmap.is_null() {
            return Err(last_error("创建截图位图失败"));
        }
        previous = unsafe { ffi::SelectObject(memory_dc, bitmap) };
        if previous.is_null() {
            return Err(last_error("选择截图位图失败"));
        }
        if unsafe {
            ffi::BitBlt(
                memory_dc,
                0,
                0,
                width as i32,
                height as i32,
                screen_dc,
                x as i32,
                y as i32,
                ffi::SRCCOPY | ffi::CAPTUREBLT,
            )
        } == 0
        {
            return Err(last_error("复制屏幕像素失败"));
        }
        Ok(bitmap)
    })();

    match outcome {
        Ok(handle) => {
            if !memory_dc.is_null() && !previous.is_null() {
                unsafe { ffi::SelectObject(memory_dc, previous) };
            }
            if !memory_dc.is_null() {
                unsafe { ffi::DeleteDC(memory_dc) };
            }
            unsafe { ffi::ReleaseDC(std::ptr::null_mut(), screen_dc) };
            Ok(handle)
        }
        Err(error) => {
            if !bitmap.is_null() {
                unsafe { ffi::DeleteObject(bitmap) };
            }
            if !memory_dc.is_null() {
                unsafe { ffi::DeleteDC(memory_dc) };
            }
            unsafe { ffi::ReleaseDC(std::ptr::null_mut(), screen_dc) };
            Err(error)
        }
    }
}

fn resize_bitmap(
    bitmap: ffi::Hbitmap,
    source_width: i64,
    source_height: i64,
    target_width: i64,
    target_height: i64,
) -> Result<ffi::Hbitmap, ToolError> {
    let screen_dc = unsafe { ffi::GetDC(std::ptr::null_mut()) };
    if screen_dc.is_null() {
        return Err(last_error("获取屏幕设备上下文失败"));
    }
    let mut source_dc: ffi::Hdc = std::ptr::null_mut();
    let mut target_dc: ffi::Hdc = std::ptr::null_mut();
    let mut target_bitmap: ffi::Hbitmap = std::ptr::null_mut();
    let mut previous_source: ffi::Hgdiobj = std::ptr::null_mut();
    let mut previous_target: ffi::Hgdiobj = std::ptr::null_mut();

    let outcome = (|| -> Result<ffi::Hbitmap, ToolError> {
        source_dc = unsafe { ffi::CreateCompatibleDC(screen_dc) };
        target_dc = unsafe { ffi::CreateCompatibleDC(screen_dc) };
        if source_dc.is_null() || target_dc.is_null() {
            return Err(last_error("创建缩放内存设备上下文失败"));
        }
        target_bitmap = unsafe {
            ffi::CreateCompatibleBitmap(screen_dc, target_width as i32, target_height as i32)
        };
        if target_bitmap.is_null() {
            return Err(last_error("创建缩放目标位图失败"));
        }
        previous_source = unsafe { ffi::SelectObject(source_dc, bitmap) };
        previous_target = unsafe { ffi::SelectObject(target_dc, target_bitmap) };
        if previous_source.is_null() || previous_target.is_null() {
            return Err(last_error("选择缩放位图失败"));
        }
        unsafe { ffi::SetStretchBltMode(target_dc, ffi::HALFTONE) };
        if unsafe {
            ffi::StretchBlt(
                target_dc,
                0,
                0,
                target_width as i32,
                target_height as i32,
                source_dc,
                0,
                0,
                source_width as i32,
                source_height as i32,
                ffi::SRCCOPY,
            )
        } == 0
        {
            return Err(last_error("缩放截图位图失败"));
        }
        Ok(target_bitmap)
    })();

    match outcome {
        Ok(handle) => {
            finish_resize(
                screen_dc,
                source_dc,
                target_dc,
                previous_source,
                previous_target,
            );
            Ok(handle)
        }
        Err(error) => {
            if !target_bitmap.is_null() {
                unsafe { ffi::DeleteObject(target_bitmap) };
            }
            finish_resize(
                screen_dc,
                source_dc,
                target_dc,
                previous_source,
                previous_target,
            );
            Err(error)
        }
    }
}

fn finish_resize(
    screen_dc: ffi::Hdc,
    source_dc: ffi::Hdc,
    target_dc: ffi::Hdc,
    previous_source: ffi::Hgdiobj,
    previous_target: ffi::Hgdiobj,
) {
    unsafe {
        if !source_dc.is_null() && !previous_source.is_null() {
            ffi::SelectObject(source_dc, previous_source);
        }
        if !target_dc.is_null() && !previous_target.is_null() {
            ffi::SelectObject(target_dc, previous_target);
        }
        if !source_dc.is_null() {
            ffi::DeleteDC(source_dc);
        }
        if !target_dc.is_null() {
            ffi::DeleteDC(target_dc);
        }
        ffi::ReleaseDC(std::ptr::null_mut(), screen_dc);
    }
}

fn save_bitmap_as_png(
    bitmap: ffi::Hbitmap,
    output_path: &Path,
    width: i64,
    height: i64,
) -> Result<u64, ToolError> {
    let pixels = bitmap_pixels(bitmap, width, height)?;
    let file = std::fs::File::create(output_path)
        .map_err(|error| ToolError::new(format!("创建截图文件失败：{error}")))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| ToolError::new(format!("写入 PNG 头失败：{error}")))?;
    writer
        .write_image_data(&pixels)
        .map_err(|error| ToolError::new(format!("写入 PNG 数据失败：{error}")))?;
    drop(writer);
    std::fs::metadata(output_path)
        .map(|meta| meta.len())
        .map_err(|error| ToolError::new(format!("读取截图大小失败：{error}")))
}

/// 用 GetDIBits 取出 BGRA 像素并转成 PNG 需要的 RGBA 顺序。
fn bitmap_pixels(bitmap: ffi::Hbitmap, width: i64, height: i64) -> Result<Vec<u8>, ToolError> {
    let mut info = ffi::BitmapInfo::default();
    info.header.size = std::mem::size_of::<ffi::BitmapInfoHeader>() as u32;
    info.header.width = width as i32;
    // 负高度取自顶向下的 DIB，省去逐行翻转。
    info.header.height = -(height as i32);
    info.header.planes = 1;
    info.header.bit_count = 32;
    info.header.compression = ffi::BI_RGB;

    let mut buffer = vec![0u8; (width * height * 4) as usize];
    let screen_dc = unsafe { ffi::GetDC(std::ptr::null_mut()) };
    if screen_dc.is_null() {
        return Err(last_error("获取屏幕设备上下文失败"));
    }
    let lines = unsafe {
        ffi::GetDIBits(
            screen_dc,
            bitmap,
            0,
            height as u32,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            &mut info,
            ffi::DIB_RGB_COLORS,
        )
    };
    unsafe { ffi::ReleaseDC(std::ptr::null_mut(), screen_dc) };
    if lines == 0 {
        return Err(last_error("读取截图像素失败"));
    }

    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn paths(name: &str) -> (WorkspacePaths, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-screenshot-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        (WorkspacePaths::new(&root), root)
    }

    #[test]
    fn desktop_screenshot_writes_png_and_returns_attachment() {
        let (paths, root) = paths("desktop");
        let outcome = windows_screenshot(
            &paths,
            &arguments(json!({"target": "desktop", "max_dimension": 320})),
        )
        .expect("桌面截图应当成功");
        let payload: Value = serde_json::from_str(&outcome.output).expect("结果是 JSON");
        assert_eq!(payload["target"], "desktop");
        assert_eq!(payload["image"]["media_type"], "image/png");
        assert!(payload["image"]["width"].as_i64().unwrap_or(0) <= 320);
        assert!(payload["_output_path"].is_null());

        let relative = payload["path"].as_str().unwrap_or_default();
        let file = root.join(relative);
        assert!(file.is_file(), "截图应当落盘：{relative}");
        assert_eq!(outcome.images.len(), 1);
        assert_eq!(outcome.images[0].media_type, "image/png");
        assert!(outcome.images[0].filename.ends_with(".png"));
        assert!(!outcome.images[0].data_base64.is_empty());
        // 截图含屏幕内容，用例结束后立即清理。
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn target_specific_arguments_are_rejected() {
        let (paths, _root) = paths("reject");
        let error = windows_screenshot(
            &paths,
            &arguments(json!({"target": "desktop", "x": 1, "y": 1})),
        )
        .expect_err("桌面截图不接受坐标");
        assert_eq!(error.message, "当前截图目标不支持参数：x、y。");

        let error = windows_screenshot(&paths, &arguments(json!({"target": "region", "x": 1})))
            .expect_err("区域截图要求成对坐标");
        assert_eq!(error.message, "x 与 y 必须同时提供。");

        let error = windows_screenshot(&paths, &arguments(json!({"target": "zoom"})))
            .expect_err("不支持的 target 应当被拒绝");
        assert!(
            error.message.starts_with("target 不支持：zoom。可用值："),
            "{}",
            error.message
        );

        let error = windows_screenshot(&paths, &arguments(json!({"target": "region", "x": 0, "y": 0, "width": 10, "height": 10, "max_dimension": 1})))
            .expect_err("越界 max_dimension 应当被拒绝");
        assert_eq!(error.message, "max_dimension 必须在 256 到 8192 之间。");
    }

    #[test]
    fn fit_dimensions_keeps_aspect_ratio() {
        assert_eq!(fit_dimensions(1_920, 1_080, 2_048), (1_920, 1_080));
        assert_eq!(fit_dimensions(4_096, 2_048, 2_048), (2_048, 1_024));
        assert_eq!(fit_dimensions(3, 1, 3), (3, 1));
    }
}
