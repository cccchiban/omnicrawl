//! 粘贴图片的落盘：发送前把 `Ctrl+V` 粘进来的剪贴板位图存进工作区临时目录。
//!
//! 图片本身只以 Base64 随 `turn.submit` 进内核，磁盘上原本没有副本。这里在发送前补一份，
//! 落点与宿主其它图片产物一致（`.omnicrawl/.agent_tmp/images/`，见 `image_gen` 与
//! `windows_screenshot`），并把相对路径写进提问正文——模型需要看原图时可以直接读文件。
//!
//! 只归档剪贴板位图：粘贴进来的图片路径本来就指向磁盘上的原文件，再复制一份没有意义。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::state::PendingImage;

/// 归档目录（按组件拼接，分隔符与平台一致）。
const ARCHIVE_SEGMENTS: [&str; 3] = [".omnicrawl", ".agent_tmp", "images"];

/// 把带归档标记的图片写进工作区临时目录，返回写进提问正文的说明段；没有可归档的图片
/// 或全部落盘失败时返回 `None`。
///
/// 失败不阻断发送：这份副本是给「后续再读」用的，丢一份不该让整轮提问发不出去，
/// 因此逐张失败就跳过该张，正文里也不报它的路径。
pub fn archive_pasted_images(workspace_root: &Path, images: &[PendingImage]) -> Option<String> {
    let pending: Vec<&PendingImage> = images.iter().filter(|image| image.archive).collect();
    if pending.is_empty() {
        return None;
    }
    let directory = archive_directory(workspace_root);
    std::fs::create_dir_all(&directory).ok()?;
    let mut lines = Vec::with_capacity(pending.len());
    for image in pending {
        // 剪贴板位图读出来就是 PNG（`clipboard::read_image_png`），无需再按 media_type 推扩展名。
        let path = directory.join(format!("pasted_{}_{}.png", timestamp(), unique_suffix()));
        if std::fs::write(&path, &image.bytes).is_ok() {
            lines.push(format!(
                "{} → {}",
                image.placeholder,
                display_path(&path, workspace_root)
            ));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!("粘贴的图片已保存：\n{}", lines.join("\n")))
}

fn archive_directory(workspace_root: &Path) -> PathBuf {
    let mut path = workspace_root.to_path_buf();
    for segment in ARCHIVE_SEGMENTS {
        path.push(segment);
    }
    path
}

/// 相对工作区的展示路径（正斜杠），路径落在工作区外时给绝对路径。
fn display_path(path: &Path, workspace_root: &Path) -> String {
    match path.strip_prefix(workspace_root) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

/// 可读的时间戳（UTC），与 `windows_screenshot` 的命名同形。
fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%S_%6fZ").to_string()
}

/// 同微秒内的多张图片也不能撞名：纳秒与进程内计数混一个后缀出来。
fn unique_suffix() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.subsec_nanos())
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:08x}", nanos ^ sequence.wrapping_mul(2_654_435_761))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-image-archive-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        root
    }

    fn image(placeholder: &str, archive: bool) -> PendingImage {
        PendingImage {
            placeholder: placeholder.to_string(),
            media_type: "image/png".to_string(),
            bytes: PNG.to_vec(),
            archive,
        }
    }

    #[test]
    fn clipboard_bitmaps_are_written_into_the_workspace_temp_dir() {
        let root = workspace("bitmap");
        let images = vec![image("[ #1 Image ]", true), image("[ #2 Image ]", true)];
        let note = archive_pasted_images(&root, &images).expect("应归档并给出说明");

        let directory = root.join(".omnicrawl").join(".agent_tmp").join("images");
        let entries: Vec<PathBuf> = std::fs::read_dir(&directory)
            .expect("归档目录应当存在")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert_eq!(entries.len(), 2, "两张图各落一份，不能互相覆盖");
        for path in &entries {
            assert_eq!(std::fs::read(path).expect("应当读到副本"), PNG);
        }
        assert!(note.starts_with("粘贴的图片已保存："), "{note}");
        assert!(note.contains("[ #1 Image ] → "), "{note}");
        assert!(note.contains("[ #2 Image ] → "), "{note}");
        assert!(
            note.contains(".omnicrawl/.agent_tmp/images/pasted_"),
            "正文里要给相对工作区的路径：{note}"
        );
    }

    #[test]
    fn images_without_the_archive_flag_are_left_alone() {
        let root = workspace("skipped");
        let images = vec![image("[ #1 Image ]", false)];
        assert!(archive_pasted_images(&root, &images).is_none());
        assert!(
            !root.join(".omnicrawl").exists(),
            "没有可归档的图片时不该凭空建目录"
        );
    }
}
