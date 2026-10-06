//! `find` 工具：按名称或工作区相对路径查找文件与目录。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `find_files` / `_scan_file_names` /
//! `_list_search_entries`：只允许在工作区内查找，枚举走忽略规则（等价于后端
//! `--files --hidden --no-require-git`），目录由命中文件的祖先推导（空目录不返回），
//! 候选按「修改时间倒序 + 路径小写」排序，超过 max_results 时落盘完整结果。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::arguments::{limited_int, optional_bool, optional_text};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::search_common::{
    entry_mtime_ns, glob_matches, has_glob_magic, render_search_result, should_skip, walk_files,
    MAX_SEARCH_RESULTS, SEARCH_PARSE_LINE_CAP,
};

pub fn find_files(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let pattern = optional_text(arguments, "pattern");
    if pattern.is_empty() {
        return Err(ToolError::new("pattern 不能为空。"));
    }
    let raw_path = optional_text(arguments, "path");
    let raw_path = if raw_path.is_empty() { "." } else { &raw_path };
    let root = paths.safe_path(raw_path)?;
    if !paths.is_within(&root) {
        return Err(ToolError::new(format!(
            "find 只能在工作区内查找：{} 不在工作区内；请把 path 指向工作区内的目录或文件。",
            paths.relative(&root)
        )));
    }
    if !root.exists() {
        return Err(ToolError::new(format!(
            "路径不存在：{}",
            paths.relative(&root)
        )));
    }
    let kind = optional_text(arguments, "kind").to_lowercase();
    let kind = if kind.is_empty() {
        "all".to_string()
    } else {
        kind
    };
    if !matches!(kind.as_str(), "all" | "file" | "directory") {
        return Err(ToolError::new("kind 仅支持 all、file 或 directory。"));
    }
    let case_sensitive = optional_bool(arguments, "case_sensitive", false);
    let max_results = limited_int(arguments, "max_results", 50, 1, MAX_SEARCH_RESULTS);

    let items: Vec<String> = scan_file_names(paths, &pattern, &root, &kind, case_sensitive)
        .into_iter()
        .map(|(relative, is_dir)| {
            if is_dir {
                format!("{relative}/")
            } else {
                relative
            }
        })
        .collect();
    Ok(render_search_result(
        paths.root(),
        &items,
        max_results,
        "find_results",
        "未找到匹配结果。",
        "条",
        false,
    ))
}

fn scan_file_names(
    paths: &WorkspacePaths,
    pattern: &str,
    root: &Path,
    kind: &str,
    case_sensitive: bool,
) -> Vec<(String, bool)> {
    let needle = fold(pattern, case_sensitive);
    let mut results: Vec<(String, bool)> = Vec::new();
    for (entry_path, is_dir) in list_search_entries(paths, root, kind != "file") {
        if should_skip(&entry_path, paths) {
            continue;
        }
        if kind == "file" && is_dir {
            continue;
        }
        if kind == "directory" && !is_dir {
            continue;
        }
        let relative = paths.relative(&entry_path);
        let candidate = fold(&relative, case_sensitive);
        let name = fold(
            &entry_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            case_sensitive,
        );
        if has_glob_magic(pattern) {
            // glob 元字符出现后按文件名或相对路径匹配（`*` 表示匹配所有文件名）。
            let glob = fold(pattern, case_sensitive).replace('\\', "/");
            let relative_for_glob = candidate.replace('\\', "/");
            if !glob_matches(&glob, &name, false) && !glob_matches(&glob, &relative_for_glob, false)
            {
                continue;
            }
        } else if !candidate.contains(&needle) && !name.contains(&needle) {
            continue;
        }
        results.push((relative, is_dir));
    }
    results
}

fn fold(text: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        text.to_string()
    } else {
        text.to_lowercase()
    }
}

