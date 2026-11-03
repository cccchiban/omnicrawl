//! 搜索类工具（list / find / grep）共用的遍历、glob 匹配与结果渲染。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 与 `omnicrawl/workspace/search_backend.py`：
//! 遍历读 `.gitignore` / `.ignore` / `.rgignore`（非 git 目录也读）、保留隐藏文件、
//! 剪枝受保护路径；glob 走 fnmatch 语义（含 `[seq]` / `[!seq]`）；超限时把完整结果写进
//! Agent 临时目录并在 footer 里给出恢复路径。Go 原生扩展（`native/ocsearch`）的注释也
//! 声明自己对齐 ripgrep 与 ignore crate，因此这里用同一族实现。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;
use regex::Regex;

use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;

pub const SEARCH_PARSE_LINE_CAP: usize = 100_000;
pub const GREP_MAX_LINE_LENGTH: usize = 2_000;
pub const MAX_SEARCH_RESULTS: i64 = 200;
/// 只在搜索里额外排除的目录（Agent 自身临时目录）：命令输出、搜索结果落盘文件。
pub const SEARCH_EXCLUDED_DIRS: [&str; 1] = [".omnicrawl"];
pub const AGENT_TEMP_DIRECTORY: &str = ".omnicrawl/.agent_tmp";
pub const COMMAND_OUTPUT_FILES_SUBDIR: &str = "files";

/// Agent 临时目录：按组件拼接，分隔符与平台一致（`pathlib.Path` 同样会把 `/` 归一）。
pub fn agent_temp_dir(root: &Path) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in AGENT_TEMP_DIRECTORY.split(['/', '\\']) {
        if !component.is_empty() {
            path.push(component);
        }
    }
    path
}

/// 遍历一个根下的普通文件（含隐藏文件，读忽略文件，剪枝受保护路径）。
pub fn walk_files(root: &Path, paths: &WorkspacePaths) -> Vec<PathBuf> {
    walk_files_with(root, paths, &WalkOptions::default())
}

/// 遍历命中的条目类型：`find` 需要能报目录与符号链接（fd 的 `-t d` / `-t l`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkKind {
    File,
    Dir,
    Symlink,
}

/// 遍历到的一个条目：路径 + 类型。符号链接按「链接本身」报告，不解析目标。
#[derive(Debug, Clone)]
pub struct WalkEntry {
    pub path: PathBuf,
    pub kind: WalkKind,
}

/// 带开关的完整遍历：返回文件、目录与符号链接（供 `find` 使用）。
///
/// 与 `walk_files_with` 的差别只有两点：目录也进结果；符号链接单独标记而不是被
/// `is_file` 过滤掉。忽略规则、受保护路径剪枝与 `WalkOptions` 全部一致。
pub fn walk_entries_with(
    root: &Path,
    paths: &WorkspacePaths,
    options: &WalkOptions,
) -> Vec<WalkEntry> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<WalkEntry> = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!options.include_hidden)
        .require_git(false)
        .git_ignore(options.use_ignore_files && options.use_vcs_ignore)
        .git_global(options.use_ignore_files && options.use_vcs_ignore)
        .git_exclude(options.use_ignore_files && options.use_vcs_ignore)
        .ignore(options.use_ignore_files)
        .parents(options.use_ignore_files)
        .follow_links(options.follow_links)
        .max_depth(options.max_depth)
        .max_filesize(options.max_filesize);
    if !options.globs.is_empty() || !options.iglobs.is_empty() {
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        for glob in &options.globs {
            let _ = overrides.add(glob);
        }
        overrides.case_insensitive(true).ok();
        for glob in &options.iglobs {
            let _ = overrides.add(glob);
        }
        if let Ok(overrides) = overrides.build() {
            builder.overrides(overrides);
        }
    }
    let root_owned = root.to_path_buf();
    builder.filter_entry(move |entry| {
        let path = entry.path();
        if path == root_owned {
            return true;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return true;
        };
        !WorkspacePaths::is_protected(path) && !SEARCH_EXCLUDED_DIRS.contains(&name)
    });
    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        // 根本身不算候选（fd 也只报根之下的条目）。
        if path == root {
            continue;
        }
        if should_skip(path, paths) {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if let Some(min_depth) = options.min_depth {
            let depth = path
                .strip_prefix(root)
                .map(|rest| rest.components().count())
                .unwrap_or(0);
            if depth < min_depth {
                continue;
            }
        }
        let kind = if file_type.is_symlink() {
            WalkKind::Symlink
        } else if file_type.is_dir() {
            WalkKind::Dir
        } else if file_type.is_file() {
            WalkKind::File
        } else {
            continue;
        };
        entries.push(WalkEntry {
            path: path.to_path_buf(),
            kind,
        });
    }
    entries
}

