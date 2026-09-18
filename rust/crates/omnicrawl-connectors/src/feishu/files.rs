//! 飞书消息资源：扩展名分类、临时目录落盘命名、post 富文本解析与文件标记扫描。
//!
//! 语义基准是 Python `omnicrawl/connectors/fsapp.py` 的 `_classify_filename` /
//! `_resolve_temp_destination` / `_post_text_and_images` / `_save_message_resource`
//! 的后缀补齐规则，以及 `[FILE:...]` 标记扫描（`_send_generated_files`）。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// 图片扩展名（与飞书消息资源类型 `image` 对应）。
pub const IMAGE_EXTENSIONS: [&str; 10] = [
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".ico", ".tif", ".tiff", ".svg",
];

/// 音频扩展名（`audio` 资源类型）。
pub const AUDIO_EXTENSIONS: [&str; 11] = [
    ".opus", ".mp3", ".wav", ".m4a", ".aac", ".ogg", ".oga", ".flac", ".wma", ".mid", ".midi",
];

/// 视频扩展名（`media` 资源类型）。
pub const VIDEO_EXTENSIONS: [&str; 7] = [".mp4", ".mov", ".avi", ".mkv", ".webm", ".flv", ".wmv"];

/// 脚本类扩展名。
pub const SCRIPT_EXTENSIONS: [&str; 10] = [
    ".py", ".js", ".ts", ".sh", ".ps1", ".bat", ".cmd", ".rb", ".lua", ".mjs",
];

/// 代码/数据类扩展名。
pub const CODE_EXTENSIONS: [&str; 21] = [
    ".c", ".cpp", ".h", ".hpp", ".java", ".go", ".rs", ".cs", ".json", ".toml", ".yaml", ".yml",
    ".xml", ".html", ".css", ".sql", ".md", ".ini", ".cfg", ".csv", ".tsx",
];

/// 可上传为飞书文件的扩展名 → 平台 `file_type`。
pub const FILE_TYPE_MAP: [(&str, &str); 9] = [
    (".opus", "opus"),
    (".mp4", "mp4"),
    (".pdf", "pdf"),
    (".doc", "doc"),
    (".docx", "doc"),
    (".xls", "xls"),
    (".xlsx", "xls"),
    (".ppt", "ppt"),
    (".pptx", "ppt"),
];

/// 需要下载落盘的飞书消息类型。
pub const MESSAGE_RESOURCE_TYPES: [&str; 4] = ["image", "audio", "file", "media"];

/// 无文件名时的落盘兜底名。
pub const SAFE_FILENAME_FALLBACK: &str = "feishu_file";

/// 按扩展名把文件分到 `.agent_tmp` 子目录，未识别归入 `files`。
pub fn classify_filename(filename: &str) -> &'static str {
    let suffix = super::super::telegram::files::path_suffix(filename).to_lowercase();
    if IMAGE_EXTENSIONS.contains(&suffix.as_str()) {
        return "images";
    }
    if AUDIO_EXTENSIONS.contains(&suffix.as_str()) {
        return "audio";
    }
    if VIDEO_EXTENSIONS.contains(&suffix.as_str()) {
        return "videos";
    }
    if SCRIPT_EXTENSIONS.contains(&suffix.as_str()) {
        return "scripts";
    }
    if CODE_EXTENSIONS.contains(&suffix.as_str()) {
        return "code";
    }
    "files"
}

/// 把飞书资源保存到 Agent 临时目录，并防止文件名路径穿越。
pub fn resolve_temp_destination(temp_root: &Path, filename: &str) -> io::Result<PathBuf> {
    let root = temp_root.to_path_buf();
    let category = classify_filename(filename);
    let category_dir = root.join(category);
    if category_dir.parent() != Some(root.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "飞书文件分类目录越出 Agent 临时目录。",
        ));
    }
    fs::create_dir_all(&category_dir)?;
    let safe_name = super::super::telegram::files::file_name_of(filename);
    let safe_name = if safe_name.is_empty() {
        SAFE_FILENAME_FALLBACK.to_string()
    } else {
        safe_name
    };
    Ok(super::super::telegram::files::unique_destination(
        &category_dir,
        &safe_name,
    ))
}

/// 资源缺少扩展名时按消息类型补一个（图片 `.jpg`、语音 `.opus`、文件 `.bin`）。
pub fn resource_file_name(name: &str, file_key: &str, resource_type: &str) -> String {
    let suffix = super::super::telegram::files::path_suffix(name);
    if !suffix.is_empty() {
        return name.to_string();
    }
    match resource_type {
        "image" => format!("{name}.jpg"),
        "audio" => format!("{name}.opus"),
        "file" if name == file_key => format!("{name}.bin"),
        _ => name.to_string(),
    }
}

/// 提取 `[FILE:...]` 标记中的路径（按出现顺序，去掉两端空白）。
pub fn file_marker_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[FILE:") {
        let after = &rest[start + "[FILE:".len()..];
        let Some(end) = after.find(']') else { break };
        let path = after[..end].trim();
        if !path.is_empty() {
            paths.push(path.to_string());
        }
        rest = &after[end + 1..];
    }
    paths
}

