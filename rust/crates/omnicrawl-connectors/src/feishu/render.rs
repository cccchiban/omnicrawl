//! 飞书显示映射：工具摘要与正文、文件变更预览、执行计划、子任务进度与卡片 JSON。
//!
//! 语义基准是 Python `omnicrawl/connectors/fsapp.py` 的显示规则段（与 TUI 消息流同规则）：
//! `_tool_summary` / `_tool_body` / `_sample_output_lines` / `_file_change_*` /
//! `_reasoning_panel` / `_normalize_todos` / `_todos_text` / `_subagents_text` / `_card_json`。
//! `difflib.SequenceMatcher` 是变更统计的一部分，这里逐行移植（见 [`sequence_matcher`]）。

use serde_json::Value;

use omnicrawl_session::{redact_sensitive_text, redact_sensitive_values};

use super::text::clean_text;
use crate::json;

/// 工具正文采样上限：最多 5 行，超出时保留首尾各 2 行有效行。
pub const TOOL_BODY_MAX_LINES: usize = 5;
pub const TOOL_BODY_HEAD_LINES: usize = 2;
pub const TOOL_BODY_TAIL_LINES: usize = 2;
pub const TOOL_BODY_MAX_CHARS_PER_LINE: usize = 160;

/// 文件变更预览行数与结果摘要行数。
pub const FILE_CHANGE_PREVIEW_LINES: usize = 5;

/// 折叠思考：运行中只显示最新 5 行，定型后保留最多 4000 字符。
pub const REASONING_PREVIEW_LINES: usize = 5;
pub const REASONING_MAX_CHARS: usize = 4000;

/// 执行计划与子任务进度消息的最大行数。
pub const MAX_TODO_LINES: usize = 20;
pub const MAX_SUBAGENT_LINES: usize = 20;

/// 工具摘要中路径/目标/命令/参数的压缩上限。
pub const MAX_PATH_CHARS: usize = 48;
pub const MAX_PATTERN_CHARS: usize = 36;
pub const MAX_COMMAND_CHARS: usize = 120;
pub const MAX_ARGS_CHARS: usize = 120;

/// 提问与执行计划工具不产生工具消息（各有独立卡片/消息）。
pub const ASK_USER_TOOL_NAME: &str = "ask_user";
pub const TODO_TOOL_NAME: &str = "update_todos";

/// 记忆与知识库工具的正文对远程用户没有展示价值，只保留摘要行。
pub const MEMORY_TOOL_OPERATIONS: [&str; 4] = [
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
];
pub const KB_TOOL_OPERATIONS: [&str; 5] =
    ["kb_search", "kb_read", "kb_write", "kb_append", "kb_list"];

/// 文件变更工具：展示变更统计与预览，而不是采样后的原始输出。
pub const FILE_CHANGE_OPERATIONS: [&str; 2] = ["write_file", "Edit_file"];

/// 工具记录终态：不再被迟到的完成事件覆盖。
pub const TOOL_TERMINAL_STATUSES: [&str; 3] = ["成功", "失败", "已取消"];

/// 子任务终态。
pub const SUBAGENT_TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "cancelled"];

/// 子任务事件 → 状态。
pub const SUBAGENT_EVENT_STATUS: [(&str, &str); 8] = [
    ("subagent.task.queued", "queued"),
    ("subagent.task.started", "running"),
    ("subagent.task.running", "running"),
    ("subagent.task.waiting_approval", "waiting_approval"),
    ("subagent.task.completed", "completed"),
    ("subagent.task.failed", "failed"),
    ("subagent.task.cancelled", "cancelled"),
    ("subagent.task.approval_cancelled", "cancelled"),
];

/// 子任务状态图标与中文标签（沿用 TUI 进度树取值）。
pub const SUBAGENT_STATUS_PRESENTATION: [(&str, &str, &str); 6] = [
    ("queued", "○", "等待中"),
    ("running", "●", "运行中"),
    ("waiting_approval", "◆", "等待审批"),
    ("completed", "✓", "完成"),
    ("failed", "×", "失败"),
    ("cancelled", "–", "已取消"),
];

