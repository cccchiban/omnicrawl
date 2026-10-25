//! 工具输出压缩：选取、裁剪、提示词与回包清洗。
//!
//! 语义基准是 Python `omnicrawl/agent/controllers/tools/compression.py` 与
//! `omnicrawl/agent/runtime/tool_output_compressor.py` 的**纯逻辑**部分：
//! 哪些结果值得压缩、给压缩模型的参数摘要与消息、超长输出采样、回包清洗与截断。
//! 真正的模型请求由内核运行时（`omnicrawl-llm`）承担，调用方拿到清理后的文本后决定是否采纳。

use serde_json::Value;

use crate::context_compaction::SourceEvent;

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

/// TUI/会话显示文本：只呈现压缩结果，不再附原始输出。
pub fn compacted_display(
    compressed: &str,
    compressed_chars: usize,
    raw_chars: usize,
    model: &str,
) -> String {
    format!("（已压缩：{raw_chars} → {compressed_chars} 字符，模型 {model}）\n{compressed}")
}

/// 回合末整轮工具调用压缩的会话区计量：折算成「省下多少 Token」。
///
/// 计量必须**可回放**：这条提示的实时值与 `/resume`、`/undo` 重放时重建的值必须逐字相同，
/// 所以事件载荷落的是字符数（原始与概括），Token 数在展示时用同一套折算重算，
/// 不在事件里另存一份（否则两处口径迟早漂移）。
///
/// 折算按 CJK 以外的保守比率（4 字符 1 token）：原文与概括的字符数是唯一留存的事实，
/// 拿不到原文时这是**下界**，不会把压缩效果说得比实际更好。文案本体与上下文压缩共用
/// [`crate::turn::compaction::format_compaction_savings_notice`]；没有净省时返回 `None`。
pub fn saved_tokens_notice(raw_chars: usize, summary_chars: usize) -> Option<String> {
    let saved = chars_token_floor(raw_chars) - chars_token_floor(summary_chars);
    crate::turn::compaction::format_compaction_savings_option(saved)
}

/// 纯字符数的 Token 下界折算（4 字符 1 token）。
///
/// 规则本体在 [`crate::context_compaction::meter::chars_token_floor`]：上下文压缩与工具段压缩
/// 的计量文案共用同一份折算，避免「两条链路报不同的省量」。
pub fn chars_token_floor(chars: usize) -> i64 {
    crate::context_compaction::meter::chars_token_floor(chars)
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
    include_str!("../../../../rust/assets/templates/tool_output_compression_system.md");

/// 回合末「全部工具调用」概括的系统提示模板。
const TURN_SUMMARY_TEMPLATE: &str =
    include_str!("../../../../rust/assets/templates/tool_call_summary_system.md");

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

// ── 回合结束的整轮工具调用概括 ─────────────────────────────────────────────

/// 本轮工具调用在概括提示里的包裹标记。
pub const CALLS_OPEN: &str = "<<<TOOL_CALLS_START>>>";
pub const CALLS_CLOSE: &str = "<<<TOOL_CALLS_END>>>";

/// 回合末概括的系统提示。
pub fn turn_summary_system_prompt() -> String {
    TURN_SUMMARY_TEMPLATE.trim().to_string()
}

/// 一次工具调用在概括输入里的形状：调用请求 + 执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCallRecord {
    pub tool: String,
    pub arguments: String,
    pub ok: bool,
    pub output: String,
}

/// 把整轮调用渲染成概括提示的正文（不可信数据段）。
pub fn render_turn_calls(calls: &[TurnCallRecord]) -> String {
    let mut sections: Vec<String> = Vec::with_capacity(calls.len());
    for (index, call) in calls.iter().enumerate() {
        let arguments = call.arguments.trim();
        let arguments = if arguments.is_empty() {
            "（无参数）"
        } else {
            arguments
        };
        sections.push(format!(
            "### 第 {} 次调用\n工具：{}\n参数：{arguments}\n状态：{}\n输出：\n{}",
            index + 1,
            call.tool,
            if call.ok { "成功" } else { "失败" },
            call.output.trim()
        ));
    }
    sections.join("\n\n")
}

