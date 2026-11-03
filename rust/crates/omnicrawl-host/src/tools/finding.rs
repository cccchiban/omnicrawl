//! `find` 工具：按名称、glob 或正则查找文件与目录。
//!
//! 相对路径按工作区解析，绝对路径可指向工作区外（只读查找，不写任何文件），枚举走
//! 忽略规则。参数面对齐 fd（`doc/fd.1`）：
//!
//! | 组 | 参数 |
//! |---|---|
//! | 匹配 | `glob` / `regex` / `fixed_strings` / `exact` / `ignore_case` / `case_sensitive` / `smart_case` / `full_path` |
//! | 类型与扩展名 | `kind` / `type` / `extension` |
//! | 过滤 | `exclude` / `glob` · `iglob` · `type_not` / `hidden` / `no_ignore` / `no_ignore_vcs` / `follow_links` |
//! | 深度 | `max_depth` / `min_depth` / `exact_depth` |
//! | 元数据 | `size` / `changed_within` / `changed_before` / `empty` |
//! | 输出 | `absolute_path` / `show_details` / `max_results` |
//!
//! 与 fd 不同的默认值（与旧 `find` 保持一致，改了会让既有调用语义漂移）：
//! ① 默认含隐藏文件（等价 fd 的 `-H`）；② 默认匹配子串而非正则（`regex=true` 取回
//! fd 默认）；③ 匹配文件名**或**相对路径（fd 默认只比文件名，`full_path=true` 才比路径）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::RegexBuilder;
use serde_json::{Map, Value};

use super::arguments::{limited_int, optional_bool, optional_text, text_list};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::search_common::{
    glob_matches, has_glob_magic, render_search_result, walk_entries_with, WalkEntry, WalkKind,
    WalkOptions, MAX_SEARCH_RESULTS, SEARCH_PARSE_LINE_CAP,
};

pub fn find_files(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let pattern = optional_text(arguments, "pattern");
    let empty_ok = optional_bool(arguments, "match_all_when_empty", false);
    if pattern.is_empty() && !empty_ok {
        return Err(ToolError::new("pattern 不能为空。"));
    }

    let raw_path = optional_text(arguments, "path");
    let raw_path = if raw_path.is_empty() { "." } else { &raw_path };
    // 只读查找不设工作区边界：相对路径按工作区解析，绝对路径可以是本机任意目录。
    // 受保护路径（`.env`、`config.toml` 等）仍由 `safe_path` / `should_skip` 拦下。
    let root = paths.safe_path(raw_path)?;
    if !root.exists() {
        return Err(ToolError::new(format!(
            "路径不存在：{}",
            paths.relative(&root)
        )));
    }

    let options = Options::parse(arguments, &pattern)?;
    let matcher = Matcher::build(&options)?;
    let walk = walk_options(arguments);
    let entries = collect_entries(paths, &root, &walk);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    let mut matched: Vec<Candidate> = Vec::new();
    for entry in entries {
        let kind = match entry.kind {
            WalkKind::File => EntryKind::File,
            WalkKind::Dir => EntryKind::Dir,
            WalkKind::Symlink => EntryKind::Symlink,
        };
        if !options.type_filter.accepts(kind) {
            continue;
        }
        if !options.extension.is_empty() && !has_extension(&entry.path, &options.extension) {
            continue;
        }
        let name = entry
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let relative = paths.relative(&entry.path);
        if !matcher.matches(&name, &relative, options.full_path) {
            continue;
        }
        if options.excluded.iter().any(|glob| {
            glob_matches(glob, &name, true) || glob_matches(glob, &relative.replace('\\', "/"), true)
        }) {
            continue;
        }
        if options.empty && !is_empty_entry(&entry) {
            continue;
        }
        let metadata = std::fs::metadata(&entry.path).ok();
        if let Some(size) = options.size {
            let actual = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
            if !size.matches(actual) {
                continue;
            }
        }
        if options.changed_within.is_some() || options.changed_before.is_some() {
            let modified = metadata
                .as_ref()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());
            let Some(modified) = modified else {
                continue;
            };
            if let Some(within) = options.changed_within {
                if now.saturating_sub(modified) > within {
                    continue;
                }
            }
            if let Some(before) = options.changed_before {
                if now.saturating_sub(modified) < before {
                    continue;
                }
            }
        }
        matched.push(Candidate {
            path: entry.path.clone(),
            relative,
            kind,
            mtime: metadata
                .as_ref()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
        });
        if matched.len() >= SEARCH_PARSE_LINE_CAP {
            break;
        }
    }

    matched.sort_by(|left, right| {
        right
            .mtime
            .cmp(&left.mtime)
            .then_with(|| left.relative.to_lowercase().cmp(&right.relative.to_lowercase()))
    });

    let items: Vec<String> = matched
        .iter()
        .map(|candidate| render_candidate(paths, candidate, &options))
        .collect();
    Ok(render_search_result(
        paths.root(),
        &items,
        options.max_results,
        "find_results",
        "未找到匹配结果。",
        "条",
        false,
    ))
}

