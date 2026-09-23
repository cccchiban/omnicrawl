//! 全屏 TUI 文件变更工具卡的 git 旁注行号 diff 渲染（对映 `rendering/tool_diff.py`）。
//!
//! 展示策略（用户选定，与 Python 逐条一致）：
//! - 1G4：旁注行号 diff；
//! - 2A：仅全屏工具卡，不改确认框/工具返回协议；
//! - 3A：write_file 覆盖且无旧内容时显示 rewrite +N lines，不编造假 diff；
//! - 4B：Edit_file 正文保留旁注行号 diff 预览，结果区只显示“替换 N 处”摘要。
//!
//! 纯格式化，不依赖终端框架；opcodes 来自 [`super::difflib`]（对映 `difflib.SequenceMatcher`）。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::ui::fullscreen::rendering::difflib::{SequenceMatcher, Tag};
use crate::ui::fullscreen::terminal::theme::{
    ACCENT_AMBER, ACCENT_BLUE, ACCENT_GREEN, ACCENT_RED, TEXT_FAINT, TEXT_MUTED, TEXT_PRIMARY,
    TEXT_SECONDARY, TOOL_TEXT,
};
use crate::ui::fullscreen::text::StyledText;
use crate::ui::fullscreen::tool_labels::{
    format_duration, format_tool_status, tool_display, ToolStatus,
};

pub const ASK_USER_TOOL_NAME: &str = "ask_user";

pub const FILE_CHANGE_TOOLS: &[&str] = &["write_file", "Edit_file"];
/// 豁免“原始输出 + 五行折叠”规则的工具：保留文件变更预览（diff/rewrite 摘要）。
pub const FULL_BODY_TOOLS: &[&str] = FILE_CHANGE_TOOLS;
/// 记忆类工具（作用域由 `scope` 参数决定）。
pub const MEMORY_TOOLS: &[&str] = &[
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
];
/// 知识库类工具：正文对用户无展示价值，与记忆工具一起隐藏。
pub const KB_TOOLS: &[&str] = &["kb_search", "kb_read", "kb_write", "kb_append", "kb_list"];
/// 正文对用户没有展示价值、完全隐藏的工具（只保留标题行）。
pub const HIDDEN_BODY_TOOLS: &[&str] = &[
    "read",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
    "kb_search",
    "kb_read",
    "kb_write",
    "kb_append",
    "kb_list",
];
pub const MAX_DIFF_BODY_LINES: usize = 80;
pub const MAX_PATH_CHARS: usize = 48;
pub const MAX_PREVIEW_CHARS_PER_LINE: usize = 160;

pub const COLOR_ADD: &str = ACCENT_GREEN;
pub const COLOR_DEL: &str = ACCENT_RED;
pub const COLOR_MOD: &str = ACCENT_AMBER;
pub const COLOR_META: &str = TEXT_MUTED;
pub const COLOR_GUTTER: &str = TEXT_FAINT;
pub const COLOR_CTX: &str = TEXT_SECONDARY;
pub const COLOR_HUNK: &str = ACCENT_BLUE;
pub const COLOR_TITLE: &str = ACCENT_AMBER;

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("静态正则"))
}

/// 压缩工具标题中的路径或搜索目标，避免长参数撑破终端。
pub fn compact_title_value(value: &str, max_chars: usize) -> String {
    let text = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "(未指定)".to_string();
    }
    if text.chars().count() <= max_chars {
        return text;
    }
    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// 从 read 的行号输出中提取实际返回内容的首尾源码行号。
pub fn read_result_line_range(result_text: &str) -> Option<(i64, i64)> {
    static LINE_RE: OnceLock<Regex> = OnceLock::new();
    if result_text.is_empty() {
        return None;
    }
    let pattern = regex(&LINE_RE, r"(?m)^\s*(\d+):\s");
    let numbers: Vec<i64> = pattern
        .captures_iter(result_text)
        .filter_map(|captures| captures.get(1)?.as_str().parse().ok())
        .collect();
    match (numbers.first(), numbers.last()) {
        (Some(first), Some(last)) => Some((*first, *last)),
        _ => None,
    }
}

/// 把目录结果压缩为标题摘要，避免把完整列表挤进消息流。
pub fn list_result_summary(result_text: &str) -> Option<String> {
    if result_text.is_empty() {
        return None;
    }
    let lines: Vec<&str> = result_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() || lines == ["目录为空。"] {
        return Some("0 项".to_string());
    }
    let truncated = lines.iter().any(|line| line.starts_with("..."));
    let visible_count = lines.iter().filter(|line| !line.starts_with("...")).count();
    Some(format!(
        "{visible_count}{} 项",
        if truncated { "+" } else { "" }
    ))
}

/// 对映 `str(args[key] or "")`：只接受可无损转文本的标量。
fn arg_text(args: &Value, key: &str) -> String {
    match args.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(_) => String::new(),
    }
}

