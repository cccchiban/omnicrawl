//! QQ 富媒体消息（`msg_type=7`）的本地取文件、类型判定与正文剥离。
//!
//! 发送富媒体要两步：先 POST `/v2/{users|groups}/{openid}/files` 拿 `file_info`
//! （`srv_send_msg=false`，不占主动消息频次），再用 `media.file_info` 发 `msg_type=7`。
//! 二进制直传上限 5MB，更大的文件由 [`super::api::QqApi::upload_media`] 走分片上传。
//!
//! 触发方式与飞书一致：Agent 在回复里写 `[FILE:路径]`。这里负责把标记从正文里摘掉，
//! 由调用方决定是否真的发出去（额度不够时保留路径文本）。

use std::fs;
use std::path::{Path, PathBuf};

use super::api::{Target, FILE_TYPE_FILE, FILE_TYPE_IMAGE, FILE_TYPE_VIDEO, FILE_TYPE_VOICE};
use crate::feishu::files::file_marker_paths;

/// 平台二进制直传上限：超过就走分片上传，不再是硬限制。
pub const MAX_MEDIA_BYTES: usize = 5 * 1024 * 1024;

/// 平台分片上传的硬限制：所有类型都是 200MB，超过只能让用户自己想办法。
pub const MAX_CHUNKED_BYTES: usize = 200 * 1024 * 1024;

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

    /// 该类型的软限制（字节）：超过会被平台降级成「文件」形态发送。
    ///
    /// 图片 20MB、视频 30MB、语音 20MB、其余文件 200MB。降级不是失败——文件照样收到，
    /// 只是不再是「可直接点开的图片/视频」。
    pub fn soft_limit(self) -> usize {
        match self {
            MediaKind::Image => 20 * 1024 * 1024,
            MediaKind::Video => 30 * 1024 * 1024,
            MediaKind::Voice => 20 * 1024 * 1024,
            MediaKind::File => MAX_CHUNKED_BYTES,
        }
    }
}

/// 该场景能否发富媒体：频道消息走另一套接口，这里只覆盖单聊与群聊。
pub fn supports(target: &Target) -> bool {
    !matches!(target, Target::Channel(_))
}

/// 按扩展名判定富媒体种类。
///
/// 图片只认 png/jpg、视频只认 mp4、语音认 silk/mp3/wav/flac/ogg（平台文档列出这五种），
/// 其余按「文件」（`file_type=4`）上传——QQ 客户端仍可下载，只是不在聊天里内联播放。
pub fn kind_of_path(path: &str) -> MediaKind {
    let suffix = Path::new(path)
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match suffix.as_str() {
        "png" | "jpg" | "jpeg" => MediaKind::Image,
        "mp4" => MediaKind::Video,
        "silk" | "mp3" | "wav" | "flac" | "ogg" => MediaKind::Voice,
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

    /// 是否超过该类型的软限制：平台会把它降级成「文件」形态。
    pub fn exceeds_soft_limit(&self) -> bool {
        self.bytes.len() > self.kind.soft_limit()
    }
}

/// 读取本地文件；不存在、不是文件、超过 200MB 硬限都返回可读中文原因。
///
/// `root` 是 Agent 工作区根：Agent 写 `[FILE:输出/a.png]` 这类相对路径时，按工作区解析
/// 而不是按连接器进程的当前目录——两者常常不是一回事。
pub fn load(path: &str, root: Option<&Path>) -> Result<MediaUpload, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("文件路径为空".to_string());
    }
    let file = resolve(trimmed, root);
    let metadata = fs::metadata(&file).map_err(|_| format!("文件不存在：{trimmed}"))?;
    if !metadata.is_file() {
        return Err(format!("不是文件：{trimmed}"));
    }
    if metadata.len() as usize > MAX_CHUNKED_BYTES {
        return Err(format!(
            "文件超过 {}MB 上限：{trimmed}",
            MAX_CHUNKED_BYTES / 1024 / 1024
        ));
    }
    let bytes = fs::read(&file).map_err(|error| format!("读取文件失败：{trimmed}（{error}）"))?;
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

