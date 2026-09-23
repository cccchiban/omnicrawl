//! 路径工具：Python `Path.resolve()` / `Path.expanduser()` / `Path.home()` 的可用子集。
//!
//! 与 `omnicrawl-extensions::path` 同一套口径（解析符号链接、展开 Windows 短名、剥掉
//! `\\?\` 前缀、把正斜杠折回平台分隔符；路径不存在时解析最近的已存在祖先再拼回剩余片段）。
//! 两处各自持有一份是因为 `omnicrawl-extensions` 的这套工具是 crate 内私有，
//! 而工作区层不该反向依赖插件子系统。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `Path.home()`：`HOME` 优先，回落 `USERPROFILE`。
pub fn home_directory() -> PathBuf {
    for name in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

/// `Path.expanduser()` 的可用子集：`~` 与 `~/...` 展开为用户主目录。
pub fn expand_user(path: &str) -> PathBuf {
    if path == "~" {
        return home_directory();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home_directory().join(rest),
        None => PathBuf::from(path),
    }
}

/// Python 的 `Path.resolve()`。
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut suffix: Vec<OsString> = Vec::new();
    let mut current = absolute.clone();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            let mut base = normalize_path(&canonical);
            for part in suffix.iter().rev() {
                base.push(part);
            }
            return base;
        }
        let Some(name) = current.file_name().map(|item| item.to_os_string()) else {
            return normalize_path(&absolute);
        };
        suffix.push(name);
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent.to_path_buf(),
            _ => return normalize_path(&absolute),
        }
    }
}

/// 剥掉 Windows 的 `\\?\` 前缀并把正斜杠折回反斜杠；其他平台原样返回。
fn normalize_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    #[cfg(windows)]
    {
        PathBuf::from(stripped.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(stripped)
    }
}
