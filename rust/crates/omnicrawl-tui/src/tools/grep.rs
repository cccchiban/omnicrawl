//! `grep` 工具：在工作区文本里按正则或精确子串搜索内容。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `grep` 系列与 `search_backend`：默认按
//! 正则（ripgrep 的 RE2 系语义，这里用同一族 `regex` crate，按字节匹配以贴近 rg 对
//! 非 UTF-8 行的处理）、`use_regex=false` 时按精确子串；枚举读 `.gitignore`/`.ignore`
//! 并剪枝受保护路径；支持上下文行、`count`、`files_with_matches` 与 include/exclude
//! 文件名 glob；结果超限时落盘完整结果并给出恢复路径。

use std::path::{Path, PathBuf};

use regex::bytes::RegexBuilder;
use serde_json::{Map, Value};

use super::arguments::{limited_int, optional_bool, optional_text, raw_text};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::search_common::{
    glob_regex, has_glob_magic, render_search_result, should_skip, truncation_footer, walk_files,
    GREP_MAX_LINE_LENGTH, MAX_SEARCH_RESULTS, SEARCH_PARSE_LINE_CAP,
};

/// 单条匹配或上下文行超过该长度时截断并标记。
const NON_UTF8_PLACEHOLDER: &str = "(line is not valid UTF-8)";

pub fn grep(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let pattern = raw_text(arguments, "pattern");
    if pattern.is_empty() {
        return Err(ToolError::new("pattern 不能为空。"));
    }
    let use_regex = optional_bool(arguments, "use_regex", true);
    let case_sensitive = optional_bool(arguments, "case_sensitive", false);

    let raw_path = optional_text(arguments, "path");
    let raw_path = if raw_path.is_empty() { "." } else { &raw_path };
    let roots = resolve_grep_roots(paths, raw_path)?;
    for root in &roots {
        if is_forbidden_content_search_root(root) {
            return Err(ToolError::new(
                "用户主目录或文件系统根目录本身不支持内容关键词搜索；请把 path 指向其下的具体项目子目录。",
            ));
        }
        if !root.exists() {
            return Err(ToolError::new(format!(
                "路径不存在：{}",
                paths.relative(root)
            )));
        }
    }

    let max_results = limited_int(arguments, "max_results", 50, 1, MAX_SEARCH_RESULTS);
    let context_lines = limited_int(arguments, "context_lines", 0, 0, 50);
    let count_only = optional_bool(arguments, "count", false);
    let files_only = optional_bool(arguments, "files_with_matches", false);
    let include_glob = optional_text(arguments, "include");
    let exclude_glob = optional_text(arguments, "exclude");

    let matcher = build_matcher(&pattern, use_regex, case_sensitive)?;
    let filters = Filters {
        include: include_glob,
        exclude: exclude_glob,
    };

    if count_only {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for file in candidate_files(paths, &roots, &filters) {
            let count = count_matches(&file, &matcher);
            if count > 0 {
                counts.push((paths.relative(&file), count));
                if counts.len() >= SEARCH_PARSE_LINE_CAP {
                    break;
                }
            }
        }
        counts.sort_by_key(|(relative, _)| relative.to_lowercase());
        let items: Vec<String> = counts
            .into_iter()
            .map(|(relative, count)| format!("{relative}: {count}"))
            .collect();
        return Ok(render_search_result(
            paths.root(),
            &items,
            max_results,
            "grep_counts",
            "未找到匹配结果。",
            "个文件",
            false,
        ));
    }

    if files_only {
        let mut matched: Vec<String> = Vec::new();
        for file in candidate_files(paths, &roots, &filters) {
            let hit = count_matches(&file, &matcher) > 0;
            if hit {
                matched.push(paths.relative(&file));
                if matched.len() >= SEARCH_PARSE_LINE_CAP {
                    break;
                }
            }
        }
        matched.sort_by_key(|relative| relative.to_lowercase());
        return Ok(render_search_result(
            paths.root(),
            &matched,
            max_results,
            "grep_files",
            "未找到匹配结果。",
            "个文件",
            false,
        ));
    }

    let mut matches: Vec<(String, usize, String)> = Vec::new();
    let mut parse_capped = false;
    for file in candidate_files(paths, &roots, &filters) {
        let relative = paths.relative(&file);
        for (line_no, text) in matching_lines(&file, &matcher) {
            matches.push((relative.clone(), line_no, text));
            if matches.len() >= SEARCH_PARSE_LINE_CAP {
                parse_capped = true;
                break;
            }
        }
        if parse_capped {
            break;
        }
    }
    matches.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.1.cmp(&right.1))
    });

    if matches.len() as i64 > max_results {
        let inline = format_matches(paths, &matches[..max_results as usize], context_lines);
        let spill: Vec<String> = matches
            .iter()
            .map(|(relative, line_no, text)| format!("{relative}:{line_no}: {text}"))
            .collect();
        return Ok(inline
            + &truncation_footer(
                paths.root(),
                &spill,
                max_results,
                "grep_matches",
                "条匹配",
                parse_capped,
            ));
    }
    Ok(format_matches(paths, &matches, context_lines))
}

