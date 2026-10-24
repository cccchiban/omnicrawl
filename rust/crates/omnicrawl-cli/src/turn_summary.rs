//! 运行中的逐批工具调用概括：把「刚变老的那一批」压成一段，替换上下文里的逐条请求与结果。
//!
//! 压缩原理与决策模型的工具调用淘汰同构：都在回合运行过程中**逐批**决定，而不是等回合
//! 结束后把所有工具调用一股脑压掉。钩子点是下一批执行之前（`ToolBatchHost::before_tool_batch`），
//! 因此压缩点始终贴着上下文尾部，最新一批（仍在被使用）永不参与。
//!
//! 结果写成 `tool_call_summary` 会话事件，投影按 `covered_event_ids` 剔除这一批的原始工具事件、
//! 就地插入概括；原始内容落盘到工作区临时目录，概括文本末尾给出路径供模型回读。
//!
//! 概括失败一律回退原文（不写事件、不动上下文），只记日志。

use omnicrawl_controllers::compression as compression_logic;

use crate::compression::KernelCompressor;

/// 一批概括的结果：写进上下文的正文与落盘事实。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnSummaryOutcome {
    pub content: String,
    /// 被概括的工具事件 ID：`tool_call_summary` 边界据此把它们从上下文里剔除。
    ///
    /// 逐批概括由调用方按批填入（见 `RemoteTools::compact_batch`）。
    pub covered_event_ids: Vec<String>,
    pub archive_path: String,
    pub tool_calls: usize,
    /// 被概括的原始调用（参数 + 输出）字符数。
    pub raw_chars: usize,
    /// 概括正文的字符数（不含末尾的落盘说明）：会话区计量要与原始字符数对照。
    pub summary_chars: usize,
}

/// 概括一批工具调用；没有调用或压缩失败时返回 `None`（调用方保留原文）。
///
/// `workspace_root`/`session_id` 用于落盘原文；为空时跳过落盘，概括照常进行。
pub fn summarize_batch(
    compressor: &KernelCompressor,
    calls: &[compression_logic::TurnCallRecord],
    task_hint: &str,
    workspace_root: &str,
    session_id: &str,
) -> Option<TurnSummaryOutcome> {
    if calls.is_empty() {
        return None;
    }
    let raw = calls
        .iter()
        .map(|call| omnicrawl_compaction::RawToolCall {
            tool: call.tool.clone(),
            arguments: call.arguments.clone(),
            ok: call.ok,
            output: call.output.clone(),
        })
        .collect::<Vec<_>>();
    let raw_chars = omnicrawl_compaction::raw_chars(&raw);
    let archive_path =
        match omnicrawl_compaction::write_raw_archive(workspace_root, session_id, &raw) {
            Ok(path) => path,
            Err(detail) => {
                // 落盘失败不阻断概括：模型只是拿不到「可回读原文」的路径。
                eprintln!("[kernel] 工具调用原文落盘失败：{detail}");
                String::new()
            }
        };
    let mut content = match compressor.summarize_turn(calls, task_hint) {
        Ok(content) => content,
        Err(detail) => {
            eprintln!("[kernel] 逐批工具调用压缩失败，保留原文：{detail}");
            return None;
        }
    };
    // 计量取概括正文本身的字符数：末尾的落盘说明是给模型回读用的，不算进压缩产出。
    let summary_chars = content.chars().count();
    if !archive_path.is_empty() {
        content.push_str(&compression_logic::archived_notice(
            &archive_path,
            raw_chars,
        ));
    }
    Some(TurnSummaryOutcome {
        content,
        covered_event_ids: Vec::new(),
        archive_path,
        tool_calls: calls.len(),
        raw_chars,
        summary_chars,
    })
}