/// 返回可放在工具标题末尾的公开操作上下文。
pub fn tool_title_context(tool_name: &str, arguments: &Value, _result_text: &str) -> String {
    let operation = tool_operation(tool_name);
    if operation == "bash" || operation == "powershell" {
        // 命令完整展示，不做长度截断：命令本身是用户最关心的信息。
        return arg_text(arguments, "command").trim().to_string();
    }
    if operation == "monitor" {
        let action = arg_text(arguments, "action").trim().to_string();
        let command = arg_text(arguments, "command").trim().to_string();
        let monitor_id = arg_text(arguments, "monitor_id").trim().to_string();
        let mut context = if action.is_empty() {
            "任务".to_string()
        } else {
            action
        };
        if !command.is_empty() {
            context.push_str(&format!("  {}", compact_title_value(&command, 48)));
        } else if !monitor_id.is_empty() {
            context.push_str(&format!("  {}", compact_title_value(&monitor_id, 32)));
        }
        return context;
    }
    if operation == "subagent" {
        let action = arg_text(arguments, "action").trim().to_string();
        let task_count = arguments
            .get("tasks")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let mut context = if action.is_empty() {
            "任务".to_string()
        } else {
            action
        };
        if task_count > 0 {
            context.push_str(&format!("  {task_count} 项"));
        }
        return context;
    }
    if operation == "windows_screenshot" {
        let target = arg_text(arguments, "target").trim().to_string();
        return if target.is_empty() {
            "截图".to_string()
        } else {
            target
        };
    }
    if MEMORY_TOOLS.contains(&operation) {
        let query = arg_text(arguments, "query").trim().to_string();
        if !query.is_empty() {
            return compact_title_value(&query, 48);
        }
        if let Some(ids) = arguments.get("memory_ids").and_then(Value::as_array) {
            return format!("{} 条记忆", ids.len());
        }
    }
    String::new()
}

/// 返回工具状态的语义色，避免整行被工具色覆盖。
pub fn status_color(status: &str) -> &'static str {
    match status {
        "成功" => ACCENT_GREEN,
        "失败" => ACCENT_RED,
        "调用中" => ACCENT_BLUE,
        "等待确认" => ACCENT_AMBER,
        "已取消" => TEXT_MUTED,
        _ => TEXT_MUTED,
    }
}

/// 向标题追加「· 状态 · 耗时」暗色尾段（方案6：状态只由行首色点表达）。
fn append_tail(rendered: &mut StyledText, status_display: &ToolStatus, duration_seconds: f64) {
    rendered.push(" · ", COLOR_META);
    rendered.push(
        &format!("{} {}", status_display.icon, status_display.label),
        TEXT_MUTED,
    );
    rendered.push(" · ", COLOR_META);
    rendered.push(&format_duration(duration_seconds), TEXT_MUTED);
}

/// 按增删语义色渲染标题中的行数统计（对映 Python `re.split` 保留捕获组）。
fn append_stats(rendered: &mut StyledText, stats_label: &str) {
    static STATS_RE: OnceLock<Regex> = OnceLock::new();
    let pattern = regex(&STATS_RE, r"([+-]\d+)");
    let mut last_end = 0usize;
    let mut parts: Vec<String> = Vec::new();
    for captures in pattern.captures_iter(stats_label) {
        let Some(matched) = captures.get(0) else {
            continue;
        };
        if matched.start() > last_end {
            parts.push(stats_label[last_end..matched.start()].to_string());
        }
        if let Some(group) = captures.get(1) {
            parts.push(group.as_str().to_string());
        }
        last_end = matched.end();
    }
    if last_end < stats_label.len() {
        parts.push(stats_label[last_end..].to_string());
    }
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let style = if part.starts_with('+') {
            COLOR_ADD
        } else if part.starts_with('-') {
            COLOR_DEL
        } else {
            COLOR_META
        };
        rendered.push(&part, style);
    }
}

/// 返回工具的末级操作名，兼容带命名空间的工具名。
pub fn tool_operation(tool_name: &str) -> &str {
    tool_name.rsplit('.').next().unwrap_or(tool_name)
}

