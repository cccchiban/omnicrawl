//! `omnicrawl/agent/controllers/tools/compression.py`：工具输出压缩的选取、文案与参数摘要。
//!
//! 压缩本身是外接小模型的一次旁路调用（`omnicrawl/agent/runtime/tool_output_compressor.py`），
//! 属于宿主边界；这里只搬「哪些结果值得压缩」「压缩后怎么呈现」「给压缩模型的参数摘要」。

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
