//! `grep` 工具：在工作区或本机文本里按正则或精确子串搜索内容。
//!
//! 只读、不改文件，因此不设工作区边界：相对路径按工作区解析，绝对路径可指向本机任意位置。
//!
//! 参数面对齐 ripgrep（rg 15.x 的常用面），语义基准是 `rg`：
//! 匹配（`-w`/`-x`/`-v`/`-i`/`-S`/`-F`/`-U`/`-m`）、上下文（`-A`/`-B`/`-C`/
//! `--context-separator`）、输出模式（`-c`/`--count-matches`/`-l`/`--files-without-match`/
//! `-o`/`-r`）、展示（`-n`/`--column`/`-b`/`-M`/`--trim`/`--include-zero`/`--stats`）、
//! 文件筛选（`-g`/`--iglob`/`-t`/`-T`/`--hidden`/`--no-ignore`/`--max-depth`/
//! `--max-filesize`/`-L`/`-a`）。此外 `omnicrawl://docs/` 内置文档 URI 走
//! `omnicrawl-mcp` 的编译期内嵌文档表（与 `read` 一致）。
//!
//! 与本机惯用法不同的三处默认值（对 Agent 更顺手，也保持既有输出不变）：
//! ① 大小写默认不敏感（rg 默认敏感）；② 默认包含隐藏文件（rg 的 `--hidden`）；
//! ③ 默认把 CRLF 当行尾（`crlf=false` 可回到 rg 的保留 `\r` 行为）。

use std::path::{Path, PathBuf};

use omnicrawl_mcp::bundled::{read_bundled_doc, BUNDLED_DOC_URI_PREFIX};
use regex::bytes::{Regex, RegexBuilder};
use serde_json::{Map, Value};

use super::arguments::{limited_int, optional_bool, optional_text, raw_text, text_list};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::search_common::{
    glob_regex, has_glob_magic, render_search_result, should_skip, truncation_footer,
    walk_files_with, WalkOptions, GREP_MAX_LINE_LENGTH, MAX_SEARCH_RESULTS, SEARCH_PARSE_LINE_CAP,
};

/// 非 UTF-8 行的占位文本（与 rg 的 `--json` 行为一致）。
const NON_UTF8_PLACEHOLDER: &str = "(line is not valid UTF-8)";
/// 超宽行被整行省略时的占位（rg 的 `-M` 行为）。
const OMITTED_LONG_LINE: &str = "[Omitted long matching line]";
/// 默认的上下文分块分隔符（rg 的 `--`）。
const DEFAULT_CONTEXT_SEPARATOR: &str = "--";

pub fn grep(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let options = Options::parse(arguments)?;
    let matcher = build_matcher(&options)?;

    let raw_path = optional_text(arguments, "path");
    if raw_path.starts_with(BUNDLED_DOC_URI_PREFIX) {
        return grep_bundled(paths, &raw_path, &options, &matcher);
    }

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

    let reports = search_roots(paths, &roots, &options, &matcher);
    Ok(render_reports(paths, reports, &options))
}

/// 大小写模式：与 rg 的 `-i` / `-S` / `-s` 对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseMode {
    Insensitive,
    Sensitive,
    Smart,
}

/// 输出模式：与 rg 的互斥模式对齐，后出现的参数覆盖先出现的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Standard,
    Count,
    CountMatches,
    FilesWithMatches,
    FilesWithoutMatch,
}

/// 一次搜索的全部开关。
struct Options {
    expression: String,
    use_regex: bool,
    case_mode: CaseMode,
    word_regexp: bool,
    line_regexp: bool,
    invert_match: bool,
    multiline: bool,
    multiline_dotall: bool,
    crlf: bool,
    max_count: Option<usize>,
    before: usize,
    after: usize,
    context_separator: Option<String>,
    mode: OutputMode,
    only_matching: bool,
    replace: Option<String>,
    line_number: bool,
    column: bool,
    byte_offset: bool,
    max_columns: Option<usize>,
    max_columns_preview: bool,
    trim: bool,
    include_zero: bool,
    stats: bool,
    include: String,
    exclude: String,
    text: bool,
    max_results: i64,
    walk: WalkOptions,
}

impl Options {
    fn parse(arguments: &Map<String, Value>) -> Result<Self, ToolError> {
        let pattern = raw_text(arguments, "pattern");
        if pattern.is_empty() {
            return Err(ToolError::new("pattern 不能为空。"));
        }

        let fixed_strings = optional_bool(arguments, "fixed_strings", false);
        let use_regex = optional_bool(arguments, "use_regex", true) && !fixed_strings;

        let ignore_case = optional_bool(arguments, "ignore_case", false);
        let case_sensitive = optional_bool(arguments, "case_sensitive", false);

        let context_lines = limited_int(arguments, "context_lines", 0, 0, 50);
        let before = limited_int(arguments, "before_context", context_lines, 0, 50) as usize;
        let after = limited_int(arguments, "after_context", context_lines, 0, 50) as usize;
        let context_separator = match arguments.get("context_separator") {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Bool(false)) | Some(Value::Null) => None,
            _ => Some(DEFAULT_CONTEXT_SEPARATOR.to_string()),
        };

