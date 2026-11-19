//! 粘贴进来的**文档**（PDF、Word、表格等）识别。
//!
//! 与 [`crate::image_path`] 的分工：图片走视觉通路、随 `turn.submit` 当附件发出去；
//! 文档没有那条通路，模型只能**按路径读文件**，因此这里不读字节，只认路径——
//! 输入框里折成 `[ #1 .pdf ]` 一块，提交时展开成文件名，并在 user 消息下方逐行列出完整路径。
//!
//! 只认「整段就是一个存在的文档路径」：夹了正文、引号或代码的粘贴一律当普通文本，
//! 避免粘贴代码时把路径片段误当文档。

use std::path::Path;

/// 承认的文档扩展名。只做扩展名筛选，不读文件内容：判定的是「这一块该不该折成附件」，
/// 真伪由模型读文件时自己裁决（与图片路径按签名裁决的做法不同——这里不打算为一次粘贴
/// 把几十 MB 的 PDF 读进内存）。
const DOCUMENT_EXTENSIONS: [&str; 11] = [
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "csv", "md", "txt", "rtf",
];

/// 粘贴进来的一个文档：完整路径 + 折进输入框的占位符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDocument {
    /// 折在输入框里的那一块（`[ #1 .pdf ]`）。
    pub placeholder: String,
    /// 绝对路径（相对路径已按工作区解析成绝对）。
    pub path: String,
}

impl PendingDocument {
    /// 文件名（提交时用它替换占位符）。
    pub fn file_name(&self) -> String {
        file_name_of(&self.path)
    }

    /// 占位符里用的类型名：小写扩展名，没有扩展名时给 `file`。
    pub fn extension(&self) -> String {
        Self::extension_of(&self.path)
    }

    pub fn extension_of(path: &str) -> String {
        extension_of(path).unwrap_or_else(|| "file".to_string())
    }
}

/// 整段粘贴内容就是一个存在的文档路径时返回它（已解析成绝对路径），否则 `None`。
pub fn read_pasted_document_path(text: &str, workspace_root: &Path) -> Option<String> {
    let candidate = lone_path(text)?;
    if !has_document_extension(candidate) {
        return None;
    }
    let path = resolve(candidate, workspace_root)?;
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// 整段是不是一个「文档路径形状」的文本（**不做**文件系统检查）。
///
/// 粘贴识别用它判断值不值得等按键流拼完：控制台会把一次粘贴按批切开，只有认得出剪贴板里
/// 是一条路径，才能让这些批次先挂起、拼成一次粘贴。真正的裁决仍归 [`read_pasted_document_path`]。
pub fn is_lone_document_path(text: &str) -> bool {
    lone_path(text).is_some_and(has_document_extension)
}

fn lone_path(text: &str) -> Option<&str> {
    let candidate = text.trim();
    if candidate.is_empty() || candidate.chars().any(char::is_control) {
        return None;
    }
    Some(candidate)
}

/// 相对路径按工作区解析（与 `read_image` 同一套规则），绝对路径原样返回。
fn resolve(path: &str, workspace_root: &Path) -> Option<std::path::PathBuf> {
    let path = Path::new(path);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    })
}

fn extension_of(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn has_document_extension(path: &str) -> bool {
    extension_of(path).is_some_and(|extension| DOCUMENT_EXTENSIONS.contains(&extension.as_str()))
}

/// 路径里的文件名；取不到时退回整段路径（不会出现空标题行）。
pub fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-document-path-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        root
    }

    #[test]
    fn an_absolute_pdf_path_is_recognized() {
        let root = workspace("absolute");
        let file = root.join("报告.pdf");
        std::fs::write(&file, b"%PDF-1.7").expect("写测试文档");
        let path = read_pasted_document_path(&file.to_string_lossy(), &root).expect("应识别为文档");
        assert_eq!(file_name_of(&path), "报告.pdf");
        assert_eq!(extension_of(&path).as_deref(), Some("pdf"));
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_workspace() {
        let root = workspace("relative");
        std::fs::create_dir_all(root.join("docs")).expect("创建目录");
        std::fs::write(root.join("docs").join("a.docx"), b"PK").expect("写测试文档");
        let path = read_pasted_document_path("docs/a.docx", &root).expect("相对路径应能解析");
        assert!(path.ends_with("a.docx"), "{path}");
        assert!(Path::new(&path).is_absolute(), "要落成绝对路径：{path}");
    }

    #[test]
    fn a_lone_document_path_shape_is_recognized_without_touching_the_disk() {
        // 粘贴识别用它决定值不值得等后续按键批次（拼完整串再比对剪贴板），因此与
        // `is_lone_image_path` 同口径：只认形状与扩展名，文件在不在都不看。
        assert!(is_lone_document_path(r"C:\下载\报告.pdf"));
        assert!(is_lone_document_path("  名单.DOCX "));
        assert!(!is_lone_document_path("a.png"), "图片扩展名不归文档");
        assert!(
            !is_lone_document_path("a.pdf\nb.pdf"),
            "夹了换行就不是单独一条路径"
        );
    }

    #[test]
    fn extra_text_and_non_documents_are_left_as_text() {
        let root = workspace("extra");
        let file = root.join("a.pdf");
        std::fs::write(&file, b"%PDF-1.7").expect("写测试文档");
        std::fs::write(root.join("a.png"), b"\x89PNG\r\n\x1a\n").expect("写测试图片");
        let path = file.to_string_lossy();
        for text in [
            format!("看一下这个 {path}"),
            format!("\"{path}\""),
            format!("{path}\n{path}"),
            root.join("a.png").to_string_lossy().into_owned(),
            root.join("missing.pdf").to_string_lossy().into_owned(),
            "note.md".to_string(),
        ] {
            assert!(
                read_pasted_document_path(&text, &root).is_none(),
                "不该当成文档：{text}"
            );
        }
        assert!(
            read_pasted_document_path("note.md", &root).is_none(),
            "文件不存在时不识别"
        );
    }
}
