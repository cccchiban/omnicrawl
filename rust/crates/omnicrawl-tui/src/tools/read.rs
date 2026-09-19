//! `read` 工具：行窗口、超长行截断、行号与续读 footer。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `read_file_result` /
//! `_read_file_window` / `_format_read_lines`。两种定位模式里，`text` 片段定位已实现；
//! `function_name` 需要 AST 与声明括号扫描（Python 侧 200 余行），本批尚未搬入，
//! 调用时会得到带 `FS_UNSUPPORTED_FEATURE` 的明确错误而不是静默读错内容。

use std::path::Path;

use serde_json::{Map, Value};

use super::arguments::{limited_int, optional_text};
use super::error::{ToolError, ToolOutcome};
use super::paths::WorkspacePaths;
use super::sample::split_lines_keepends;

pub const READ_MAX_LINES: i64 = 500;
pub const READ_MAX_LINE_LENGTH: usize = 2_000;
/// MCP Server 路径使用的整读上限；主 Agent 路径不设上限（与 Python 一致）。
pub const MAX_FILE_READ_CHARS: usize = 200_000;
const READ_START_LINE_MAX: i64 = 100_000;
const READ_CONTEXT_LINES_MAX: i64 = 200;
const BUNDLED_DOC_PREFIX: &str = "omnicrawl://docs/";

