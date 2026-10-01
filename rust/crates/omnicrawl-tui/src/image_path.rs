//! 「粘贴内容就是一个图片路径」的识别。
//!
//! 判据收得很紧：整段粘贴内容去掉首尾空白后必须正好是一个**存在的**图片文件路径，
//! 中间不能夹换行或控制字符。多出来的任何字符（引号、代码片段、别的正文）都会让整段
//! 解析不出文件，于是回落成普通文本粘贴——这正是「粘贴代码时不要把路径片段误当图片」。
//!
//! 路径本身允许中文目录名、空格与括号（都是合法的 Windows 文件名组成部分），
//! 是否真的是图片以文件签名为准，与 `read_image` 共用同一份判定。

use std::path::Path;

use omnicrawl_host::tools::read_image::{read_local_image, LocalImage};

/// 承认的图片扩展名：只是为了避免把别的大文件整个读进内存，
/// 真正裁决格式的仍是文件签名（因此扩展名对但不含图片内容的文件不会被当成图片）。
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// 整段粘贴内容就是一个本机图片文件路径时读出该图片，否则返回 `None`。
///
/// `workspace_root` 用于解析相对路径（与 `read_image` 同一套规则）。
pub fn read_pasted_image_path(text: &str, workspace_root: &Path) -> Option<LocalImage> {
    let candidate = lone_path(text)?;
    if !has_image_extension(candidate) {
        return None;
    }
    read_local_image(candidate, workspace_root).ok()
}

/// 整段是不是一个「图片路径形状」的文本（**不做**文件系统检查）。
///
/// 粘贴识别用它判断值不值得等按键流拼完：控制台会把一次粘贴按批切开，只有认得出剪贴板里
/// 是一条图片路径，才能让这些批次先挂起、拼成一次粘贴。真正的裁决仍归 `read_pasted_image_path`。
pub fn is_lone_image_path(text: &str) -> bool {
    lone_path(text).is_some_and(has_image_extension)
}

/// 整段内容能否当作单独一条路径：去掉首尾空白后非空，且不含换行与其它控制字符。
fn lone_path(text: &str) -> Option<&str> {
    let candidate = text.trim();
    if candidate.is_empty() || candidate.chars().any(char::is_control) {
        return None;
    }
    Some(candidate)
}

fn has_image_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            let lowered = extension.to_ascii_lowercase();
            IMAGE_EXTENSIONS.contains(&lowered.as_str())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0data";

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-image-path-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        root
    }

    #[test]
    fn an_absolute_png_path_is_recognized() {
        let root = workspace("absolute");
        let file = root.join("trace-3200x2922.png");
        std::fs::write(&file, PNG).expect("写测试图片");
        let image = read_pasted_image_path(&file.to_string_lossy(), &root).expect("应识别为图片");
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.bytes, PNG);
    }

    #[test]
    fn the_path_may_contain_chinese_spaces_and_parentheses() {
        let root = workspace("chinese");
        let directory = root.join("下载").join("1991");
        std::fs::create_dir_all(&directory).expect("创建目录");
        let file = directory.join("trace-2266x3200 (1).png");
        std::fs::write(&file, PNG).expect("写测试图片");
        assert!(
            read_pasted_image_path(&file.to_string_lossy(), &root).is_some(),
            "中文目录名、空格与括号都是路径的一部分，不该被排除"
        );
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_workspace() {
        let root = workspace("relative");
        std::fs::create_dir_all(root.join("images")).expect("创建目录");
        std::fs::write(root.join("images").join("a.jpg"), JPEG).expect("写测试图片");
        let image = read_pasted_image_path("images/a.jpg", &root).expect("相对路径应能解析");
        assert_eq!(image.media_type, "image/jpeg");
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let root = workspace("whitespace");
        let file = root.join("a.png");
        std::fs::write(&file, PNG).expect("写测试图片");
        let text = format!("  {}\r\n", file.to_string_lossy());
        assert!(read_pasted_image_path(&text, &root).is_some(), "整段只是路径时尾随空白可以忽略");
    }

    #[test]
    fn extra_text_around_the_path_is_not_an_image() {
        let root = workspace("extra");
        let file = root.join("a.png");
        std::fs::write(&file, PNG).expect("写测试图片");
        let path = file.to_string_lossy();
        for text in [
            format!("看一下这张图 {path}"),
            format!("{path} 是什么"),
            format!("\"{path}\""),
            format!("read_image(\"{path}\")"),
            format!("{path}\n{path}"),
        ] {
            assert!(
                read_pasted_image_path(&text, &root).is_none(),
                "混有其它内容时不能当成图片：{text}"
            );
        }
    }

    #[test]
    fn a_missing_or_non_image_file_is_not_an_image() {
        let root = workspace("missing");
        std::fs::write(root.join("note.txt"), b"hello").expect("写测试文件");
        std::fs::write(root.join("fake.png"), b"hello").expect("写非图片内容");
        assert!(
            read_pasted_image_path(&root.join("missing.png").to_string_lossy(), &root).is_none(),
            "文件不存在时不识别"
        );
        assert!(
            read_pasted_image_path("note.txt", &root).is_none(),
            "扩展名不是图片时不识别"
        );
        assert!(
            read_pasted_image_path("fake.png", &root).is_none(),
            "内容是别的格式时不识别（签名裁决）"
        );
    }
}