/// 候选条目类型（fd 的 `-t`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Empty,
    Executable,
}

/// `-t/--type` 的累积筛选。
#[derive(Debug, Default)]
struct TypeFilter {
    kinds: Vec<EntryKind>,
    exclude: Vec<EntryKind>,
}

impl TypeFilter {
    fn accepts(&self, kind: EntryKind) -> bool {
        if self.exclude.contains(&kind) {
            return false;
        }
        let wants_empty = self.kinds.contains(&EntryKind::Empty);
        let wants_executable = self.kinds.contains(&EntryKind::Executable);
        let concrete: Vec<EntryKind> = self
            .kinds
            .iter()
            .copied()
            .filter(|wanted| !matches!(wanted, EntryKind::Empty | EntryKind::Executable))
            .collect();

        // `-t e` 与 `-t x` 是「附加条件」：没有并列的具体类型时，`e` 只看文件、
        // `x` 只看文件；与 `-t f`/`-t d` 并列时按并列类型收窄。
        if !concrete.is_empty() {
            return concrete.contains(&kind);
        }
        if wants_empty && wants_executable {
            return kind == EntryKind::File;
        }
        if wants_empty {
            return kind == EntryKind::File || kind == EntryKind::Dir;
        }
        if wants_executable {
            return kind == EntryKind::File;
        }
        self.kinds.is_empty()
    }
}

/// `--size` 的 `<+->NUM[UNIT]` 约束。
#[derive(Debug, Clone, Copy)]
struct SizeFilter {
    comparison: Comparison,
    bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    AtLeast,
    AtMost,
    Exactly,
}

impl SizeFilter {
    fn matches(&self, actual: u64) -> bool {
        match self.comparison {
            Comparison::AtLeast => actual >= self.bytes,
            Comparison::AtMost => actual <= self.bytes,
            Comparison::Exactly => actual == self.bytes,
        }
    }
}

/// 一次查找的全部开关。
struct Options {
    pattern: String,
    mode: MatchMode,
    case_mode: CaseMode,
    full_path: bool,
    type_filter: TypeFilter,
    extension: Vec<String>,
    excluded: Vec<String>,
    empty: bool,
    size: Option<SizeFilter>,
    changed_within: Option<u64>,
    changed_before: Option<u64>,
    absolute_path: bool,
    show_details: bool,
    max_results: i64,
}