/// 工具状态图标（标签就是状态原文，与 TUI 的 `format_tool_status` 一致）。
pub fn tool_status_icon(status: &str) -> &'static str {
    match status {
        "成功" => "✓",
        "失败" => "✗",
        "调用中" => "…",
        "等待确认" => "!",
        "已取消" => "↷",
        _ => "·",
    }
}

/// 与 TUI 的 `format_duration` 一致：<1s 用毫秒，<60s 用一位小数秒，之后用 `Nm SSs`。
pub fn format_duration(seconds: f64) -> String {
    let duration = seconds.max(0.0);
    if duration < 1.0 {
        return format!("{:.0}ms", duration * 1000.0);
    }
    if duration < 60.0 {
        return format!("{duration:.1}s");
    }
    let total = duration as i64;
    format!("{}m {:02}s", total / 60, total % 60)
}

/// 子任务耗时格式（MM:SS / HH:MM:SS）。
pub fn format_elapsed(seconds: f64) -> String {
    let total = seconds.max(0.0) as i64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds_part = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds_part:02}")
    } else {
        format!("{minutes:02}:{seconds_part:02}")
    }
}

/// 工具末级操作名，兼容 `server.operation` 形式的 MCP 工具。
pub fn operation_of(tool_name: &str) -> String {
    tool_name.rsplit('.').next().unwrap_or("").to_string()
}

