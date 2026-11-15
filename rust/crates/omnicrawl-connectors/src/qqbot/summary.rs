//! 群聊消息的定时总结：每 12 小时把各群最近的消息压成一条记忆。
//!
//! 与主 Agent 循环完全隔离：总结跑在连接器自己的线程上，用的是
//! [`AgentDriver::run_isolated_turn`]——宿主为它另起会话，既不占主会话，也不改写用户的
//! 对话历史，更不参与「同一时刻只有一个任务」的名额竞争。
//!
//! 写记忆走宿主的独立回合（Agent 自己调 `memory_write`），而不是连接器直接落盘：记忆的
//! 作用域解析、去重与索引维护都在宿主侧，连接器绕过去会写出宿主读不到的记录。
//!
//! 每个群一条记忆，互不混：prompt 里明确要求只总结这一个群，并且带上群 openid 作为
//! 「这条记忆属于哪个群」的唯一依据。

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::history::{GroupHistory, GroupMessage};

/// 默认总结间隔：12 小时。
pub const DEFAULT_SUMMARY_INTERVAL_SECONDS: u64 = 12 * 3600;

/// 总结时喂给模型的最大消息条数：窗口内条数上限已经是 500，再多就压成一段采样。
pub const MAX_SUMMARY_MESSAGES: usize = 200;

/// 定时总结器：一个后台线程，按间隔扫各群记录并落记忆。
pub struct GroupSummarizer {
    interval: Duration,
    /// 上一次成功总结的时间点（Unix 秒），按群记录。
    last_run: std::sync::Mutex<std::collections::BTreeMap<String, i64>>,
}