        let mode = if optional_bool(arguments, "files_without_match", false) {
            OutputMode::FilesWithoutMatch
        } else if optional_bool(arguments, "files_with_matches", false) {
            OutputMode::FilesWithMatches
        } else if optional_bool(arguments, "count_matches", false) {
            OutputMode::CountMatches
        } else if optional_bool(arguments, "count", false) {
            OutputMode::Count
        } else {
            OutputMode::Standard
        };

        let replace = match arguments.get("replace") {
            Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
            _ => None,
        };
        let max_columns = limited_int(arguments, "max_columns", 0, 0, GREP_MAX_LINE_LENGTH as i64);
        let max_count = limited_int(arguments, "max_count", 0, 0, 100_000);

        Ok(Self {
            expression: pattern,
            use_regex,
            // 三个大小写开关互斥，优先级与 rg 一致：`-i` > `-S` > `-s` > 默认（不敏感）。
            case_mode: if ignore_case {
                CaseMode::Insensitive
            } else if case_sensitive {
                CaseMode::Sensitive
            } else if optional_bool(arguments, "smart_case", false) {
                CaseMode::Smart
            } else {
                CaseMode::Insensitive
            },
            word_regexp: optional_bool(arguments, "word_regexp", false),
            line_regexp: optional_bool(arguments, "line_regexp", false),
            invert_match: optional_bool(arguments, "invert_match", false),
            multiline: optional_bool(arguments, "multiline", false),
            multiline_dotall: optional_bool(arguments, "multiline_dotall", false),
            crlf: optional_bool(arguments, "crlf", true),
            max_count: (max_count > 0).then_some(max_count as usize),
            before,
            after,
            context_separator,
            mode,
            only_matching: optional_bool(arguments, "only_matching", false),
            replace,
            line_number: optional_bool(arguments, "line_number", true),
            column: optional_bool(arguments, "column", false),
            byte_offset: optional_bool(arguments, "byte_offset", false),
            max_columns: (max_columns > 0).then_some(max_columns as usize),
            max_columns_preview: optional_bool(arguments, "max_columns_preview", false),
            trim: optional_bool(arguments, "trim", false),
            include_zero: optional_bool(arguments, "include_zero", false),
            stats: optional_bool(arguments, "stats", false),
            include: optional_text(arguments, "include"),
            exclude: optional_text(arguments, "exclude"),
            text: optional_bool(arguments, "text", false),
            max_results: limited_int(arguments, "max_results", 50, 1, MAX_SEARCH_RESULTS),
            walk: walk_options(arguments),
        })
    }

    /// 是否需要逐行上下文（context 与 passthru 之外的输出模式都不用）。
    fn wants_context(&self) -> bool {
        self.mode == OutputMode::Standard && !self.only_matching && (self.before > 0 || self.after > 0)
    }

    /// 是否按整文件匹配（跨行）。
    fn whole_file(&self) -> bool {
        self.multiline || self.multiline_dotall
    }

    /// 只需要「有无匹配」的模式：命中即可停止读该文件。
    fn stops_at_first_match(&self) -> bool {
        matches!(
            self.mode,
            OutputMode::FilesWithMatches | OutputMode::FilesWithoutMatch
        )
    }
}

/// 遍历与文件筛选开关（rg 的 `-g`/`-t`/`--hidden`/`--no-ignore`/`--max-depth` 等）。
fn walk_options(arguments: &Map<String, Value>) -> WalkOptions {
    let no_ignore = optional_bool(arguments, "no_ignore", false);
    let no_ignore_vcs = optional_bool(arguments, "no_ignore_vcs", false);
    let max_depth = limited_int(arguments, "max_depth", 0, 0, 1000);
    let max_filesize = limited_int(arguments, "max_filesize", 0, 0, i64::MAX / 2);
    let mut globs = text_list(arguments, "glob");
    // `include` / `exclude` 是旧参数：转成同一套 gitignore 语义 glob，与 `-g` 同权。
    let include = optional_text(arguments, "include");
    if !include.is_empty() {
        globs.push(include);
    }
    let exclude = optional_text(arguments, "exclude");
    if !exclude.is_empty() {
        globs.push(format!("!{exclude}"));
    }
    WalkOptions {
        include_hidden: optional_bool(arguments, "hidden", true),
        use_ignore_files: !no_ignore,
        use_vcs_ignore: !no_ignore_vcs,
        follow_links: optional_bool(arguments, "follow_links", false),
        max_depth: (max_depth > 0).then_some(max_depth as usize),
        min_depth: None,
        max_filesize: (max_filesize > 0).then_some(max_filesize as u64),
        iglobs: text_list(arguments, "iglob"),
        types: text_list(arguments, "type"),
        type_not: text_list(arguments, "type_not"),
        globs,
    }
}

/// 构建匹配器：`use_regex=false` 时转义成精确子串（rg 的 `--fixed-strings`）。
fn build_matcher(options: &Options) -> Result<Regex, ToolError> {
    let mut expression = if options.use_regex {
        options.expression.clone()
    } else {
        regex::escape(&options.expression)
    };
    if options.word_regexp {
        expression = format!(r"\b(?:{expression})\b");
    }
    if options.line_regexp {
        expression = format!(r"\A(?:{expression})\z");
    }
    if options.multiline_dotall {
        expression = format!(r"(?s){expression}");
    }
    let case_insensitive = match options.case_mode {
        CaseMode::Insensitive => true,
        CaseMode::Sensitive => false,
        // rg 的 `--smart-case`：模式里没有大写字母时按不敏感处理。
        CaseMode::Smart => !options.expression.chars().any(char::is_uppercase),
    };
    RegexBuilder::new(&expression)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|error| {
            let hint = if options.use_regex {
                "。可设置 use_regex=false 按精确子串匹配。"
            } else {
                "。"
            };
            ToolError::new(format!("无效的正则表达式：{error}{hint}"))
        })
}

