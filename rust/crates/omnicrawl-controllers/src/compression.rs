//! 工具输出压缩：选取、裁剪、提示词与回包清洗。
//!
//! 语义基准是 Python `omnicrawl/agent/controllers/tools/compression.py` 与
//! `omnicrawl/agent/runtime/tool_output_compressor.py` 的**纯逻辑**部分：
//! 哪些结果值得压缩、给压缩模型的参数摘要与消息、超长输出采样、回包清洗与截断。
//! 真正的模型请求由内核运行时（`omnicrawl-llm`）承担，调用方拿到清理后的文本后决定是否采纳。

use serde_json::Value;

pub const ARGUMENTS_PREVIEW_CHARS: usize = 600;

pub const MAX_PARALLEL_COMPRESSIONS: usize = 4;

pub const COMPACTION_GRACE_SECONDS: f64 = 5.0;

pub const COMPACTABLE_TOOLS: [&str; 4] = ["bash", "powershell", "git", "grep"];

/// 工具结果是否值得压缩：属于压缩作用域且模型可见文本够长。
pub fn should_compact(tool_name: &str, output: &str, min_chars: usize) -> bool {
    if !COMPACTABLE_TOOLS.contains(&tool_name) {
        return false;
    }
    if output.trim().is_empty() {
        return false;
    }
    output.chars().count() >= min_chars
}

/// 并发压缩的线程数上限。
pub fn parallel_compressions(eligible: usize) -> usize {
    std::cmp::min(MAX_PARALLEL_COMPRESSIONS, eligible)
}

/// 整批压缩的等待上限：模型请求超时 + 宽限。
pub fn compaction_wait_seconds(timeout_seconds: f64) -> f64 {
    timeout_seconds + COMPACTION_GRACE_SECONDS
}

/// TUI/会话显示文本：只呈现压缩结果，不再附原始输出。
pub fn compacted_display(
    compressed: &str,
    compressed_chars: usize,
    raw_chars: usize,
    model: &str,
) -> String {
    format!("（已压缩：{raw_chars} → {compressed_chars} 字符，模型 {model}）\n{compressed}")
}

/// 工具参数摘要：超长（如 `write_file` 正文）截断，供压缩模型判断调用意图。
pub fn arguments_summary(arguments: &Value) -> String {
    let Value::Object(map) = arguments else {
        return String::new();
    };
    if map.is_empty() {
        return String::new();
    }
    let text = crate::json::python_dumps(arguments, 0);
    if text.chars().count() <= ARGUMENTS_PREVIEW_CHARS {
        return text;
    }
    let head: String = text.chars().take(ARGUMENTS_PREVIEW_CHARS).collect();
    format!("{head}…")
}

/// 压缩器使用的内置系统提示模板（与 Python 同名文件逐字节一致，编译期嵌入）。
const SYSTEM_TEMPLATE: &str =
    include_str!("../../../../omnicrawl/templates/tool_output_compression_system.md");

/// 原始输出在提示词里的包裹标记。
pub const OUTPUT_OPEN: &str = "<<<TOOL_OUTPUT_START>>>";
pub const OUTPUT_CLOSE: &str = "<<<TOOL_OUTPUT_END>>>";
/// 超长输出中间省略说明。
pub const OMITTED_NOTE: &str = "…（原始输出过长，中间部分已省略）…";
/// 截断后的收尾说明。
pub const TRUNCATED_NOTE: &str = "\n…压缩结果已截断。";

/// 内置压缩系统提示（去掉首尾空白，与 Python `system_prompt_text` 一致）。
pub fn system_prompt_text() -> String {
    SYSTEM_TEMPLATE.trim().to_string()
}

/// 本次压缩请求的思考深度：关闭思考时显式下发 `none`。
pub fn effective_reasoning_effort(thinking_enabled: bool, reasoning_effort: &str) -> String {
    if thinking_enabled {
        reasoning_effort.to_string()
    } else {
        "none".to_string()
    }
}

/// 构造压缩请求：任务背景 + 工具调用摘要 + 被包裹的原始输出。
pub fn build_messages(
    tool_name: &str,
    arguments_summary: &str,
    task_hint: &str,
    output: &str,
) -> Vec<Value> {
    let task_text = task_hint.trim();
    let task_text = if task_text.is_empty() {
        "（未提供）"
    } else {
        task_text
    };
    let arguments_text = arguments_summary.trim();
    let arguments_text = if arguments_text.is_empty() {
        "（无参数）"
    } else {
        arguments_text
    };
    let content = format!(
        "请压缩下面这次工具调用的原始输出。\n\n\
## 当前任务\n{task_text}\n\n\
## 工具调用\n工具：{tool_name}\n参数摘要：{arguments_text}\n\n\
## 原始输出（{} 字符）\n{OUTPUT_OPEN}\n{output}\n{OUTPUT_CLOSE}\n",
        output.chars().count()
    );
    vec![serde_json::json!({"role": "user", "content": content})]
}

/// 按头尾采样（头 60%）把超长输出压到输入预算内，保留「已省略」说明。
pub fn sample_output(output: &str, max_chars: usize) -> String {
    let budget = max_chars.max(1);
    let total = output.chars().count();
    if total <= budget {
        return output.to_string();
    }
    let note_cost = OMITTED_NOTE.chars().count();
    let usable = std::cmp::max(2, budget.saturating_sub(note_cost));
    // 与 Python 的 `int(usable * 0.6)` 同一套浮点语义（截断），避免整数除法在边界上分道。
    let head_chars = (usable as f64 * 0.6) as usize;
    let tail_chars = usable - head_chars;
    let head: String = output.chars().take(head_chars).collect();
    let tail: String = if tail_chars == 0 {
        String::new()
    } else {
        let skip = total - tail_chars;
        output.chars().skip(skip).collect()
    };
    format!("{head}{OMITTED_NOTE}{tail}")
}

/// 回包清洗：去掉代码围栏与「压缩结果：」这类标签首行。
pub fn clean_reply_text(text: &str) -> String {
    let mut cleaned = text.trim().to_string();
    if cleaned.starts_with("```") {
        let mut lines: Vec<&str> = cleaned.lines().collect();
        lines.remove(0);
        if lines
            .last()
            .map(|line| line.trim().starts_with("```"))
            .unwrap_or(false)
        {
            lines.pop();
        }
        cleaned = lines.join("\n").trim().to_string();
    }
    let lines: Vec<&str> = cleaned.lines().collect();
    if lines.len() > 1 && is_label_line(lines[0]) {
        cleaned = lines[1..].join("\n").trim().to_string();
    }
    cleaned
}

/// 是否是「压缩 / 摘要」一类标签行（去尾冒号后不超过 8 字）。
pub fn is_label_line(line: &str) -> bool {
    let compact = line.trim().trim_end_matches(['：', ':']).trim();
    compact.chars().count() <= 8
        && matches!(
            compact,
            "压缩" | "摘要" | "结果" | "输出" | "压缩结果" | "压缩后" | "压缩输出" | "摘要结果"
        )
}

/// 按输出预算截断压缩结果。
pub fn bound_text(text: &str, max_chars: usize) -> String {
    let limit = max_chars.max(1);
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}{TRUNCATED_NOTE}")
}

/// 异常是否看起来是取消：类型名或消息里含 `cancel`（大小写折叠）。
pub fn looks_like_cancellation(type_name: &str, message: &str) -> bool {
    type_name.to_lowercase().contains("cancel") || message.to_lowercase().contains("cancel")
}
