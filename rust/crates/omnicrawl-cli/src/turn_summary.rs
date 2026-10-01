//! 回合末的整轮工具调用概括：把一轮全部工具调用压成一段，替换上下文里的逐条请求与结果。
//!
//! 与逐条压缩的差别只在粒度与落点：
//!
//! - 触发时机从「工具批次执行完」变成「回合结束」，一次请求覆盖整轮全部调用；
//! - 参与范围不再按工具名与长度筛选，本轮所有调用都进来；
//! - 原始内容落盘到工作区临时目录，概括文本里给出路径告知模型可以回读；
//! - 概括结果写成 `tool_call_summary` 会话事件，投影据此剔除原来的工具事件、插入概括。
//!
//! 概括失败一律回退原文（不写事件、不动上下文），只记日志。

use omnicrawl_controllers::compression as compression_logic;
use omnicrawl_controllers::context_compaction::SourceEvent;

use crate::compression::KernelCompressor;

/// 一轮概括的输入。
pub struct TurnSummaryRequest<'a> {
    pub events: &'a [SourceEvent],
    pub task_hint: &'a str,
    pub workspace_root: &'a str,
    pub session_id: &'a str,
}

/// 一轮概括的结果：写进上下文的正文与落盘事实。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnSummaryOutcome {
    pub content: String,
    /// 被概括的工具事件 ID：`tool_call_summary` 边界据此把它们从上下文里剔除。
    pub covered_event_ids: Vec<String>,
    pub archive_path: String,
    pub tool_calls: usize,
    /// 被概括的原始调用（参数 + 输出）字符数。
    pub raw_chars: usize,
    /// 概括正文的字符数（不含末尾的落盘说明）：会话区计量要与原始字符数对照。
    pub summary_chars: usize,
}

/// 跑一次整轮概括；没有可概括的调用、或概括失败时返回 `None`。
pub fn summarize_turn(
    compressor: &KernelCompressor,
    request: &TurnSummaryRequest<'_>,
) -> Option<TurnSummaryOutcome> {
    let calls = compression_logic::collect_tool_calls(request.events);
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
    let archive_path = match omnicrawl_compaction::write_raw_archive(
        request.workspace_root,
        request.session_id,
        &raw,
    ) {
        Ok(path) => path,
        Err(detail) => {
            // 落盘失败不阻断概括：模型只是拿不到「可回读原文」的路径。
            eprintln!("[kernel] 工具调用原文落盘失败：{detail}");
            String::new()
        }
    };
    let mut content = match compressor.summarize_turn(&calls, request.task_hint) {
        Ok(content) => content,
        Err(detail) => {
            eprintln!("[kernel] 本轮工具调用概括失败，保留原文：{detail}");
            return None;
        }
    };
    // 计量取概括正文本身的字符数：末尾的落盘说明是给模型回读用的，不算进压缩产出。
    let summary_chars = content.chars().count();
    if !archive_path.is_empty() {
        content.push_str(&compression_logic::archived_notice(&archive_path, raw_chars));
    }
    Some(TurnSummaryOutcome {
        content,
        covered_event_ids: compression_logic::tool_event_ids(request.events),
        archive_path,
        tool_calls: calls.len(),
        raw_chars,
        summary_chars,
    })
}
