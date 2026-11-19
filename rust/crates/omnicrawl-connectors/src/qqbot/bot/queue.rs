//! 任务队列：同一会话单人在跑，其余排队；跑完自动推进下一条。
//!
//! QQ 侧是「被动回复」模型，一条入站消息只有 5 条回复额度，因此并发跑多个任务会互相抢额度、
//! 也会让用户分不清哪条进度属于哪句话。这里的排队是刻意的节流，不是简化实现。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::agent::AgentDriver;

use crate::qqbot::dispatch::Inbound;
use super::reply::ReplyHandle;
use super::task::{ActiveTask, QueuedTask};
use super::timeline::TaskTimeline;
use super::QqBot;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use crate::agent::{TurnError, TurnEvent};
use crate::qqbot::files::{classify_file_name, destination, relative_display_path, safe_name};
use omnicrawl_session::redact_sensitive_text;
use std::fs;
use std::thread;
use super::task::BotState;

impl<D: AgentDriver> QqBot<D> {
    /// 单活动校验后，在后台线程执行 Agent 任务；已有任务在跑时改为**排队**。
    ///
    /// 排队而不是拒绝：QQ 这边一次问一句很常见，直接回一句「请等」会把用户的话丢掉。
    /// 入队后当前任务一结束就自动接着跑，用户不必重发。队伍的推进全部发生在本函数与
    /// [`QqBot::execute_task`] 的收尾里，都在同一把状态锁下改 `active`/`queue`。
    pub fn start_task(self: &Arc<Self>, inbound: Inbound, reply: Arc<ReplyHandle>) {
        let mut state = self.state.lock().expect("状态锁中毒");
        let busy = state
            .active
            .as_ref()
            .map(|task| task.running.load(Ordering::SeqCst))
            .unwrap_or(false);
        if busy {
            state.queued_total += 1;
            let position = state.queue.len() + 1;
            // 排队后这条提问的过程消息与最终回答都要挂引用：同一会话里可能堆着几条，
            // 不引用就分不清哪条进度、哪句回答属于谁的提问。
            reply.quote_intermediate_in_place(true);
            state.queue.push_back(QueuedTask {
                inbound,
                reply: Arc::clone(&reply),
            });
            drop(state);
            eprintln!("[qqbot] 已有任务在跑，新消息排队（第 {position} 位）。");
            // 只报「已排队、排在第几位」：既不说「开始执行」（它还没开始），也带上引用，
            // 让用户一眼认出这条回执对应的是自己哪句话。
            reply.send_notice(&format!(
                "🕒 已加入队列（前面还有 {position} 条），轮到后自动开始。"
            ));
            return;
        }
        state.queued_total = 0;
        let _ = self.spawn_task_locked(&mut state, inbound, reply, 0);
    }

    /// 在已持锁的状态上起一个任务线程（`start_task` 与队首推进共用）。
    pub(super) fn spawn_task_locked(
        self: &Arc<Self>,
        state: &mut BotState,
        inbound: Inbound,
        reply: Arc<ReplyHandle>,
        queued: u64,
    ) -> bool {
        let task = Arc::new(ActiveTask {
            sender: inbound.sender.clone(),
            photos: self.history_photos(&inbound),
            text: self.task_text_for(&inbound),
            reply: Arc::clone(&reply),
            cancel: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
            running: Arc::new(AtomicBool::new(true)),
            queued,
        });
        state.active = Some(Arc::clone(&task));
        let bot = self.clone();
        let spawned = thread::Builder::new()
            .name("omnicrawl-qqbot-task".to_string())
            .spawn(move || bot.execute_task(task));
        match spawned {
            Ok(_) => true,
            Err(error) => {
                eprintln!("[qqbot] 任务线程启动失败：{error}");
                state.active = None;
                reply.send_final("❌ 任务线程启动失败。");
                false
            }
        }
    }