/// 构造回合概括请求：任务背景 + 整轮调用。
pub fn build_turn_summary_messages(calls: &[TurnCallRecord], task_hint: &str) -> Vec<Value> {
    let task_text = task_hint.trim();
    let task_text = if task_text.is_empty() {
        "（未提供）"
    } else {
        task_text
    };
    let body = render_turn_calls(calls);
    let content = format!(
        "请把下面这一轮的全部工具调用概括成一段内容。\n\n\
## 当前任务\n{task_text}\n\n\
## 本轮工具调用（共 {} 次）\n{CALLS_OPEN}\n{body}\n{CALLS_CLOSE}\n",
        calls.len()
    );
    vec![serde_json::json!({"role": "user", "content": content})]
}

/// 原始调用原文的落盘说明：告知模型内容没有丢，只是换了存放位置。
pub fn archived_notice(path: &str, chars: usize) -> String {
    format!(
        "\n\n（本轮工具调用原文共 {chars} 字符已存放于：{path}；\
         需要逐字核对原始输出时读取该文件。）"
    )
}

/// 把「工具调用压缩」与「会话压缩摘要」两段拼成一条上下文文本。
///
/// 顺序固定：工具段在前、摘要段在后。这是压缩链的执行顺序决定的——先淘汰无用的工具调用，
/// 再把剩下的工具调用压成一段，最后把非工具部分交给上下文压缩；两段合成一条 assistant
/// 消息注入历史。
pub fn join_tool_and_summary_sections(tools: &str, summary: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let tools = tools.trim();
    if !tools.is_empty() {
        parts.push(format!("## 工具调用压缩\n{tools}"));
    }
    let summary = summary.trim();
    if !summary.is_empty() {
        parts.push(summary.to_string());
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n\n"))
}

/// 把两次压缩结果拼成一段接在系统提示词后的上下文文本。
///
/// 顺序固定：先工具调用压缩，再模型回复压缩；空段跳过，两段都空返回 `None`。
///
/// 模型回复压缩已不在压缩链上（非工具部分改由结构化摘要承担），保留本函数与其调用方
/// 供需要时回接。
pub fn join_compaction_sections(tools: &str, replies: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let tools = tools.trim();
    if !tools.is_empty() {
        parts.push(format!("## 工具调用压缩\n{tools}"));
    }
    let replies = replies.trim();
    if !replies.is_empty() {
        parts.push(format!("## 模型回复压缩\n{replies}"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n\n"))
}

// ── 阈值触发的模型回复压缩 ─────────────────────────────────────────────────

/// 历史回复在压缩提示里的包裹标记。
pub const REPLIES_OPEN: &str = "<<<MODEL_REPLIES_START>>>";
pub const REPLIES_CLOSE: &str = "<<<MODEL_REPLIES_END>>>";

// ── 阈值触发的双段压缩 ────────────────────────────────────────────────────

/// 一次双段压缩的输入：被压缩的工具调用、模型回复与落盘位置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DualCompactionInput {
    pub tool_calls: Vec<TurnCallRecord>,
    pub replies: Vec<String>,
    pub task_hint: String,
    /// 工作区根：原始工具调用落盘到它下的临时目录。
    pub workspace_root: String,
    pub session_id: String,
}

/// 一次双段压缩的结果：拼接后的正文与落盘事实。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DualCompactionOutput {
    /// 两段拼接后的正文（工具调用压缩在前，模型回复压缩在后）。
    pub text: String,
    /// 原始工具调用的落盘路径（空表示没有落盘）。
    pub archive_path: String,
    pub raw_chars: usize,
}

/// 双段压缩端口：宿主实现（两次请求并发发起），失败返回可读错误。
///
/// 压缩链当前的顺序是「淘汰 → 工具段 → 非工具段 → 拼接」，工具段由宿主侧的整轮概括
/// （`turn_summary`）承担、非工具段由结构化摘要承担，因此本端口暂时没有调用方；
/// 连同模型回复压缩（`compact_replies`）一起保留实现，以便需要时回接。
pub trait DualCompactionPort {
    fn compact(&self, input: &DualCompactionInput) -> Result<DualCompactionOutput, String>;
}

// ── 阈值触发的工具段压缩 ──────────────────────────────────────────────────

/// 一次工具段压缩的输入：被压缩的工具调用与落盘位置。
///
/// 工具段只处理工具调用——非工具部分（用户消息、助手回复、更早的工具索引）由上下文压缩
/// 承担，两段在驱动侧拼成一条助手消息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCompactionInput {
    pub tool_calls: Vec<TurnCallRecord>,
    pub task_hint: String,
    /// 工作区根：原始工具调用落盘到它下的临时目录。
    pub workspace_root: String,
    pub session_id: String,
}

/// 一次工具段压缩的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCompactionOutput {
    /// 工具段正文（概括 + 可选的落盘说明）。
    pub text: String,
    /// 被概括的原始调用（参数 + 输出）字符数。
    pub raw_chars: usize,
    /// 概括正文本身的字符数（不含末尾的落盘说明）：会话区计量要与原始字符数对照。
    pub summary_chars: usize,
}