/// 折叠空白并限制长度。
pub fn safe_label(value: Option<&Value>, max_chars: usize) -> String {
    let text: String = match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    text.split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

/// 把参数压缩成单行摘要；超长时截断并附加省略号。
pub fn compact_line(value: Option<&Value>, max_chars: usize) -> String {
    let text: String = match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let text = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    if text.is_empty() {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text;
    }
    format!("{}…", take_chars(&text, max_chars - 1))
}

/// 限制单行宽度。
pub fn clip_line(line: &str, max_chars: usize) -> String {
    let text = line.trim_end();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    format!("{}…", take_chars(text, max_chars - 1))
}

/// 把正文放入围栏代码块；正文内的三反引号先替换掉。
pub fn fenced_body(body: &str) -> String {
    format!("```\n{}\n```", body.replace("```", "'''"))
}

/// 按 TUI 规则采样工具输出：最多 5 行，超出时保留首尾各 2 行有效行。
pub fn sample_output_lines(output: &str) -> Vec<String> {
    let lines: Vec<String> = split_lines(output)
        .iter()
        .map(|line| clip_line(line, TOOL_BODY_MAX_CHARS_PER_LINE))
        .collect();
    if lines.len() <= TOOL_BODY_MAX_LINES {
        return lines;
    }
    let effective: Vec<String> = lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if effective.len() <= TOOL_BODY_MAX_LINES {
        return effective;
    }
    let mut sampled: Vec<String> = effective[..TOOL_BODY_HEAD_LINES].to_vec();
    sampled.extend_from_slice(&effective[effective.len() - TOOL_BODY_TAIL_LINES..]);
    sampled
}

/// 从 read 的行号输出中解析实际返回的首尾源码行号（`^\s*(\d+):\s`）。
pub fn read_result_line_range(result_text: &str) -> Option<(i64, i64)> {
    if result_text.is_empty() {
        return None;
    }
    let mut numbers: Vec<i64> = Vec::new();
    for line in result_text.split('\n') {
        let trimmed = line.trim_start();
        let digits: String = trimmed
            .chars()
            .take_while(|value| value.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            continue;
        }
        let rest = &trimmed[digits.len()..];
        let mut characters = rest.chars();
        if characters.next() != Some(':') {
            continue;
        }
        let follows_with_space = characters
            .next()
            .map(|value| value.is_whitespace())
            .unwrap_or(false);
        if !follows_with_space {
            continue;
        }
        if let Ok(value) = digits.parse::<i64>() {
            numbers.push(value);
        }
    }
    match (numbers.first(), numbers.last()) {
        (Some(first), Some(last)) => Some((*first, *last)),
        _ => None,
    }
}

/// 把目录列表结果压缩为「N 项」摘要。
pub fn list_result_summary(result_text: &str) -> Option<String> {
    if result_text.is_empty() {
        return None;
    }
    let lines: Vec<String> = result_text
        .split('\n')
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() || lines == ["目录为空。".to_string()] {
        return Some("0 项".to_string());
    }
    let truncated = lines.iter().any(|line| line.starts_with("..."));
    let visible = lines.iter().filter(|line| !line.starts_with("...")).count();
    Some(format!("{visible}{} 项", if truncated { "+" } else { "" }))
}

/// 变更统计文案：`+N -M`，都为零时是 `0`。
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

/// 生成带 `+`/`-` 前缀的变更预览，并返回完整改动的 (added, removed)。
pub fn diff_preview_lines(old_text: &str, new_text: &str) -> (Vec<String>, usize, usize) {
    let old_lines = split_lines(old_text);
    let new_lines = split_lines(new_text);
    let mut preview: Vec<String> = Vec::new();
    let mut added = 0_usize;
    let mut removed = 0_usize;
    for (tag, i1, i2, j1, j2) in sequence_matcher::opcodes(&old_lines, &new_lines) {
        if matches!(tag, OpTag::Replace | OpTag::Delete) {
            removed += i2 - i1;
            for line in &old_lines[i1..i2] {
                preview.push(format!("- {line}"));
            }
        }
        if matches!(tag, OpTag::Replace | OpTag::Insert) {
            added += j2 - j1;
            for line in &new_lines[j1..j2] {
                preview.push(format!("+ {line}"));
            }
        }
    }
    (preview, added, removed)
}

/// 文件变更统计：`Edit_file` 用 diff 统计，`write_file` 用 `rewrite +N lines`。
pub fn file_change_summary(operation: &str, arguments: &Value) -> String {
    if operation == "Edit_file" {
        let (_preview, added, removed) = diff_preview_lines(
            field_text(arguments, "old_text").as_str(),
            field_text(arguments, "new_text").as_str(),
        );
        return format_line_stats(added, removed);
    }
    let content = field_text(arguments, "content");
    let line_count = if content.is_empty() {
        0
    } else {
        split_lines(&content).len()
    };
    let label = if field_text(arguments, "mode").to_lowercase() == "append" {
        "append"
    } else {
        "rewrite"
    };
    format!("{label} +{line_count} lines")
}

/// 文件变更预览：`Edit_file` 输出 +/- diff，`write_file` 输出新增行。
pub fn file_change_preview(operation: &str, arguments: &Value) -> String {
    let preview = if operation == "Edit_file" {
        let (preview, _added, _removed) = diff_preview_lines(
            field_text(arguments, "old_text").as_str(),
            field_text(arguments, "new_text").as_str(),
        );
        preview
    } else {
        let content = field_text(arguments, "content");
        let lines: Vec<String> = split_lines(&content)
            .into_iter()
            .map(|line| format!("+ {line}"))
            .collect();
        if lines.is_empty() {
            vec!["+ (empty)".to_string()]
        } else {
            lines
        }
    };
    let visible: Vec<String> = preview
        .iter()
        .take(FILE_CHANGE_PREVIEW_LINES)
        .cloned()
        .collect();
    let mut body = visible
        .iter()
        .map(|line| clip_line(line, TOOL_BODY_MAX_CHARS_PER_LINE))
        .collect::<Vec<String>>()
        .join("\n");
    let omitted = preview.len().saturating_sub(visible.len());
    if omitted > 0 {
        body.push_str(&format!("\n… 还有 {omitted} 行未展示"));
    }
    body
}

/// 生成工具记录标题摘要（工具名 + 关键参数 + 结果摘要），与 TUI 同规则。
pub fn tool_summary(tool_name: &str, arguments: &Value, result_text: &str) -> String {
    let name = if tool_name.is_empty() { "?" } else { tool_name };
    let operation = operation_of(name);
    let args = arguments_object(arguments);
    match operation.as_str() {
        "list" => {
            let raw_path = compact_line(args.get("path"), MAX_PATH_CHARS);
            let path = if raw_path.is_empty() {
                "."
            } else {
                raw_path.as_str()
            };
            let summary = format!("{name} {path}");
            return match list_result_summary(result_text) {
                Some(count) => format!("{summary} · {count}"),
                None => summary,
            };
        }
        "read" => {
            let mut summary = format!(
                "{name} {}",
                required_compact(&args, "path", "(未指定文件)", MAX_PATH_CHARS)
            );
            if let Some((first, last)) = read_result_line_range(result_text) {
                summary.push_str(&format!(" · 第 {first}-{last} 行"));
            }
            return summary;
        }
        "read_image" => {
            return format!(
                "{name} {} · 图片",
                required_compact(&args, "path", "(未指定图片)", MAX_PATH_CHARS)
            );
        }
        "find" | "grep" => {
            let path = required_compact(&args, "path", ".", MAX_PATH_CHARS);
            let target = compact_line(args.get("pattern"), MAX_PATTERN_CHARS);
            let target = if target.is_empty() {
                "(未指定)"
            } else {
                target.as_str()
            };
            return format!("{name} {path} · 目标: {target}");
        }
        "bash" | "powershell" => {
            let command = compact_line(args.get("command"), MAX_COMMAND_CHARS);
            return format!("{name} {command}").trim_end().to_string();
        }
        _ if FILE_CHANGE_OPERATIONS.contains(&operation.as_str()) => {
            let path = required_compact(&args, "path", "(unknown path)", MAX_PATH_CHARS);
            return format!(
                "{name} {path} · {}",
                file_change_summary(&operation, arguments)
            );
        }
        "monitor" => {
            let context = required_compact(&args, "action", "任务", MAX_PATTERN_CHARS);
            let detail = compact_line(
                args.get("command").or_else(|| args.get("monitor_id")),
                MAX_COMMAND_CHARS,
            );
            return format!("{name} {context} {detail}").trim_end().to_string();
        }
        "subagent" => {
            let context = required_compact(&args, "action", "任务", MAX_PATTERN_CHARS);
            let count = args
                .get("tasks")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            let suffix = if count > 0 {
                format!(" · {count} 项")
            } else {
                String::new()
            };
            return format!("{name} {context}{suffix}");
        }
        _ if MEMORY_TOOL_OPERATIONS.contains(&operation.as_str()) => {
            let query = compact_line(args.get("query"), MAX_PATH_CHARS);
            if !query.is_empty() {
                return format!("{name} {query}");
            }
            if let Some(ids) = args.get("memory_ids").and_then(Value::as_array) {
                return format!("{name} {} 条记忆", ids.len());
            }
            return name.to_string();
        }
        _ if KB_TOOL_OPERATIONS.contains(&operation.as_str()) => {
            let detail = compact_line(
                args.get("query").or_else(|| args.get("path")),
                MAX_PATH_CHARS,
            );
            return format!("{name} {detail}").trim_end().to_string();
        }
        _ => {}
    }
    // 其余工具（含 MCP）：附带紧凑参数摘要。
    let safe = if args.is_empty() {
        Value::Null
    } else {
        redact_sensitive_values(arguments)
    };
    let payload = if safe.is_null() {
        String::new()
    } else {
        compact_line(Some(&Value::String(json::dumps(&safe))), MAX_ARGS_CHARS)
    };
    format!("{name} {payload}").trim_end().to_string()
}

/// 生成工具记录正文：原始输出采样，或隐藏/展示文件变更预览。
pub fn tool_body(tool_name: &str, arguments: &Value, result_text: &str) -> String {
    let operation = operation_of(tool_name);
    if operation == "read"
        || MEMORY_TOOL_OPERATIONS.contains(&operation.as_str())
        || KB_TOOL_OPERATIONS.contains(&operation.as_str())
    {
        return String::new();
    }
    if FILE_CHANGE_OPERATIONS.contains(&operation.as_str()) {
        let preview = file_change_preview(&operation, arguments);
        let note = file_change_result_note(&operation, result_text);
        return if note.is_empty() {
            preview
        } else {
            format!("{preview}\n{note}")
        };
    }
    sample_output_lines(result_text)
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

/// 文件变更工具的结果摘要：`Edit_file` 只保留「替换 N 处」。
pub fn file_change_result_note(operation: &str, result_text: &str) -> String {
    if operation == "Edit_file" {
        if let Some(found) = find_replace_count(result_text) {
            return found;
        }
    }
    sample_output_lines(result_text)
        .into_iter()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
}

/// 匹配 `替换\s*\d+\s*处`。
fn find_replace_count(result_text: &str) -> Option<String> {
    let characters: Vec<char> = result_text.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '替' && characters.get(index + 1) == Some(&'换') {
            let mut cursor = index + 2;
            while cursor < characters.len() && characters[cursor].is_whitespace() {
                cursor += 1;
            }
            let digits_start = cursor;
            while cursor < characters.len() && characters[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor > digits_start {
                let number_end = cursor;
                while cursor < characters.len() && characters[cursor].is_whitespace() {
                    cursor += 1;
                }
                if characters.get(cursor) == Some(&'处') {
                    let matched: String = characters[index..=cursor].iter().collect();
                    let _ = number_end;
                    return Some(matched);
                }
            }
        }
        index += 1;
    }
    None
}

/// 思考折叠面板：运行中只显示最新五行，收口后给完整内容。
pub fn reasoning_panel(text: &str, streaming: bool) -> Value {
    let cleaned = clean_text(text);
    let content = if streaming {
        let lines: Vec<&str> = cleaned
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(REASONING_PREVIEW_LINES);
        let preview = lines[start..].join("\n");
        let hint = format!("· 正在思考，仅显示最新 {REASONING_PREVIEW_LINES} 行");
        if preview.is_empty() {
            hint
        } else {
            format!("{preview}\n\n{hint}")
        }
    } else if cleaned.chars().count() > REASONING_MAX_CHARS {
        let total = cleaned.chars().count();
        let tail: String = cleaned.chars().skip(total - REASONING_MAX_CHARS).collect();
        format!("…（更早内容已省略）\n{tail}")
    } else {
        cleaned
    };
    serde_json::json!({
        "tag": "collapsible_panel",
        "expanded": false,
        "header": {"title": {"tag": "plain_text", "content": "💭 思考内容"}},
        "elements": [{"tag": "markdown", "content": if content.is_empty() { "（无内容）" } else { content.as_str() }}],
    })
}

/// 规范化执行计划清单：过滤空步骤并限制条数。
pub fn normalize_todos(items: Option<&Value>) -> Vec<(String, bool)> {
    let mut normalized: Vec<(String, bool)> = Vec::new();
    let Some(Value::Array(items)) = items else {
        return normalized;
    };
    for item in items.iter().take(MAX_TODO_LINES) {
        let Value::Object(map) = item else { continue };
        let text = map
            .get("step")
            .or_else(|| map.get("description"))
            .or_else(|| map.get("title"))
            .map(value_text)
            .unwrap_or_default();
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let completed = map
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || matches!(
                map.get("status")
                    .map(value_text)
                    .unwrap_or_default()
                    .to_lowercase()
                    .as_str(),
                "completed" | "done" | "complete"
            );
        let label: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
        normalized.push((take_chars(&label, 240), completed));
    }
    normalized
}

/// 执行计划文本：标题 + 每步完成标记。
pub fn todos_text(todos: &[(String, bool)]) -> String {
    let completed = todos.iter().filter(|(_step, done)| *done).count();
    let mut lines = vec![format!("**执行计划 · {completed}/{} 完成**", todos.len())];
    for (step, done) in todos {
        lines.push(format!("{} {step}", if *done { "▣" } else { "▢" }));
    }
    lines.join("\n")
}

/// 子任务进度段：根标签 + `├─`/`└─` 节点，与 TUI 进度树同形。
pub fn subagents_text(order: &[String], nodes: &[(String, SubagentNode)]) -> String {
    let total = order.len();
    let lookup = |key: &str| {
        nodes
            .iter()
            .find(|(name, _node)| name == key)
            .map(|(_name, node)| node)
    };
    let completed = order
        .iter()
        .filter(|key| {
            lookup(key)
                .map(|node| node.status == "completed")
                .unwrap_or(false)
        })
        .count();
    let root = if total > 1 {
        "◇ 并行子任务"
    } else {
        "◇ 子任务进度"
    };
    let mut lines = vec![format!("**{root} · {completed}/{total} 完成**")];
    let now = std::time::Instant::now();
    let visible = total.min(MAX_SUBAGENT_LINES);
    for (index, key) in order.iter().take(visible).enumerate() {
        let Some(node) = lookup(key) else { continue };
        let (icon, label) = SUBAGENT_STATUS_PRESENTATION
            .iter()
            .find(|(status, _icon, _label)| *status == node.status)
            .map(|(_status, icon, label)| (*icon, *label))
            .unwrap_or(("·", node.status.as_str()));
        let connector = if index == total - 1 {
            "└─"
        } else {
            "├─"
        };
        let mut line = format!(
            "{connector} {icon} {} · {} · {label}",
            node.description, node.agent_type
        );
        if let Some(started) = node.started_at {
            let ended = node.finished_at.unwrap_or(now);
            let seconds = ended.duration_since(started).as_secs_f64();
            line.push_str(&format!(" · {}", format_elapsed(seconds)));
        }
        lines.push(line);
    }
    if total > visible {
        lines.push(format!("… 还有 {} 个任务", total - visible));
    }
    lines.join("\n")
}

/// 子任务进度节点（只保留安全字段，不展示 prompt 或结果）。
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentNode {
    pub agent_type: String,
    pub description: String,
    pub status: String,
    pub started_at: Option<std::time::Instant>,
    pub finished_at: Option<std::time::Instant>,
}

/// 组装单元素卡片消息。
pub fn markdown_card(content: &str) -> String {
    card_json(&[serde_json::json!({"tag": "markdown", "content": content})])
}

/// 生成飞书卡片 JSON；保留中文可读性（`ensure_ascii=False`）。
pub fn card_json(elements: &[Value]) -> String {
    json::dumps(&serde_json::json!({
        "schema": "2.0",
        "config": {"streaming_mode": false, "width_mode": "fill"},
        "body": {"elements": elements},
    }))
}

/// `ask_user` 选项卡片：按钮值只携带不可变问题 ID 和答案。
pub fn question_card_json(question: &str, options: &[String], question_id: &str) -> String {
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": format!("**{question}**"),
    })];
    for answer in options {
        elements.push(serde_json::json!({
            "tag": "button",
            "text": {"tag": "plain_text", "content": answer},
            "type": "primary",
            "value": {"type": "ask_user", "question_id": question_id, "answer": answer},
        }));
    }
    card_json(&elements)
}