/// 遍历开关：默认值即 `walk_files` 的历史行为（等价 rg 的 `--hidden --no-require-git`）。
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// rg 的 `--hidden`：默认包含隐藏文件；`false` 时跳过 `.` 开头的条目。
    pub include_hidden: bool,
    /// rg 的 `--no-ignore`：`false` 时不读任何忽略文件（`.gitignore`/`.ignore`/全局）。
    pub use_ignore_files: bool,
    /// rg 的 `--no-ignore-vcs`：只关掉 `.gitignore` 与 git 全局忽略，保留 `.ignore`。
    pub use_vcs_ignore: bool,
    /// rg 的 `--follow`：跟随符号链接。
    pub follow_links: bool,
    /// rg 的 `--max-depth`：从根算起的最大目录层数。
    pub max_depth: Option<usize>,
    /// fd 的 `--min-depth`：只报从根算起层数达到该值的条目。
    pub min_depth: Option<usize>,
    /// rg 的 `--max-filesize`：超过该字节数的文件不搜。
    pub max_filesize: Option<u64>,
    /// rg 的 `-g/--glob`：`!pattern` 表示排除；`--iglob` 的同族条目大小写不敏感。
    pub globs: Vec<String>,
    pub iglobs: Vec<String>,
    /// rg 的 `-t/--type` 与 `-T/--type-not`：按内置类型表筛选。
    pub types: Vec<String>,
    pub type_not: Vec<String>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            include_hidden: true,
            use_ignore_files: true,
            use_vcs_ignore: true,
            follow_links: false,
            max_depth: None,
            min_depth: None,
            max_filesize: None,
            globs: Vec::new(),
            iglobs: Vec::new(),
            types: Vec::new(),
            type_not: Vec::new(),
        }
    }
}

/// 带开关的遍历。
pub fn walk_files_with(root: &Path, paths: &WorkspacePaths, options: &WalkOptions) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder
        // rg 的 `--hidden`：`hidden(false)` 表示保留隐藏文件。
        .hidden(!options.include_hidden)
        // rg 的 `--no-require-git`：非 git 目录也读 .gitignore/.ignore。
        .require_git(false)
        .git_ignore(options.use_ignore_files && options.use_vcs_ignore)
        .git_global(options.use_ignore_files && options.use_vcs_ignore)
        .git_exclude(options.use_ignore_files && options.use_vcs_ignore)
        .ignore(options.use_ignore_files)
        .parents(options.use_ignore_files)
        .follow_links(options.follow_links)
        .max_depth(options.max_depth)
        .max_filesize(options.max_filesize);
    if !options.globs.is_empty() || !options.iglobs.is_empty() {
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        for glob in &options.globs {
            let _ = overrides.add(glob);
        }
        overrides.case_insensitive(true).ok();
        for glob in &options.iglobs {
            let _ = overrides.add(glob);
        }
        if let Ok(overrides) = overrides.build() {
            builder.overrides(overrides);
        }
    }
    if !options.types.is_empty() || !options.type_not.is_empty() {
        let mut types = ignore::types::TypesBuilder::new();
        types.add_defaults();
        for name in &options.types {
            types.select(name);
        }
        for name in &options.type_not {
            types.negate(name);
        }
        if let Ok(types) = types.build() {
            builder.types(types);
        }
    }
    let root_owned = root.to_path_buf();
    builder.filter_entry(move |entry| {
        let path = entry.path();
        if path == root_owned {
            return true;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return true;
        };
        !WorkspacePaths::is_protected(path) && !SEARCH_EXCLUDED_DIRS.contains(&name)
    });
    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if should_skip(path, paths) {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files
}