/// 生成读取/搜索/目录列表工具的单行摘要（方案6：色点 + 原名 + 暗色上下文）。
fn workspace_tool_title(
    operation: &str,
    display_name: &str,
    arguments: &Value,
    result_text: &str,
    status: &str,
    status_display: &ToolStatus,
    duration_seconds: f64,
) -> Option<StyledText> {
    let mut rendered = StyledText::new();
    rendered.push("● ", status_color(status));
    match operation {
        "list" => {
            let raw_path = arg_text(arguments, "path");
            let path = if raw_path.is_empty() {
                "."
            } else {
                raw_path.as_str()
            };
            rendered.push(display_name, TEXT_PRIMARY);
            rendered.push(
                &format!(" {}", compact_title_value(path, MAX_PATH_CHARS)),
                TEXT_MUTED,
            );
            match list_result_summary(result_text) {
                Some(summary) => rendered.push(&format!(" · {summary}"), TEXT_MUTED),
                None => rendered.push(" · 目录", TEXT_MUTED),
            }
        }
        "read" => {
            let raw_path = arg_text(arguments, "path");
            let path = if raw_path.is_empty() {
                "(未指定文件)"
            } else {
                raw_path.as_str()
            };
            rendered.push(display_name, TEXT_PRIMARY);
            rendered.push(
                &format!(" {}", compact_title_value(path, MAX_PATH_CHARS)),
                TEXT_MUTED,
            );
            if let Some((first, last)) = read_result_line_range(result_text) {
                rendered.push(&format!(" · 第 {first}-{last} 行"), TEXT_MUTED);
            }
        }
        "read_image" => {
            let raw_path = arg_text(arguments, "path");
            let path = if raw_path.is_empty() {
                "(未指定图片)"
            } else {
                raw_path.as_str()
            };
            rendered.push(display_name, TEXT_PRIMARY);
            rendered.push(
                &format!(" {}", compact_title_value(path, MAX_PATH_CHARS)),
                TEXT_MUTED,
            );
            rendered.push(" · 图片", TEXT_MUTED);
        }
        "find" | "grep" => {
            let raw_path = arg_text(arguments, "path");
            let path = if raw_path.is_empty() {
                "."
            } else {
                raw_path.as_str()
            };
            let pattern = compact_title_value(&arg_text(arguments, "pattern"), 36);
            rendered.push(display_name, TEXT_PRIMARY);
            rendered.push(
                &format!(
                    " {} · 目标: {pattern}",
                    compact_title_value(path, MAX_PATH_CHARS)
                ),
                TEXT_MUTED,
            );
        }
        _ => return None,
    }

    append_tail(&mut rendered, status_display, duration_seconds);
    Some(rendered)
}

/// ask_user 工具卡标题：等待回复/已收到回复 + 耗时。
pub fn ask_user_tool_title(status: &str, duration_seconds: f64) -> Option<StyledText> {
    let (label, color) = match status {
        "等待回复" => ("↘ 等待回复...", ACCENT_BLUE),
        "已收到回复" => ("↗ 已收到回复", ACCENT_GREEN),
        _ => return None,
    };
    let mut rendered = StyledText::new();
    rendered.push("● ", color);
    rendered.push(label, color);
    rendered.push(" · ", COLOR_META);
    rendered.push(&format_duration(duration_seconds), TEXT_MUTED);
    Some(rendered)
}

pub fn is_file_change_tool(tool_name: &str) -> bool {
    FILE_CHANGE_TOOLS.contains(&tool_name)
}

/// 生成折叠/展开标题行。
pub fn tool_disclosure_title(
    tool_name: &str,
    arguments: &Value,
    status: &str,
    duration_seconds: f64,
    _expanded: bool,
    result_text: &str,
) -> StyledText {
    let display = tool_display(tool_name);
    let status_display = format_tool_status(status);
    if tool_name == ASK_USER_TOOL_NAME {
        if let Some(ask_title) = ask_user_tool_title(status, duration_seconds) {
            return ask_title;
        }
    }
    let operation = tool_operation(tool_name);
    if let Some(workspace_title) = workspace_tool_title(
        operation,
        &display.name,
        arguments,
        result_text,
        status,
        &status_display,
        duration_seconds,
    ) {
        return workspace_title;
    }

    let context = tool_title_context(tool_name, arguments, result_text);
    if !is_file_change_tool(tool_name) {
        let mut rendered = StyledText::new();
        rendered.push("● ", status_color(status));
        rendered.push(&display.name, TEXT_PRIMARY);
        if !context.is_empty() {
            rendered.push(" ", COLOR_META);
            rendered.push(&context, TEXT_MUTED);
        }
        append_tail(&mut rendered, &status_display, duration_seconds);
        return rendered;
    }

    let change = describe_file_change(tool_name, arguments, 1);
    let mut rendered = StyledText::new();
    rendered.push("● ", status_color(status));
    rendered.push(&display.name, TEXT_PRIMARY);
    rendered.push(&format!(" {}", change.path_display), TEXT_MUTED);
    rendered.push(" · ", COLOR_META);
    append_stats(&mut rendered, &change.stats_label);
    append_tail(&mut rendered, &status_display, duration_seconds);
    rendered
}

