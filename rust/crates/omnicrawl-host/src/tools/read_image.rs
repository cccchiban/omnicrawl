//! 本地图片读取工具执行体：路径校验、签名识别与视觉附件构造。
//!
//! 语义基准是 `omnicrawl/agent/toolkit/image_tools.py`：只读本机文件（拒绝 URL 与 data URI），
//! 相对路径绑定工作区且不得越界，图片按签名判定 MIME 并整体 Base64 编码，作为
//! `ToolImageAttachment`（视觉附件）随工具结果交给循环。

use std::path::{Path, PathBuf};

use base64::Engine;
use omnicrawl_controllers::json::python_dumps_compact;
use omnicrawl_controllers::types::ToolImageAttachment;
use serde_json::{json, Map, Value};

use super::error::ToolError;
use super::paths::{resolve_lenient, WorkspacePaths};

const SIGNATURES: [(&[u8], &str); 4] = [
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"\xff\xd8\xff", "image/jpeg"),
];
const WEBP_RIFF_PREFIX: &[u8] = b"RIFF";
const WEBP_FORMAT_MARKER: &[u8] = b"WEBP";
const SUPPORTED_MEDIA_TYPES: [&str; 4] = ["image/gif", "image/jpeg", "image/png", "image/webp"];

/// 一次成功读取的产物：给模型的紧凑 JSON 文本、视觉附件与分析提示词。
#[derive(Debug, Clone, PartialEq)]
pub struct ReadImageOutcome {
    pub output: String,
    pub images: Vec<ToolImageAttachment>,
    pub prompt: String,
}

pub fn read_image(
    paths: &WorkspacePaths,
    arguments: &Map<String, Value>,
) -> Result<ReadImageOutcome, ToolError> {
    let raw_path = match arguments.get("path") {
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        _ => return Err(ToolError::new("path 必须是非空的本地图片路径。")),
    };
    let prompt = match arguments.get("prompt") {
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        _ => return Err(ToolError::new("prompt 必须是非空的图片分析提示词。")),
    };
    let detail = read_detail(arguments.get("detail"))?;
    let path = resolve_image_path(&raw_path, paths.root())?;
    let image_bytes = read_bytes(&path, &raw_path)?;
    let media_type = detect_media_type(&path, &image_bytes)?;

    let display_path = display_image_path(&path, paths.root());
    let payload = json!({
        "path": display_path,
        "media_type": media_type,
        "bytes": image_bytes.len(),
        "detail": detail,
        "vision_attachment": true,
    });
    Ok(ReadImageOutcome {
        output: python_dumps_compact(&payload),
        images: vec![ToolImageAttachment {
            media_type,
            data_base64: base64::engine::general_purpose::STANDARD.encode(&image_bytes),
            filename: path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            detail,
        }],
        prompt,
    })
}

fn read_detail(value: Option<&Value>) -> Result<String, ToolError> {
    // 对齐 Python 的 `str(value or "auto")`：falsy 值（false/0/空串/空列表/空对象/null）都回落 auto。
    let detail = match value {
        None => "auto".to_string(),
        Some(value) if !truthy(value) => "auto".to_string(),
        Some(Value::String(text)) => text.trim().to_lowercase(),
        Some(other) => omnicrawl_controllers::json::python_repr(other)
            .trim()
            .to_lowercase(),
    };
    if !matches!(detail.as_str(), "auto" | "low" | "high") {
        return Err(ToolError::new("detail 仅支持 auto、low 或 high。"));
    }
    Ok(detail)
}

/// Python 真值语义：`value or "auto"` 的判定基础。
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn resolve_image_path(raw_path: &str, workspace_root: &Path) -> Result<PathBuf, ToolError> {
    let candidate_text = raw_path.trim();
    let lowered = candidate_text.to_lowercase();
    if lowered.contains("://") || lowered.starts_with("data:") {
        return Err(ToolError::new(
            "read_image 只支持本机文件路径，不支持 URL 或 data URI。",
        ));
    }

    let candidate = PathBuf::from(expand_user(candidate_text));
    let was_absolute = candidate.is_absolute();
    let candidate = if was_absolute {
        candidate
    } else {
        workspace_root.join(candidate)
    };
    let resolved = resolve_lenient(&candidate);
    if !was_absolute && !resolved.starts_with(resolve_lenient(workspace_root)) {
        return Err(ToolError::new(
            "相对图片路径不能越出当前工作区；请使用明确的本机绝对路径。",
        ));
    }
    if !resolved.is_file() {
        return Err(ToolError::new(format!(
            "图片路径不是文件：{candidate_text}"
        )));
    }
    Ok(resolved)
}

/// `Path.expanduser()` 的可用子集：`~` 与 `~/...`、`~\...` 展开为用户主目录。
fn expand_user(path: &str) -> String {
    let home = || -> PathBuf {
        for name in ["USERPROFILE", "HOME"] {
            if let Ok(value) = std::env::var(name) {
                if !value.trim().is_empty() {
                    return PathBuf::from(value);
                }
            }
        }
        PathBuf::from(".")
    };
    if path == "~" {
        return home().to_string_lossy().to_string();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home().join(rest).to_string_lossy().to_string(),
        None => path.to_string(),
    }
}

fn read_bytes(path: &Path, raw_path: &str) -> Result<Vec<u8>, ToolError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            let message = match error.kind() {
                std::io::ErrorKind::NotFound => format!("图片文件不存在：{raw_path}"),
                std::io::ErrorKind::PermissionDenied => format!("没有权限读取图片：{raw_path}"),
                _ => format!("读取图片失败：{raw_path}，{error}"),
            };
            Err(ToolError::new(message))
        }
    }
}