/// 提取飞书 post 富文本结构中的文字和图片 key。
pub fn post_text_and_images(content: &Value) -> (String, Vec<String>) {
    let root = content.get("post").unwrap_or(content);
    let Value::Object(map) = root else {
        return (String::new(), Vec::new());
    };
    let mut candidates: Vec<&Value> = vec![root];
    for language in ["zh_cn", "en_us", "ja_jp"] {
        if let Some(block @ Value::Object(_)) = map.get(language) {
            candidates.insert(0, block);
        }
    }
    for candidate in candidates {
        let (text, images) = parse_post_block(candidate);
        if !text.is_empty() || !images.is_empty() {
            return (text, images);
        }
    }
    (String::new(), Vec::new())
}

fn parse_post_block(block: &Value) -> (String, Vec<String>) {
    let Value::Object(map) = block else {
        return (String::new(), Vec::new());
    };
    let Some(Value::Array(rows)) = map.get("content") else {
        return (String::new(), Vec::new());
    };
    let mut texts: Vec<String> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    if let Some(title) = map.get("title") {
        let title = text_of(title);
        if !title.is_empty() {
            texts.push(title);
        }
    }
    for row in rows {
        let Value::Array(elements) = row else {
            continue;
        };
        for element in elements {
            let Value::Object(element) = element else {
                continue;
            };
            match element.get("tag").and_then(Value::as_str) {
                Some("text") | Some("a") => {
                    if let Some(text) = element
                        .get("text")
                        .map(text_of)
                        .filter(|text| !text.is_empty())
                    {
                        texts.push(text);
                    }
                }
                Some("at") => {
                    let name = element.get("user_name").map(text_of).unwrap_or_default();
                    let name = if name.is_empty() {
                        "user".to_string()
                    } else {
                        name
                    };
                    texts.push(format!("@{name}"));
                }
                Some("img") => {
                    if let Some(key) = element.get("image_key").and_then(Value::as_str) {
                        images.push(key.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    (texts.join(" ").trim().to_string(), images)
}

/// 资源下载用的 file_key（`file_key` 优先，其次 `image_key`）。
pub fn resource_file_key(content: &Value) -> Option<String> {
    let map = content.as_object()?;
    for key in ["file_key", "image_key"] {
        if let Some(text) = map.get(key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classification_matches_agent_temp_categories() {
        assert_eq!(classify_filename("shot.PNG"), "images");
        assert_eq!(classify_filename("voice.opus"), "audio");
        assert_eq!(classify_filename("clip.mp4"), "videos");
        assert_eq!(classify_filename("run.py"), "scripts");
        assert_eq!(classify_filename("data.csv"), "code");
        assert_eq!(classify_filename("archive.zip"), "files");
        assert_eq!(classify_filename(""), "files");
    }

    #[test]
    fn travel_path_is_neutralised() {
        let root = std::env::temp_dir().join(format!("ocl-feishu-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let resolved = resolve_temp_destination(&root, "../../evil.png").expect("落盘路径");
        assert_eq!(
            resolved
                .strip_prefix(&root)
                .expect("在根内")
                .to_string_lossy()
                .replace('\\', "/"),
            "images/evil.png"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resource_names_gain_suffix_by_type() {
        assert_eq!(
            resource_file_name("key123", "key123", "image"),
            "key123.jpg"
        );
        assert_eq!(
            resource_file_name("key123", "key123", "audio"),
            "key123.opus"
        );
        assert_eq!(resource_file_name("key123", "key123", "file"), "key123.bin");
        assert_eq!(resource_file_name("a.png", "key", "image"), "a.png");
    }

    #[test]
    fn markers_are_extracted_in_order() {
        let paths = file_marker_paths("结果如下\n[FILE:out/a.png] 与 [FILE: .omnicrawl/x.py ]\n");
        assert_eq!(
            paths,
            vec!["out/a.png".to_string(), ".omnicrawl/x.py".to_string()]
        );
        assert!(file_marker_paths("没有标记").is_empty());
    }

    #[test]
    fn post_blocks_follow_python_candidate_order() {
        // Python 侧对每个语言都 insert(0)，因此候选顺序是 en_us → zh_cn → 根节点：
        // 词典序更靠后的语言反而先被取用，这里按同规则断言。
        let content = json!({
            "post": {
                "en_us": {"title": "Title", "content": [[{"tag": "text", "text": "english"}]]},
                "zh_cn": {
                    "title": "标题",
                    "content": [[
                        {"tag": "text", "text": "正文"},
                        {"tag": "at", "user_name": "小明"},
                        {"tag": "img", "image_key": "img_1"},
                    ]],
                },
            }
        });
        let (text, images) = post_text_and_images(&content);
        assert_eq!(text, "Title english");
        assert!(images.is_empty());
    }

    #[test]
    fn post_block_extracts_text_at_and_images() {
        let content = json!({
            "post": {"zh_cn": {"title": "标题", "content": [[
                {"tag": "text", "text": "正文"},
                {"tag": "at", "user_name": "小明"},
                {"tag": "img", "image_key": "img_1"},
            ]]}}
        });
        let (text, images) = post_text_and_images(&content);
        assert_eq!(text, "标题 正文 @小明");
        assert_eq!(images, vec!["img_1".to_string()]);
    }
}