impl Options {
    /// 模式里是否有大写字母（`--smart-case` 用）。
    fn pattern_has_uppercase(&self) -> bool {
        self.pattern.chars().any(char::is_uppercase)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    /// 旧默认：子串匹配（不算正则）。
    Substring,
    Glob,
    Regex,
    Literal,
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseMode {
    Insensitive,
    Sensitive,
    Smart,
}

impl Options {
    fn parse(arguments: &Map<String, Value>, pattern: &str) -> Result<Self, ToolError> {
        let case_sensitive = optional_bool(arguments, "case_sensitive", false);
        let ignore_case = optional_bool(arguments, "ignore_case", false);
        let case_mode = if ignore_case {
            CaseMode::Insensitive
        } else if case_sensitive {
            CaseMode::Sensitive
        } else if optional_bool(arguments, "smart_case", false) {
            CaseMode::Smart
        } else {
            CaseMode::Insensitive
        };

        let exact = optional_bool(arguments, "exact", false);
        let fixed = optional_bool(arguments, "fixed_strings", false);
        let regex = optional_bool(arguments, "regex", false);
        let glob_flag = optional_bool(arguments, "glob", false);
        if (exact || fixed || regex) && glob_flag {
            return Err(ToolError::new(
                "exact/fixed_strings/regex 不能与 glob 同时使用。",
            ));
        }
        let mode = if exact {
            MatchMode::Exact
        } else if glob_flag {
            MatchMode::Glob
        } else if regex {
            MatchMode::Regex
        } else if fixed {
            MatchMode::Literal
        } else if has_glob_magic(pattern) {
            // 旧行为：模式里出现 glob 元字符就按 glob 处理。
            MatchMode::Glob
        } else {
            MatchMode::Substring
        };

        let mut kinds: Vec<EntryKind> = text_list(arguments, "type")
            .iter()
            .map(|name| parse_type(name))
            .collect::<Result<_, _>>()?;
        // `kind` 是旧参数，只有 all/file/directory 三种取值；fd 的类型面走 `type`。
        for name in text_list(arguments, "kind") {
            match name.to_lowercase().as_str() {
                "all" => {}
                "file" => kinds.push(EntryKind::File),
                "directory" => kinds.push(EntryKind::Dir),
                _ => return Err(ToolError::new("kind 仅支持 all、file 或 directory。")),
            }
        }
        let exclude = text_list(arguments, "type_not")
            .iter()
            .map(|name| parse_type(name))
            .collect::<Result<Vec<_>, _>>()?;

        let excluded = text_list(arguments, "exclude");
        let size = match optional_text(arguments, "size") {
            text if text.is_empty() => None,
            text => Some(parse_size(&text)?),
        };
        let empty = optional_bool(arguments, "empty", false)
            || kinds.contains(&EntryKind::Empty);

        Ok(Self {
            pattern: pattern.to_string(),
            mode,
            case_mode,
            full_path: optional_bool(arguments, "full_path", false),
            type_filter: TypeFilter { kinds, exclude },
            extension: text_list(arguments, "extension")
                .iter()
                .map(|item| item.trim().trim_start_matches('.').to_lowercase())
                .filter(|item| !item.is_empty())
                .collect(),
            excluded,
            empty,
            size,
            changed_within: parse_duration(&optional_text(arguments, "changed_within"))?,
            changed_before: parse_duration(&optional_text(arguments, "changed_before"))?,
            absolute_path: optional_bool(arguments, "absolute_path", false),
            show_details: optional_bool(arguments, "show_details", false),
            max_results: limited_int(arguments, "max_results", 50, 1, MAX_SEARCH_RESULTS),
        })
    }
}

fn parse_type(name: &str) -> Result<EntryKind, ToolError> {
    match name.trim().to_lowercase().as_str() {
        "f" | "file" => Ok(EntryKind::File),
        "d" | "dir" | "directory" => Ok(EntryKind::Dir),
        "l" | "symlink" | "link" => Ok(EntryKind::Symlink),
        "e" | "empty" => Ok(EntryKind::Empty),
        "x" | "executable" => Ok(EntryKind::Executable),
        other => Err(ToolError::new(format!(
            "type 仅支持 file/directory/symlink/empty/executable，收到 {other}。"
        ))),
    }
}

/// `<+->NUM[UNIT]`：`k`/`m`/`g`/`t` 十进制，`ki`/`mi`/`gi`/`ti` 二进制。
fn parse_size(text: &str) -> Result<SizeFilter, ToolError> {
    let trimmed = text.trim();
    let (comparison, rest) = match trimmed.chars().next() {
        Some('+') => (Comparison::AtLeast, &trimmed[1..]),
        Some('-') => (Comparison::AtMost, &trimmed[1..]),
        _ => (Comparison::Exactly, trimmed),
    };
    let split = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, unit) = rest.split_at(split);
    let value: u64 = digits
        .parse()
        .map_err(|_| ToolError::new(format!("size 数值非法：{text}")))?;
    let multiplier: u64 = match unit.trim().to_lowercase().as_str() {
        "" | "b" => 1,
        "k" => 1_000,
        "m" => 1_000_000,
        "g" => 1_000_000_000,
        "t" => 1_000_000_000_000,
        "ki" => 1_024,
        "mi" => 1_048_576,
        "gi" => 1_073_741_824,
        "ti" => 1_099_511_627_776,
        other => {
            return Err(ToolError::new(format!(
                "size 单位非法：{other}（支持 b/k/m/g/t/ki/mi/gi/ti）。"
            )))
        }
    };
    Ok(SizeFilter {
        comparison,
        bytes: value.saturating_mul(multiplier),
    })
}

/// `--changed-within` / `--changed-before`：`10h`、`2weeks`、`35min` 这类时长。
fn parse_duration(text: &str) -> Result<Option<u64>, ToolError> {
    let trimmed = text.trim().to_lowercase();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, unit) = trimmed.split_at(split);
    let value: u64 =
        digits.parse().map_err(|_| ToolError::new(format!("时长非法：{text}")))?;
    let seconds_per_unit: u64 = match unit.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 604_800,
        "month" | "months" => 2_592_000,
        "y" | "year" | "years" => 31_536_000,
        other => {
            return Err(ToolError::new(format!(
                "时长单位非法：{other}（支持 s/min/h/d/w/month/y）。"
            )))
        }
    };
    Ok(Some(value.saturating_mul(seconds_per_unit)))
}

