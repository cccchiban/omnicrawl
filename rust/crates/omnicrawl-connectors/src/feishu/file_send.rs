//! `[FILE:...]` 发文件的编排（对齐 Python `omnicrawl/connectors/fsapp.py` 的
//! `_send_local_file` / `_send_generated_files` 与 `_upload_image` / `_upload_file` 的判定面）。
//!
//! 判定与文案在内核，网络与消息发送由宿主经 [`FileTransport`] 注入：上传成功与否决定
//! 走哪条分支，失败与路径问题各有固定文案。
//!
//! 分流规则：图片扩展名走 image 通道（消息类型 `image`、消息体带 `image_key`）；
//! 音视频扩展名走 media、其余走 file（消息类型 `media` / `file`、消息体带 `file_key`）。

use std::path::{Path, PathBuf};

use crate::feishu::files::{AUDIO_EXTENSIONS, IMAGE_EXTENSIONS, VIDEO_EXTENSIONS};
use crate::json;

/// 上传通道与消息类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRoute {
    /// 图片：消息类型 `image`，消息体带 `image_key`。
    Image,
    /// 音视频：消息类型 `media`，消息体带 `file_key`。
    Media,
    /// 其余文件：消息类型 `file`，消息体带 `file_key`。
    File,
}

impl FileRoute {
    pub fn message_type(self) -> &'static str {
        match self {
            FileRoute::Image => "image",
            FileRoute::Media => "media",
            FileRoute::File => "file",
        }
    }

    /// 上传成功后要发的消息体（`json.dumps(..., ensure_ascii=False)` 写法）。
    pub fn body(self, key: &str) -> String {
        match self {
            FileRoute::Image => json::dumps(&serde_json::json!({"image_key": key})),
            FileRoute::Media | FileRoute::File => {
                json::dumps(&serde_json::json!({"file_key": key}))
            }
        }
    }

    /// 是否走图片上传接口。
    pub fn uploads_as_image(self) -> bool {
        matches!(self, FileRoute::Image)
    }
}

/// 按扩展名（含前导点，大小写不敏感）决定上传通道。
pub fn route_for_suffix(suffix: &str) -> FileRoute {
    let suffix = suffix.to_lowercase();
    if IMAGE_EXTENSIONS.contains(&suffix.as_str()) {
        return FileRoute::Image;
    }
    if AUDIO_EXTENSIONS.contains(&suffix.as_str()) || VIDEO_EXTENSIONS.contains(&suffix.as_str()) {
        return FileRoute::Media;
    }
    FileRoute::File
}

/// 宿主能力：文件上传与消息发送。
pub trait FileTransport {
    fn upload_image(&self, path: &Path) -> Option<String>;
    fn upload_file(&self, path: &Path) -> Option<String>;
    fn send_raw(&self, receive_id: &str, body: &str, message_type: &str, receive_id_type: &str);
    fn send_text(&self, receive_id: &str, text: &str, receive_id_type: &str);
}

/// 把 Agent 输出的 `[FILE:path]` 文件上传回飞书；返回是否发送成功。
pub fn send_local_file(
    transport: &dyn FileTransport,
    receive_id: &str,
    file_path: &str,
    receive_id_type: &str,
) -> bool {
    // Python 的 `Path("")` 即当前目录，`resolve(strict=True)` 不会失败；这里对齐这一点。
    let raw = if file_path.is_empty() { "." } else { file_path };
    let expanded = expand_user(raw);
    let Ok(path) = std::fs::canonicalize(&expanded) else {
        transport.send_text(
            receive_id,
            &format!("⚠️ 文件不存在：{file_path}"),
            receive_id_type,
        );
        return false;
    };
    if !path.is_file() {
        transport.send_text(
            receive_id,
            &format!("⚠️ 输出路径不是文件：{file_path}"),
            receive_id_type,
        );
        return false;
    }

    let suffix = suffix_of(&path);
    let route = route_for_suffix(&suffix);
    let key = if route.uploads_as_image() {
        transport.upload_image(&path)
    } else {
        transport.upload_file(&path)
    };
    if let Some(key) = key.filter(|value| !value.is_empty()) {
        transport.send_raw(
            receive_id,
            &route.body(&key),
            route.message_type(),
            receive_id_type,
        );
        return true;
    }

    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    transport.send_text(
        receive_id,
        &format!("⚠️ 文件发送失败：{name}"),
        receive_id_type,
    );
    false
}

/// 扫描回复正文里的 `[FILE:...]` 标记并逐个发送。
pub fn send_generated_files(
    transport: &dyn FileTransport,
    receive_id: &str,
    raw_text: &str,
    receive_id_type: &str,
) {
    for path in file_marker_matches(raw_text) {
        send_local_file(transport, receive_id, &path, receive_id_type);
    }
}

/// `re.compile(r"\[FILE:([^\]]+)\]")` 的所有捕获组（去首尾空白，保留空串）。
pub fn file_marker_matches(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[FILE:") {
        let after = &rest[start + "[FILE:".len()..];
        let Some(end) = after.find(']') else {
            break;
        };
        // `[^\]]+` 要求至少一个字符：`[FILE:]` 不是标记，跳过这个 `]` 继续扫描。
        if end == 0 {
            rest = &after[1..];
            continue;
        }
        paths.push(after[..end].trim().to_string());
        rest = &after[end + 1..];
    }
    paths
}

fn suffix_of(path: &Path) -> String {
    match path.extension() {
        Some(extension) => format!(".{}", extension.to_string_lossy().to_lowercase()),
        None => String::new(),
    }
}

/// `Path.expanduser()` 的可用子集：`~` 与 `~/...`、`~\...`。
fn expand_user(path: &str) -> PathBuf {
    if path == "~" {
        return home_directory();
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            return home_directory().join(rest);
        }
    }
    PathBuf::from(path)
}

fn home_directory() -> PathBuf {
    for name in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    std::env::temp_dir()
}
