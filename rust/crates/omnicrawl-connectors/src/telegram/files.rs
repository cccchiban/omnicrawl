//! Telegram 文件消息的提取、分类与落盘命名。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `_extract_telegram_file` /
//! `_classify_file_name` / `_resolve_temp_destination` / `_relative_to_workspace`。
//! 路径判定与 `pathlib.Path` 对齐（`suffix` / `name` / `stem` 的边界写法一致）。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// `.agent_tmp` 分类子目录的顺序即优先级，未识别归入 `files`。
pub const FILE_CATEGORY_RULES: [(&str, &[&str]); 5] = [
    (
        "images",
        &[
            ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg", ".tif", ".tiff",
        ],
    ),
    (
        "videos",
        &[".mp4", ".mov", ".avi", ".mkv", ".webm", ".flv", ".wmv"],
    ),
    (
        "audio",
        &[
            ".mp3", ".wav", ".flac", ".m4a", ".aac", ".ogg", ".oga", ".opus", ".wma", ".mid",
            ".midi",
        ],
    ),
    (
        "scripts",
        &[
            ".py", ".js", ".mjs", ".ts", ".sh", ".ps1", ".bat", ".cmd", ".rb", ".lua",
        ],
    ),
    (
        "code",
        &[
            ".c", ".cpp", ".h", ".hpp", ".java", ".go", ".rs", ".cs", ".json", ".toml", ".yaml",
            ".yml", ".xml", ".html", ".css", ".sql", ".md", ".ini", ".cfg", ".csv",
        ],
    ),
];

/// 未识别扩展名的归类目录。
pub const DEFAULT_CATEGORY: &str = "files";

/// 消息里提取出的文件：`file_id` 用于 getFile，`file_name` 可能为空串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramFile {
    pub file_id: String,
    pub file_name: String,
}

/// 从消息中提取文件（`file_id`, 文件名）；无文件返回 None。
///
/// 字段优先级：document → video / audio / animation → voice / sticker → photo（取尺寸最大的一张）。
pub fn extract_telegram_file(message: &Value) -> Option<TelegramFile> {
    let mut candidates: Vec<(Option<String>, String)> = Vec::new();
    if let Some(document) = message.get("document").filter(|value| value.is_object()) {
        candidates.push((
            identifier(document, "file_id"),
            text_field(document, "file_name"),
        ));
    }
    for key in ["video", "audio", "animation"] {
        if let Some(item) = message.get(key).filter(|value| value.is_object()) {
            candidates.push((identifier(item, "file_id"), text_field(item, "file_name")));
        }
    }
    for key in ["voice", "sticker"] {
        if let Some(item) = message.get(key).filter(|value| value.is_object()) {
            candidates.push((identifier(item, "file_id"), String::new()));
        }
    }
    if let Some(photo) = message.get("photo").and_then(Value::as_array) {
        if let Some(largest) = largest_photo(photo) {
            candidates.push((identifier(largest, "file_id"), String::new()));
        }
    }
    for (file_id, file_name) in candidates {
        if let Some(file_id) = file_id {
            return Some(TelegramFile { file_id, file_name });
        }
    }
    None
}

/// 按扩展名把文件分到 `.agent_tmp` 子目录，未识别归入 `files`。
pub fn classify_file_name(filename: &str) -> &'static str {
    let suffix = path_suffix(filename).to_lowercase();
    for (subdir, extensions) in FILE_CATEGORY_RULES {
        if extensions.contains(&suffix.as_str()) {
            return subdir;
        }
    }
    DEFAULT_CATEGORY
}

/// `pathlib.Path.suffix` 的等价实现：`0 < 最后一个点 < 末尾` 才算扩展名。
pub fn path_suffix(path: &str) -> String {
    let name = file_name_of(path);
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => name[index..].to_string(),
        _ => String::new(),
    }
}

/// `pathlib.Path.name` 的等价实现：同时按 `/` 与 `\` 取最后一段。
pub fn file_name_of(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let candidate = trimmed.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if candidate == "." {
        return String::new();
    }
    candidate.to_string()
}

/// `pathlib.Path.stem` 的等价实现（去掉扩展名后的文件名）。
pub fn file_stem(name: &str) -> String {
    let suffix = path_suffix(name);
    if suffix.is_empty() {
        return name.to_string();
    }
    name[..name.len() - suffix.len()].to_string()
}

