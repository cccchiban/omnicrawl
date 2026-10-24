//! QQ 附件的分类与落盘命名。
//!
//! 复用 Telegram 那份扩展名分类（`.agent_tmp/images|videos|audio|scripts|code|files`）
//! 与重名递增规则：两个连接器收到的东西落到同一个临时目录树里，规则必须一致，
//! 否则 Agent 在不同通道里看到的路径约定会不一样。

use std::path::{Path, PathBuf};

pub use crate::telegram::files::{
    classify_file_name, file_name_of, file_stem, path_suffix, relative_display_path,
    temp_destination, unix_seconds,
};

/// 没有原始文件名时按 URL 猜一个名字。
///
/// QQ 的附件 `url` 一般是签名链接（查询串很长），因此只取路径最后一段；
/// 它通常没有扩展名的依据，猜不到就回落到时间戳 + 内容类型给的扩展名。
pub fn fallback_name(url: &str, stamp: i64, mime: &str) -> String {
    let suffix = suffix_for_mime(mime);
    if let Some(name) = last_path_segment(url) {
        if !path_suffix(&name).is_empty() {
            return name;
        }
    }
    format!("qqbot_{stamp}{suffix}")
}

/// 落盘用的安全文件名：去掉目录成分，空名回落到时间戳名。
pub fn safe_name(name: &str, url: &str, stamp: i64, mime: &str) -> String {
    let cleaned = file_name_of(name);
    if cleaned.is_empty() {
        return fallback_name(url, stamp, mime);
    }
    cleaned
}

/// MIME → 扩展名；只覆盖 QQ 常见附件类型。
pub fn suffix_for_mime(mime: &str) -> &'static str {
    let lowered = mime.trim().to_ascii_lowercase();
    match lowered.as_str() {
        "image/png" => ".png",
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        "audio/wav" | "audio/x-wav" => ".wav",
        "audio/mpeg" | "audio/mp3" => ".mp3",
        "audio/flac" => ".flac",
        "audio/silk" => ".silk",
        "application/pdf" => ".pdf",
        "application/zip" => ".zip",
        "text/plain" => ".txt",
        "text/markdown" => ".md",
        _ => "",
    }
}

/// `.agent_tmp/<分类>/<安全文件名>`（目录不存在则创建）。
pub fn destination(temp_root: &Path, subdir: &str, name: &str) -> std::io::Result<PathBuf> {
    temp_destination(temp_root, subdir, name)
}

fn last_path_segment(url: &str) -> Option<String> {
    let without_query = url.split('?').next().unwrap_or(url);
    let trimmed = without_query.trim_end_matches('/');
    let segment = trimmed.rsplit('/').next()?.trim();
    if segment.is_empty() || segment.contains('.') && path_suffix(segment).is_empty() {
        return None;
    }
    Some(segment.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_mime_types_give_suffixes() {
        assert_eq!(suffix_for_mime("image/png"), ".png");
        assert_eq!(suffix_for_mime("IMAGE/JPEG"), ".jpg");
        assert_eq!(suffix_for_mime("audio/silk"), ".silk");
        assert_eq!(suffix_for_mime("application/octet-stream"), "");
    }

    #[test]
    fn classification_reuses_the_telegram_rules() {
        assert_eq!(classify_file_name("报告.PDF"), "files");
        assert_eq!(classify_file_name("截图.png"), "images");
        assert_eq!(classify_file_name("main.rs"), "code");
    }

    #[test]
    fn fallback_name_prefers_a_real_filename_in_the_url() {
        let name = fallback_name(
            "https://multimedia.nt.qq.com.cn/download?fileid=abc",
            1_700_000_000,
            "image/png",
        );
        assert_eq!(name, "qqbot_1700000000.png");
        assert_eq!(
            fallback_name("https://x/y/photo.jpg?sign=1", 1, "image/jpeg"),
            "photo.jpg"
        );
    }

    #[test]
    fn safe_name_strips_directories_and_falls_back() {
        assert_eq!(
            safe_name("../../etc/passwd", "https://x/y", 5, ""),
            "passwd"
        );
        assert_eq!(safe_name("", "https://x/y/a.txt", 5, ""), "a.txt");
    }
}
