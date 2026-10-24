//! QQ 富媒体消息（`msg_type=7`）的本地取文件、类型判定与正文剥离。
//!
//! 发送富媒体要两步：先 POST `/v2/{users|groups}/{openid}/files` 拿 `file_info`
//! （`srv_send_msg=false`，不占主动消息频次），再用 `media.file_info` 发 `msg_type=7`。
//! 直传二进制上限 5MB（更大的要走分片上传），因此这里只发不超过该阈值的文件，
//! 其余保持文本形式把路径告诉用户。
//!
//! 触发方式与飞书一致：Agent 在回复里写 `[FILE:路径]`。这里负责把标记从正文里摘掉，
//! 由调用方决定是否真的发出去（额度不够时保留路径文本）。

use std::fs;
use std::path::Path;

use super::api::{Target, FILE_TYPE_FILE, FILE_TYPE_IMAGE, FILE_TYPE_VIDEO, FILE_TYPE_VOICE};
use crate::feishu::files::file_marker_paths;

/// 平台二进制直传上限。
pub const MAX_MEDIA_BYTES: usize = 5 * 1024 * 1024;

/// 富媒体种类：决定上传时的 `file_type`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Voice,
    File,
}

impl MediaKind {
    /// 平台 `file_type`：1 图片、2 视频、3 语音、4 文件。
    pub fn file_type(self) -> i64 {
        match self {
            MediaKind::Image => FILE_TYPE_IMAGE,
            MediaKind::Video => FILE_TYPE_VIDEO,
            MediaKind::Voice => FILE_TYPE_VOICE,
            MediaKind::File => FILE_TYPE_FILE,
        }
    }
}

/// 该场景能否发富媒体：频道消息走另一套接口，这里只覆盖单聊与群聊。
pub fn supports(target: &Target) -> bool {
    !matches!(target, Target::Channel(_))
}

/// 按扩展名判定富媒体种类；图片只发 png/jpg，视频只发 mp4，语音只发 silk/mp3/wav，
/// 其余按「文件」（`file_type=4`）上传。
pub fn kind_of_path(path: &str) -> MediaKind {
    let suffix = Path::new(path)
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match suffix.as_str() {
        "png" | "jpg" | "jpeg" => MediaKind::Image,
        "mp4" => MediaKind::Video,
        "silk" | "mp3" | "wav" => MediaKind::Voice,
        _ => MediaKind::File,
    }
}

/// 一份待上传的富媒体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaUpload {
    /// 上传时用的文件名（平台只用来判类型与展示）。
    pub file_name: String,
    pub kind: MediaKind,
    pub bytes: Vec<u8>,
}

impl MediaUpload {
    pub fn file_type(&self) -> i64 {
        self.kind.file_type()
    }
}

/// 读取本地文件；不存在、不是文件、超过直传上限都返回可读中文原因。
pub fn load(path: &str) -> Result<MediaUpload, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("文件路径为空".to_string());
    }
    let file = Path::new(trimmed);
    let metadata = fs::metadata(file).map_err(|_| format!("文件不存在：{trimmed}"))?;
    if !metadata.is_file() {
        return Err(format!("不是文件：{trimmed}"));
    }
    if metadata.len() as usize > MAX_MEDIA_BYTES {
        return Err(format!(
            "文件超过 {}MB 直传上限：{trimmed}",
            MAX_MEDIA_BYTES / 1024 / 1024
        ));
    }
    let bytes = fs::read(file).map_err(|error| format!("读取文件失败：{trimmed}（{error}）"))?;
    let file_name = file
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    Ok(MediaUpload {
        file_name,
        kind: kind_of_path(trimmed),
        bytes,
    })
}

/// 正文里的 `[FILE:...]` 路径（按出现顺序）。
pub fn marker_paths(text: &str) -> Vec<String> {
    file_marker_paths(text)
}

/// 去掉正文里的 `[FILE:...]` 标记：单独成行的整行删除，行内标记就地抹掉。
pub fn strip_markers(text: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if full_line_marker(line.trim()).is_some() {
            continue;
        }
        kept.push(remove_inline_markers(line));
    }
    let mut squeezed: Vec<&str> = Vec::new();
    for line in kept.iter() {
        // 整行标记删掉后可能留下连续空行，压成一个。
        if line.is_empty() && squeezed.last().map(|last| last.is_empty()).unwrap_or(false) {
            continue;
        }
        squeezed.push(line);
    }
    squeezed.join("\n").trim_end().to_string()
}

/// 整行就是一个标记（`[FILE:路径]`）时返回路径。
fn full_line_marker(trimmed: &str) -> Option<&str> {
    let inner = trimmed.strip_prefix("[FILE:")?.strip_suffix(']')?;
    (!inner.contains(']')).then_some(inner)
}

fn remove_inline_markers(line: &str) -> String {
    let mut rest = line;
    let mut out = String::new();
    while let Some(start) = rest.find("[FILE:") {
        let after = &rest[start + "[FILE:".len()..];
        let Some(end) = after.find(']') else { break };
        out.push_str(&rest[..start]);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_platform_file_types() {
        assert_eq!(kind_of_path("a.PNG").file_type(), FILE_TYPE_IMAGE);
        assert_eq!(kind_of_path("a.mp4").file_type(), FILE_TYPE_VIDEO);
        assert_eq!(kind_of_path("a.silk").file_type(), FILE_TYPE_VOICE);
        assert_eq!(kind_of_path("a.pdf").file_type(), FILE_TYPE_FILE);
        assert_eq!(kind_of_path("noext").file_type(), FILE_TYPE_FILE);
    }

    #[test]
    fn channels_do_not_support_rich_media() {
        assert!(supports(&Target::User("F6A8".to_string())));
        assert!(supports(&Target::Group("A1B2".to_string())));
        assert!(!supports(&Target::Channel("9".to_string())));
    }

    #[test]
    fn markers_are_stripped_from_the_visible_text() {
        assert_eq!(
            strip_markers("结果如下\n[FILE:out/a.png]\n完成"),
            "结果如下\n完成"
        );
        assert_eq!(strip_markers("见 [FILE:a.png] 图"), "见  图");
    }
    #[test]
    fn markers_are_listed_in_order() {
        assert_eq!(
            marker_paths("x [FILE:a.png] y [FILE: b.pdf ]"),
            vec!["a.png".to_string(), "b.pdf".to_string()]
        );
    }

    #[test]
    fn missing_files_report_a_reason() {
        let error = load("definitely-missing.png").expect_err("应报错");
        assert!(error.contains("文件不存在"), "{error}");
    }
}
