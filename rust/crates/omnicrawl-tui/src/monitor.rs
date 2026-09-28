//! 全屏工作台的 Monitor 状态适配（对映 Python `ui/fullscreen/support/monitor.py`）。
//!
//! 这里只保存**仅属于本界面**的日志消费游标与工作区切换暂停状态：后台任务的进程、
//! 日志缓冲与事件序号都在 `MonitorManager` 里。本模块不启动定时器、也不渲染组件——
//! `App` 按 `MONITOR_POLL_INTERVAL` 调度 `refresh`，再把返回的结构化批次按任务归并进
//! 消息流的同一张卡片（`AppState::push_monitor_batch`）。
//!
//! 这样本地 API、模型工具与其他消费者各自持有独立的消费游标，互不影响。

use std::collections::BTreeMap;
use std::time::Duration;

use crate::tools::monitor::{MonitorEventView, MonitorPollView, MonitorTaskView};
use crate::tools::MonitorManager;

/// 轮询周期（与 Python `OmniCrawlApp.MONITOR_POLL_INTERVAL_SECONDS` 一致：0.5 秒）。
pub const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 单次轮询每个任务最多取回的事件数（与 Python `MonitorStateAdapter(max_events=50)` 一致）。
pub const MONITOR_MAX_EVENTS: usize = 50;

/// 一次刷新里可整批追加进消息流的单个任务增量。
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorDisplayBatch {
    pub monitor_id: String,
    pub status: String,
    pub events: Vec<MonitorEventView>,
}

/// 刷新依赖的最小只读来源：真实现是 `MonitorManager`，测试可注入桩。
///
/// 对映 Python 的 `MonitorAgent` Protocol（`list_monitor_tasks` / `poll_monitor_events`）：
/// 两种实现都只读，不启动也不终止任务。取不到任务时返回空集合、取不到单任务时返回 `None`，
/// 由适配器按「静默跳过本轮」处理——UI 的轮询不干扰模型回合与其他后台任务。
pub trait MonitorSource {
    fn list_tasks(&self) -> Vec<MonitorTaskView>;
    fn poll_events(
        &self,
        monitor_id: &str,
        cursor: u64,
        max_events: usize,
    ) -> Option<MonitorPollView>;
}

impl MonitorSource for MonitorManager {
    fn list_tasks(&self) -> Vec<MonitorTaskView> {
        self.tasks()
    }

    fn poll_events(
        &self,
        monitor_id: &str,
        cursor: u64,
        max_events: usize,
    ) -> Option<MonitorPollView> {
        self.poll_view(monitor_id, cursor, max_events)
    }
}

/// 本界面独有的 Monitor 消费状态。
#[derive(Debug)]
pub struct MonitorStateAdapter {
    max_events: usize,
    cursors: BTreeMap<String, u64>,
    polling_suspended: bool,
}

impl Default for MonitorStateAdapter {
    fn default() -> Self {
        Self::new(MONITOR_MAX_EVENTS)
    }
}

impl MonitorStateAdapter {
    pub fn new(max_events: usize) -> Self {
        Self {
            max_events: max_events.max(1),
            cursors: BTreeMap::new(),
            polling_suspended: false,
        }
    }

    /// 已消费位置的快照（外部只读，避免改坏长期状态）。
    pub fn cursors(&self) -> &BTreeMap<String, u64> {
        &self.cursors
    }

    /// 当前是否因工作区切换暂停轮询。
    pub fn polling_suspended(&self) -> bool {
        self.polling_suspended
    }

    /// 工作区切换开始前暂停轮询并废弃旧工作区的游标。
    ///
    /// 必须在切换真正成功之前清空：新工作区可能复用任务 ID，沿用旧位置会跳过它的首批事件。
    pub fn suspend_for_workspace_switch(&mut self) {
        self.polling_suspended = true;
        self.cursors.clear();
    }

    /// 工作区切换结束后恢复后续轮询。
    pub fn resume_polling(&mut self) {
        self.polling_suspended = false;
    }