    /// 把历史图片下载到 `.agent_tmp` 并拼进任务文本；单张失败只跳过，不影响本轮。
    pub(super) fn with_history_photos(&self, task: &ActiveTask) -> String {
        if task.photos.is_empty() {
            return task.text.clone();
        }
        let temp_root = self.driver.temp_root();
        let workspace_root = PathBuf::from(self.driver.status().workspace_root);
        let stamp = crate::qqbot::files::unix_seconds();
        let mut saved: Vec<String> = Vec::new();
        for (index, (url, name)) in task.photos.iter().enumerate() {
            let content = match self.port.download(url) {
                Ok(content) => content,
                Err(error) => {
                    // 签名链接过期是常态，只记日志：历史上的一张图丢了不该让本轮回答失败。
                    eprintln!(
                        "[qqbot] 历史图片下载失败：{}",
                        redact_sensitive_text(&error)
                    );
                    continue;
                }
            };
            let file_name = safe_name(name, url, stamp + index as i64, "");
            let subdir = classify_file_name(&file_name);
            let Ok(target) = destination(&temp_root, subdir, &file_name) else {
                continue;
            };
            if fs::write(&target, &content).is_err() {
                continue;
            }
            saved.push(relative_display_path(&target, &workspace_root));
        }
        if saved.is_empty() {
            return task.text.clone();
        }
        format!(
            "{}\n\n这个群最近消息里出现过的图片（已取到本地，需要时用 read_image 查看）：{}",
            task.text,
            saved.join("、")
        )
    }

    /// 当前任务结束后推进队伍：把队首变成新任务接着跑。
    ///
    /// 返回 `true` 表示确实又起了一个任务；`false` 表示队伍空、可以安静收尾。
    ///
    /// 队首起线程失败时继续推进下一条：否则 `active` 已清空、队列却没人接着跑，
    /// 后面排着的消息会永久滞留。
    pub(super) fn advance_queue(self: &Arc<Self>) -> bool {
        loop {
            let mut state = self.state.lock().expect("状态锁中毒");
            let Some(QueuedTask { inbound, reply }) = state.queue.pop_front() else {
                return false;
            };
            let queued = state.queued_total;
            let started = self.spawn_task_locked(&mut state, inbound, reply, queued);
            drop(state);
            if started {
                return true;
            }
            // 失败已经发过提示消息，接着试下一条，避免整条队列卡死。
            eprintln!("[qqbot] 队首任务启动失败，继续推进下一条。");
        }
    }

    /// 任务线程：驱动回合，并把事件映射成按发生顺序发出的消息。
    pub(super) fn execute_task(self: &Arc<Self>, task: Arc<ActiveTask>) {
        let text = self.with_history_photos(task.as_ref());
        let mut timeline = TaskTimeline::new(self, Arc::clone(&task.reply));
        let result = {
            let mut events = |event: TurnEvent| timeline.handle(event);
            self.driver.run_turn(&text, &mut events)
        };
        let cancelled = task.cancel.load(Ordering::SeqCst);
        self.finish_task(task.as_ref(), &mut timeline, result, cancelled);
        task.running.store(false, Ordering::SeqCst);
        {
            let mut state = self.state.lock().expect("状态锁中毒");
            let same = state
                .active
                .as_ref()
                .map(|active| Arc::ptr_eq(active, &task))
                .unwrap_or(false);
            if same {
                state.active = None;
            }
        }
        // 有排队的话立刻接着跑，用户不必重发。
        self.advance_queue();
    }

    pub(super) fn finish_task(
        &self,
        task: &ActiveTask,
        timeline: &mut TaskTimeline<D>,
        result: Result<String, TurnError>,
        cancelled: bool,
    ) {
        if cancelled {
            timeline.flush_intermediate();
            self.abort_task(task, "⏹ 任务已取消。");
            return;
        }
        match result {
            Ok(reply) => {
                let raw = if reply.is_empty() {
                    timeline.all_deltas()
                } else {
                    reply
                };
                timeline.flush_intermediate();
                timeline.finish(&raw);
                if !timeline.sent_any {
                    task.reply.send_final("（任务完成，无文本输出）");
                }
            }
            Err(TurnError::Cancelled) => {
                timeline.flush_intermediate();
                self.abort_task(task, "⏹ 任务已取消。");
            }
            Err(TurnError::Failed(message)) => {
                timeline.flush_intermediate();
                eprintln!("[qqbot] 任务执行失败：{message}");
                task.reply.send_final(&format!(
                    "❌ 任务执行失败：{}",
                    redact_sensitive_text(&message)
                ));
            }
        }
    }

    /// 保留已输出的正文段，错误或取消提示单独发送。
    pub(super) fn abort_task(&self, task: &ActiveTask, error_text: &str) {
        task.reply.send_final(error_text);
    }
}
