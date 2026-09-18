//! `omnicrawl/agent/controllers/shared.py`：常量表、整数配置校验与工具结果的共享文案。

use crate::error::AgentError;
use crate::sha1::sha1_digest10;
use crate::types::ToolResult;

pub const DEFAULT_TOOL_TIMEOUT_SECONDS: i64 = 600;

pub const MAX_TOOL_TIMEOUT_SECONDS: i64 = 3600;

pub const TOOL_OUTPUT_INLINE_LIMIT_CHARS: usize = 50_000;

pub const TOOL_OUTPUT_BATCH_BUDGET_CHARS: usize = 200_000;

pub const TOOL_OUTPUT_ARCHIVED_PREVIEW_CHARS: usize = 4_000;

pub const SUBAGENT_LIFECYCLE_WAIT_SECONDS: f64 = 5.0;

pub const SYSTEM_PROMPT_FILE: &str = "system_prompt.md";

pub const AGENTS_INSTRUCTIONS_FILE: &str = "AGENTS.md";

pub const CONTEXT_OVERFLOW_RECOVERY_PROMPT: &str = "请依据上方的结构化工作摘要继续完成当前任务。";

pub const CONTEXT_OVERFLOW_ERROR_MARKERS: [&str; 16] = [
    "context length",
    "context window",
    "maximum context",
    "max context",
    "context limit",
    "too many tokens",
    "token limit",
    "input is too long",
    "prompt is too long",
    "请求过长",
    "上下文过长",
    "上下文长度",
    "超过上下文",
    "超出上下文",
    "token 超限",
    "令牌超限",
];

pub const RATE_LIMIT_ERROR_MARKERS: [&str; 5] = [
    "rate limit",
    "too many requests",
    "insufficient_quota",
    "quota",
    "429",
];

pub const CONTINUE_LAST_TASK_TEXTS: [&str; 10] = [
    "继续",
    "继续上次",
    "继续上一轮",
    "接着来",
    "接着做",
    "重试",
    "再试一次",
    "再试试",
    "retry",
    "continue",
];

pub const READ_ONLY_UNDO_TOOLS: [&str; 9] = [
    "list",
    "find",
    "read",
    "read_image",
    "grep",
    "recall_session_evidence",
    "memory_search",
    "memory_read",
    "memory_expand_related",
];

pub const REVERSIBLE_UNDO_TOOLS: [&str; 2] = ["Edit_file", "write_file"];

pub const MEMORY_UNDO_EXEMPT_TOOLS: [&str; 1] = ["memory_write"];

pub const ASK_USER_ADVISOR_HINT: &str = "（若用户暂时不在或无法作答：本环境已启用顾问策略，可调用一次 advisor 代替用户评估并给出合理的决策方向，避免任务空等；但高危或需审批的操作仍须获得用户的明确授权。）";

pub fn contains_text(table: &[&str], text: &str) -> bool {
    table.contains(&text)
}

pub fn read_int_env(
    name: &str,
    raw: Option<&str>,
    default: i64,
    min_value: i64,
    max_value: i64,
) -> Result<i64, AgentError> {
    let raw_value = match raw {
        Some(value) => value,
        None => return Ok(default),
    };
    if raw_value.trim().is_empty() {
        return Ok(default);
    }

    let value = parse_python_int(raw_value.trim()).ok_or_else(|| {
        AgentError::new(format!(
            "{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{raw_value}。"
        ))
    })?;
    validate_int_range(name, value, min_value, max_value)
}

pub fn validate_int_range(
    name: &str,
    value: i64,
    min_value: i64,
    max_value: i64,
) -> Result<i64, AgentError> {
    if value < min_value || value > max_value {
        return Err(AgentError::new(format!(
            "{name} 必须是 {min_value} 到 {max_value} 的整数，当前值：{value}。"
        )));
    }
    Ok(value)
}

pub fn unknown_tool_result(requested_name: &str, active_tools: &[&str]) -> ToolResult {
    let resolved = resolve_tool_name_from_hashed_function_name(requested_name, active_tools);
    let output = match resolved {
        Some(name) if name != requested_name => format!(
            "未知工具：{requested_name}。该名称疑似 {name} 的哈希函数名变体，\
             正确名称为 {name}。请直接调用 {name}。"
        ),
        _ => format!("未知工具：{requested_name}。请从已注册的工具名中选择正确的名称重试。"),
    };
    ToolResult {
        ok: false,
        output,
        ..ToolResult::default()
    }
}

pub fn tool_timeout_result(timeout_seconds: i64, hint: &str) -> ToolResult {
    let mut output = format!(
        "工具执行超时（超过 {timeout_seconds} 秒未完成），已中止等待。\
         （后台线程仍在运行，其结果已被丢弃。）"
    );
    if !hint.is_empty() {
        output.push_str(hint);
    }
    ToolResult {
        ok: false,
        output,
        ..ToolResult::default()
    }
}

/// 在独立线程执行串行工具并限时等待；超时返回错误结果，不阻塞回合。
///
/// 与 Python 侧的差别：线程不携带 `contextvars` 副本，调用方需要
/// 线程本地状态时自行捕获。超时后线程继续运行、结果被丢弃，与 Python 同语义。
pub fn execute_call_with_timeout<F>(
    execute_call: F,
    index: usize,
    timeout_seconds: u64,
) -> ToolResult
where
    F: FnOnce(usize) -> ToolResult + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(execute_call(index));
    });
    match receiver.recv_timeout(std::time::Duration::from_secs(timeout_seconds)) {
        Ok(result) => result,
        Err(_) => tool_timeout_result(timeout_seconds as i64, ""),
    }
}

/// 从 `tool_<可读段>_<sha1 前 10 位>` 的函数名中按 digest 段反查真实工具名。
///
/// 对应 `omnicrawl/agent/runtime/llm_protocol.py` 的同名函数与
/// `_HASHED_FUNCTION_NAME_RE`：模型回显哈希函数名时可能截断可读段，digest 段
/// 通常保留，据此反查可容忍这类改写。
pub fn resolve_tool_name_from_hashed_function_name(
    function_name: &str,
    tool_names: &[&str],
) -> Option<String> {
    let digest = hashed_function_name_digest(function_name)?;
    tool_names
        .iter()
        .find(|name| sha1_digest10(name) == digest)
        .map(|name| (*name).to_string())
}

fn hashed_function_name_digest(function_name: &str) -> Option<&str> {
    let rest = function_name.strip_prefix("tool_")?;
    if rest.len() < 12 {
        return None;
    }
    let (readable, digest) = rest.split_at(rest.len() - 11);
    if digest.as_bytes()[0] != b'_' {
        return None;
    }
    let digest = &digest[1..];
    if readable.is_empty()
        || readable.len() > 40
        || !readable
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        || !digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return None;
    }
    Some(digest)
}

/// Python `int()` 的可用子集：可选符号、ASCII 数字、数字之间的下划线分隔。
///
/// 不搬的部分：Unicode 数字、`int()` 允许的其它空白形态；整数溢出按解析失败处理
/// （Python 是任意精度整数）。
fn parse_python_int(text: &str) -> Option<i64> {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() {
        return None;
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    if cleaned.is_empty() || !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    cleaned.parse::<i64>().ok().map(|value| value * sign)
}