    /// 读取各任务的新事件，返回可渲染的批次。
    ///
    /// 成功轮询即使没有事件也会写入 `next_cursor`（遵守来源给出的消费位置，避免重复读取）；
    /// 单个任务轮询失败不会推进它的游标，也不阻断同轮其他任务——下一轮从原游标重试。
    pub fn refresh(&mut self, source: &dyn MonitorSource) -> Vec<MonitorDisplayBatch> {
        if self.polling_suspended {
            return Vec::new();
        }
        let mut batches = Vec::new();
        for task in source.list_tasks() {
            let monitor_id = task.monitor_id;
            if monitor_id.is_empty() {
                continue;
            }
            let cursor = self.cursors.get(&monitor_id).copied().unwrap_or(0);
            let Some(result) = source.poll_events(&monitor_id, cursor, self.max_events) else {
                continue;
            };
            self.cursors.insert(monitor_id.clone(), result.next_cursor);
            if result.events.is_empty() {
                continue;
            }
            batches.push(MonitorDisplayBatch {
                monitor_id,
                status: result.snapshot.status,
                events: result.events,
            });
        }
        batches
    }
}

/// Monitor 工具消息文案（与 Python `format_monitor_display_batch` 逐字一致）。
pub fn format_monitor_display_batch(batch: &MonitorDisplayBatch) -> String {
    let mut lines = vec![format!("Monitor · {} · {}", batch.monitor_id, batch.status)];
    for event in &batch.events {
        let text = if event.text.is_empty() {
            "(空行)"
        } else {
            event.text.as_str()
        };
        lines.push(format!("[{}] {}", event.stream, text));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn task(monitor_id: &str, status: &str) -> MonitorTaskView {
        MonitorTaskView {
            monitor_id: monitor_id.to_string(),
            command: "npm run dev".to_string(),
            shell: "bash".to_string(),
            status: status.to_string(),
            exit_code: None,
            started_at: 0.0,
            next_cursor: 0,
            dropped_events: 0,
        }
    }

    fn event(sequence: u64, stream: &str, text: &str) -> MonitorEventView {
        MonitorEventView {
            sequence,
            created_at: 0.0,
            stream: stream.to_string(),
            text: text.to_string(),
        }
    }

    fn poll(
        task: MonitorTaskView,
        events: Vec<MonitorEventView>,
        next_cursor: u64,
    ) -> MonitorPollView {
        MonitorPollView {
            snapshot: task,
            events,
            next_cursor,
            first_available_cursor: 0,
        }
    }

    /// 桩来源：任务列表与「按 (id, cursor) 取事件」的结果都由测试给定。
    ///
    /// 回复按队列消费：队列用尽后模拟「没有新事件」——回空事件、游标不前进，
    /// 这样就能断言适配器确实把推进后的游标用在了下一轮。
    struct StubSource {
        tasks: Vec<MonitorTaskView>,
        /// 每次 poll 收到的 cursor，用来断言适配器确实按游标推进。
        seen: std::cell::RefCell<Vec<(String, u64)>>,
        replies: std::cell::RefCell<BTreeMap<String, VecDeque<MonitorPollView>>>,
    }

    impl StubSource {
        fn new(tasks: Vec<MonitorTaskView>, replies: Vec<(&str, Vec<MonitorPollView>)>) -> Self {
            Self {
                tasks,
                seen: std::cell::RefCell::new(Vec::new()),
                replies: std::cell::RefCell::new(
                    replies
                        .into_iter()
                        .map(|(id, views)| (id.to_string(), views.into_iter().collect()))
                        .collect(),
                ),
            }
        }
    }

    impl MonitorSource for StubSource {
        fn list_tasks(&self) -> Vec<MonitorTaskView> {
            self.tasks.clone()
        }

        fn poll_events(
            &self,
            monitor_id: &str,
            cursor: u64,
            _max_events: usize,
        ) -> Option<MonitorPollView> {
            self.seen
                .borrow_mut()
                .push((monitor_id.to_string(), cursor));
            let mut replies = self.replies.borrow_mut();
            // 没有登记回复的任务等同于「任务已被回收」：返回 None 让适配器跳过。
            let queue = replies.get_mut(monitor_id)?;
            if let Some(view) = queue.pop_front() {
                return Some(view);
            }
            self.tasks
                .iter()
                .find(|task| task.monitor_id == monitor_id)
                .map(|task| poll(task.clone(), Vec::new(), cursor))
        }
    }

    #[test]
    fn refresh_advances_the_cursor_even_without_events() {
        // 成功轮询但不带事件：游标必须推进到来源给的位置，否则下一轮会重复读取。
        let source = StubSource::new(
            vec![task("m1", "running")],
            vec![("m1", vec![poll(task("m1", "running"), Vec::new(), 7)])],
        );
        let mut adapter = MonitorStateAdapter::default();
        assert!(adapter.refresh(&source).is_empty());
        assert_eq!(adapter.cursors().get("m1"), Some(&7));
        // 队列已空即「无新事件」：游标不前进，也不会倒退回 0。
        assert!(adapter.refresh(&source).is_empty());
        assert_eq!(adapter.cursors().get("m1"), Some(&7));
        assert_eq!(source.seen.borrow()[1], ("m1".to_string(), 7));
    }

    #[test]
    fn refresh_returns_batches_and_reuses_the_advanced_cursor() {
        let source = StubSource::new(
            vec![task("m1", "running")],
            vec![(
                "m1",
                vec![poll(
                    task("m1", "running"),
                    vec![event(1, "stdout", "ready in 300ms"), event(2, "stderr", "")],
                    2,
                )],
            )],
        );
        let mut adapter = MonitorStateAdapter::default();
        let batches = adapter.refresh(&source);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].monitor_id, "m1");
        assert_eq!(batches[0].status, "running");
        assert_eq!(batches[0].events.len(), 2);

        // 第二轮：桩的队列已空，等同于「没有新事件」。
        assert!(adapter.refresh(&source).is_empty());
        let seen = source.seen.borrow();
        assert_eq!(seen[0], ("m1".to_string(), 0), "首轮从 0 起读");
        assert_eq!(seen[1], ("m1".to_string(), 2), "次轮从推进后的游标起读");
    }

    #[test]
    fn unknown_task_is_skipped_without_touching_other_tasks() {
        // 桩只给 m2 的回复；m1 取不到（任务已被回收）应静默跳过，游标不写。
        let source = StubSource::new(
            vec![task("m1", "completed"), task("m2", "running")],
            vec![(
                "m2",
                vec![poll(
                    task("m2", "running"),
                    vec![event(3, "stdout", "listening")],
                    3,
                )],
            )],
        );
        let mut adapter = MonitorStateAdapter::default();
        let batches = adapter.refresh(&source);
        assert_eq!(batches.len(), 1, "只有 m2 有增量");
        assert_eq!(batches[0].monitor_id, "m2");
        assert!(adapter.cursors().get("m1").is_none(), "取不到就不写游标");
        assert_eq!(adapter.cursors().get("m2"), Some(&3));
    }

    #[test]
    fn suspended_polling_reads_nothing_and_clears_cursors() {
        let source = StubSource::new(
            vec![task("m1", "running")],
            vec![("m1", vec![poll(task("m1", "running"), Vec::new(), 5)])],
        );
        let mut adapter = MonitorStateAdapter::default();
        adapter.refresh(&source);
        assert_eq!(adapter.cursors().len(), 1);

        // 切换工作区：旧工作区的任务 ID 不再有语义，游标必须先清掉。
        adapter.suspend_for_workspace_switch();
        assert!(adapter.polling_suspended());
        assert!(adapter.cursors().is_empty());
        assert!(adapter.refresh(&source).is_empty(), "暂停期间不读");
        assert!(source.seen.borrow().len() == 1, "暂停期间不应发起轮询");

        adapter.resume_polling();
        assert!(!adapter.polling_suspended());
        adapter.refresh(&source);
        assert_eq!(source.seen.borrow().len(), 2, "恢复后重新轮询");
    }

    #[test]
    fn format_matches_python_wording() {
        let batch = MonitorDisplayBatch {
            monitor_id: "m1".to_string(),
            status: "completed".to_string(),
            events: vec![
                event(1, "stdout", "done"),
                event(2, "stdout", ""),
                event(3, "stderr", "boom"),
            ],
        };
        assert_eq!(
            format_monitor_display_batch(&batch),
            "Monitor · m1 · completed\n[stdout] done\n[stdout] (空行)\n[stderr] boom"
        );
    }
}
