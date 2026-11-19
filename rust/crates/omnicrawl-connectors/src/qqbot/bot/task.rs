//! 运行态类型：审批/提问桥、任务队列与网关会话。
//!
//! 这些结构彼此强耦合（`BotState` 同时持有审批与提问的槽位，`ActiveTask` 由任务队列排队），
//! 又与网关循环、消息记录无关，因此独立成模块。可见性统一放宽到 `pub(super)`：只有
//! `bot` 主模块及其子模块读写它们，对外部 crate 不暴露。

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::qqbot::dispatch::Inbound;
use crate::platform::PendingSlot;
use super::reply::ReplyHandle;

pub(super) struct PendingConfirm {
    pub(super) tool_name: String,
    pub(super) sender: String,
    pub(super) slot: PendingSlot<bool>,
}

impl PendingConfirm {
    pub(super) fn new(tool_name: &str, sender: &str) -> PendingConfirm {
        PendingConfirm {
            tool_name: tool_name.to_string(),
            sender: sender.to_string(),
            slot: PendingSlot::new(),
        }
    }

    pub(super) fn wait(&self, timeout: Duration) -> Option<bool> {
        self.slot.wait(timeout)
    }

    pub(super) fn resolve(&self, decision: bool) {
        self.slot.resolve(Some(decision));
    }

    /// 停止指令：解除等待但不记录答复，调用方按超时（拒绝）处理。
    pub(super) fn release(&self) {
        self.slot.release();
    }

}

/// 一次等待用户回答的 `ask_user` 请求。
pub(super) struct PendingQuestion {
    pub(super) kind: String,
    pub(super) question: String,
    pub(super) options: Vec<String>,
    pub(super) sender: String,
    pub(super) slot: PendingSlot<String>,
}

impl PendingQuestion {
    /// 提问提示：没有选项时只提示直接回复，有选项时列出编号。
    pub(super) fn prompt(&self) -> String {
        if self.options.is_empty() {
            let mut text = format!("❓ {}\n请直接回复答案。", self.question);
            if self.kind == "confirm" {
                text.push_str("\n（例如：是 / 否）");
            }
            return text;
        }
        let list = self
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| format!("{}. {option}", index + 1))
            .collect::<Vec<String>>()
            .join("\n");
        format!("❓ {}\n{list}\n直接回复选项正文即可作答。", self.question)
    }

    pub(super) fn wait(&self, timeout: Duration) -> Option<String> {
        self.slot.wait(timeout)
    }

    pub(super) fn resolve(&self, answer: Option<String>) {
        self.slot.resolve(answer);
    }

    /// 停止指令：解除等待但不记录答复，本轮按未作答继续。
    pub(super) fn release(&self) {
        self.slot.release();
    }

}

/// 当前正在执行的任务。
pub(super) struct ActiveTask {
    pub sender: String,
    pub text: String,
    /// 群聊历史里的图片（`(url, 建议文件名)`）：任务线程开工前尽量补下来交给 Agent。
    pub photos: Vec<(String, String)>,
    pub reply: Arc<ReplyHandle>,
    pub cancel: Arc<AtomicBool>,
    pub started_at: Instant,
    pub(super) running: Arc<AtomicBool>,
    /// 入队序号：>0 表示这条任务是排队进来的（不是到来即执行）。
    pub queued: u64,
}

/// 排队等待执行的任务。
pub(super) struct QueuedTask {
    pub inbound: Inbound,
    pub reply: Arc<ReplyHandle>,
}

#[derive(Default)]
pub(super) struct BotState {
    pub(super) active: Option<Arc<ActiveTask>>,
    /// 等待执行的任务（先进先出）。忙碌时新消息进这里，而不是被拒绝。
    pub(super) queue: VecDeque<QueuedTask>,
    /// 入队计数：只用于给任务编号、判断「这条是不是排队进来的」。
    pub(super) queued_total: u64,
    pub(super) pending: Option<Arc<PendingConfirm>>,
    pub(super) question: Option<Arc<PendingQuestion>>,
}

/// 网关会话（Resume 要用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct GatewaySession {
    pub(super) session_id: String,
    pub(super) seq: i64,
}