fn detect_media_type(path: &Path, image_bytes: &[u8]) -> Result<String, ToolError> {
    for (signature, media_type) in SIGNATURES {
        if image_bytes.starts_with(signature) {
            return Ok(media_type.to_string());
        }
    }
    if image_bytes.len() >= 12
        && image_bytes.starts_with(WEBP_RIFF_PREFIX)
        && &image_bytes[8..12] == WEBP_FORMAT_MARKER
    {
        return Ok("image/webp".to_string());
    }
    Err(ToolError::new(format!(
        "文件不是受支持的图片格式：{}。支持的 MIME 类型：{}。",
        path.display(),
        SUPPORTED_MEDIA_TYPES.join("、")
    )))
}

fn display_image_path(path: &Path, workspace_root: &Path) -> String {
    match path.strip_prefix(resolve_lenient(workspace_root)) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0data";
    const GIF: &[u8] = b"GIF89a....";
    const WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 ";

    fn workspace(name: &str) -> (WorkspacePaths, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-image-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        (WorkspacePaths::new(&root), root)
    }

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn signatures_map_to_media_types() {
        let (paths, root) = workspace("signature");
        for (name, bytes, media_type) in [
            ("a.png", PNG, "image/png"),
            ("a.jpg", JPEG, "image/jpeg"),
            ("a.gif", GIF, "image/gif"),
            ("a.webp", WEBP, "image/webp"),
        ] {
            std::fs::write(root.join(name), bytes).expect("写测试图片");
            let outcome = read_image(
                &paths,
                &arguments(json!({"path": name, "prompt": "描述图片"})),
            )
            .expect("读取应当成功");
            assert!(
                outcome
                    .output
                    .contains(&format!("\"media_type\":\"{media_type}\"")),
                "{}",
                outcome.output
            );
            assert_eq!(outcome.images[0].media_type, media_type);
            assert_eq!(outcome.images[0].filename, name);
            assert_eq!(outcome.images[0].detail, "auto");
        }
    }

    #[test]
    fn relative_paths_cannot_escape_the_workspace() {
        let (paths, _root) = workspace("escape");
        let error = read_image(
            &paths,
            &arguments(json!({"path": "../outside.png", "prompt": "x"})),
        )
        .expect_err("越界相对路径应当被拒绝");
        assert!(
            error.message.contains("不能越出当前工作区"),
            "{}",
            error.message
        );
    }

    #[test]
    fn urls_data_uris_and_unsupported_formats_are_rejected() {
        let (paths, root) = workspace("reject");
        for path in ["https://example.com/a.png", "data:image/png;base64,AAAA"] {
            let error = read_image(&paths, &arguments(json!({"path": path, "prompt": "x"})))
                .expect_err("URL 应当被拒绝");
            assert!(error.message.contains("不支持 URL"), "{}", error.message);
        }
        std::fs::write(root.join("note.txt"), "不是图片").expect("写文本文件");
        let error = read_image(
            &paths,
            &arguments(json!({"path": "note.txt", "prompt": "x"})),
        )
        .expect_err("非图片格式应当被拒绝");
        assert!(
            error.message.contains("不是受支持的图片格式"),
            "{}",
            error.message
        );
        assert!(error
            .message
            .contains("image/gif、image/jpeg、image/png、image/webp"));
    }

    #[test]
    fn falsy_details_fall_back_to_auto() {
        let (paths, root) = workspace("detail");
        std::fs::write(root.join("a.png"), PNG).expect("写测试图片");
        for value in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            let outcome = read_image(
                &paths,
                &arguments(json!({"path": "a.png", "prompt": "x", "detail": value})),
            )
            .unwrap_or_else(|error| panic!("detail={value} 应当回落 auto：{}", error.message));
            assert_eq!(outcome.images[0].detail, "auto", "detail={value}");
        }
        for value in [json!("low "), json!("LOW")] {
            let outcome = read_image(
                &paths,
                &arguments(json!({"path": "a.png", "prompt": "x", "detail": value})),
            )
            .unwrap_or_else(|error| panic!("detail={value} 应当被接受：{}", error.message));
            assert_eq!(outcome.images[0].detail, "low", "detail={value}");
        }
        // truthy 但不是三个取值之一：与 Python 一样判非法。
        for value in [json!(true), json!(1), json!([1]), json!({"a": 1})] {
            let error = read_image(
                &paths,
                &arguments(json!({"path": "a.png", "prompt": "x", "detail": value})),
            )
            .expect_err("非法的 truthy detail 应当被拒绝");
            assert_eq!(
                error.message, "detail 仅支持 auto、low 或 high。",
                "detail={value}"
            );
        }
    }

    #[test]
    fn path_prompt_and_detail_are_validated() {
        let (paths, _root) = workspace("validate");
        assert_eq!(
            read_image(&paths, &arguments(json!({"path": " ", "prompt": "x"})))
                .expect_err("空路径应当被拒绝")
                .message,
            "path 必须是非空的本地图片路径。"
        );
        assert_eq!(
            read_image(&paths, &arguments(json!({"path": "a.png"})))
                .expect_err("缺 prompt 应当被拒绝")
                .message,
            "prompt 必须是非空的图片分析提示词。"
        );
        assert_eq!(
            read_image(
                &paths,
                &arguments(json!({"path": "a.png", "prompt": "x", "detail": "huge"}))
            )
            .expect_err("非法 detail 应当被拒绝")
            .message,
            "detail 仅支持 auto、low 或 high。"
        );
        assert_eq!(
            read_image(&paths, &arguments(json!({"path": "a.png", "prompt": "x"})))
                .expect_err("不存在的文件应当被拒绝")
                .message,
            "图片路径不是文件：a.png"
        );
    }
}