/// 命中行或上下文行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Match,
    Context,
}

#[derive(Debug, Clone)]
struct RenderedLine {
    kind: LineKind,
    line_no: usize,
    column: usize,
    byte_offset: usize,
    text: String,
}

#[derive(Debug, Clone, Default)]
struct FileReport {
    display: String,
    lines: Vec<RenderedLine>,
    matched_lines: usize,
    match_count: usize,
}

impl FileReport {
    fn has_match(&self) -> bool {
        self.matched_lines > 0
    }
}

/// 一个文件（或内置文档）的扫描结果。
fn search_document(display: &str, bytes: &[u8], options: &Options, matcher: &Regex) -> FileReport {
    let mut report = FileReport {
        display: display.to_string(),
        ..FileReport::default()
    };
    if options.whole_file() {
        search_whole(display, bytes, options, matcher, &mut report);
    } else {
        search_lines(display, bytes, options, matcher, &mut report);
    }
    report
}

/// 逐行匹配（rg 的默认行模式）。
fn search_lines(
    _display: &str,
    bytes: &[u8],
    options: &Options,
    matcher: &Regex,
    report: &mut FileReport,
) {
    let lines = split_lines(bytes, options.crlf);
    let mut matched_flags: Vec<bool> = Vec::with_capacity(lines.len());
    for (start, end) in &lines {
        matched_flags.push(matcher.is_match(&bytes[*start..*end]) != options.invert_match);
    }
    collect_lines(bytes, &lines, &matched_flags, options, matcher, report);
}

/// 整文件匹配（`-U`/`--multiline`）：一条匹配可以跨行。
fn search_whole(
    _display: &str,
    bytes: &[u8],
    options: &Options,
    matcher: &Regex,
    report: &mut FileReport,
) {
    let Some(captures) = matcher.captures(bytes) else {
        if options.invert_match && !bytes.is_empty() {
            let lines = split_lines(bytes, options.crlf);
            let flags = vec![true; lines.len()];
            report.matched_lines = lines.len();
            if options.mode == OutputMode::Standard {
                collect_lines(bytes, &lines, &flags, options, matcher, report);
            }
        }
        return;
    };
    let mut found = vec![captures];
    found.extend(matcher.captures_iter(bytes).skip(1));
    let lines = split_lines(bytes, options.crlf);
    for captures in found {
        if options.invert_match {
            continue;
        }
        if options
            .max_count
            .is_some_and(|limit| report.matched_lines >= limit)
        {
            break;
        }
        let whole = captures.get(0).expect("captures 一定含整体匹配");
        let start = whole.start();
        let line_index = line_index_of(&lines, start);
        report.match_count += 1;
        report.matched_lines += 1;
        if options.mode != OutputMode::Standard {
            continue;
        }
        let text = render_snippet(whole.as_bytes(), options);
        report.lines.push(RenderedLine {
            kind: LineKind::Match,
            line_no: line_index + 1,
            column: start.saturating_sub(lines[line_index].0) + 1,
            byte_offset: start,
            text,
        });
    }
}

/// 把逐行命中拼成展示行（含上下文与 `-o`/`-r` 两种改写）。
fn collect_lines(
    bytes: &[u8],
    lines: &[(usize, usize)],
    matched_flags: &[bool],
    options: &Options,
    matcher: &Regex,
    report: &mut FileReport,
) {
    let mut included: Vec<bool> = vec![false; matched_flags.len()];
    for (index, matched) in matched_flags.iter().enumerate() {
        if !*matched {
            continue;
        }
        if let Some(limit) = options.max_count {
            if report.matched_lines >= limit {
                break;
            }
        }
        report.matched_lines += 1;
        included[index] = true;
        if options.wants_context() {
            let start = index.saturating_sub(options.before);
            let end = (index + options.after + 1).min(lines.len());
            for slot in &mut included[start..end] {
                *slot = true;
            }
        }
    }

    if options.mode != OutputMode::Standard {
        report.match_count = matched_flags
            .iter()
            .enumerate()
            .filter(|(index, matched)| **matched && included[*index])
            .map(|(index, _)| match_count_in_line(bytes, lines[index], options, matcher))
            .sum();
        return;
    }

    for (index, (start, end)) in lines.iter().enumerate() {
        let matched = matched_flags[index];
        if !included[index] {
            continue;
        }
        report.match_count += if matched {
            match_count_in_line(bytes, (*start, *end), options, matcher)
        } else {
            0
        };
        if matched && options.only_matching {
            for found in matcher.find_iter(&bytes[*start..*end]) {
                let text = if options.replace.is_some() {
                    render_replaced(found.as_bytes(), options, matcher)
                } else {
                    render_snippet(found.as_bytes(), options)
                };
                report.lines.push(RenderedLine {
                    kind: LineKind::Match,
                    line_no: index + 1,
                    column: start + found.start() + 1,
                    byte_offset: *start + found.start(),
                    text,
                });
            }
            continue;
        }
        let line = &bytes[*start..*end];
        let (text, column) = if matched {
            let column = matcher
                .find(line)
                .map(|found| found.start() + 1)
                .unwrap_or(1);
            let body = if options.replace.is_some() {
                render_replaced(line, options, matcher)
            } else {
                render_snippet(line, options)
            };
            (body, column)
        } else {
            (render_snippet(line, options), 1)
        };
        report.lines.push(RenderedLine {
            kind: if matched {
                LineKind::Match
            } else {
                LineKind::Context
            },
            line_no: index + 1,
            column,
            byte_offset: *start,
            text,
        });
    }
}

