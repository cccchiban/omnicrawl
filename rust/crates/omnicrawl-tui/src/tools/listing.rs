//! `list` 工具：列出工作区目录（可递归），跳过受保护路径与 Agent 临时目录。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `list_files`：非递归只列直接子项，
//! 递归列全部后代；结果按小写绝对路径排序；超过 500 项追加截断提示；空目录返回
//! 「目录为空。」。这里刻意用普通目录遍历（与 Python 的 `iterdir` / `rglob` 一致），
//! 不套 .gitignore 规则——`list` 的契约是「看目录里有什么」，不是「搜索候选文件」。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::arguments::{optional_bool, optional_text};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::search_common::should_skip;

pub const MAX_LIST_ENTRIES: usize = 500;

pub fn list_files(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let raw_path = optional_text(arguments, "path");
    let raw_path = if raw_path.is_empty() { "." } else { &raw_path };
    let path = paths.safe_path(raw_path)?;
    let recursive = optional_bool(arguments, "recursive", false);
    if !path.exists() {
        return Err(ToolError::new(format!(
            "路径不存在：{}",
            paths.relative(&path)
        )));
    }
    if path.is_file() {
        return Ok(paths.relative(&path));
    }

    let mut collected: Vec<(PathBuf, bool)> = Vec::new();
    collect_entries(&path, recursive, paths, &mut collected);
    collected.sort_by_key(|(entry, _)| entry.to_string_lossy().to_lowercase());

    let mut entries: Vec<String> = Vec::new();
    for (entry, is_dir) in collected {
        if should_skip(&entry, paths) {
            continue;
        }
        entries.push(format!(
            "{}{}",
            paths.relative(&entry),
            if is_dir { "/" } else { "" }
        ));
        if entries.len() >= MAX_LIST_ENTRIES {
            entries.push(format!("... 已截断，结果超过 {MAX_LIST_ENTRIES} 项。"));
            break;
        }
    }
    if entries.is_empty() {
        return Ok("目录为空。".to_string());
    }
    Ok(entries.join("\n"))
}

fn collect_entries(
    root: &Path,
    recursive: bool,
    _paths: &WorkspacePaths,
    collected: &mut Vec<(PathBuf, bool)>,
) {
    let Ok(reader) = std::fs::read_dir(root) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        let is_dir = path.is_dir();
        collected.push((path.clone(), is_dir));
        if !recursive || !is_dir {
            continue;
        }
        // 递归跳过受保护目录（其后代同样受保护）与目录符号链接（与 Python 的 rglob 一致）。
        if WorkspacePaths::is_protected(&path) || path.is_symlink() {
            continue;
        }
        collect_entries(&path, recursive, _paths, collected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 期望值里的相对路径按平台分隔符拼接。
    fn native(path: &str) -> String {
        path.replace('/', std::path::MAIN_SEPARATOR_STR)
    }

    fn workspace(name: &str) -> (WorkspacePaths, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-list-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("建目录");
        std::fs::create_dir_all(root.join(".git")).expect("建受保护目录");
        std::fs::write(root.join("README.md"), "文档").expect("写文件");
        std::fs::write(root.join("src").join("main.rs"), "fn main() {}").expect("写文件");
        std::fs::write(root.join(".git").join("HEAD"), "ref").expect("写受保护文件");
        (WorkspacePaths::new(&root), root)
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn non_recursive_lists_direct_children_with_dir_suffix() {
        let (paths, _root) = workspace("flat");
        let text = list_files(&paths, &args(json!({}))).expect("列目录");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.contains(&"README.md"), "{text}");
        assert!(lines.contains(&"src/"), "{text}");
        assert!(
            !lines.iter().any(|line| line.contains(".git")),
            "受保护目录不出现：{text}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("main.rs")),
            "非递归不列子目录内容：{text}"
        );
    }

    #[test]
    fn recursive_lists_descendants_and_skips_protected() {
        let (paths, _root) = workspace("deep");
        let text = list_files(&paths, &args(json!({"recursive": true}))).expect("递归列目录");
        assert!(text.contains(&native("src/main.rs")), "{text}");
        assert!(!text.contains(".git"), "{text}");
    }

    #[test]
    fn file_path_and_missing_path_are_handled() {
        let (paths, root) = workspace("file");
        assert_eq!(
            list_files(&paths, &args(json!({"path": "README.md"}))).expect("列文件"),
            "README.md"
        );
        let error = list_files(&paths, &args(json!({"path": "nope"}))).unwrap_err();
        assert_eq!(error.message, "路径不存在：nope");

        std::fs::create_dir_all(root.join("empty")).expect("建空目录");
        assert_eq!(
            list_files(&paths, &args(json!({"path": "empty"}))).expect("列空目录"),
            "目录为空。"
        );
    }

    #[test]
    fn truncation_kicks_in_at_the_entry_cap() {
        let root = std::env::temp_dir().join("omnicrawl-tui-list-cap");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建目录");
        for index in 0..MAX_LIST_ENTRIES + 5 {
            std::fs::write(root.join(format!("f{index:04}.txt")), "").expect("写文件");
        }
        let paths = WorkspacePaths::new(&root);
        let text = list_files(&paths, &args(json!({}))).expect("列目录");
        assert!(
            text.ends_with("... 已截断，结果超过 500 项。"),
            "{}",
            &text[text.len().saturating_sub(60)..]
        );
        assert_eq!(text.lines().count(), MAX_LIST_ENTRIES + 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
