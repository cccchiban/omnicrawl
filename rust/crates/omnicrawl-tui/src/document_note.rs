//! 粘贴文档的正文注脚：发送前把完整路径追加到提问正文末尾。
//!
//! 输入框里的占位符（`[ #1 .pdf ]`）在提交时展开成**文件名**（用户选定），正文里因此
//! 不再有可用的路径。文档没有附件通路（不像图片能随 `turn.submit` 发字节），模型只能
//! 按路径读文件——所以这里补一段注脚，与 `image_archive` 给剪贴板位图补路径是同一个理由。
//!
//! 界面上另在 user 消息下方逐行显示同一批路径（见 `ui::conversation::push_user`）。

/// 把文档路径追加成正文末尾的一段；没有文档时原样返回。
pub fn append_document_paths(text: String, documents: Vec<String>) -> String {
    if documents.is_empty() {
        return text;
    }
    let mut note = String::from("粘贴的文档：");
    for path in &documents {
        note.push('\n');
        note.push_str(path);
    }
    format!("{text}\n\n{note}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_go_under_one_heading_in_paste_order() {
        let text = append_document_paths(
            "总结一下".to_string(),
            vec![r"C:\a\报告.pdf".to_string(), r"C:\a\名单.docx".to_string()],
        );
        assert_eq!(
            text,
            "总结一下\n\n粘贴的文档：\nC:\\a\\报告.pdf\nC:\\a\\名单.docx"
        );
    }

    #[test]
    fn no_documents_leaves_the_prompt_untouched() {
        assert_eq!(
            append_document_paths("原样".to_string(), Vec::new()),
            "原样"
        );
    }
}
