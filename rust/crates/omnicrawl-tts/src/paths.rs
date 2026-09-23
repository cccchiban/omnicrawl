//! 路径归一化：与 `omnicrawl-tui` 的 `tools::paths::resolve_lenient` 同语义。
//!
//! TTS 在写音频、找模型目录、定位音色库时都需要「近似 Python `Path.resolve()`」
//! 的归一化：目标不存在时也要给出绝对路径，Windows 上还要去掉 `\\?\` 前缀。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// 近似 Python `Path.resolve()`：目标不存在时也要给出归一化路径。
pub fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return strip_verbatim(canonical);
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent() {
        match current.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            // 路径以分隔符或 `..` 结尾：这一段无法回拼，直接放弃快速路径。
            None => return path.to_path_buf(),
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(canonical) = parent.canonicalize() {
            let mut resolved = strip_verbatim(canonical);
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        current = parent.to_path_buf();
    }
    path.to_path_buf()
}

/// 去掉 Windows 规范化路径的 `\\?\` 前缀（保留 UNC 形式）。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            return PathBuf::from(rest.to_string());
        }
    }
    path
}