/// 名称匹配器：三种模式各自编译一次，逐条复用。
enum Matcher {
    Substring { needle: String, case_sensitive: bool },
    Glob { pattern: String, case_sensitive: bool },
    Regex(regex::Regex),
    Exact { pattern: String, case_sensitive: bool },
}

impl Matcher {
    fn build(options: &Options) -> Result<Self, ToolError> {
        let case_sensitive = match options.case_mode {
            CaseMode::Sensitive => true,
            CaseMode::Insensitive => false,
            CaseMode::Smart => options.pattern_has_uppercase(),
        };
        let pattern = options.pattern.clone();
        Ok(match options.mode {
            MatchMode::Substring => Matcher::Substring {
                needle: fold(&pattern, case_sensitive),
                case_sensitive,
            },
            MatchMode::Glob => Matcher::Glob {
                pattern: fold(&pattern.replace('\\', "/"), case_sensitive),
                case_sensitive,
            },
            MatchMode::Literal => Matcher::Regex(
                RegexBuilder::new(&regex::escape(&pattern))
                    .case_insensitive(!case_sensitive)
                    .build()
                    .map_err(|error| ToolError::new(format!("正则非法：{error}")))?,
            ),
            MatchMode::Exact => Matcher::Exact {
                pattern: fold(&pattern, case_sensitive),
                case_sensitive,
            },
            MatchMode::Regex => Matcher::Regex(
                RegexBuilder::new(&pattern)
                    .case_insensitive(!case_sensitive)
                    .build()
                    .map_err(|error| ToolError::new(format!("正则非法：{error}")))?,
            ),
        })
    }

    /// 命中判定：`full_name` 是文件名，`relative` 是工作区相对路径。
    fn matches(&self, name: &str, relative: &str, full_path: bool) -> bool {
        let subject = if full_path {
            relative.replace('\\', "/")
        } else {
            name.to_string()
        };
        match self {
            Matcher::Substring {
                needle,
                case_sensitive,
            } => {
                let candidate = fold(&subject, *case_sensitive);
                if candidate.contains(needle) {
                    return true;
                }
                // 旧行为：模式不含 glob 元字符时同时比对相对路径。
                !full_path && fold(&relative.replace('\\', "/"), *case_sensitive).contains(needle)
            }
            Matcher::Glob {
                pattern,
                case_sensitive,
            } => {
                if glob_matches(pattern, &fold(&subject, *case_sensitive), false) {
                    return true;
                }
                !full_path
                    && glob_matches(
                        pattern,
                        &fold(&relative.replace('\\', "/"), *case_sensitive),
                        false,
                    )
            }
            Matcher::Regex(regex) => {
                if regex.is_match(&subject) {
                    return true;
                }
                !full_path && regex.is_match(&relative.replace('\\', "/"))
            }
            Matcher::Exact {
                pattern,
                case_sensitive,
            } => fold(&subject, *case_sensitive) == *pattern,
        }
    }
}