fn match_count_in_line(
    bytes: &[u8],
    (start, end): (usize, usize),
    options: &Options,
    matcher: &Regex,
) -> usize {
    if options.invert_match {
        return 1;
    }
    matcher.find_iter(&bytes[start..end]).count()
}

/// `-r/--replace`：整行替换后渲染。
fn render_replaced(line: &[u8], options: &Options, matcher: &Regex) -> String {
    let replacement = options.replace.clone().unwrap_or_default();
    let replaced = matcher.replace_all(line, replacement.as_bytes());
    render_snippet(replaced.as_ref(), options)
}

/// 单行展示文本：非 UTF-8 给占位符，超长截断或按 `-M` 省略。
fn render_snippet(line: &[u8], options: &Options) -> String {
    let mut text = match std::str::from_utf8(line) {
        Ok(text) => text.to_string(),
        Err(_) => NON_UTF8_PLACEHOLDER.to_string(),
    };
    if !options.multiline {
        text = text.replace(['\r', '\n'], "");
    } else {
        // 跨行匹配的文本压成一行，换行写成转义，避免破坏 `path:line: text` 的形状。
        text = text.replace('\n', "\\n").replace('\r', "");
    }
    if options.trim {
        text = text.trim_start().to_string();
    }
    if let Some(limit) = options.max_columns {
        if text.len() > limit {
            if options.max_columns_preview {
                let head: String = text.chars().take(limit).collect();
                text = format!("{head} [... omitted end of long line]");
            } else {
                return OMITTED_LONG_LINE.to_string();
            }
        }
    }
    cap_match_line(&text)
}

fn cap_match_line(text: &str) -> String {
    if text.chars().count() <= GREP_MAX_LINE_LENGTH {
        return text.to_string();
    }
    let head: String = text.chars().take(GREP_MAX_LINE_LENGTH).collect();
    format!("{head} (line truncated)")
}

/// 按行切分并给出行首偏移；`strip_cr` 为真时 CRLF 当作单一行尾。
fn split_lines(bytes: &[u8], strip_cr: bool) -> Vec<(usize, usize)> {
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let mut end = index;
        if strip_cr && end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        lines.push((start, end));
        start = index + 1;
    }
    if start < bytes.len() {
        lines.push((start, bytes.len()));
    }
    lines
}