/// fetcher 结果正文：保留汇总/URL/状态/标题，隐藏网页正文内容。
pub fn fetcher_body(result_text: &str) -> StyledText {
    static ITEM_RE: OnceLock<Regex> = OnceLock::new();
    let item_pattern = regex(&ITEM_RE, r"^\d+\.\s");
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping_content = false;
    for line in result_text.lines() {
        if skipping_content {
            // 内容块结束后（下一个条目、失败条目或结尾）恢复保留。
            if item_pattern.is_match(line) || line.starts_with("   失败: ") {
                skipping_content = false;
            } else {
                continue;
            }
        }
        if line.starts_with("   内容: ") {
            skipping_content = true;
            continue;
        }
        kept.push(line);
    }

    let mut rendered = StyledText::new();
    if !kept.is_empty() {
        rendered.push(&kept.join("\n"), TOOL_TEXT);
    }
    rendered
}

/// 生成展开后的正文（不含标题行）。
pub fn tool_disclosure_body(tool_name: &str, arguments: &Value, result_text: &str) -> StyledText {
    let operation = tool_operation(tool_name);
    if FULL_BODY_TOOLS.contains(&operation) {
        return file_change_body(tool_name, arguments, result_text, true);
    }
    if operation == "fetcher" {
        return fetcher_body(result_text);
    }
    if HIDDEN_BODY_TOOLS.contains(&operation) {
        // read 与记忆/知识库类工具的正文不展示给终端用户：正文为空，无提示行。
        return StyledText::new();
    }

    let mut rendered = StyledText::new();
    if !result_text.is_empty() {
        rendered.push(result_text, TOOL_TEXT);
    }
    rendered
}

/// 一次文件变更工具调用的展示摘要。
#[derive(Debug, Clone)]
pub struct FileChangeView {
    pub status_code: String,
    pub status_color: String,
    pub path: String,
    pub path_display: String,
    pub stats_label: String,
    pub body: StyledText,
}

pub fn describe_file_change(
    tool_name: &str,
    arguments: &Value,
    start_line: usize,
) -> FileChangeView {
    let raw_path = arg_text(arguments, "path").trim().to_string();
    let path = if raw_path.is_empty() {
        "(unknown path)".to_string()
    } else {
        raw_path
    };

    if tool_name == "Edit_file" {
        let old_text = arg_text(arguments, "old_text");
        let new_text = arg_text(arguments, "new_text");
        let (body, added, removed) = gutter_diff_text(&old_text, &new_text, start_line);
        return FileChangeView {
            status_code: "M".to_string(),
            status_color: COLOR_MOD.to_string(),
            path_display: compact_path(&path),
            stats_label: format_line_stats(added, removed),
            path,
            body,
        };
    }

    let content = arg_text(arguments, "content");
    let raw_mode = arg_text(arguments, "mode");
    let mode = if raw_mode.is_empty() {
        "overwrite".to_string()
    } else {
        raw_mode.to_lowercase()
    };
    let line_count = if content.is_empty() {
        0
    } else {
        content.lines().count()
    };

    if mode == "append" {
        // 追加写：无旧内容可比，展示 new content only 预览。
        let body = preview_as_added_lines(&content, "append");
        return FileChangeView {
            status_code: "M".to_string(),
            status_color: COLOR_MOD.to_string(),
            path_display: compact_path(&path),
            stats_label: format!("append +{line_count} lines"),
            path,
            body,
        };
    }

    // 3A：覆盖写无旧内容 → rewrite 摘要，不编造删除侧
    let body = preview_as_added_lines(&content, "rewrite");
    FileChangeView {
        status_code: "M".to_string(),
        status_color: COLOR_MOD.to_string(),
        path_display: compact_path(&path),
        stats_label: format!("rewrite +{line_count} lines"),
        path,
        body,
    }
}

/// 从 Edit_file 返回文本中提取“替换 N 处”摘要，丢弃带行号上下文。
pub fn edit_result_summary(result_text: &str) -> String {
    static SUMMARY_RE: OnceLock<Regex> = OnceLock::new();
    if result_text.is_empty() {
        return String::new();
    }
    let pattern = regex(&SUMMARY_RE, r"替换\s*\d+\s*处");
    pattern
        .find(result_text)
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_default()
}