/// 已终结的提问卡片：只保留状态行，移除所有选项按钮。
pub fn question_resolved_card_json(question: &str, status: &str) -> String {
    card_json(&[serde_json::json!({
        "tag": "markdown",
        "content": redact_sensitive_text(&format!("**{question}**\n{status}")),
    })])
}

/// 工具记录渲染：`● 摘要 · ✓ 成功 · 136ms`，正文放围栏代码块。
pub fn render_tool_record(
    summary: &str,
    status: &str,
    duration_seconds: Option<f64>,
    body: &str,
    with_body: bool,
) -> String {
    let mut line = format!(
        "● {} · {} {status}",
        redact_sensitive_text(summary),
        tool_status_icon(status)
    );
    if let Some(seconds) = duration_seconds {
        line.push_str(&format!(" · {}", format_duration(seconds)));
    }
    if !with_body || body.is_empty() {
        return line;
    }
    format!("{line}\n{}", fenced_body(&redact_sensitive_text(body)))
}

fn required_compact(
    args: &serde_json::Map<String, Value>,
    key: &str,
    fallback: &str,
    max_chars: usize,
) -> String {
    let value = compact_line(args.get(key), max_chars);
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn arguments_object(arguments: &Value) -> serde_json::Map<String, Value> {
    match arguments {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    }
}

fn field_text(arguments: &Value, key: &str) -> String {
    arguments.get(key).map(value_text).unwrap_or_default()
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Python `str.splitlines()` 的等价实现（`\n`、`\r\n`、`\r` 都算换行，空串没有行）。
fn split_lines(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = normalized
        .split('\n')
        .map(|line| line.to_string())
        .collect();
    if normalized.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// `difflib.SequenceMatcher`（`autojunk=False`）的最小移植：只产出 opcodes。
pub mod sequence_matcher {
    use super::OpTag;

    pub fn opcodes(
        old_lines: &[String],
        new_lines: &[String],
    ) -> Vec<(OpTag, usize, usize, usize, usize)> {
        let blocks = matching_blocks(old_lines, new_lines);
        let mut result = Vec::new();
        let mut i = 0;
        let mut j = 0;
        for (a, b, size) in blocks {
            let mut tag = None;
            if i < a && j < b {
                tag = Some(OpTag::Replace);
            } else if i < a {
                tag = Some(OpTag::Delete);
            } else if j < b {
                tag = Some(OpTag::Insert);
            }
            if let Some(tag) = tag {
                result.push((tag, i, a, j, b));
            }
            i = a + size;
            j = b + size;
            if size > 0 {
                result.push((OpTag::Equal, a, i, b, j));
            }
        }
        result
    }

    fn matching_blocks(old_lines: &[String], new_lines: &[String]) -> Vec<(usize, usize, usize)> {
        let mut queue = vec![(0_usize, old_lines.len(), 0_usize, new_lines.len())];
        let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (best_i, best_j, best_size) =
                find_longest_match(old_lines, new_lines, alo, ahi, blo, bhi);
            if best_size == 0 {
                continue;
            }
            blocks.push((best_i, best_j, best_size));
            if alo < best_i && blo < best_j {
                queue.push((alo, best_i, blo, best_j));
            }
            if best_i + best_size < ahi && best_j + best_size < bhi {
                queue.push((best_i + best_size, ahi, best_j + best_size, bhi));
            }
        }
        blocks.sort_unstable();
        // 递归切分出来的块互不重叠，去掉完全相同的重复块后补终止哨兵即可。
        let mut merged: Vec<(usize, usize, usize)> = Vec::new();
        for block in blocks {
            if merged.last() != Some(&block) {
                merged.push(block);
            }
        }
        merged.push((old_lines.len(), new_lines.len(), 0));
        merged
    }

    fn find_longest_match(
        old_lines: &[String],
        new_lines: &[String],
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> (usize, usize, usize) {
        let mut best_i = alo;
        let mut best_j = blo;
        let mut best_size = 0_usize;
        let mut lengths: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for (offset, old_line) in old_lines[alo..ahi].iter().enumerate() {
            let i = alo + offset;
            let mut next: std::collections::HashMap<usize, usize> =
                std::collections::HashMap::new();
            for (j, new_line) in new_lines[blo..bhi].iter().enumerate() {
                let j = blo + j;
                if old_line == new_line {
                    let previous = if j > blo {
                        lengths.get(&(j - 1)).copied().unwrap_or(0)
                    } else {
                        0
                    };
                    let size = previous + 1;
                    next.insert(j, size);
                    if size > best_size {
                        best_i = i + 1 - size;
                        best_j = j + 1 - size;
                        best_size = size;
                    }
                }
            }
            lengths = next;
        }
        (best_i, best_j, best_size)
    }
}

/// diff 操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpTag {
    Equal,
    Replace,
    Delete,
    Insert,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operation_of_takes_last_segment() {
        assert_eq!(operation_of("mcp.files.read"), "read");
        assert_eq!(operation_of("bash"), "bash");
    }

    #[test]
    fn tool_summary_covers_read_and_list() {
        let summary = tool_summary(
            "read",
            &json!({"path": "a/b.py", "offset": 1}),
            "     1: x\n     9: y\n",
        );
        assert_eq!(summary, "read a/b.py · 第 1-9 行");
        assert_eq!(
            tool_summary("list", &json!({"path": "src"}), "a.py\nb.py\n"),
            "list src · 2 项"
        );
        assert_eq!(
            tool_summary("list", &json!({}), "目录为空。\n"),
            "list . · 0 项"
        );
        assert_eq!(
            tool_summary("mcp.files.list", &json!({"path": "x"}), "a\n"),
            "mcp.files.list x · 1 项"
        );
    }

    #[test]
    fn file_change_summary_counts_lines() {
        assert_eq!(
            file_change_summary(
                "Edit_file",
                &json!({"old_text": "a\nb", "new_text": "a\nc"})
            ),
            "+1 -1"
        );
        assert_eq!(
            file_change_summary(
                "write_file",
                &json!({"content": "a\nb\n", "mode": "append"})
            ),
            "append +2 lines"
        );
        assert_eq!(
            file_change_summary("write_file", &json!({"content": ""})),
            "rewrite +0 lines"
        );
    }

    #[test]
    fn tool_body_hides_read_and_shows_diff() {
        assert_eq!(tool_body("read", &json!({"path": "a"}), "1: x"), "");
        let body = tool_body(
            "Edit_file",
            &json!({"path": "a.py", "old_text": "old", "new_text": "new"}),
            "已修改 a.py，替换 1 处",
        );
        assert!(body.starts_with("- old\n+ new\n替换 1 处"), "{body}");
    }

    #[test]
    fn todos_and_subagents_render() {
        let todos = normalize_todos(Some(&json!([
            {"step": " 第一步 ", "completed": true},
            {"step": "", "completed": false},
            {"description": "第二步", "status": "done"},
        ])));
        assert_eq!(todos.len(), 2);
        assert_eq!(
            todos_text(&todos),
            "**执行计划 · 2/2 完成**\n▣ 第一步\n▣ 第二步"
        );
    }

    #[test]
    fn elapsed_format_matches_tui() {
        assert_eq!(format_elapsed(65.0), "01:05");
        assert_eq!(format_elapsed(3661.0), "01:01:01");
    }

    #[test]
    fn reasoning_panel_keeps_latest_lines_while_streaming() {
        let text = (1..=8)
            .map(|index| format!("行{index}"))
            .collect::<Vec<String>>()
            .join("\n");
        let panel = reasoning_panel(&text, true);
        let content = panel["elements"][0]["content"].as_str().expect("content");
        assert!(content.starts_with("行4"), "{content}");
        assert!(content.contains("仅显示最新 5 行"));
    }

    #[test]
    fn card_json_uses_python_separators() {
        let card = markdown_card("◇ 正文");
        assert!(card.starts_with("{\"schema\": \"2.0\""), "{card}");
        assert!(card.contains("\"tag\": \"markdown\""));
    }
}