impl GroupSummarizer {
    /// 按间隔构造；`interval` 下限 1 分钟，避免配置写错把 shell 打满。
    pub fn new(interval: Duration) -> GroupSummarizer {
        GroupSummarizer {
            interval: interval.max(Duration::from_secs(60)),
            last_run: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// 默认 12 小时。
    pub fn default_interval() -> GroupSummarizer {
        GroupSummarizer::new(Duration::from_secs(DEFAULT_SUMMARY_INTERVAL_SECONDS))
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// 某群是否已到总结时间：从未总结过的群立刻到期。
    pub fn is_due(&self, group: &str, now: i64) -> bool {
        let last = self
            .last_run
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(group)
            .copied();
        match last {
            None => true,
            Some(at) => now - at >= self.interval.as_secs() as i64,
        }
    }

    /// 记下某群这次总结的时间点。
    pub fn mark_done(&self, group: &str, now: i64) {
        self.last_run
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(group.to_string(), now);
    }
}

/// 总结 prompt：把「只总结这个群」「用 memory_write 落项目级记忆」说死。
///
/// 模型看到的是**已经渲染好的消息行**，不重新解释原始事件：连接器已经把 @ 提及剥掉、
/// 把多媒体压成标记，模型只需要读这些行。
pub fn summary_prompt(group: &str, records: &[GroupMessage]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for record in records {
        lines.push(record.line());
    }
    format!(
        "下面是一个 QQ 群最近 72 小时的部分聊天记录，群 openid 是 {group}。\n\
         请把它压缩成一条可长期复用的记忆，用 memory_write 写入，scope 用 project，\
         related_directories 填 \"qqbot/group/{group}\"。\n\
         要求：\n\
         - 只总结这一个群，不要提及其他群；\n\
         - 记事实与结论（在聊什么、定了什么、谁在负责什么），不要逐句复述；\n\
         - 记录里的 [图片]/[视频]/[文件] 只是「当时有人发过这类东西」的标记，你没有看到内容，\
         不要猜测内容；\n\
         - 内容没有长期价值时，写一条「本群近期没有需要长期记住的事情」即可，不要编造；\n\
         - 不要调用 memory_write 以外的写操作。\n\n\
         聊天记录：\n{}",
        lines.join("\n")
    )
}

/// 把窗口里的记录压到 [`MAX_SUMMARY_MESSAGES`] 条：均匀抽样，两头都保留。
pub fn sample_records(records: &[GroupMessage]) -> Vec<GroupMessage> {
    if records.len() <= MAX_SUMMARY_MESSAGES {
        return records.to_vec();
    }
    let last = records.len() - 1;
    let slots = MAX_SUMMARY_MESSAGES - 1;
    let mut picked: Vec<GroupMessage> = Vec::with_capacity(MAX_SUMMARY_MESSAGES);
    for index in 0..slots {
        // 把 `0..=slots` 平摊到 `0..=last`：两端都锚定，最新一条一定在。
        let at = index * last / slots;
        picked.push(records[at].clone());
    }
    picked.push(records[last].clone());
    picked
}

/// 一条记录是否值得送进总结：正文非空白，或者带多媒体标记。
///
/// 纯空白消息（只有空格/换行）没有任何可总结的信息；只发图片的消息仍算内容。
pub fn worth_summarizing(record: &GroupMessage) -> bool {
    !record.content.trim().is_empty() || !record.media.is_empty()
}

/// 一次总结要跑的内容：群 openid 与它的记录。
#[derive(Debug, Clone)]
pub struct SummaryJob {
    pub group: String,
    pub records: Vec<GroupMessage>,
}

/// 从记录里挑出到期的群。
pub fn due_jobs(
    history: &Arc<GroupHistory>,
    summarizer: &GroupSummarizer,
    now: i64,
) -> Vec<SummaryJob> {
    let mut jobs: Vec<SummaryJob> = Vec::new();
    for group in history.groups_tracked() {
        if !summarizer.is_due(&group, now) {
            continue;
        }
        // 窗口裁剪交给 recent：过期的读出来也会被丢掉。
        let records: Vec<GroupMessage> = history
            .recent(&group, usize::MAX, now)
            .into_iter()
            .filter(worth_summarizing)
            .collect();
        if records.is_empty() {
            continue;
        }
        jobs.push(SummaryJob {
            group,
            records: sample_records(&records),
        });
    }
    jobs
}

/// 起一个后台线程按间隔跑总结。
///
/// `run` 是实际执行体（宿主侧注入，内部调 `run_isolated_turn`）：把它与线程分开，测试
/// 才能只验证调度逻辑而不碰网络。每轮把到期的群逐个交给 `run`；某群失败不会连累其他群，
/// 也不会停掉循环。
pub fn spawn_scheduler<F>(
    history: Arc<GroupHistory>,
    summarizer: Arc<GroupSummarizer>,
    mut run: F,
) -> thread::JoinHandle<()>
where
    F: FnMut(&SummaryJob) -> Result<(), String> + Send + 'static,
{
    let interval = summarizer.interval();
    thread::Builder::new()
        .name("omnicrawl-qqbot-summary".to_string())
        .spawn(move || loop {
            thread::sleep(interval);
            let now = super::files::unix_seconds();
            for job in due_jobs(&history, &summarizer, now) {
                // 失败的群不标记完成：下一轮还会再试。
                match run(&job) {
                    Ok(()) => summarizer.mark_done(&job.group, now),
                    Err(error) => eprintln!(
                        "[qqbot] 群 {} 的消息总结失败：{error}",
                        job.group
                    ),
                }
            }
        })
        .expect("总结线程启动失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qqbot::history::MediaNote;

    fn record(at: i64, sender: &str, content: &str) -> GroupMessage {
        GroupMessage {
            at,
            sender: sender.to_string(),
            sender_name: String::new(),
            content: content.to_string(),
            media: Vec::new(),
            photos: Vec::new(),
        }
    }

    #[test]
    fn a_group_is_due_until_it_is_summarised() {
        let summarizer = GroupSummarizer::new(Duration::from_secs(12 * 3600));
        assert!(summarizer.is_due("G-1", 1000));
        summarizer.mark_done("G-1", 1000);
        assert!(!summarizer.is_due("G-1", 1000 + 11 * 3600));
        assert!(summarizer.is_due("G-1", 1000 + 12 * 3600));
        // 另一个群各自计时，不受影响。
        assert!(summarizer.is_due("G-2", 1000));
    }

    #[test]
    fn the_interval_has_a_floor() {
        let summarizer = GroupSummarizer::new(Duration::from_secs(1));
        assert_eq!(summarizer.interval(), Duration::from_secs(60));
    }

    #[test]
    fn the_prompt_names_only_this_group_and_forbids_guessing_media() {
        let records = vec![record(1000, "A", "周五上线")];
        let prompt = summary_prompt("GROUP-XYZ", &records);
        assert!(prompt.contains("GROUP-XYZ"));
        assert!(prompt.contains("scope 用 project"));
        assert!(prompt.contains("memory_write"));
        assert!(prompt.contains("不要猜测内容"));
        assert!(prompt.contains("周五上线"));
    }

    #[test]
    fn long_histories_are_sampled_down() {
        let records: Vec<GroupMessage> = (0..500)
            .map(|index| record(1000 + index, "A", &format!("第{index}条")))
            .collect();
        let sampled = sample_records(&records);
        assert_eq!(sampled.len(), MAX_SUMMARY_MESSAGES);
        // 首尾都要保留：抽样不能把最新的消息丢掉。
        assert_eq!(sampled.first().unwrap().content, "第0条");
        assert_eq!(sampled.last().unwrap().content, "第499条");
    }

    #[test]
    fn short_histories_pass_through_unchanged() {
        let records = vec![record(1000, "A", "一"), record(1001, "B", "二")];
        assert_eq!(sample_records(&records).len(), 2);
    }

    #[test]
    fn media_only_records_still_count() {
        let mut media_only = record(1000, "A", "");
        media_only.media = vec![MediaNote::Image];
        assert!(worth_summarizing(&media_only));
        assert!(!worth_summarizing(&record(1001, "B", "   ")));
    }
}