/// 从 Edit_file 返回文本的上下文块解析替换位置的真实起始文件行号。
pub fn edit_start_line(result_text: &str, new_text: &str) -> usize {
    static CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
    static NUMBERED_RE: OnceLock<Regex> = OnceLock::new();
    if result_text.is_empty() {
        return 1;
    }
    let lines: Vec<&str> = result_text.lines().collect();
    let mut context_lines = 2usize;
    let mut block_start: Option<usize> = None;
    for (index, line) in lines.iter().enumerate() {
        if line.contains("首个替换位置上下文") {
            block_start = Some(index + 1);
            if let Some(captures) = regex(&CONTEXT_RE, r"前后各\s*(\d+)\s*行").captures(line) {
                if let Some(count) = captures
                    .get(1)
                    .and_then(|group| group.as_str().parse().ok())
                {
                    context_lines = count;
                }
            }
            break;
        }
    }
    let Some(block_start) = block_start else {
        return 1;
    };
    let number_pattern = regex(&NUMBERED_RE, r"^\s*(\d+):\s*(.*)$");
    let mut numbered: Vec<(usize, String)> = Vec::new();
    for line in lines.iter().skip(block_start) {
        if let Some(captures) = number_pattern.captures(line) {
            let lineno = captures
                .get(1)
                .and_then(|group| group.as_str().parse().ok())
                .unwrap_or(0);
            let content = captures
                .get(2)
                .map(|group| group.as_str().trim().to_string())
                .unwrap_or_default();
            numbered.push((lineno, content));
        }
    }
    if numbered.is_empty() {
        return 1;
    }
    let new_lines: Vec<&str> = new_text.lines().collect();
    let new_first = new_lines.first().map(|line| line.trim()).unwrap_or("");
    if !new_first.is_empty() {
        for (lineno, content) in &numbered {
            if content == new_first {
                return *lineno;
            }
        }
    }
    // 回退：跳过前置上下文行，取替换位置首行；否则取块首行。
    if numbered.len() > context_lines {
        return numbered[context_lines].0;
    }
    numbered[0].0
}

/// 文件变更正文：`工具：<name>` + 变更预览（+ 结果摘要）。
pub fn file_change_body(
    tool_name: &str,
    arguments: &Value,
    result_text: &str,
    include_result: bool,
) -> StyledText {
    let start_line = if tool_name == "Edit_file" {
        edit_start_line(result_text, &arg_text(arguments, "new_text"))
    } else {
        1
    };
    let change = describe_file_change(tool_name, arguments, start_line);
    let mut rendered = StyledText::new();
    rendered.push("工具：", COLOR_META);
    rendered.push(tool_name, COLOR_CTX);
    rendered.push("\n", "");
    rendered.append_text(&change.body);
    if !include_result || result_text.trim().is_empty() {
        return rendered;
    }
    let plain = rendered.plain();
    if !plain.is_empty() && !plain.ends_with('\n') {
        rendered.push("\n", "");
    }
    if tool_name == "Edit_file" {
        let summary = edit_result_summary(result_text);
        if !summary.is_empty() {
            rendered.push("结果：", COLOR_META);
            rendered.push(&summary, COLOR_META);
        }
    } else {
        rendered.push("结果：", COLOR_META);
        rendered.push(result_text.trim(), COLOR_META);
    }
    rendered
}

pub fn format_line_stats(added: usize, removed: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if added > 0 {
        parts.push(format!("+{added}"));
    }
    if removed > 0 {
        parts.push(format!("-{removed}"));
    }
    if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(" ")
    }
}