/// 工具段压缩端口：宿主实现（一次请求），失败返回可读错误。
pub trait ToolCompactionPort {
    fn compact(&self, input: &ToolCompactionInput) -> Result<ToolCompactionOutput, String>;
}

/// 窗口里被 `tool_call_evicted` 标注淘汰的调用 ID（去重、按发生顺序）。
///
/// 工具段压缩必须与历史投影同一口径：淘汰的语义是「这次调用的原文已离开模型上下文」，
/// 若照旧送进概括，原文会顺着概括文本回流。
pub fn evicted_call_ids(events: &[SourceEvent]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for event in events {
        if event.event_type != "tool_call_evicted" {
            continue;
        }
        for call_id in event
            .payload
            .get("evicted_call_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let call_id = call_id.trim();
            if !call_id.is_empty() && !seen.iter().any(|item| item == call_id) {
                seen.push(call_id.to_string());
            }
        }
    }
    seen
}

/// 从事件窗口里挑出工具调用：`tool_call_requested` 与配对的 `tool_result` 合成一条。
///
/// 配不上结果的调用也保留（状态为失败、输出留空），否则模型看不到「调用过但没有结果」。
/// 已被淘汰标注的调用不进结果：它们的原文已经不在上下文里，再概括一遍等于让它回流。
pub fn collect_tool_calls(events: &[SourceEvent]) -> Vec<TurnCallRecord> {
    let evicted = evicted_call_ids(events);
    let mut records: Vec<TurnCallRecord> = Vec::new();
    for event in events {
        if event.event_type != "tool_call_requested" {
            continue;
        }
        let tool = payload_text(&event.payload, "tool");
        if tool.is_empty() {
            continue;
        }
        let call_id = payload_text(&event.payload, "tool_call_id");
        if !call_id.is_empty() && evicted.iter().any(|item| *item == call_id) {
            continue;
        }
        let paired = events
            .iter()
            .find(|candidate| {
                candidate.event_type == "tool_result"
                    && !call_id.is_empty()
                    && payload_text(&candidate.payload, "tool_call_id") == call_id
            })
            .or_else(|| {
                // 没有 call_id 的旧事件按工具名就近配对。
                events.iter().find(|candidate| {
                    candidate.event_type == "tool_result"
                        && payload_text(&candidate.payload, "tool") == tool
                })
            });
        let (ok, output) = match paired {
            Some(result) => (
                result
                    .payload
                    .get("ok")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                result_output_text(&result.payload),
            ),
            None => (false, String::new()),
        };
        records.push(TurnCallRecord {
            tool,
            arguments: arguments_preview(&event.payload),
            ok,
            output,
        });
    }
    records
}

/// 窗口里的模型回复（按发生顺序）；空白内容跳过。
pub fn collect_assistant_replies(events: &[SourceEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.event_type == "assistant_message")
        .filter_map(|event| {
            event
                .payload
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .collect()
}

/// 工具相关的事件 ID（请求 / 结果 / 拒绝）：概括边界据此把它们从上下文里剔除。
pub fn tool_event_ids(events: &[SourceEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "tool_call_requested" | "tool_result" | "tool_call_denied"
            )
        })
        .map(|event| event.event_id.clone())
        .collect()
}

/// 指定调用集合相关的工具事件 ID（请求 / 结果 / 拒绝），按发生顺序。
///
/// 逐批压缩用它精确覆盖「刚变老的那一批」：只剔除这些调用自己的事件，更早的批次
/// 与最新一批都不受影响，压缩点因此始终贴着上下文尾部。
pub fn tool_event_ids_for_calls(events: &[SourceEvent], call_ids: &[String]) -> Vec<String> {
    if call_ids.is_empty() {
        return Vec::new();
    }
    events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "tool_call_requested" | "tool_result" | "tool_call_denied"
            )
        })
        .filter(|event| {
            event
                .payload
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(|call_id| call_ids.iter().any(|item| item == call_id))
                .unwrap_or(false)
        })
        .map(|event| event.event_id.clone())
        .collect()
}