/// 落盘用的安全文件名：去掉目录成分，空名回落到时间戳名。
pub fn safe_download_name(file_name: &str, stamp: i64) -> String {
    let name = file_name_of(file_name);
    if name.is_empty() {
        return format!("telegram_{stamp}");
    }
    name
}

/// 没有原始文件名时，用远程路径的扩展名加时间戳命名。
pub fn fallback_download_name(remote_path: &str, stamp: i64) -> String {
    format!("telegram_{stamp}{}", path_suffix(remote_path))
}

/// 重名时追加序号：`a.png` → `a_1.png` → `a_1_2.png`（与 Python 逐轮取当前名一致）。
pub fn unique_destination(directory: &Path, name: &str) -> PathBuf {
    let mut candidate = directory.join(name);
    let mut counter = 1;
    while candidate.exists() {
        let current = candidate
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        candidate = directory.join(format!(
            "{}_{counter}{}",
            file_stem(&current),
            path_suffix(&current)
        ));
        counter += 1;
    }
    candidate
}

/// 计算 `.agent_tmp/<subdir>/<安全文件名>` 并建好目录。
///
/// `temp_root` 是宿主给出的 `.agent_tmp` 根目录；它已经以 `subdir` 结尾时不再重复追加。
/// 文件名在这里再做一次净化，调用方漏了也能挡住路径穿越。
pub fn temp_destination(temp_root: &Path, subdir: &str, name: &str) -> io::Result<PathBuf> {
    let mut directory = temp_root.to_path_buf();
    let ends_with_subdir = directory
        .file_name()
        .map(|value| value.to_string_lossy() == subdir)
        .unwrap_or(false);
    if !ends_with_subdir {
        directory = directory.join(subdir);
    }
    fs::create_dir_all(&directory)?;
    Ok(unique_destination(
        &directory,
        &safe_download_name(name, unix_seconds()),
    ))
}

/// 当前 Unix 秒（文件名回落与展示用）。
pub fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

/// 相对工作区的展示路径（统一正斜杠）；不在工作区内时返回原路径。
pub fn relative_display_path(path: &Path, workspace_root: &Path) -> String {
    let resolved_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let resolved_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    match resolved_path.strip_prefix(&resolved_root) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

fn identifier(item: &Value, key: &str) -> Option<String> {
    match item.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

fn text_field(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `max(photo, key=file_size)`：并列取先出现的那张。
fn largest_photo(photo: &[Value]) -> Option<&Value> {
    let mut best: Option<&Value> = None;
    let mut best_size = 0_i64;
    for item in photo {
        let size = match item.get("file_size") {
            Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
            Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
            _ => 0,
        };
        if best.is_none() || size > best_size {
            best = Some(item);
            best_size = size;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn document_wins_over_photo() {
        let message = json!({
            "document": {"file_id": "doc", "file_name": "报告.pdf"},
            "photo": [{"file_id": "small", "file_size": 10}],
        });
        let file = extract_telegram_file(&message).expect("有文件");
        assert_eq!(file.file_id, "doc");
        assert_eq!(file.file_name, "报告.pdf");
    }

    #[test]
    fn photo_picks_largest() {
        let message = json!({
            "photo": [
                {"file_id": "small", "file_size": 10},
                {"file_id": "large", "file_size": 99},
                {"file_id": "tie", "file_size": 99},
            ],
        });
        assert_eq!(
            extract_telegram_file(&message).expect("有文件").file_id,
            "large"
        );
    }

    #[test]
    fn sticker_without_file_id_is_ignored() {
        assert!(extract_telegram_file(&json!({"sticker": {"emoji": "🙂"}})).is_none());
    }

    #[test]
    fn suffix_follows_pathlib_rules() {
        assert_eq!(path_suffix("a/b.tar.gz"), ".gz");
        assert_eq!(path_suffix(".gitignore"), "");
        assert_eq!(path_suffix("trailing."), "");
        assert_eq!(path_suffix("no_dot"), "");
    }

    #[test]
    fn classification_uses_casefolded_suffix() {
        assert_eq!(classify_file_name("../../etc/PASSWD.PNG"), "images");
        assert_eq!(classify_file_name("archive.zip"), "files");
    }
}