pub fn compact_path(path: &str) -> String {
    let text = path.replace('\\', "/");
    if text.chars().count() <= MAX_PATH_CHARS {
        return text;
    }
    let tail: String = text
        .chars()
        .rev()
        .take(MAX_PATH_CHARS - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// 无旧文件时，把新内容以 + 行预览；不伪造 - 行。
pub fn preview_as_added_lines(content: &str, header: &str) -> StyledText {
    let mut rendered = StyledText::new();
    rendered.push(
        &format!("@@ {header} · showing new content only @@\n"),
        COLOR_HUNK,
    );
    if content.is_empty() {
        rendered.push("   1 │ ", COLOR_GUTTER);
        rendered.push("+ ", &format!("{COLOR_ADD} bold"));
        rendered.push("(empty)\n", COLOR_ADD);
        return rendered;
    }

    let lines: Vec<&str> = content.lines().collect();
    let visible = lines.len().min(MAX_DIFF_BODY_LINES);
    for (index, line) in lines.iter().take(visible).enumerate() {
        rendered.push(&format!("{:>4} │ ", index + 1), COLOR_GUTTER);
        rendered.push("+ ", &format!("{COLOR_ADD} bold"));
        rendered.push(&format!("{}\n", clip_line(line)), COLOR_ADD);
    }
    let omitted = lines.len() - visible;
    if omitted > 0 {
        rendered.push(&format!(" ... {omitted} more lines omitted\n"), COLOR_META);
    }
    rendered
}

/// 把 old/new 渲染成旁注行号 diff，并返回完整 diff 的 `(+added, -removed)`。
pub fn gutter_diff_text(
    old_text: &str,
    new_text: &str,
    start_line: usize,
) -> (StyledText, usize, usize) {
    let old_lines: Vec<String> = old_text.lines().map(str::to_string).collect();
    let new_lines: Vec<String> = new_text.lines().map(str::to_string).collect();
    let mut matcher = SequenceMatcher::new(&old_lines, &new_lines);
    let mut rendered = StyledText::new();
    rendered.push("@@ snippet @@\n", COLOR_HUNK);

    // 完整 diff 的增删统计：单独遍历 opcode，不受渲染行数上限影响。
    let (added, removed) = full_change_counts(&mut matcher);

    let mut body_lines = 0usize;
    let mut old_no = start_line;
    let mut new_no = start_line;
    let mut truncated = false;

    for opcode in matcher.get_opcodes() {
        if body_lines >= MAX_DIFF_BODY_LINES {
            truncated = true;
            break;
        }

        if opcode.tag == Tag::Equal {
            for line in &old_lines[opcode.i1..opcode.i2] {
                if body_lines >= MAX_DIFF_BODY_LINES {
                    truncated = true;
                    break;
                }
                append_gutter_line(&mut rendered, old_no, " ", line, COLOR_CTX);
                body_lines += 1;
                old_no += 1;
                new_no += 1;
            }
            continue;
        }

        if matches!(opcode.tag, Tag::Delete | Tag::Replace) {
            for line in &old_lines[opcode.i1..opcode.i2] {
                if body_lines >= MAX_DIFF_BODY_LINES {
                    truncated = true;
                    break;
                }
                append_gutter_line(&mut rendered, old_no, "-", line, COLOR_DEL);
                body_lines += 1;
                old_no += 1;
            }
        }

        if truncated {
            break;
        }

        if matches!(opcode.tag, Tag::Insert | Tag::Replace) {
            for line in &new_lines[opcode.j1..opcode.j2] {
                if body_lines >= MAX_DIFF_BODY_LINES {
                    truncated = true;
                    break;
                }
                append_gutter_line(&mut rendered, new_no, "+", line, COLOR_ADD);
                body_lines += 1;
                new_no += 1;
            }
        }
    }

    if truncated {
        rendered.push(" ... diff truncated\n", COLOR_META);
    }
    if added == 0 && removed == 0 {
        rendered.push(" (no textual changes)\n", COLOR_META);
    }

    (rendered, added, removed)
}

/// 统计完整 diff 的真实增删行数（不受预览行数上限影响）。
fn full_change_counts(matcher: &mut SequenceMatcher) -> (usize, usize) {
    use crate::ui::fullscreen::rendering::difflib::Tag;
    let mut added = 0usize;
    let mut removed = 0usize;
    for opcode in matcher.get_opcodes() {
        match opcode.tag {
            Tag::Delete => removed += opcode.i2 - opcode.i1,
            Tag::Insert => added += opcode.j2 - opcode.j1,
            Tag::Replace => {
                removed += opcode.i2 - opcode.i1;
                added += opcode.j2 - opcode.j1;
            }
            Tag::Equal => {}
        }
    }
    (added, removed)
}

fn append_gutter_line(
    rendered: &mut StyledText,
    line_no: usize,
    marker: &str,
    line: &str,
    color: &str,
) {
    rendered.push(&format!("{line_no:>4} │ "), COLOR_GUTTER);
    match marker {
        "+" => rendered.push("+ ", &format!("{COLOR_ADD} bold")),
        "-" => rendered.push("- ", &format!("{COLOR_DEL} bold")),
        _ => rendered.push("  ", COLOR_GUTTER),
    }
    rendered.push(&format!("{}\n", clip_line(line)), color);
}

fn clip_line(line: &str) -> String {
    let text = line.replace('\t', "    ");
    if text.chars().count() <= MAX_PREVIEW_CHARS_PER_LINE {
        return text;
    }
    let head: String = text.chars().take(MAX_PREVIEW_CHARS_PER_LINE - 1).collect();
    format!("{head}…")
}

/// 只取标题纯文本（供需要字符串的调用方）。
pub fn plain_tool_title(
    tool_name: &str,
    arguments: &Value,
    status: &str,
    duration_seconds: f64,
    expanded: bool,
    result_text: &str,
) -> String {
    tool_disclosure_title(
        tool_name,
        arguments,
        status,
        duration_seconds,
        expanded,
        result_text,
    )
    .plain()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_title_value_falls_back_and_truncates() {
        assert_eq!(compact_title_value("", 10), "(未指定)");
        assert_eq!(compact_title_value("  a   b ", 10), "a b");
        assert_eq!(compact_title_value("abcdef", 4), "abc…");
    }

    #[test]
    fn read_result_line_range_reads_first_and_last_numbered_lines() {
        assert_eq!(read_result_line_range(""), None);
        assert_eq!(read_result_line_range("无行号文本"), None);
        assert_eq!(
            read_result_line_range("   12: abc\n   13: def\n   15: ghi"),
            Some((12, 15))
        );
    }

    #[test]
    fn list_result_summary_counts_visible_entries() {
        assert_eq!(list_result_summary(""), None);
        assert_eq!(list_result_summary("目录为空。").as_deref(), Some("0 项"));
        assert_eq!(list_result_summary("a.txt\nb.txt").as_deref(), Some("2 项"));
        assert_eq!(
            list_result_summary("a.txt\n...\n（截断）").as_deref(),
            Some("2+ 项")
        );
    }

    #[test]
    fn tool_title_context_covers_special_tools() {
        assert_eq!(
            tool_title_context("bash", &json!({"command": "ls -la"}), ""),
            "ls -la"
        );
        assert_eq!(tool_title_context("bash", &json!({}), ""), "");
        assert_eq!(
            tool_title_context(
                "monitor",
                &json!({"action": "poll", "monitor_id": "m1"}),
                ""
            ),
            "poll  m1"
        );
        assert_eq!(tool_title_context("monitor", &json!({}), ""), "任务");
        assert_eq!(
            tool_title_context("subagent", &json!({"action": "run", "tasks": [{}, {}]}), ""),
            "run  2 项"
        );
        assert_eq!(
            tool_title_context("windows_screenshot", &json!({}), ""),
            "截图"
        );
        assert_eq!(
            tool_title_context("memory_search", &json!({"query": "偏好"}), ""),
            "偏好"
        );
        assert_eq!(
            tool_title_context("memory_read", &json!({"memory_ids": ["a", "b"]}), ""),
            "2 条记忆"
        );
        assert_eq!(tool_title_context("read", &json!({"path": "x"}), ""), "");
    }

    #[test]
    fn status_color_maps_semantic_states() {
        assert_eq!(status_color("成功"), ACCENT_GREEN);
        assert_eq!(status_color("失败"), ACCENT_RED);
        assert_eq!(status_color("调用中"), ACCENT_BLUE);
        assert_eq!(status_color("等待确认"), ACCENT_AMBER);
        assert_eq!(status_color("已取消"), TEXT_MUTED);
        assert_eq!(status_color("等待回复"), TEXT_MUTED);
    }

    #[test]
    fn line_stats_and_path_helpers_match_python() {
        assert_eq!(format_line_stats(0, 0), "0");
        assert_eq!(format_line_stats(3, 0), "+3");
        assert_eq!(format_line_stats(0, 2), "-2");
        assert_eq!(format_line_stats(3, 2), "+3 -2");
        assert_eq!(compact_path("a\\b\\c"), "a/b/c");
        let long = format!("D:/{}", "x".repeat(60));
        let compacted = compact_path(&long);
        assert_eq!(compacted.chars().count(), MAX_PATH_CHARS);
        assert!(compacted.starts_with('…'));
    }

    #[test]
    fn clip_line_expands_tabs_and_truncates() {
        assert_eq!(clip_line("\ta"), "    a");
        let long = "x".repeat(200);
        let clipped = clip_line(&long);
        assert_eq!(clipped.chars().count(), MAX_PREVIEW_CHARS_PER_LINE);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn preview_as_added_lines_marks_empty_and_numbers_rows() {
        let empty = preview_as_added_lines("", "rewrite");
        assert_eq!(
            empty.plain(),
            "@@ rewrite · showing new content only @@\n   1 │ + (empty)\n"
        );
        let body = preview_as_added_lines("first\nsecond", "append");
        let plain = body.plain();
        assert!(plain.starts_with("@@ append · showing new content only @@\n"));
        assert!(plain.contains("   1 │ + first\n"));
        assert!(plain.contains("   2 │ + second\n"));
    }

    #[test]
    fn gutter_diff_reports_counts_and_marks_lines() {
        let (body, added, removed) = gutter_diff_text("a\nb\nc", "a\nx\nc", 1);
        assert_eq!((added, removed), (1, 1));
        let plain = body.plain();
        assert!(plain.starts_with("@@ snippet @@\n"));
        assert!(plain.contains("   2 │ - b\n"));
        assert!(plain.contains("   2 │ + x\n"));
        assert!(!plain.contains("no textual changes"));
    }

    #[test]
    fn gutter_diff_notes_identical_content_and_start_line() {
        let (body, added, removed) = gutter_diff_text("same", "same", 10);
        assert_eq!((added, removed), (0, 0));
        let plain = body.plain();
        assert!(plain.contains("  10 │   same\n"));
        assert!(plain.contains(" (no textual changes)\n"));
    }

    #[test]
    fn gutter_diff_truncates_long_bodies_but_counts_everything() {
        let old_lines: Vec<String> = (0..120).map(|index| format!("line {index}")).collect();
        let mut new_lines = old_lines.clone();
        new_lines.push("tail".to_string());
        let old_text = old_lines.join("\n");
        let new_text = new_lines.join("\n");
        let (body, added, removed) = gutter_diff_text(&old_text, &new_text, 1);
        assert_eq!((added, removed), (1, 0));
        assert!(body.plain().contains(" ... diff truncated\n"));
    }

    #[test]
    fn edit_result_summary_extracts_replacement_count() {
        assert_eq!(edit_result_summary(""), "");
        assert_eq!(edit_result_summary("已替换 3 处后继续"), "替换 3 处");
        assert_eq!(edit_result_summary("没有摘要"), "");
    }

    #[test]
    fn edit_start_line_prefers_new_text_match_then_context_offset() {
        let result = "首个替换位置上下文（第 10-14 行，前后各 2 行）：\n\
                      9: before\n\
                      10: target\n\
                      11: after";
        // new_text 首行命中上下文块中的真实行号。
        assert_eq!(edit_start_line(result, "target"), 10);
        // 匹配失败时跳过前置上下文行（前后各 2 行 → 取第 3 行）。
        assert_eq!(edit_start_line(result, "不存在"), 11);
        assert_eq!(edit_start_line("", "whatever"), 1);
        assert_eq!(edit_start_line("没有上下文块", "whatever"), 1);
    }

    #[test]
    fn describe_file_change_covers_append_rewrite_and_edit() {
        let append = describe_file_change(
            "write_file",
            &json!({"path": "a.txt", "content": "x\ny", "mode": "append"}),
            1,
        );
        assert_eq!(append.status_code, "M");
        assert_eq!(append.stats_label, "append +2 lines");
        assert_eq!(append.path_display, "a.txt");

        let rewrite =
            describe_file_change("write_file", &json!({"path": "a.txt", "content": ""}), 1);
        assert_eq!(rewrite.stats_label, "rewrite +0 lines");

        let edit = describe_file_change(
            "Edit_file",
            &json!({"path": "a.txt", "old_text": "b", "new_text": "x"}),
            1,
        );
        assert_eq!(edit.stats_label, "+1 -1");
        assert!(edit.body.plain().contains("- b\n"));

        let unnamed = describe_file_change("Edit_file", &json!({}), 1);
        assert_eq!(unnamed.path, "(unknown path)");
    }

    #[test]
    fn tool_disclosure_title_renders_workspace_and_generic_forms() {
        let title = tool_disclosure_title(
            "read",
            &json!({"path": "src/main.rs"}),
            "成功",
            0.4567,
            false,
            "   1: a\n   2: b",
        );
        assert_eq!(
            title.plain(),
            "● read src/main.rs · 第 1-2 行 · ✓ 成功 · 457ms"
        );

        let generic =
            tool_disclosure_title("bash", &json!({"command": "ls"}), "调用中", 2.0, false, "");
        assert_eq!(generic.plain(), "● bash ls · … 调用中 · 2.0s");

        let change = tool_disclosure_title(
            "write_file",
            &json!({"path": "/tmp/a.txt", "content": "x"}),
            "成功",
            0.0,
            false,
            "",
        );
        assert!(change
            .plain()
            .starts_with("● write_file /tmp/a.txt · rewrite +1 lines · ✓ 成功"));

        let ask = tool_disclosure_title(ASK_USER_TOOL_NAME, &json!({}), "等待回复", 1.5, false, "");
        assert_eq!(ask.plain(), "● ↘ 等待回复... · 1.5s");
    }

    #[test]
    fn body_hides_configured_tools_and_keeps_raw_output_elsewhere() {
        assert!(tool_disclosure_body("read", &json!({}), "内容")
            .plain()
            .is_empty());
        assert!(tool_disclosure_body("memory_write", &json!({}), "内容")
            .plain()
            .is_empty());
        assert!(tool_disclosure_body("kb_search", &json!({}), "内容")
            .plain()
            .is_empty());
        assert_eq!(
            tool_disclosure_body("bash", &json!({}), "out").plain(),
            "out"
        );
    }

    #[test]
    fn fetcher_body_drops_page_content_but_keeps_metadata() {
        let result = "汇总：2 个\n1. https://a\n   状态: 200\n   标题: A\n   内容: 很长的正文\n2. https://b\n   内容: 正文\n   失败: 超时";
        let plain = fetcher_body(result).plain();
        assert!(plain.contains("   标题: A\n"));
        assert!(!plain.contains("很长的正文"));
        assert!(plain.contains("   失败: 超时"));
    }

    #[test]
    fn file_change_body_appends_edit_summary_only() {
        let body = file_change_body(
            "Edit_file",
            &json!({"path": "a.txt", "old_text": "b", "new_text": "x"}),
            "已替换 1 处",
            true,
        );
        let plain = body.plain();
        assert!(plain.starts_with("工具：Edit_file\n"));
        assert!(plain.ends_with("结果：替换 1 处"));

        let write = file_change_body(
            "write_file",
            &json!({"path": "a.txt", "content": "x"}),
            "写入 1 行",
            true,
        );
        assert!(write.plain().ends_with("结果：写入 1 行"));

        let no_result = file_change_body(
            "write_file",
            &json!({"path": "a.txt", "content": "x"}),
            "",
            true,
        );
        assert!(!no_result.plain().contains("结果："));
    }

    #[test]
    fn is_file_change_tool_only_matches_write_tools() {
        assert!(is_file_change_tool("write_file"));
        assert!(is_file_change_tool("Edit_file"));
        assert!(!is_file_change_tool("read"));
        assert!(!is_file_change_tool(""));
    }
}