/// 工具结果的模型可见正文：与投影同一优先级。
fn result_output_text(payload: &Value) -> String {
    for key in ["model_output", "output_preview", "output"] {
        if let Some(text) = payload.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

/// 工具参数摘要：优先用落盘的公开投影，其次用请求参数；超长按字符截断。
fn arguments_preview(payload: &Value) -> String {
    for key in ["arguments_json", "arguments"] {
        let Some(value) = payload.get(key) else {
            continue;
        };
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Null => continue,
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        return bound_preview(&text);
    }
    String::new()
}

fn bound_preview(text: &str) -> String {
    if text.chars().count() <= ARGUMENTS_PREVIEW_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(ARGUMENTS_PREVIEW_CHARS).collect();
    format!("{head}…")
}

fn payload_text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(test)]
mod dual_compaction_tests {
    use super::*;
    use crate::context_compaction::SourceEvent;
    use serde_json::json;

    fn event(event_id: &str, event_type: &str, payload: Value) -> SourceEvent {
        SourceEvent {
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            payload,
        }
    }

    /// 工具调用与配对结果合成一条；没有结果的调用也保留（状态失败、输出为空）。
    #[test]
    fn tool_calls_pair_with_results_and_keep_orphans() {
        let events = vec![
            event(
                "e1",
                "tool_call_requested",
                json!({"tool": "bash", "tool_call_id": "c1"}),
            ),
            event(
                "e2",
                "tool_result",
                json!({"tool": "bash", "tool_call_id": "c1", "ok": true, "output": "done"}),
            ),
            event(
                "e3",
                "tool_call_requested",
                json!({"tool": "grep", "tool_call_id": "c2"}),
            ),
            event("e4", "assistant_message", json!({"content": "回复"})),
        ];
        let calls = collect_tool_calls(&events);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].output, "done");
        assert!(calls[0].ok);
        assert!(calls[1].output.is_empty(), "没有结果的调用输出留空");
        assert!(!calls[1].ok);

        let replies = collect_assistant_replies(&events);
        assert_eq!(replies, vec!["回复".to_string()]);
    }

    /// 工具事件 ID 只含请求 / 结果 / 拒绝三类：概括边界据此剔除上下文。
    #[test]
    fn tool_event_ids_cover_requests_results_and_denials() {
        let events = vec![
            event("e1", "tool_call_requested", json!({})),
            event("e2", "tool_result", json!({})),
            event("e3", "tool_call_denied", json!({})),
            event("e4", "assistant_message", json!({})),
        ];
        assert_eq!(tool_event_ids(&events), vec!["e1", "e2", "e3"]);
    }

    /// 两段拼接固定顺序，空段跳过；原文落盘说明跟在工具段之后。
    #[test]
    fn sections_join_in_a_fixed_order_and_skip_empty_ones() {
        let joined = join_compaction_sections("工具段", "回复段").expect("两段都非空");
        let tool_at = joined.find("## 工具调用压缩").expect("工具段在前");
        let reply_at = joined.find("## 模型回复压缩").expect("回复段在后");
        assert!(tool_at < reply_at);

        let only_replies = join_compaction_sections("  ", "回复段").expect("只剩回复段");
        assert!(!only_replies.contains("工具调用压缩"));
        assert!(join_compaction_sections("", "").is_none());
    }

    /// 落盘说明给出路径与字符数，模型据此知道原文没有丢。
    #[test]
    fn archived_notice_carries_the_path_and_size() {
        let notice = archived_notice(".omnicrawl/.agent_tmp/files/a.txt", 1234);
        assert!(notice.contains(".omnicrawl/.agent_tmp/files/a.txt"));
        assert!(notice.contains("1234"));
    }

    /// 会话区计量：把字符数折算成「省下多少 Token」，数字带千分位。
    ///
    /// 折算率与 `estimate_text_tokens` 的非 CJK 档同源（4 字符 1 token），取的是下界：
    /// 拿不到原文时宁可少报，也不把压缩效果说得比实际更好。
    #[test]
    fn saved_tokens_notice_converts_char_counts_to_net_savings() {
        assert_eq!(
            saved_tokens_notice(12_345, 1_234).as_deref(),
            Some("已压缩：省 2,778 Token")
        );
        // 没有净省（原文与概括同长、或概括更长）就不出计量行：报一行「省 0 Token」
        // 只会让人以为压缩失败却看到了成功提示。
        assert_eq!(saved_tokens_notice(0, 0), None);
        assert_eq!(saved_tokens_notice(999, 12_345_678), None);
    }
}