fn fold(text: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        text.to_string()
    } else {
        text.to_lowercase()
    }
}

/// 一次匹配用的候选。
struct Candidate {
    path: PathBuf,
    relative: String,
    kind: EntryKind,
    mtime: u128,
}

/// 收集候选条目：目录根走完整遍历，文件根只报它自己。
fn collect_entries(paths: &WorkspacePaths, root: &Path, walk: &WalkOptions) -> Vec<WalkEntry> {
    if root.is_file() {
        return vec![WalkEntry {
            path: root.to_path_buf(),
            kind: WalkKind::File,
        }];
    }
    walk_entries_with(root, paths, walk)
}

/// 目录是否为空（fd 的 `-t e` 对目录指「不含任何条目」）。
fn is_empty_entry(entry: &WalkEntry) -> bool {
    match entry.kind {
        WalkKind::Dir => std::fs::read_dir(&entry.path)
            .map(|mut reader| reader.next().is_none())
            .unwrap_or(false),
        WalkKind::File => std::fs::metadata(&entry.path)
            .map(|meta| meta.len() == 0)
            .unwrap_or(false),
        WalkKind::Symlink => false,
    }
}

fn has_extension(path: &Path, extensions: &[String]) -> bool {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .is_some_and(|ext| extensions.contains(&ext))
}

/// 结果行：目录以 `/` 结尾（跨平台一致的固定标记），`show_details` 时前置元数据。
fn render_candidate(paths: &WorkspacePaths, candidate: &Candidate, options: &Options) -> String {
    let mut text = if options.absolute_path {
        candidate.path.to_string_lossy().replace('\\', "/")
    } else {
        candidate.relative.clone()
    };
    if candidate.kind == EntryKind::Dir && !text.ends_with('/') {
        text.push('/');
    }
    if options.show_details {
        let metadata = std::fs::metadata(&candidate.path).ok();
        let size = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let kind_mark = match candidate.kind {
            EntryKind::Dir => "d",
            EntryKind::Symlink => "l",
            _ => "-",
        };
        let absolute = candidate.path.to_string_lossy().replace('\\', "/");
        return format!(
            "{kind_mark} {size:>10} {text}  ({absolute}, {})",
            relative_age(candidate.mtime)
        );
    }
    let _ = paths;
    text
}

/// 修改时间的粗略描述（`show_details` 用）。
fn relative_age(mtime_ns: u128) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seconds = now.saturating_sub(mtime_ns) / 1_000_000_000;
    match seconds {
        0..=59 => format!("{seconds} 秒前"),
        60..=3_599 => format!("{} 分钟前", seconds / 60),
        3_600..=86_399 => format!("{} 小时前", seconds / 3_600),
        _ => format!("{} 天前", seconds / 86_400),
    }
}

