//! `write_file` 工具：overwrite / append 写 UTF-8 文本。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `write_file`：父目录自动创建，
//! mode 空值按 overwrite 处理，只接受 overwrite / append / write 三种写法，
//! 成功文案给出工作区相对路径与字符数。

use std::io::Write;

use serde_json::{Map, Value};

use super::arguments::{optional_text, raw_text};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;

pub fn write_file(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let path = paths.safe_path(&optional_text(arguments, "path"))?;
    let content = raw_text(arguments, "content");
    let mode = optional_text(arguments, "mode").to_lowercase();
    let mode = if mode.is_empty() {
        "overwrite".to_string()
    } else {
        mode
    };
    if !matches!(mode.as_str(), "overwrite" | "append" | "write") {
        return Err(ToolError::new("mode 仅支持 overwrite 或 append。"));
    }
    let display = paths.relative(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ToolError::new(format!("写入文件失败：{display}，{error}")))?;
    }

    let action = if mode == "append" {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| ToolError::new(format!("写入文件失败：{display}，{error}")))?;
        file.write_all(content.as_bytes())
            .map_err(|error| ToolError::new(format!("写入文件失败：{display}，{error}")))?;
        "追加"
    } else {
        std::fs::write(&path, content.as_bytes())
            .map_err(|error| ToolError::new(format!("写入文件失败：{display}，{error}")))?;
        "写入"
    };
    Ok(format!(
        "已{action} {display}，字符数：{}。",
        content.chars().count()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(name: &str) -> (WorkspacePaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-write-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        (WorkspacePaths::new(&root), root)
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn overwrite_creates_parents_and_reports_char_count() {
        let (paths, root) = workspace("overwrite");
        let message = write_file(
            &paths,
            &args(json!({"path": "nested/dir/note.md", "content": "你好世界"})),
        )
        .expect("写入应成功");
        let expected = std::path::Path::new("nested").join("dir").join("note.md");
        assert_eq!(
            message,
            format!("已写入 {}，字符数：4。", expected.to_string_lossy())
        );
        assert_eq!(
            std::fs::read_to_string(root.join("nested/dir/note.md")).expect("文件应存在"),
            "你好世界"
        );
    }

    #[test]
    fn append_keeps_existing_content() {
        let (paths, root) = workspace("append");
        std::fs::write(root.join("a.txt"), "一").expect("预置文件");
        let message = write_file(
            &paths,
            &args(json!({"path": "a.txt", "content": "二", "mode": "append"})),
        )
        .expect("追加应成功");
        assert!(
            message.starts_with("已追加 a.txt，字符数：1。"),
            "{message}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "一二"
        );

        // 空 mode 按 overwrite 处理（与 Python 的 `or "overwrite"` 一致）。
        write_file(
            &paths,
            &args(json!({"path": "a.txt", "content": "覆盖", "mode": ""})),
        )
        .expect("空 mode 应回落 overwrite");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).expect("读回"),
            "覆盖"
        );
    }

    #[test]
    fn invalid_mode_and_protected_paths_are_rejected() {
        let (paths, _root) = workspace("reject");
        let error = write_file(
            &paths,
            &args(json!({"path": "a.txt", "content": "x", "mode": "patch"})),
        )
        .unwrap_err();
        assert_eq!(error.message, "mode 仅支持 overwrite 或 append。");

        let protected = write_file(
            &paths,
            &args(json!({"path": "config.toml", "content": "x"})),
        )
        .unwrap_err();
        assert!(protected.message.starts_with("拒绝访问受保护路径："));
    }
}