struct Filters {
    include: String,
    exclude: String,
}

impl Filters {
    /// 与 Python `_passes_grep_filters` 一致：include/exclude 只按文件名匹配且大小写不敏感。
    fn accepts(&self, paths: &WorkspacePaths, file: &Path) -> bool {
        if should_skip(file, paths) {
            return false;
        }
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if !self.include.is_empty()
            && !glob_regex(&self.include, true).is_ok_and(|re| re.is_match(&name))
        {
            return false;
        }
        if !self.exclude.is_empty()
            && glob_regex(&self.exclude, true).is_ok_and(|re| re.is_match(&name))
        {
            return false;
        }
        true
    }
}

/// 构建匹配器：`use_regex=false` 时把模式转义成精确子串，与 rg 的 `--fixed-strings` 同义。
fn build_matcher(
    pattern: &str,
    use_regex: bool,
    case_sensitive: bool,
) -> Result<regex::bytes::Regex, ToolError> {
    let expression = if use_regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };
    let regex = RegexBuilder::new(&expression)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|error| {
            let hint = if use_regex {
                "。可设置 use_regex=false 按精确子串匹配。"
            } else {
                "。"
            };
            ToolError::new(format!("无效的正则表达式：{error}{hint}"))
        })?;
    Ok(regex)
}

/// 命中文件列表：逐 root 枚举（忽略规则 + 保护路径）并应用 include/exclude。
fn candidate_files(paths: &WorkspacePaths, roots: &[PathBuf], filters: &Filters) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for root in roots {
        if root.is_file() {
            if filters.accepts(paths, root) {
                files.push(root.clone());
            }
            continue;
        }
        for file in walk_files(root, paths) {
            if filters.accepts(paths, &file) {
                files.push(file);
            }
        }
    }
    files
}

fn count_matches(file: &Path, regex: &regex::bytes::Regex) -> usize {
    read_lines(file)
        .iter()
        .filter(|line| regex.is_match(line))
        .count()
}

fn matching_lines(file: &Path, matcher: &regex::bytes::Regex) -> Vec<(usize, String)> {
    let mut matched = Vec::new();
    for (index, line) in read_lines(file).iter().enumerate() {
        if matcher.is_match(line) {
            matched.push((index + 1, display_line(line)));
        }
    }
    matched
}

/// 按行读取文件字节；含 NUL 的文件按二进制跳过（与 rg 的默认行为一致）。
fn read_lines(file: &Path) -> Vec<Vec<u8>> {
    let Ok(bytes) = std::fs::read(file) else {
        return Vec::new();
    };
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Vec::new();
    }
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for raw in bytes.split(|byte| *byte == b'\n') {
        let mut line = raw;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        lines.push(line.to_vec());
    }
    // 文件以换行结尾时 split 会多出一个空段，与逐行读取的语义不同。
    if lines.len() > 1 && bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    lines
}

/// 单行展示文本：非 UTF-8 行给占位符（与 rg 的 `--json` 行为一致），超长行截断。
fn display_line(line: &[u8]) -> String {
    match std::str::from_utf8(line) {
        Ok(text) => cap_match_line(text),
        Err(_) => NON_UTF8_PLACEHOLDER.to_string(),
    }
}

fn cap_match_line(text: &str) -> String {
    if text.chars().count() <= GREP_MAX_LINE_LENGTH {
        return text.to_string();
    }
    let head: String = text.chars().take(GREP_MAX_LINE_LENGTH).collect();
    format!("{head} (line truncated)")
}