/// 搜索候选：文件 + 由文件祖先推导出的目录，按「修改时间倒序 + 路径小写」排序。
fn list_search_entries(
    paths: &WorkspacePaths,
    root: &Path,
    include_dirs: bool,
) -> Vec<(PathBuf, bool)> {
    if root.is_file() {
        return vec![(root.to_path_buf(), false)];
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for path in walk_files(root, paths) {
        files.push(path.clone());
        if include_dirs {
            let mut parent = path.parent().map(|parent| parent.to_path_buf());
            while let Some(current) = parent {
                if current == root || !current.starts_with(root) {
                    break;
                }
                if !dirs.contains(&current) {
                    dirs.push(current.clone());
                }
                parent = current.parent().map(|next| next.to_path_buf());
            }
        }
        if files.len() >= SEARCH_PARSE_LINE_CAP {
            break;
        }
    }
    let mut entries: Vec<(PathBuf, bool)> = files.into_iter().map(|path| (path, false)).collect();
    if include_dirs {
        entries.extend(dirs.into_iter().map(|path| (path, true)));
    }
    entries.sort_by(|left, right| {
        let left_mtime = entry_mtime_ns(&left.0);
        let right_mtime = entry_mtime_ns(&right.0);
        right_mtime.cmp(&left_mtime).then_with(|| {
            left.0
                .to_string_lossy()
                .to_lowercase()
                .cmp(&right.0.to_string_lossy().to_lowercase())
        })
    });
    entries
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
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-find-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src").join("deep")).expect("建目录");
        std::fs::write(root.join("main.py"), "").expect("写文件");
        std::fs::write(root.join("src").join("agent.py"), "").expect("写文件");
        std::fs::write(root.join("src").join("deep").join("helper.py"), "").expect("写文件");
        std::fs::write(root.join("src").join("readme.md"), "").expect("写文件");
        (WorkspacePaths::new(&root), root)
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn substring_match_hits_names_and_relative_paths() {
        let (paths, _root) = workspace("substring");
        let text = find_files(&paths, &args(json!({"pattern": "agent"}))).expect("查找");
        assert!(text.contains(&native("src/agent.py")), "{text}");
        assert!(!text.contains("main.py"), "{text}");
    }

    #[test]
    fn glob_pattern_matches_names_and_paths() {
        let (paths, _root) = workspace("glob");
        let text = find_files(&paths, &args(json!({"pattern": "*.py"}))).expect("glob 查找");
        assert!(text.contains("main.py"), "{text}");
        assert!(text.contains(&native("src/agent.py")), "{text}");
        assert!(text.contains(&native("src/deep/helper.py")), "{text}");
        assert!(!text.contains("readme.md"), "{text}");
    }

    #[test]
    fn kind_filters_and_case_sensitivity_work() {
        let (paths, _root) = workspace("kind");
        let files_only =
            find_files(&paths, &args(json!({"pattern": "src", "kind": "file"}))).expect("只查文件");
        // 目录在输出里带尾随分隔符，用它判断「目录没出现」（分隔符随平台）。
        assert!(
            !files_only.contains(&format!("src{}", std::path::MAIN_SEPARATOR)),
            "只查文件时目录不出现：{files_only}"
        );

        let dirs_only = find_files(
            &paths,
            &args(json!({"pattern": "src", "kind": "directory"})),
        )
        .expect("只查目录");
        assert!(
            dirs_only
                .lines()
                .all(|line| line.ends_with(std::path::MAIN_SEPARATOR)),
            "{dirs_only}"
        );

        let sensitive = find_files(
            &paths,
            &args(json!({"pattern": "AGENT", "case_sensitive": true})),
        )
        .expect("大小写敏感");
        assert_eq!(sensitive, "未找到匹配结果。");
        let insensitive =
            find_files(&paths, &args(json!({"pattern": "AGENT"}))).expect("忽略大小写");
        assert!(
            insensitive.contains(&native("src/agent.py")),
            "{insensitive}"
        );
    }

    #[test]
    fn errors_match_python_texts() {
        let (paths, _root) = workspace("errors");
        assert_eq!(
            find_files(&paths, &args(json!({}))).unwrap_err().message,
            "pattern 不能为空。"
        );
        assert_eq!(
            find_files(&paths, &args(json!({"pattern": "x", "kind": "symlink"})))
                .unwrap_err()
                .message,
            "kind 仅支持 all、file 或 directory。"
        );
        let outside = std::env::temp_dir().join("omnicrawl-tui-find-outside");
        let error = find_files(
            &paths,
            &args(json!({"pattern": "x", "path": outside.to_string_lossy()})),
        )
        .unwrap_err();
        assert!(
            error.message.starts_with("find 只能在工作区内查找："),
            "{}",
            error.message
        );
    }

    #[test]
    fn results_are_capped_and_spilled() {
        let root = std::env::temp_dir().join("omnicrawl-tui-find-cap");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建目录");
        for index in 0..8 {
            std::fs::write(root.join(format!("item{index}.py")), "").expect("写文件");
        }
        let paths = WorkspacePaths::new(&root);
        let text =
            find_files(&paths, &args(json!({"pattern": "*.py", "max_results": 3}))).expect("查找");
        assert!(
            text.contains("已达到 max_results（3），共 8 条。"),
            "{text}"
        );
        assert!(text.contains("完整结果已保存至："), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