/// 字节偏移落在第几行（0 基）；越界时给最后一行。
fn line_index_of(lines: &[(usize, usize)], offset: usize) -> usize {
    match lines.binary_search_by(|(start, end)| {
        if offset < *start {
            std::cmp::Ordering::Greater
        } else if offset > *end {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(index) => index,
        Err(_) => lines.len().saturating_sub(1),
    }
}

/// 逐 root 枚举候选文件（忽略规则、glob 与类型筛选都在遍历里完成）。
fn search_roots(
    paths: &WorkspacePaths,
    roots: &[PathBuf],
    options: &Options,
    matcher: &Regex,
) -> Vec<FileReport> {
    let mut reports: Vec<FileReport> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in roots {
        let files: Vec<PathBuf> = if root.is_file() {
            vec![root.clone()]
        } else {
            walk_files_with(root, paths, &options.walk)
        };
        for file in files {
            if seen.contains(&file) {
                continue;
            }
            seen.push(file.clone());
            if should_skip(root, &file) || !basename_accepts(&file, options) {
                continue;
            }
            let Some(bytes) = read_file_bytes(&file, options.text) else {
                continue;
            };
            let report = search_document(&paths.relative(&file), &bytes, options, matcher);
            let keep = if options.mode == OutputMode::FilesWithoutMatch {
                !report.has_match()
            } else {
                report.has_match() || options.include_zero && options.mode == OutputMode::Count
            };
            if keep {
                reports.push(report);
            }
            if reports.len() >= SEARCH_PARSE_LINE_CAP {
                break;
            }
        }
        if options.stops_at_first_match()
            && reports.len() as i64 >= options.max_results.saturating_mul(4).max(200)
        {
            break;
        }
    }
    reports.sort_by(|left, right| {
        left.display
            .to_lowercase()
            .cmp(&right.display.to_lowercase())
    });
    reports
}

/// `include`/`exclude`：旧参数，按文件名 glob 过滤且大小写不敏感。
fn basename_accepts(file: &Path, options: &Options) -> bool {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    if !options.include.is_empty()
        && !glob_regex(&options.include, true).is_ok_and(|re| re.is_match(&name))
    {
        return false;
    }
    if !options.exclude.is_empty()
        && glob_regex(&options.exclude, true).is_ok_and(|re| re.is_match(&name))
    {
        return false;
    }
    true
}

/// 读文件字节；默认跳过二进制文件（含 NUL），`-a`/`text` 时照读。
fn read_file_bytes(file: &Path, as_text: bool) -> Option<Vec<u8>> {
    let bytes = std::fs::read(file).ok()?;
    if !as_text && bytes.iter().take(8192).any(|byte| *byte == 0) {
        return None;
    }
    Some(bytes)
}

/// `omnicrawl://docs/<name>.md`：内置文档不进工作区，直接在编译期内嵌文本上匹配。
fn grep_bundled(
    paths: &WorkspacePaths,
    uri: &str,
    options: &Options,
    matcher: &Regex,
) -> ToolOutcome {
    let text = read_bundled_doc(uri).map_err(|error| ToolError::new(error.message()))?;
    let report = search_document(uri, text.as_bytes(), options, matcher);
    let keep = if options.mode == OutputMode::FilesWithoutMatch {
        !report.has_match()
    } else {
        report.has_match()
    };
    let reports = if keep { vec![report] } else { Vec::new() };
    Ok(render_reports(paths, reports, options))
}

/// 渲染全部结果：超限保留前 `max_results` 条并把完整结果落盘。
fn render_reports(paths: &WorkspacePaths, reports: Vec<FileReport>, options: &Options) -> String {
    let files_searched = reports.len();
    let matched_files = reports.iter().filter(|report| report.has_match()).count();
    let total_matches: usize = reports.iter().map(|report| report.match_count).sum();

    let body = match options.mode {
        OutputMode::Count | OutputMode::CountMatches => {
            let items: Vec<String> = reports
                .iter()
                .map(|report| {
                    let count = if options.mode == OutputMode::Count {
                        report.matched_lines
                    } else {
                        report.match_count
                    };
                    format!("{}: {count}", report.display)
                })
                .collect();
            render_search_result(
                paths.root(),
                &items,
                options.max_results,
                "grep_counts",
                "未找到匹配结果。",
                "个文件",
                false,
            )
        }
        OutputMode::FilesWithMatches | OutputMode::FilesWithoutMatch => {
            let items: Vec<String> = reports
                .iter()
                .map(|report| report.display.clone())
                .collect();
            render_search_result(
                paths.root(),
                &items,
                options.max_results,
                "grep_files",
                "未找到匹配结果。",
                "个文件",
                false,
            )
        }
        OutputMode::Standard => render_standard(paths, &reports, options),
    };

    if options.stats {
        return format!(
            "{body}\n统计：扫描 {files_searched} 个文件，{matched_files} 个文件命中，共 {total_matches} 条匹配。"
        );
    }
    body
}

/// 标准模式的展示：`path:line: text`，上下文行 `path-line- text`，块间给分隔符。
fn render_standard(paths: &WorkspacePaths, reports: &[FileReport], options: &Options) -> String {
    let mut rendered_files: Vec<Vec<String>> = Vec::new();
    let mut matched_entries: usize = 0;
    let mut truncated = false;

    for report in reports {
        if !report.has_match() {
            continue;
        }
        let mut block: Vec<String> = Vec::new();
        let mut previous: Option<usize> = None;
        for line in &report.lines {
            if line.kind == LineKind::Match {
                if matched_entries as i64 >= options.max_results {
                    truncated = true;
                    break;
                }
                matched_entries += 1;
            }
            if options.wants_context() {
                if let Some(previous) = previous {
                    if line.line_no > previous + 1 {
                        if let Some(separator) = &options.context_separator {
                            block.push(separator.clone());
                        }
                    }
                }
            }
            previous = Some(line.line_no);
            block.push(format_line(&report.display, line, options));
        }
        if !block.is_empty() {
            rendered_files.push(block);
        }
        if truncated {
            break;
        }
    }

    if rendered_files.is_empty() {
        return "未找到匹配结果。".to_string();
    }

    let inline: Vec<String> = rendered_files
        .iter()
        .flatten()
        .cloned()
        .collect();
    if !truncated {
        return inline.join("\n");
    }
    let spill: Vec<String> = reports
        .iter()
        .filter(|report| report.has_match())
        .flat_map(|report| {
            report
                .lines
                .iter()
                .filter(|line| line.kind == LineKind::Match)
                .map(move |line| format!("{}:{}: {}", report.display, line.line_no, line.text))
        })
        .collect();
    inline.join("\n")
        + &truncation_footer(
            paths.root(),
            &spill,
            options.max_results,
            "grep_matches",
            "条匹配",
            false,
        )
}

/// 单行前缀：`path:line:col:offset: text`；上下文行各段用 `-` 连接（与 rg 一致）。
fn format_line(display: &str, line: &RenderedLine, options: &Options) -> String {
    let joiner = if line.kind == LineKind::Match {
        ':'
    } else {
        '-'
    };
    let mut fields: Vec<String> = vec![display.to_string()];
    if options.line_number {
        fields.push(line.line_no.to_string());
    }
    if options.column && line.kind == LineKind::Match {
        fields.push(line.column.to_string());
    }
    if options.byte_offset && line.kind == LineKind::Match {
        fields.push(line.byte_offset.to_string());
    }
    let head = fields.join(&joiner.to_string());
    if line.kind == LineKind::Match {
        format!("{head}: {}", line.text)
    } else {
        format!("{head}- {}", line.text)
    }
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
    // 排除按用户给出的这个 glob 的静态前缀判定：点名进 `.omnicrawl` 时子项照常参与。
    let skip_root = static_prefix(&candidate).unwrap_or_else(|| paths.root().to_path_buf());
    for matched in matches {
        if should_skip(&skip_root, &matched) {
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
///
/// 排除按 glob 的静态前缀判定，而不是按工作区根：这样 `grep` 的 `path` 点名进
/// `.omnicrawl` 时子项照常参与，而 `**` 之类的宽泛 glob 仍不会把临时目录带进来。
fn expand_glob(_paths: &WorkspacePaths, candidate: &Path) -> Vec<PathBuf> {
    let Some(prefix) = static_prefix(candidate) else {
        return Vec::new();
    };
    let regex = match glob_path_regex(&candidate.to_string_lossy()) {
        Ok(regex) => regex,
        Err(_) => return Vec::new(),
    };
    let skip_root = prefix.clone();
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
            if regex.is_match(&text) && !should_skip(&skip_root, &path) {
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

    /// 只读搜索同样不受工作区边界约束：绝对路径可指向本机任意位置。
    #[test]
    fn greps_an_absolute_path_outside_the_workspace() {
        let (paths, _root) = workspace("outside");
        let outside = std::env::temp_dir().join("omnicrawl-tui-grep-outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).expect("建目录");
        std::fs::write(outside.join("note.txt"), "外部搜索命中行\n").expect("写文件");

        let text = grep(
            &paths,
            &args(json!({
                "pattern": "命中行",
                "path": outside.to_string_lossy().to_string(),
            })),
        )
        .expect("工作区外的绝对路径应当可搜");
        assert!(text.contains("命中行"), "{text}");
        let _ = std::fs::remove_dir_all(&outside);
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

        let fixed = grep(
            &paths,
            &args(json!({"pattern": "self.tools", "fixed_strings": true})),
        )
        .expect("fixed_strings 与 use_regex=false 同义");
        assert_eq!(fixed, text);
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

        let text_mode = grep(&paths, &args(json!({"pattern": "hidden", "text": true})))
            .expect("-a 应当读二进制文件");
        assert!(text_mode.contains("hidden"), "{text_mode}");
    }

    #[test]
    fn bundled_doc_uri_searches_embedded_text() {
        let (paths, _root) = workspace("bundled");
        let text = grep(
            &paths,
            &args(json!({"pattern": "启动", "path": "omnicrawl://docs/API.md"})),
        )
        .expect("搜索内置文档");
        assert!(
            text.lines()
                .all(|line| line.starts_with("omnicrawl://docs/API.md:")),
            "{text}"
        );
        assert!(text.contains("## 1. 启动与配置"), "{text}");

        let counts = grep(
            &paths,
            &args(json!({
                "pattern": "启动",
                "path": "omnicrawl://docs/API.md",
                "count": true
            })),
        )
        .expect("计数");
        let count: usize = counts
            .strip_prefix("omnicrawl://docs/API.md: ")
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_else(|| panic!("计数格式不符：{counts}"));
        assert_eq!(count, text.lines().count(), "{counts}");

        let files = grep(
            &paths,
            &args(json!({
                "pattern": "启动",
                "path": "omnicrawl://docs/API.md",
                "files_with_matches": true
            })),
        )
        .expect("只列文件");
        assert_eq!(files, "omnicrawl://docs/API.md");
    }

    #[test]
    fn bundled_doc_context_lines_use_dash_markers() {
        let (paths, _root) = workspace("bundled-context");
        let text = grep(
            &paths,
            &args(json!({
                "pattern": "## 1. 启动与配置",
                "path": "omnicrawl://docs/API.md",
                "context_lines": 1
            })),
        )
        .expect("上下文搜索");
        assert!(
            text.contains("omnicrawl://docs/API.md:3: ## 1. 启动与配置"),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|line| line.starts_with("omnicrawl://docs/API.md-")),
            "{text}"
        );
    }

    #[test]
    fn bundled_doc_errors_keep_read_texts() {
        let (paths, _root) = workspace("bundled-errors");
        let missing = grep(
            &paths,
            &args(json!({"pattern": "x", "path": "omnicrawl://docs/NOPE.md"})),
        )
        .unwrap_err();
        assert_eq!(missing.message, "内置文档不存在：omnicrawl://docs/NOPE.md");

        let traversal = grep(
            &paths,
            &args(json!({"pattern": "x", "path": "omnicrawl://docs/../config.toml"})),
        )
        .unwrap_err();
        assert!(
            traversal
                .message
                .starts_with("内置文档 URI 仅允许单个 Markdown 文件名："),
            "{}",
            traversal.message
        );

        let no_match = grep(
            &paths,
            &args(json!({
                "pattern": "不可能出现的字符串zzz",
                "path": "omnicrawl://docs/API.md"
            })),
        )
        .expect("搜索");
        assert_eq!(no_match, "未找到匹配结果。");
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

    // ---------------------------------------------------------------- rg 对齐的参数

    #[test]
    fn word_and_line_regexp_narrow_matches() {
        let (paths, _root) = workspace("boundary");
        std::fs::write(
            _root.join("src").join("words.py"),
            "agent\nsubagent\nagent_x\nAgent\n",
        )
        .expect("写文件");

        let word = grep(
            &paths,
            &args(json!({"pattern": "agent", "word_regexp": true, "include": "words.py"})),
        )
        .expect("整词匹配");
        let lines: Vec<&str> = word.lines().collect();
        assert!(lines.iter().any(|line| line.ends_with(":1: agent")), "{word}");
        assert!(lines.iter().any(|line| line.ends_with(":4: Agent")), "{word}");
        assert!(!word.contains("subagent"), "{word}");
        assert!(!word.contains("agent_x"), "{word}");

        let line = grep(
            &paths,
            &args(json!({"pattern": "agent", "line_regexp": true, "include": "words.py"})),
        )
        .expect("整行匹配");
        assert!(line.contains(":1: agent"), "{line}");
        assert!(!line.contains("agent_x"), "{line}");
    }

    #[test]
    fn invert_match_lists_non_matching_lines() {
        let (paths, _root) = workspace("invert");
        let text = grep(
            &paths,
            &args(json!({"pattern": "Agent", "invert_match": true, "path": "notes.md"})),
        )
        .expect("反向匹配");
        assert!(text.contains("notes.md:2: 无关内容"), "{text}");
        assert!(!text.contains("Agent 说明"), "{text}");
    }

    #[test]
    fn smart_case_ignores_case_only_for_lowercase_patterns() {
        let (paths, _root) = workspace("smart");
        let lower = grep(
            &paths,
            &args(json!({"pattern": "agent", "smart_case": true, "include": "agent.py"})),
        )
        .expect("小写模式按不敏感");
        assert!(lower.contains("class Agent:"), "{lower}");

        let upper = grep(
            &paths,
            &args(json!({"pattern": "AGENT", "smart_case": true})),
        )
        .expect("含大写按敏感");
        assert_eq!(upper, "未找到匹配结果。");
    }

    #[test]
    fn max_count_limits_matches_per_file() {
        let (paths, root) = workspace("max-count");
        std::fs::write(root.join("many.txt"), "hit\nhit\nhit\n").expect("写文件");
        let text = grep(
            &paths,
            &args(json!({"pattern": "hit", "max_count": 2, "include": "many.txt"})),
        )
        .expect("每文件上限");
        assert_eq!(text.lines().count(), 2, "{text}");
    }

    #[test]
    fn before_and_after_context_are_independent() {
        let (paths, _root) = workspace("context-split");
        let text = grep(
            &paths,
            &args(json!({"pattern": "def run", "before_context": 1, "after_context": 0})),
        )
        .expect("只给前文");
        assert!(text.contains(&format!("{}-1- class Agent:", native("src/agent.py"))), "{text}");
        assert!(!text.contains(":3:"), "{text}");
    }

    #[test]
    fn only_matching_prints_each_match() {
        let (paths, _root) = workspace("only");
        let text = grep(
            &paths,
            &args(json!({"pattern": r"\w+", "only_matching": true, "include": "helper.py"})),
        )
        .expect("只输出匹配片段");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("{}:1: def", native("src/helper.py")), "{text}");
        assert!(lines.iter().any(|line| line.ends_with(":1: helper")), "{text}");
    }

    #[test]
    fn replace_rewrites_each_matching_line() {
        let (paths, _root) = workspace("replace");
        let text = grep(
            &paths,
            &args(json!({"pattern": "Agent", "replace": "Robot", "include": "agent.py"})),
        )
        .expect("替换展示");
        assert!(text.contains("class Robot:"), "{text}");
    }

    #[test]
    fn count_matches_counts_every_occurrence() {
        let (paths, root) = workspace("count-matches");
        std::fs::write(root.join("pair.txt"), "aa aa\n").expect("写文件");
        let counts = grep(
            &paths,
            &args(json!({"pattern": "aa", "count_matches": true, "include": "pair.txt"})),
        )
        .expect("匹配条数");
        assert_eq!(counts, "pair.txt: 2");
    }

    #[test]
    fn files_without_match_lists_files_with_no_hit() {
        let (paths, _root) = workspace("without-match");
        let text = grep(
            &paths,
            &args(json!({"pattern": "Agent", "files_without_match": true})),
        )
        .expect("无命中文件");
        assert_eq!(text, native("src/helper.py"));
    }

    #[test]
    fn glob_and_type_filters_narrow_the_walk() {
        let (paths, _root) = workspace("walk-filters");
        std::fs::write(_root.join("src").join("notes.txt"), "Agent txt\n").expect("写文件");

        let globbed = grep(&paths, &args(json!({"pattern": "Agent", "glob": "*.txt"})))
            .expect("glob 过滤");
        assert!(globbed.contains(".txt"), "{globbed}");
        assert!(!globbed.contains(".py"), "{globbed}");

        let negated = grep(
            &paths,
            &args(json!({"pattern": "Agent", "glob": ["*.py", "!agent.py"]})),
        )
        .expect("排除 glob");
        assert!(!negated.contains("agent.py"), "{negated}");
        assert!(negated.contains("helper") || negated == "未找到匹配结果。", "{negated}");

        let typed = grep(&paths, &args(json!({"pattern": "Agent", "type": "py"})))
            .expect("类型过滤");
        assert!(typed.contains(".py"), "{typed}");
        assert!(!typed.contains("notes.md"), "{typed}");
    }

    #[test]
    fn hidden_and_no_ignore_switch_the_walk() {
        let (paths, root) = workspace("walk-hidden");
        std::fs::write(root.join(".hidden.txt"), "Agent hidden\n").expect("写隐藏文件");
        std::fs::write(root.join(".gitignore"), "skipme.txt\n").expect("写忽略文件");
        std::fs::write(root.join("skipme.txt"), "Agent ignored\n").expect("写忽略目标");

        let default_run = grep(&paths, &args(json!({"pattern": "hidden"}))).expect("默认含隐藏");
        assert!(default_run.contains(".hidden.txt"), "{default_run}");

        let skipped = grep(
            &paths,
            &args(json!({"pattern": "hidden", "hidden": false})),
        )
        .expect("hidden=false");
        assert_eq!(skipped, "未找到匹配结果。");

        let ignored = grep(&paths, &args(json!({"pattern": "ignored"}))).expect("默认读忽略");
        assert_eq!(ignored, "未找到匹配结果。");

        let unignored = grep(
            &paths,
            &args(json!({"pattern": "ignored", "no_ignore": true})),
        )
        .expect("no_ignore");
        assert!(unignored.contains("skipme.txt"), "{unignored}");
    }

    #[test]
    fn max_depth_and_max_filesize_bound_the_walk() {
        let (paths, root) = workspace("walk-bounds");
        std::fs::write(root.join("src").join("deep.txt"), "Agent deep\n").expect("写文件");

        let shallow = grep(
            &paths,
            &args(json!({"pattern": "Agent", "max_depth": 1, "include": "deep.txt"})),
        )
        .expect("深度限制");
        assert_eq!(shallow, "未找到匹配结果。");

        let sized = grep(
            &paths,
            &args(json!({"pattern": "Agent", "max_filesize": 5, "include": "notes.md"})),
        )
        .expect("大小限制");
        assert_eq!(sized, "未找到匹配结果。");
    }

    #[test]
    fn multiline_matches_across_line_breaks() {
        let (paths, root) = workspace("multiline");
        std::fs::write(root.join("block.txt"), "alpha\nbeta\n").expect("写文件");
        let text = grep(
            &paths,
            &args(json!({
                "pattern": "alpha\\nbeta",
                "multiline": true,
                "include": "block.txt"
            })),
        )
        .expect("跨行匹配");
        assert!(text.contains("block.txt:1:"), "{text}");
        assert!(text.contains("alpha\\nbeta"), "{text}");
    }

    #[test]
    fn line_number_column_and_byte_offset_shape_the_prefix() {
        let (paths, _root) = workspace("prefix");
        let no_number = grep(
            &paths,
            &args(json!({"pattern": "Agent", "line_number": false, "include": "notes.md"})),
        )
        .expect("去掉行号");
        assert_eq!(no_number, "notes.md: Agent 说明");

        let column = grep(
            &paths,
            &args(json!({"pattern": "说明", "column": true, "include": "notes.md"})),
        )
        .expect("列号");
        assert!(column.starts_with("notes.md:1:7: "), "{column}");

        // rg 的 `--byte-offset` 给行首偏移；只有配合 `-o` 才给匹配片段自身的偏移。
        let offset = grep(
            &paths,
            &args(json!({"pattern": "说明", "byte_offset": true, "include": "notes.md"})),
        )
        .expect("字节偏移");
        assert!(offset.starts_with("notes.md:1:0: "), "{offset}");

        let snippet_offset = grep(
            &paths,
            &args(json!({
                "pattern": "说明",
                "byte_offset": true,
                "only_matching": true,
                "include": "notes.md"
            })),
        )
        .expect("片段偏移");
        assert!(snippet_offset.starts_with("notes.md:1:6: "), "{snippet_offset}");
    }

    #[test]
    fn max_columns_omits_or_previews_long_lines() {
        let (paths, root) = workspace("max-columns");
        std::fs::write(root.join("wide.txt"), format!("Agent{}\n", "x".repeat(60)))
            .expect("写文件");
        let omitted = grep(
            &paths,
            &args(json!({"pattern": "Agent", "max_columns": 20, "include": "wide.txt"})),
        )
        .expect("整行省略");
        assert!(omitted.contains(OMITTED_LONG_LINE), "{omitted}");

        let preview = grep(
            &paths,
            &args(json!({
                "pattern": "Agent",
                "max_columns": 20,
                "max_columns_preview": true,
                "include": "wide.txt"
            })),
        )
        .expect("预览截断");
        assert!(preview.contains("[... omitted end of long line]"), "{preview}");
    }

    #[test]
    fn stats_appends_a_summary_line() {
        let (paths, _root) = workspace("stats");
        let text = grep(&paths, &args(json!({"pattern": "Agent", "stats": true})))
            .expect("统计");
        assert!(text.contains("统计：扫描 "), "{text}");
        assert!(text.contains("条匹配。"), "{text}");
    }

    #[test]
    fn context_separator_splits_non_contiguous_chunks() {
        let (paths, root) = workspace("separator");
        std::fs::write(
            root.join("spread.txt"),
            "hit\nskip\nskip\nskip\nhit\n",
        )
        .expect("写文件");
        let text = grep(
            &paths,
            &args(json!({
                "pattern": "hit",
                "context_lines": 1,
                "context_separator": "===",
                "include": "spread.txt"
            })),
        )
        .expect("分块分隔");
        assert!(text.contains("\n===\n"), "{text}");

        let disabled = grep(
            &paths,
            &args(json!({
                "pattern": "hit",
                "context_lines": 1,
                "context_separator": false,
                "include": "spread.txt"
            })),
        )
        .expect("关闭分隔符");
        assert!(!disabled.contains("==="), "{disabled}");
    }
}