/// 遍历与忽略相关开关（与 grep 共用 `WalkOptions` 语义）。
fn walk_options(arguments: &Map<String, Value>) -> WalkOptions {
    let no_ignore = optional_bool(arguments, "no_ignore", false);
    let no_ignore_vcs = optional_bool(arguments, "no_ignore_vcs", false);
    let max_depth = limited_int(arguments, "max_depth", 0, 0, 1000);
    let exact_depth = limited_int(arguments, "exact_depth", 0, 0, 1000);
    let min_depth = limited_int(arguments, "min_depth", 0, 0, 1000);
    let effective_max = if exact_depth > 0 { exact_depth } else { max_depth };
    WalkOptions {
        include_hidden: optional_bool(arguments, "hidden", true),
        use_ignore_files: !no_ignore,
        use_vcs_ignore: !no_ignore_vcs,
        follow_links: optional_bool(arguments, "follow_links", false),
        max_depth: (effective_max > 0).then_some(effective_max as usize),
        max_filesize: None,
        globs: Vec::new(),
        iglobs: Vec::new(),
        types: Vec::new(),
        type_not: Vec::new(),
        min_depth: if exact_depth > 0 {
            Some(exact_depth as usize)
        } else {
            (min_depth > 0).then_some(min_depth as usize)
        },
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

    fn all_files() -> Value {
        json!({"pattern": "", "match_all_when_empty": true})
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
        assert!(
            !files_only.lines().any(|line| line.ends_with('/')),
            "只查文件时目录不出现：{files_only}"
        );

        let dirs_only = find_files(
            &paths,
            &args(json!({"pattern": "src", "kind": "directory"})),
        )
        .expect("只查目录");
        assert!(
            dirs_only.lines().all(|line| line.ends_with('/')),
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
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).expect("建目录");
        std::fs::write(outside.join("outside_hit.py"), "").expect("写文件");
        let found = find_files(
            &paths,
            &args(json!({"pattern": "outside_hit", "path": outside.to_string_lossy()})),
        )
        .expect("工作区外查找应当成功");
        assert!(found.contains("outside_hit.py"), "{found}");
        let _ = std::fs::remove_dir_all(&outside);
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

    // ---------------------------------------------------------------- fd 对齐的参数

    #[test]
    fn type_filter_reaches_directories_including_empty_ones() {
        let (paths, root) = workspace("types");
        std::fs::create_dir_all(root.join("empty-dir")).expect("建空目录");
        let mut value = all_files();
        value["type"] = json!("d");
        let dirs = find_files(&paths, &args(value)).expect("只查目录");
        assert!(dirs.lines().any(|line| line == "src/"), "{dirs}");
        assert!(
            dirs.lines()
                .any(|line| line == format!("{}deep/", native("src/"))),
            "{dirs}"
        );
        // 空目录只有靠「遍历返回目录」才可能出现。
        assert!(dirs.lines().any(|line| line == "empty-dir/"), "{dirs}");
        assert!(
            dirs.lines().all(|line| line.ends_with('/')),
            "只查目录时不该出现文件：{dirs}"
        );

        let only_files = find_files(&paths, &args(json!({"pattern": "", "type": "f", "match_all_when_empty": true})))
            .expect("只查文件");
        assert!(!only_files.lines().any(|line| line.ends_with('/')), "{only_files}");

        let excluded = find_files(
            &paths,
            &args(json!({"pattern": "", "type_not": "d", "match_all_when_empty": true})),
        )
        .expect("排除目录");
        assert!(!excluded.lines().any(|line| line.ends_with('/')), "{excluded}");
    }

    #[test]
    fn extension_filter_narrows_by_suffix() {
        let (paths, _root) = workspace("extension");
        let py = find_files(&paths, &args(json!({"pattern": "", "extension": "py", "match_all_when_empty": true})))
            .expect("按扩展名");
        assert!(py.contains("main.py"), "{py}");
        assert!(!py.contains("readme.md"), "{py}");

        let both = find_files(
            &paths,
            &args(json!({"pattern": "", "extension": [".py", "md"], "match_all_when_empty": true})),
        )
        .expect("多扩展名");
        assert!(both.contains("readme.md"), "{both}");
    }

    #[test]
    fn exclude_globs_remove_entries() {
        let (paths, _root) = workspace("exclude");
        let text = find_files(
            &paths,
            &args(json!({"pattern": "", "exclude": ["*.md", "*deep*"], "match_all_when_empty": true})),
        )
        .expect("排除");
        assert!(!text.contains("readme.md"), "{text}");
        assert!(!text.contains("helper.py"), "{text}");
        assert!(text.contains("main.py"), "{text}");
    }

    #[test]
    fn regex_and_exact_modes_change_matching() {
        let (paths, _root) = workspace("modes");
        let regex = find_files(
            &paths,
            &args(json!({"pattern": "^main\\.py$", "regex": true})),
        )
        .expect("正则匹配");
        assert!(regex.contains("main.py"), "{regex}");
        assert!(!regex.contains("agent.py"), "{regex}");

        let exact = find_files(
            &paths,
            &args(json!({"pattern": "main.py", "exact": true})),
        )
        .expect("精确匹配");
        assert!(exact.contains("main.py"), "{exact}");
        assert!(!exact.contains("agent.py"), "{exact}");

        assert_eq!(
            find_files(
                &paths,
                &args(json!({"pattern": "x", "exact": true, "glob": true}))
            )
            .unwrap_err()
            .message,
            "exact/fixed_strings/regex 不能与 glob 同时使用。"
        );
    }

    #[test]
    fn depth_limits_and_full_path_matching() {
        let (paths, _root) = workspace("depth");
        let shallow = find_files(&paths, &args(json!({"pattern": "*.py", "max_depth": 1})))
            .expect("深度限制");
        assert!(shallow.contains("main.py"), "{shallow}");
        assert!(!shallow.contains("helper.py"), "{shallow}");

        let exact = find_files(&paths, &args(json!({"pattern": "*.py", "exact_depth": 2})))
            .expect("精确深度");
        assert!(exact.contains(&native("src/agent.py")), "{exact}");
        assert!(!exact.contains("main.py"), "{exact}");

        // `full_path` 让正则能匹配到目录段。
        let by_path = find_files(
            &paths,
            &args(json!({"pattern": "^src/deep/", "regex": true, "full_path": true})),
        )
        .expect("整路径匹配");
        assert!(by_path.contains("helper.py"), "{by_path}");
        assert!(!by_path.contains("main.py"), "{by_path}");
    }

    #[test]
    fn size_and_empty_filters_narrow_results() {
        let (paths, root) = workspace("size");
        std::fs::write(root.join("big.bin"), vec![b'x'; 2048]).expect("写大文件");

        let at_least = find_files(
            &paths,
            &args(json!({"pattern": "", "size": "+1k", "match_all_when_empty": true})),
        )
        .expect("按大小");
        assert!(at_least.contains("big.bin"), "{at_least}");
        assert!(!at_least.contains("main.py"), "{at_least}");

        let empty_only = find_files(
            &paths,
            &args(json!({
                "pattern": "",
                "empty": true,
                "type": "file",
                "match_all_when_empty": true
            })),
        )
        .expect("空文件");
        assert!(empty_only.contains("main.py"), "{empty_only}");
        assert!(!empty_only.contains("big.bin"), "{empty_only}");
    }

    #[test]
    fn changed_within_selects_recent_entries() {
        let (paths, _root) = workspace("changed");
        let recent = find_files(
            &paths,
            &args(json!({"pattern": "", "changed_within": "1h", "match_all_when_empty": true})),
        )
        .expect("近期修改");
        assert!(recent.contains("main.py"), "{recent}");

        assert!(
            find_files(&paths, &args(json!({"pattern": "x", "changed_within": "5x"})))
                .unwrap_err()
                .message
                .starts_with("时长单位非法"),
            "非法时长单位应当报错"
        );
    }

    #[test]
    fn absolute_path_and_show_details_change_the_line() {
        let (paths, _root) = workspace("render");
        let absolute = find_files(
            &paths,
            &args(json!({"pattern": "main.py", "absolute_path": true})),
        )
        .expect("绝对路径");
        assert!(
            absolute.contains("main.py") && (absolute.starts_with('/') || absolute.contains(":/")),
            "{absolute}"
        );

        let details = find_files(
            &paths,
            &args(json!({"pattern": "main.py", "show_details": true})),
        )
        .expect("详细信息");
        assert!(details.starts_with('-'), "{details}");
        assert!(details.contains("main.py"), "{details}");
        assert!(details.contains("前"), "{details}");
    }

    #[test]
    fn hidden_and_no_ignore_switch_the_walk() {
        let (paths, root) = workspace("walk-flags");
        std::fs::write(root.join(".hidden.py"), "").expect("写隐藏文件");
        std::fs::write(root.join(".gitignore"), "ignored.py\n").expect("写忽略文件");
        std::fs::write(root.join("ignored.py"), "").expect("写忽略目标");

        let mut default_value = all_files();
        default_value["extension"] = json!("py");
        let default_run = find_files(&paths, &args(default_value)).expect("默认");
        assert!(default_run.contains(".hidden.py"), "{default_run}");

        let no_hidden = find_files(
            &paths,
            &args(json!({"pattern": "", "extension": "py", "hidden": false, "match_all_when_empty": true})),
        )
        .expect("hidden=false");
        assert!(!no_hidden.contains(".hidden.py"), "{no_hidden}");
        

        let unignored = find_files(
            &paths,
            &args(json!({"pattern": "ignored", "no_ignore": true})),
        )
        .expect("no_ignore");
        assert!(unignored.contains("ignored.py"), "{unignored}");
    }
}