pub fn read(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolOutcome {
    let raw_path = optional_text(arguments, "path");
    if raw_path.starts_with(BUNDLED_DOC_PREFIX) {
        return Err(ToolError::coded(
            "内置文档（omnicrawl://docs/）读取在当前 Rust 宿主尚未实现。",
            "FS_UNSUPPORTED_FEATURE",
            false,
        ));
    }
    let path = paths.safe_path(&raw_path)?;
    let function_name = optional_text(arguments, "function_name");
    let snippet = optional_text(arguments, "text");
    if !function_name.is_empty() && !snippet.is_empty() {
        return Err(ToolError::new("function_name 和 text 不能同时指定。"));
    }
    let max_lines = limited_int(arguments, "max_lines", READ_MAX_LINES, 1, READ_MAX_LINES);
    let display = paths.relative(&path);
    if !path.is_file() {
        return Err(ToolError::new(format!("不是文件：{display}")));
    }
    if !function_name.is_empty() {
        return Err(ToolError::coded(
            format!(
                "function_name 定位在当前 Rust 宿主尚未实现：{function_name}；请改用 text 片段或 start_line/max_lines 读取。"
            ),
            "FS_UNSUPPORTED_FEATURE",
            false,
        ));
    }
    if !snippet.is_empty() {
        return read_snippet(paths, &path, &snippet, arguments, max_lines);
    }
    let start_line = limited_int(arguments, "start_line", 1, 1, READ_START_LINE_MAX);
    read_window(paths, &path, start_line, max_lines)
}

/// 整文件读取（片段定位与其它工具共用）；`max_chars` 非空时超限截断。
pub fn read_text(path: &Path, max_chars: Option<usize>) -> ToolOutcome {
    let display = path.to_string_lossy().to_string();
    let bytes = std::fs::read(path)
        .map_err(|error| ToolError::new(format!("读取文件失败：{display}，{error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("文件不是 UTF-8 文本或包含二进制内容：{display}")))?;
    match max_chars {
        Some(limit) if text.chars().count() > limit => {
            let kept: String = text.chars().take(limit).collect();
            Ok(format!("{kept}\n... 文件内容已截断。"))
        }
        _ => Ok(text),
    }
}

fn read_window(
    paths: &WorkspacePaths,
    path: &Path,
    start_line: i64,
    max_lines: i64,
) -> ToolOutcome {
    let display = paths.relative(path);
    let bytes = std::fs::read(path)
        .map_err(|error| ToolError::new(format!("读取文件失败：{display}，{error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("文件不是 UTF-8 文本或包含二进制内容：{display}")))?;

    let mut selected: Vec<(usize, String)> = Vec::new();
    let mut total_lines = 0usize;
    for (index, raw_line) in split_lines_keepends(&text).into_iter().enumerate() {
        let line_no = index + 1;
        total_lines = line_no;
        if (line_no as i64) < start_line {
            continue;
        }
        if selected.len() as i64 >= max_lines {
            continue;
        }
        let line = raw_line.trim_end_matches(['\r', '\n']);
        selected.push((line_no, truncate_read_line(line)));
    }

    // 起始行超出文件末尾时直接报错，而不是返回无意义的空窗口。
    if start_line > total_lines as i64 && total_lines > 0 {
        return Err(out_of_range_error(&display, start_line, total_lines));
    }
    if total_lines == 0 && start_line > 1 {
        return Err(out_of_range_error(&display, start_line, 0));
    }

    let end_line = selected
        .last()
        .map(|(line_no, _)| *line_no as i64)
        .unwrap_or(start_line - 1);
    let truncated = end_line < total_lines as i64;
    let footer = format_read_footer(start_line, end_line, total_lines, truncated);
    let mut numbered: Vec<String> = selected
        .iter()
        .map(|(line_no, line)| format!("{line_no}: {line}"))
        .collect();
    if numbered.is_empty() {
        numbered.push("文件为空，或指定范围没有内容。".to_string());
    }
    Ok(format!("{}\n{footer}", numbered.join("\n")))
}

fn read_snippet(
    paths: &WorkspacePaths,
    path: &Path,
    snippet: &str,
    arguments: &Map<String, Value>,
    max_lines: i64,
) -> ToolOutcome {
    let display = paths.relative(path);
    let text = read_text(path, None)?;
    let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    let Some(offset) = text.find(snippet) else {
        return Err(ToolError::new(format!(
            "未找到指定文字片段（文件：{display}）。"
        )));
    };
    let context_lines = limited_int(arguments, "context_lines", 20, 0, READ_CONTEXT_LINES_MAX);
    let anchor_start_line = text[..offset].matches('\n').count() as i64 + 1;
    let last_char_start = offset
        + snippet
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
    let anchor_end_line = text[..last_char_start].matches('\n').count() as i64 + 1;
    let start_line = (anchor_start_line - context_lines).max(1);
    let end_line = (anchor_end_line + context_lines).min(lines.len() as i64);
    let header = format!(
        "定位：文字片段首次匹配（第 {anchor_start_line}-{anchor_end_line} 行，上下文 {context_lines} 行）"
    );
    Ok(format_read_lines(
        &lines,
        start_line,
        end_line,
        max_lines,
        &header,
        "文字片段上下文超过 max_lines，可提高 max_lines 继续读取。",
    ))
}

/// 定位类读取的行号窗口格式化（Python 侧 `_format_read_lines`）。
fn format_read_lines(
    lines: &[String],
    start_line: i64,
    end_line: i64,
    max_lines: i64,
    header: &str,
    truncation_hint: &str,
) -> String {
    let total_lines = lines.len() as i64;
    let selected_start = start_line.max(1).min(total_lines + 1);
    let selected_end = end_line.max(selected_start - 1).min(total_lines);
    let take = (selected_end.min(selected_start - 1 + max_lines) - (selected_start - 1)).max(0);
    let selected = lines
        .get((selected_start - 1).max(0) as usize..)
        .unwrap_or_default()
        .iter()
        .take(take as usize);

    let mut numbered: Vec<String> = selected
        .enumerate()
        .map(|(index, line)| format!("{}: {}", selected_start + index as i64, line))
        .collect();
    if selected_start <= selected_end && selected_start - 1 + max_lines < selected_end {
        numbered.push(format!("... {truncation_hint}"));
    }
    if numbered.is_empty() {
        numbered.push("文件为空，或指定范围没有内容。".to_string());
    }
    if header.is_empty() {
        numbered.join("\n")
    } else {
        format!("{header}\n{}", numbered.join("\n"))
    }
}

fn truncate_read_line(line: &str) -> String {
    if line.chars().count() <= READ_MAX_LINE_LENGTH {
        return line.to_string();
    }
    let head: String = line.chars().take(READ_MAX_LINE_LENGTH).collect();
    format!("{head}... (line truncated to {READ_MAX_LINE_LENGTH} chars)")
}

fn format_read_footer(
    start_line: i64,
    end_line: i64,
    total_lines: usize,
    truncated: bool,
) -> String {
    if !truncated {
        return format!("(End of file - total {total_lines} lines)");
    }
    format!(
        "(Showing lines {start_line}-{end_line} of {total_lines}. Use start_line={} to continue.)",
        end_line + 1
    )
}

fn out_of_range_error(display: &str, start_line: i64, total_lines: usize) -> ToolError {
    ToolError::new(format!(
        "start_line {start_line} 超出文件范围：{display}（共 {total_lines} 行）。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(name: &str) -> (WorkspacePaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-read-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        (WorkspacePaths::new(&root), root)
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn window_reports_line_numbers_and_end_of_file() {
        let (paths, root) = workspace("window");
        std::fs::write(root.join("a.txt"), "第一行\n第二行\n第三行\n").expect("写测试文件");
        let text = read(&paths, &args(json!({"path": "a.txt"}))).expect("读取应成功");
        assert_eq!(
            text,
            "1: 第一行\n2: 第二行\n3: 第三行\n(End of file - total 3 lines)"
        );
    }

    #[test]
    fn window_truncates_and_points_at_the_next_start_line() {
        let (paths, root) = workspace("truncate");
        std::fs::write(root.join("a.txt"), "1\n2\n3\n4\n5\n").expect("写测试文件");
        let text = read(
            &paths,
            &args(json!({"path": "a.txt", "start_line": 2, "max_lines": 2})),
        )
        .expect("读取应成功");
        assert_eq!(
            text,
            "2: 2\n3: 3\n(Showing lines 2-3 of 5. Use start_line=4 to continue.)"
        );
    }

    #[test]
    fn long_lines_are_marked_truncated() {
        let (paths, root) = workspace("long-line");
        let long = "x".repeat(READ_MAX_LINE_LENGTH + 50);
        std::fs::write(root.join("a.txt"), format!("{long}\n")).expect("写测试文件");
        let text = read(&paths, &args(json!({"path": "a.txt"}))).expect("读取应成功");
        assert!(text.contains("(line truncated to 2000 chars)"), "{text}");
        assert!(text.starts_with(&format!("1: {}", "x".repeat(READ_MAX_LINE_LENGTH))));
    }

    #[test]
    fn errors_match_python_texts() {
        let (paths, root) = workspace("errors");
        std::fs::write(root.join("a.txt"), "one\n").expect("写测试文件");

        let not_a_file = read(&paths, &args(json!({"path": "missing.txt"}))).unwrap_err();
        assert!(
            not_a_file.message.starts_with("不是文件："),
            "{}",
            not_a_file.message
        );

        let out_of_range =
            read(&paths, &args(json!({"path": "a.txt", "start_line": 9}))).unwrap_err();
        assert_eq!(
            out_of_range.message,
            "start_line 9 超出文件范围：a.txt（共 1 行）。"
        );

        let empty_path = read(&paths, &args(json!({"path": " "}))).unwrap_err();
        assert_eq!(empty_path.message, "路径不能为空。");

        let both_modes = read(
            &paths,
            &args(json!({"path": "a.txt", "function_name": "f", "text": "o"})),
        )
        .unwrap_err();
        assert_eq!(both_modes.message, "function_name 和 text 不能同时指定。");

        let unsupported = read(
            &paths,
            &args(json!({"path": "a.txt", "function_name": "f"})),
        )
        .unwrap_err();
        assert_eq!(unsupported.code.as_deref(), Some("FS_UNSUPPORTED_FEATURE"));
    }

    #[test]
    fn snippet_mode_shows_context_lines() {
        let (paths, root) = workspace("snippet");
        let body: String = (1..=10).map(|index| format!("行{index}\n")).collect();
        std::fs::write(root.join("a.txt"), body).expect("写测试文件");
        let text = read(
            &paths,
            &args(json!({"path": "a.txt", "text": "行5", "context_lines": 1, "max_lines": 10})),
        )
        .expect("片段定位应成功");
        assert!(
            text.starts_with("定位：文字片段首次匹配（第 5-5 行，上下文 1 行）"),
            "{text}"
        );
        assert!(text.contains("4: 行4"), "{text}");
        assert!(text.contains("6: 行6"), "{text}");

        let missing = read(&paths, &args(json!({"path": "a.txt", "text": "不存在"}))).unwrap_err();
        assert_eq!(missing.message, "未找到指定文字片段（文件：a.txt）。");
    }

    #[test]
    fn empty_file_renders_placeholder_and_footer() {
        let (paths, root) = workspace("empty");
        std::fs::write(root.join("a.txt"), "").expect("写空文件");
        let text = read(&paths, &args(json!({"path": "a.txt"}))).expect("读取应成功");
        assert_eq!(
            text,
            "文件为空，或指定范围没有内容。\n(End of file - total 0 lines)"
        );
    }
}