/// 相对路径按工作区根解析；绝对路径原样返回。
fn resolve(path: &str, root: Option<&Path>) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        return candidate;
    }
    match root {
        Some(root) if !root.as_os_str().is_empty() => {
            let joined = root.join(&candidate);
            // 工作区里没有、当前目录里有时，仍按原样读：路径是用户/模型给的，别替他改主意。
            if joined.exists() {
                joined
            } else {
                candidate
            }
        }
        _ => candidate,
    }
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
        assert_eq!(kind_of_path("a.jpg").file_type(), FILE_TYPE_IMAGE);
        assert_eq!(kind_of_path("a.mp4").file_type(), FILE_TYPE_VIDEO);
        assert_eq!(kind_of_path("a.silk").file_type(), FILE_TYPE_VOICE);
        assert_eq!(kind_of_path("a.mp3").file_type(), FILE_TYPE_VOICE);
        assert_eq!(kind_of_path("a.flac").file_type(), FILE_TYPE_VOICE);
        assert_eq!(kind_of_path("a.ogg").file_type(), FILE_TYPE_VOICE);
        assert_eq!(kind_of_path("a.pdf").file_type(), FILE_TYPE_FILE);
        assert_eq!(kind_of_path("noext").file_type(), FILE_TYPE_FILE);
    }

    #[test]
    fn formats_the_platform_will_not_inline_go_out_as_files() {
        // 平台只内联 png/jpg 与 mp4：gif/webp/mov 一律按文件发，否则会被 850019 拒。
        assert_eq!(kind_of_path("a.gif"), MediaKind::File);
        assert_eq!(kind_of_path("a.webp"), MediaKind::File);
        assert_eq!(kind_of_path("a.mov"), MediaKind::File);
    }

    #[test]
    fn soft_limits_follow_the_platform_table() {
        assert_eq!(MediaKind::Image.soft_limit(), 20 * 1024 * 1024);
        assert_eq!(MediaKind::Video.soft_limit(), 30 * 1024 * 1024);
        assert_eq!(MediaKind::Voice.soft_limit(), 20 * 1024 * 1024);
        assert_eq!(MediaKind::File.soft_limit(), MAX_CHUNKED_BYTES);
        let small = MediaUpload {
            file_name: "a.png".to_string(),
            kind: MediaKind::Image,
            bytes: vec![0; 1024],
        };
        assert!(!small.exceeds_soft_limit());
        let big = MediaUpload {
            file_name: "a.mp4".to_string(),
            kind: MediaKind::Video,
            bytes: vec![0; MediaKind::Video.soft_limit() + 1],
        };
        assert!(big.exceeds_soft_limit());
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
        let error = load("definitely-missing.png", None).expect_err("应报错");
        assert!(error.contains("文件不存在"), "{error}");
    }

    #[test]
    fn relative_paths_resolve_against_the_workspace_root() {
        let root = std::env::temp_dir().join("omnicrawl-qqbot-relative-media");
        let nested = root.join("输出");
        fs::create_dir_all(&nested).expect("建目录");
        fs::write(nested.join("a.png"), b"PNG").expect("写文件");
        let upload = load("输出/a.png", Some(&root)).expect("按工作区根读到");
        assert_eq!(upload.bytes, b"PNG");
        assert_eq!(upload.kind, MediaKind::Image);
        // 工作区里没有的文件：仍报「文件不存在」，而不是悄悄换成别的路径。
        let error = load("输出/b.png", Some(&root)).expect_err("应报错");
        assert!(error.contains("文件不存在"), "{error}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn absolute_paths_ignore_the_workspace_root() {
        let dir = std::env::temp_dir().join("omnicrawl-qqbot-absolute-media");
        fs::create_dir_all(&dir).expect("建目录");
        let file = dir.join("b.mp4");
        fs::write(&file, b"MP4").expect("写文件");
        let other = std::env::temp_dir().join("omnicrawl-qqbot-elsewhere");
        let upload = load(&file.to_string_lossy(), Some(&other)).expect("绝对路径照读");
        assert_eq!(upload.bytes, b"MP4");
        assert_eq!(upload.kind, MediaKind::Video);
        fs::remove_dir_all(&dir).ok();
    }
}