/// 匹配行 `path:line: text`；上下文行 `path-line- text`（与 grep -n -C 一致，区间去重）。
fn format_matches(
    paths: &WorkspacePaths,
    matches: &[(String, usize, String)],
    context_lines: i64,
) -> String {
    if matches.is_empty() {
        return "未找到匹配结果。".to_string();
    }
    if context_lines <= 0 {
        return matches
            .iter()
            .map(|(relative, line_no, text)| format!("{relative}:{line_no}: {text}"))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut file_order: Vec<String> = Vec::new();
    let mut by_file: Vec<(String, Vec<usize>)> = Vec::new();
    for (relative, line_no, _) in matches {
        match by_file.iter_mut().find(|(name, _)| name == relative) {
            Some((_, lines)) => lines.push(*line_no),
            None => {
                file_order.push(relative.clone());
                by_file.push((relative.clone(), vec![*line_no]));
            }
        }
    }

    let mut out: Vec<String> = Vec::new();
    for relative in file_order {
        let Some((_, line_numbers)) = by_file.iter().find(|(name, _)| name == &relative) else {
            continue;
        };
        let file_path = paths.root().join(&relative);
        let Ok(text) = super::search_common::read_utf8_text(&file_path, &relative) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let mut covered: Vec<usize> = Vec::new();
        for line_no in line_numbers {
            let start = line_no.saturating_sub(1 + context_lines as usize);
            let end = (line_no + context_lines as usize).min(lines.len());
            for (offset, text) in lines[start..end].iter().enumerate() {
                let current = start + offset + 1;
                if covered.contains(&current) {
                    continue;
                }
                covered.push(current);
                let line_text = cap_match_line(text);
                if current == *line_no {
                    out.push(format!("{relative}:{current}: {line_text}"));
                } else {
                    out.push(format!("{relative}-{current}- {line_text}"));
                }
            }
        }
    }
    out.join("\n")
}

/// grep 的 path：支持文件、目录以及绝对/相对 glob；glob 展开后每个目标再走 safe_path。
fn resolve_grep_roots(paths: &WorkspacePaths, raw_path: &str) -> Result<Vec<PathBuf>, ToolError> {
    let candidate = {
        let path = Path::new(raw_path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            paths.root().join(path)
        }
    };
    if !has_glob_magic(&candidate.to_string_lossy()) {
        return Ok(vec![paths.safe_path(raw_path)?]);
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    let mut matches = expand_glob(paths, &candidate);
    matches.sort_by_key(|path| path.to_string_lossy().to_lowercase());
    for matched in matches {
        if should_skip(&matched, paths) {
            continue;
        }
        let path = paths.safe_path(&matched.to_string_lossy())?;
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    if roots.is_empty() {
        return Err(ToolError::new(format!(
            "路径不存在或 glob 未匹配：{raw_path}"
        )));
    }
    Ok(roots)
}

/// 展开文件系统 glob（`*` 不跨目录分隔符，`**` 跨层级）。
fn expand_glob(paths: &WorkspacePaths, candidate: &Path) -> Vec<PathBuf> {
    let Some(prefix) = static_prefix(candidate) else {
        return Vec::new();
    };
    let regex = match glob_path_regex(&candidate.to_string_lossy()) {
        Ok(regex) => regex,
        Err(_) => return Vec::new(),
    };
    let mut matched: Vec<PathBuf> = Vec::new();
    let mut stack = vec![prefix];
    while let Some(current) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in reader.flatten() {
            let path = entry.path();
            let is_dir = path.is_dir();
            let text = path.to_string_lossy().replace('\\', "/");
            if regex.is_match(&text) && !should_skip(&path, paths) {
                matched.push(path.clone());
            }
            if is_dir && !path.is_symlink() {
                stack.push(path);
            }
        }
    }
    matched
}

/// glob 的最长静态前缀（不含元字符的祖先目录）。
fn static_prefix(candidate: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    let mut found = false;
    for component in candidate.components() {
        let text = component.as_os_str().to_string_lossy().to_string();
        if has_glob_magic(&text) {
            break;
        }
        prefix.push(component.as_os_str());
        found = true;
    }
    if found {
        Some(prefix)
    } else {
        None
    }
}

/// 路径 glob → 正则：`*` 不跨分隔符，`**` 跨层级，`?` 单字符。
fn glob_path_regex(pattern: &str) -> Result<regex::Regex, ToolError> {
    let normalized = pattern.replace('\\', "/");
    let mut out = String::from(r"\A(?:");
    let chars: Vec<char> = normalized.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        match chars[index] {
            '*' => {
                if chars.get(index + 1) == Some(&'*') {
                    out.push_str(".*");
                    index += 1;
                } else {
                    out.push_str("[^/]*");
                }
            }
            '?' => out.push_str("[^/]"),
            '[' => {
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
                    out.push_str(&regex::escape("["));
                } else {
                    let mut body = String::new();
                    let mut position = index + 1;
                    if chars[position] == '!' {
                        body.push('^');
                        position += 1;
                    }
                    while position < cursor {
                        let character = chars[position];
                        if character == '\\' {
                            body.push_str(r"\\");
                        } else {
                            body.push(character);
                        }
                        position += 1;
                    }
                    out.push('[');
                    out.push_str(&body);
                    out.push(']');
                }
                index = cursor;
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
        index += 1;
    }
    out.push_str(r")\z");
    regex::Regex::new(&out).map_err(|error| ToolError::new(format!("glob 模式非法：{error}")))
}

/// 用户主目录或文件系统根目录本身不允许内容关键词搜索。
fn is_forbidden_content_search_root(path: &Path) -> bool {
    let resolved = super::paths::resolve_lenient(path);
    let has_no_parent = resolved
        .parent()
        .map(|parent| parent.as_os_str().is_empty())
        .unwrap_or(true);
    if has_no_parent {
        return true;
    }
    if resolved.parent() == Some(resolved.as_path()) {
        return true;
    }
    for key in ["USERPROFILE", "HOME"] {
        if let Ok(home) = std::env::var(key) {
            if !home.trim().is_empty()
                && resolved == super::paths::resolve_lenient(Path::new(&home))
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 期望值里的相对路径按平台分隔符拼接（Windows 上是反斜杠，与 Python 一致）。
    fn native(path: &str) -> String {
        path.replace('/', std::path::MAIN_SEPARATOR_STR)
    }

    fn workspace(name: &str) -> (WorkspacePaths, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-grep-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("建目录");
        std::fs::write(
            root.join("src").join("agent.py"),
            "class Agent:\n    def run(self):\n        return self.tools\n",
        )
        .expect("写文件");
        std::fs::write(
            root.join("src").join("helper.py"),
            "def helper():\n    return 1\n",
        )
        .expect("写文件");
        std::fs::write(root.join("notes.md"), "Agent 说明\n无关内容\n").expect("写文件");
        (WorkspacePaths::new(&root), root)
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn default_mode_reports_path_line_text() {
        let (paths, _root) = workspace("default");
        let text = grep(&paths, &args(json!({"pattern": "Agent"}))).expect("搜索");
        assert!(
            text.contains(&format!("{}:1: class Agent:", native("src/agent.py"))),
            "{text}"
        );
        assert!(text.contains("notes.md:1: Agent 说明"), "{text}");
        assert!(!text.contains("helper.py"), "{text}");
    }

    #[test]
    fn use_regex_false_matches_literal_text() {
        let (paths, _root) = workspace("literal");
        let text = grep(
            &paths,
            &args(json!({"pattern": "self.tools", "use_regex": false})),
        )
        .expect("精确子串匹配");
        assert_eq!(
            text,
            format!("{}:3:         return self.tools", native("src/agent.py"))
        );

        let literal_dot = grep(
            &paths,
            &args(json!({"pattern": "self.tools", "use_regex": true})),
        )
        .expect("正则匹配");
        assert_eq!(
            literal_dot,
            format!("{}:3:         return self.tools", native("src/agent.py"))
        );
    }

    #[test]
    fn count_and_files_modes_match_python_shape() {
        let (paths, _root) = workspace("modes");
        let counts = grep(&paths, &args(json!({"pattern": "Agent", "count": true}))).expect("计数");
        let lines: Vec<&str> = counts.lines().collect();
        assert!(lines.contains(&"notes.md: 1"), "{counts}");
        assert!(
            lines.contains(&format!("{}: 1", native("src/agent.py")).as_str()),
            "{counts}"
        );

        let files = grep(
            &paths,
            &args(json!({"pattern": "Agent", "files_with_matches": true})),
        )
        .expect("只列文件");
        assert_eq!(files, format!("notes.md\n{}", native("src/agent.py")));
    }

    #[test]
    fn context_lines_render_with_dash_markers() {
        let (paths, _root) = workspace("context");
        let text = grep(
            &paths,
            &args(json!({"pattern": "def run", "context_lines": 1})),
        )
        .expect("上下文搜索");
        assert!(
            text.contains(&format!("{}:2:     def run(self):", native("src/agent.py"))),
            "{text}"
        );
        assert!(
            text.contains(&format!("{}-1- class Agent:", native("src/agent.py"))),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "{}-3-         return self.tools",
                native("src/agent.py")
            )),
            "{text}"
        );
    }

    #[test]
    fn include_exclude_filter_by_basename() {
        let (paths, _root) = workspace("filters");
        let only_py = grep(
            &paths,
            &args(json!({"pattern": "Agent", "include": "*.py"})),
        )
        .expect("include 过滤");
        assert!(only_py.contains(&native("src/agent.py")), "{only_py}");
        assert!(!only_py.contains("notes.md"), "{only_py}");

        let excluded = grep(
            &paths,
            &args(json!({"pattern": "Agent", "exclude": "*.md"})),
        )
        .expect("exclude 过滤");
        assert!(!excluded.contains("notes.md"), "{excluded}");
    }

    #[test]
    fn invalid_regex_hints_fixed_string_mode() {
        let (paths, _root) = workspace("regex-error");
        let error = grep(&paths, &args(json!({"pattern": "(unclosed"}))).unwrap_err();
        assert!(
            error.message.starts_with("无效的正则表达式："),
            "{}",
            error.message
        );
        assert!(
            error
                .message
                .contains("可设置 use_regex=false 按精确子串匹配。"),
            "{}",
            error.message
        );
    }

    #[test]
    fn missing_pattern_and_paths_report_python_texts() {
        let (paths, _root) = workspace("errors");
        assert_eq!(
            grep(&paths, &args(json!({}))).unwrap_err().message,
            "pattern 不能为空。"
        );
        assert_eq!(
            grep(&paths, &args(json!({"pattern": "x", "path": "nope"})))
                .unwrap_err()
                .message,
            "路径不存在：nope"
        );
        assert_eq!(
            grep(&paths, &args(json!({"pattern": "x", "path": "*.nope"})))
                .unwrap_err()
                .message,
            "路径不存在或 glob 未匹配：*.nope"
        );
    }

    #[test]
    fn glob_path_limits_to_matching_roots() {
        let (paths, _root) = workspace("glob-path");
        let text = grep(&paths, &args(json!({"pattern": "def", "path": "src/*.py"})))
            .expect("glob 根搜索");
        assert!(text.contains(&native("src/agent.py")), "{text}");
        assert!(text.contains(&native("src/helper.py")), "{text}");
        assert!(!text.contains("notes.md"), "{text}");
    }

    #[test]
    fn binary_files_are_skipped() {
        let (paths, root) = workspace("binary");
        std::fs::write(root.join("blob.bin"), b"Agent\x00hidden").expect("写二进制文件");
        let text = grep(&paths, &args(json!({"pattern": "hidden"}))).expect("搜索");
        assert_eq!(text, "未找到匹配结果。");
    }

    #[test]
    fn long_lines_are_capped_and_results_spill() {
        let (paths, root) = workspace("cap");
        let long = format!("{}Agent", "x".repeat(GREP_MAX_LINE_LENGTH + 50));
        std::fs::write(root.join("long.txt"), long).expect("写长行文件");
        let text = grep(&paths, &args(json!({"pattern": "Agent"}))).expect("搜索");
        assert!(
            text.contains("(line truncated)"),
            "{}",
            &text[..text.len().min(60)]
        );

        std::fs::write(
            root.join("many.txt"),
            (0..5).map(|i| format!("Agent {i}\n")).collect::<String>(),
        )
        .expect("写文件");
        let capped = grep(
            &paths,
            &args(json!({"pattern": "Agent", "max_results": 2, "include": "many.txt"})),
        )
        .expect("搜索");
        assert!(
            capped.contains("已达到 max_results（2），共 5 条匹配。"),
            "{capped}"
        );
        assert!(capped.contains("完整结果已保存至："), "{capped}");
    }
}