/// 逐条过滤：受保护路径与临时目录不进结果。
pub fn should_skip(path: &Path, _paths: &WorkspacePaths) -> bool {
    if WorkspacePaths::is_protected(path) {
        return true;
    }
    path.components().any(|component| {
        SEARCH_EXCLUDED_DIRS.contains(&component.as_os_str().to_string_lossy().as_ref())
    })
}

/// 条目修改时间（纳秒）；条目并发消失或不可访问时按 Python 一致记 0。
pub fn entry_mtime_ns(path: &Path) -> u128 {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// 是否包含 glob 元字符（Python 的 `_has_glob_magic`）。
pub fn has_glob_magic(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// fnmatch 语义 → 正则；`case_insensitive` 对应 Python `_compile_glob` 的 `re.IGNORECASE`。
pub fn glob_regex(pattern: &str, case_insensitive: bool) -> Result<Regex, ToolError> {
    let mut out = String::from(r"\A(?:");
    let chars: Vec<char> = pattern.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        match chars[index] {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '[' => {
                let start = index;
                let mut cursor = index + 1;
                if cursor < chars.len() && chars[cursor] == '!' {
                    cursor += 1;
                }
                if cursor < chars.len() && chars[cursor] == ']' {
                    cursor += 1;
                }
                while cursor < chars.len() && chars[cursor] != ']' {
                    cursor += 1;
                }
                if cursor >= chars.len() {
                    // 没有闭合的 `]`：`[` 按字面处理，后续字符照常翻译。
                    out.push_str(&regex::escape("["));
                    index += 1;
                    continue;
                }
                let mut body = String::new();
                let mut position = start + 1;
                if chars[position] == '!' {
                    body.push('^');
                    position += 1;
                }
                if position < cursor && chars[position] == ']' {
                    body.push_str(r"\]");
                    position += 1;
                }
                while position < cursor {
                    let character = chars[position];
                    if character == '\\' {
                        body.push_str(r"\\");
                    } else if character == '^' && body.is_empty() {
                        body.push_str(r"\^");
                    } else {
                        body.push(character);
                    }
                    position += 1;
                }
                out.push('[');
                out.push_str(&body);
                out.push(']');
                index = cursor;
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
        index += 1;
    }
    out.push_str(r")\z");
    regex::RegexBuilder::new(&out)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|error| ToolError::new(format!("glob 模式非法：{error}")))
}

/// 单段 glob 匹配（大小写由调用方决定）。
pub fn glob_matches(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    match glob_regex(pattern, case_insensitive) {
        Ok(regex) => regex.is_match(text),
        Err(_) => false,
    }
}

/// 把完整搜索结果写入 Agent 临时目录；返回绝对路径，失败返回 None。
pub fn save_search_results(workspace_root: &Path, content: &str, prefix: &str) -> Option<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    let path = agent_temp_dir(workspace_root)
        .join(COMMAND_OUTPUT_FILES_SUBDIR)
        .join(format!(
            "{prefix}_{:08x}{stamp:08x}.txt",
            std::process::id()
        ));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// 超限 footer：完整结果落盘并给出恢复路径（deepseek 风格）。
pub fn truncation_footer(
    workspace_root: &Path,
    body_lines: &[String],
    max_results: i64,
    prefix: &str,
    label: &str,
    partial: bool,
) -> String {
    let total = body_lines.len();
    let saved = save_search_results(workspace_root, &body_lines.join("\n"), prefix);
    let saved_text = saved
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let hint = if partial {
        match &saved_text {
            Some(path) => format!("完整结果过大，已保存前 {total} 条至：{path}"),
            None => "结果过大且完整结果未保存，请缩小 pattern/path/include 范围。".to_string(),
        }
    } else {
        match &saved_text {
            Some(path) => format!("完整结果已保存至：{path}"),
            None => "完整结果未保存，请缩小 pattern/path/include 范围。".to_string(),
        }
    };
    format!("\n... 已达到 max_results（{max_results}），共 {total} {label}。{hint}")
}

/// 渲染搜索结果：不超限原样返回，超限保留前 max_results 条并落盘完整结果。
pub fn render_search_result(
    workspace_root: &Path,
    all_items: &[String],
    max_results: i64,
    prefix: &str,
    empty_text: &str,
    label: &str,
    partial: bool,
) -> String {
    if all_items.is_empty() {
        return empty_text.to_string();
    }
    if all_items.len() as i64 <= max_results {
        return all_items.join("\n");
    }
    let inline = all_items[..max_results as usize].join("\n");
    inline
        + &truncation_footer(
            workspace_root,
            all_items,
            max_results,
            prefix,
            label,
            partial,
        )
}

/// 读 UTF-8 文本（与 Python `read_text` 一致：非 UTF-8 给专用错误）。
pub fn read_utf8_text(path: &Path, display: &str) -> ToolOutcome {
    let bytes = std::fs::read(path)
        .map_err(|error| ToolError::new(format!("读取文件失败：{display}，{error}")))?;
    String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("文件不是 UTF-8 文本或包含二进制内容：{display}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching_follows_fnmatch_semantics() {
        assert!(glob_matches("*.py", "main.py", false));
        assert!(!glob_matches("*.py", "main.rs", false));
        assert!(glob_matches("main.?y", "main.py", false));
        assert!(glob_matches("[a-c]*.rs", "b_mod.rs", false));
        assert!(!glob_matches("[!a-c]*.rs", "b_mod.rs", false));
        assert!(glob_matches("[!a-c]*.rs", "z_mod.rs", false));
        assert!(
            glob_matches("*.PY", "main.py", true),
            "大小写不敏感时应当命中"
        );
        assert!(!glob_matches("*.PY", "main.py", false));
        // 没有闭合的 `]` 时 `[` 按字面处理。
        assert!(glob_matches("[abc", "[abc", false));
    }

    #[test]
    fn magic_detection_matches_python() {
        assert!(has_glob_magic("*.py"));
        assert!(has_glob_magic("a?b"));
        assert!(has_glob_magic("[ab]"));
        assert!(!has_glob_magic("main.py"));
    }

    #[test]
    fn search_results_spill_to_temp_directory_when_capped() {
        let root = std::env::temp_dir().join("omnicrawl-tui-search-common");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let items: Vec<String> = (0..5).map(|index| format!("第{index}条")).collect();

        let rendered = render_search_result(
            &root,
            &items,
            2,
            "grep_matches",
            "未找到匹配结果。",
            "条匹配",
            false,
        );
        assert!(rendered.starts_with("第0条\n第1条"), "{rendered}");
        assert!(
            rendered.contains("已达到 max_results（2），共 5 条匹配"),
            "{rendered}"
        );
        assert!(rendered.contains("完整结果已保存至："), "{rendered}");
        let saved_line = rendered.lines().last().expect("footer");
        let path = saved_line.rsplit("：").next().expect("保存路径");
        let content = std::fs::read_to_string(path).expect("完整结果应落盘");
        assert_eq!(content.lines().count(), 5);
        assert!(path.contains(".omnicrawl"), "{path}");

        assert_eq!(
            render_search_result(
                &root,
                &[],
                2,
                "grep_matches",
                "未找到匹配结果。",
                "条匹配",
                false
            ),
            "未找到匹配结果。"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
