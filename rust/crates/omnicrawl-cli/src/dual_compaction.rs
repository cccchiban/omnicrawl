//! 阈值触发的双段压缩：工具调用压缩与模型回复压缩**并发**发起，两段结果拼接。
//!
//! 两次请求都走主模型通道（`[tool_output_compression].model_key` 未设置时即主模型），
//! 以 Low 思考深度、流式发起；工具调用压缩排在前面、模型回复压缩排在后面，拼起来由
//! 调用方接在系统提示词之后作为上下文。
//!
//! 原始工具调用先落盘到工作区临时目录，概括文本末尾给出路径，告知模型需要逐字核对时去读。

use std::sync::mpsc;
use std::thread;

use omnicrawl_compaction::dual::{write_raw_archive, RawToolCall};
use omnicrawl_controllers::compression::{
    archived_notice, join_compaction_sections, DualCompactionInput, DualCompactionOutput,
    DualCompactionPort, TurnCallRecord,
};

use crate::compression::KernelCompressor;

/// 双段压缩的宿主实现。
pub struct KernelDualCompaction {
    compressor: KernelCompressor,
}

impl KernelDualCompaction {
    pub fn new(compressor: KernelCompressor) -> Self {
        Self { compressor }
    }

    /// 两次请求各在自己的线程里跑：一次请求一个运行时，互不排队。
    fn run_both(
        &self,
        input: &DualCompactionInput,
    ) -> (Result<String, String>, Result<String, String>) {
        let (sender, receiver) = mpsc::channel();
        let tools = {
            let compressor = self.compressor.clone();
            let calls = input.tool_calls.clone();
            let hint = input.task_hint.clone();
            let sender = sender.clone();
            thread::spawn(move || {
                let result = if calls.is_empty() {
                    Ok(String::new())
                } else {
                    compressor.summarize_turn(&calls, &hint)
                };
                let _ = sender.send(("tools", result));
            })
        };
        let replies = {
            let compressor = self.compressor.clone();
            let texts = input.replies.clone();
            let hint = input.task_hint.clone();
            let sender = sender.clone();
            thread::spawn(move || {
                let result = if texts.is_empty() {
                    Ok(String::new())
                } else {
                    compressor.compact_replies(&texts, &hint)
                };
                let _ = sender.send(("replies", result));
            })
        };
        drop(sender);

        let mut tools_result: Option<Result<String, String>> = None;
        let mut replies_result: Option<Result<String, String>> = None;
        for (kind, result) in receiver {
            if kind == "tools" {
                tools_result = Some(result);
            } else {
                replies_result = Some(result);
            }
        }
        let _ = tools.join();
        let _ = replies.join();
        (
            tools_result.unwrap_or_else(|| Ok(String::new())),
            replies_result.unwrap_or_else(|| Ok(String::new())),
        )
    }
}

impl DualCompactionPort for KernelDualCompaction {
    /// 落盘 → 并发两段压缩 → 拼接；两段都失败时返回 `Err`（调用方保留原文）。
    fn compact(&self, input: &DualCompactionInput) -> Result<DualCompactionOutput, String> {
        let raw = raw_records(&input.tool_calls);
        let raw_chars = raw_chars(&raw);
        let archive_path = if raw.is_empty() {
            String::new()
        } else {
            match write_raw_archive(&input.workspace_root, &input.session_id, &raw) {
                Ok(path) => path,
                Err(detail) => {
                    eprintln!("[kernel] 工具调用落盘失败：{detail}");
                    String::new()
                }
            }
        };

        let (tools, replies) = self.run_both(&input);
        let mut tools_text = match tools {
            Ok(text) => text,
            Err(detail) => {
                eprintln!("[kernel] 工具输出压缩失败：{detail}");
                String::new()
            }
        };
        let replies_text = match replies {
            Ok(text) => text,
            Err(detail) => {
                eprintln!("[kernel] 模型回复压缩失败：{detail}");
                String::new()
            }
        };
        if !archive_path.is_empty() {
            tools_text.push_str(&archived_notice(&archive_path, raw_chars));
        }
        let Some(text) = join_compaction_sections(&tools_text, &replies_text) else {
            return Err("两次压缩都没有产出可用文本。".to_string());
        };
        Ok(DualCompactionOutput {
            text,
            archive_path,
            raw_chars,
        })
    }
}

/// 控制器层的调用记录 → 落盘用的原始调用。
fn raw_records(calls: &[TurnCallRecord]) -> Vec<RawToolCall> {
    calls
        .iter()
        .map(|call| RawToolCall {
            tool: call.tool.clone(),
            arguments: call.arguments.clone(),
            ok: call.ok,
            output: call.output.clone(),
        })
        .collect()
}

fn raw_chars(calls: &[RawToolCall]) -> usize {
    calls
        .iter()
        .map(|call| call.arguments.chars().count() + call.output.chars().count())
        .sum()
}
