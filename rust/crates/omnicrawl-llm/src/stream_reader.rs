//! 可放弃的模型流读取（`omnicrawl/llm/stream_reader.py` 的移植）。
//!
//! Python 侧把建连与迭代放进后台线程、用队列把事件转发给回合线程：消费方每
//! `poll_seconds` 重新检查一次取消状态，取消后立即返回；被放弃的读取线程在收到
//! 下一个事件、或底层连接被取消路径关闭时自行退出。内核侧的传输本来就是阻塞式
//! 线程模型，这里把同一机制抽成独立模块，供四路 Provider 运行时与其它长流消费方复用。
//!
//! 与 Python 的形状差异：Python 的消费端是生成器（`Iterator`，异常沿 yield 传播）；
//! 内核的消费端是 [`InterruptionReader::next`]（返回 [`InterruptibleOutcome`] 枚举，
//! 错误与结束都是值），由调用方决定如何映射到自己的错误面。
//!
//! 两个入口对应 Python 的两种用法：
//! - [`interruptible_stream_events`]：事件迭代器已备好（建连已完成的场景）；
//! - [`interruptible_stream_events_with_opener`]：`open_stream(abandoned)` 回调形态，
//!   建连也放进后台线程，与 Python 的 `open_stream` 签名同构。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// 取消检查间隔（秒）：与审批等待的轮询节奏一致，决定取消后的最坏感知延迟。
pub const DEFAULT_POLL_SECONDS: f64 = 0.05;

/// [`DEFAULT_POLL_SECONDS`] 的 `Duration` 形态。
pub const DEFAULT_POLL: Duration = Duration::from_millis(50);

const KIND_ITEM: u8 = 0;
const KIND_END: u8 = 1;
const KIND_ERROR: u8 = 2;

/// 已放弃信号：读取线程在建连后与每次产出前检查它，被放弃时立刻停止产出并
/// 关闭刚建立的流，避免取消后留下未关闭的响应。对应 Python 的 `threading.Event`。
pub type AbandonedFlag = Arc<AtomicBool>;

/// 消费方每次轮询的返回值。
///
/// `Idle` 对应 Python 里 `queue.Empty` 后调 `cancel_check()` 的时机；
/// `Error` 对应 Python 里重新抛出的异常；`End` 对应生成器正常耗尽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptibleOutcome<T, E> {
    /// 一条事件。
    Item(T),
    /// 轮询窗口内没有事件到达，调用方应趁机检查取消状态。
    Idle,
    /// 上游流正常耗尽。
    End,
    /// 上游失败，携带错误值。
    Error(E),
}

/// 打开器：在后台线程里建连并返回事件迭代器（与 Python 的
/// `open_stream(abandoned) -> Iterator` 同构）。
pub trait StreamOpener {
    type Item: Send + 'static;
    type Error: Send + 'static;
    type Events: Iterator<Item = Self::Item> + Send + 'static;

    fn open_stream(&mut self, abandoned: AbandonedFlag) -> (Self::Events, Option<Self::Error>);
}

/// 一次性打开器：包住「已备好的迭代器 + 可选打开期错误」。
struct OnceOpener<I, E> {
    slot: Option<(I, Option<E>)>,
}

impl<I, E> StreamOpener for OnceOpener<I, E>
where
    I: Iterator + Send + 'static,
    I::Item: Send + 'static,
    E: Send + 'static,
{
    type Item = I::Item;
    type Error = E;
    type Events = I;

    fn open_stream(&mut self, _abandoned: AbandonedFlag) -> (Self::Events, Option<Self::Error>) {
        // 打开器只应被消费一次；二次打开是调用方错误（panic 在后台线程里会把
        // 发送端一起带走，消费端表现为 Disconnected → End，不会挂住回合）。
        match self.slot.take() {
            Some((events, error)) => (events, error),
            None => panic!("流打开器被重复消费"),
        }
    }
}

/// 在后台线程驱动事件迭代器，把事件转发给当前线程。
///
/// 对应 Python 的 `interruptible_stream_events(open_stream, cancel_check=…,
/// poll_seconds=…)`：这里的 `events` 是已备好的迭代器（建连已完成的场景），
/// `pre_built_error` 携带建连期错误（有的话先于任何事件抛出）。返回的
/// [`InterruptionReader`] 每次 `next` 按 `poll` 窗口取事件；Reader 被 drop 或
/// 手动 [`InterruptionReader::abandon`] 时置位放弃信号，后台线程在收到下一个
/// 事件或迭代结束时自行退出。
pub fn interruptible_stream_events<I, E>(
    events: I,
    pre_built_error: Option<E>,
) -> InterruptionReader<I::Item, E>
where
    I: Iterator + Send + 'static,
    I::Item: Send + 'static,
    E: Send + 'static,
{
    let opener = OnceOpener {
        slot: Some((events, pre_built_error)),
    };
    spawn_reader(opener)
}

/// 打开器形态的入口：建连也放进后台线程，与 Python 的 `open_stream` 回调同构。
///
/// `open_stream` 在后台线程里被调用并拿到放弃信号；返回「事件迭代器 + 可选的
/// 打开期错误」。
pub fn interruptible_stream_events_with_opener<O>(
    opener: O,
) -> InterruptionReader<O::Item, O::Error>
where
    O: StreamOpener + Send + 'static,
{
    spawn_reader(opener)
}

fn spawn_reader<O>(mut opener: O) -> InterruptionReader<O::Item, O::Error>
where
    O: StreamOpener + Send + 'static,
{
    let (sender, receiver) = mpsc::channel::<(u8, Payload<O::Item, O::Error>)>();
    let abandoned: AbandonedFlag = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&abandoned);

    thread::Builder::new()
        .name("omnicrawl-stream-reader".to_string())
        .spawn(move || {
            let (mut events, error) = opener.open_stream(Arc::clone(&flag));
            if let Some(error) = error {
                let _ = sender.send((KIND_ERROR, Payload::Error(error)));
                return;
            }
            for item in events.by_ref() {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                if sender.send((KIND_ITEM, Payload::Item(item))).is_err() {
                    return;
                }
            }
            let _ = sender.send((KIND_END, Payload::End));
        })
        .expect("启动流读取线程失败");

    InterruptionReader {
        receiver,
        abandoned,
        ended: false,
    }
}

enum Payload<T, E> {
    Item(T),
    End,
    Error(E),
}

/// 可放弃的流读取端：见 [`interruptible_stream_events`]。
pub struct InterruptionReader<T, E> {
    receiver: mpsc::Receiver<(u8, Payload<T, E>)>,
    abandoned: AbandonedFlag,
    ended: bool,
}

impl<T, E> InterruptionReader<T, E> {
    /// 取下一条事件；`poll` 是轮询窗口，窗口内没有事件给 [`InterruptibleOutcome::Idle`]。
    pub fn next(&mut self, poll: Duration) -> InterruptibleOutcome<T, E> {
        if self.ended {
            return InterruptibleOutcome::End;
        }
        match self.receiver.recv_timeout(poll) {
            Ok((KIND_ITEM, Payload::Item(item))) => InterruptibleOutcome::Item(item),
            Ok((KIND_END, _)) => {
                self.ended = true;
                InterruptibleOutcome::End
            }
            Ok((KIND_ERROR, Payload::Error(error))) => {
                self.ended = true;
                InterruptibleOutcome::Error(error)
            }
            Ok((_, Payload::End)) => {
                self.ended = true;
                InterruptibleOutcome::End
            }
            Ok((_, _)) => {
                self.ended = true;
                InterruptibleOutcome::End
            }
            Err(RecvTimeoutError::Timeout) => InterruptibleOutcome::Idle,
            Err(RecvTimeoutError::Disconnected) => {
                self.ended = true;
                InterruptibleOutcome::End
            }
        }
    }

    /// 置位放弃信号；后续迭代由后台线程自行收口。
    pub fn abandon(&self) {
        self.abandoned.store(true, Ordering::Relaxed);
    }

    /// 放弃信号本体（供底层连接关闭路径共享，对应 Python 把 `abandoned` 传进
    /// `open_stream` 的那个 `threading.Event`）。
    pub fn abandoned_flag(&self) -> AbandonedFlag {
        Arc::clone(&self.abandoned)
    }

    /// 是否已置位放弃。
    pub fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Relaxed)
    }
}

impl<T, E> Drop for InterruptionReader<T, E> {
    fn drop(&mut self) {
        self.abandoned.store(true, Ordering::Relaxed);
    }
}

/// Python 侧的 `poll_seconds` 秒数形态转 `Duration`（向下取整到毫秒，最少 1ms）。
#[must_use]
pub fn poll_duration(poll_seconds: f64) -> Duration {
    let millis = (poll_seconds * 1000.0).floor().max(1.0) as u64;
    Duration::from_millis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect<T, E>(reader: &mut InterruptionReader<T, E>) -> (Vec<T>, Option<E>) {
        let mut items = Vec::new();
        let mut error = None;
        loop {
            match reader.next(DEFAULT_POLL) {
                InterruptibleOutcome::Item(item) => items.push(item),
                InterruptibleOutcome::Idle => continue,
                InterruptibleOutcome::End => break,
                InterruptibleOutcome::Error(err) => {
                    error = Some(err);
                    break;
                }
            }
        }
        (items, error)
    }

    #[test]
    fn forwards_items_in_order_then_ends() {
        let mut reader = interruptible_stream_events(vec![1, 2, 3].into_iter(), None::<String>);
        let (items, error) = collect(&mut reader);
        assert_eq!(items, vec![1, 2, 3]);
        assert!(error.is_none());
        // 已耗尽的 reader 再次 next 仍给 End（ended 状态稳定）。
        assert!(matches!(
            reader.next(DEFAULT_POLL),
            InterruptibleOutcome::End
        ));
    }

    #[test]
    fn empty_stream_ends_immediately() {
        let mut reader = interruptible_stream_events(Vec::<u8>::new().into_iter(), None::<String>);
        let (items, error) = collect(&mut reader);
        assert!(items.is_empty());
        assert!(error.is_none());
    }

    #[test]
    fn pre_built_error_is_reported() {
        let mut reader =
            interruptible_stream_events(Vec::<u8>::new().into_iter(), Some("建连失败".to_string()));
        let (items, error) = collect(&mut reader);
        assert!(items.is_empty());
        assert_eq!(error.as_deref(), Some("建连失败"));
    }

    #[test]
    fn iteration_result_items_pass_through() {
        // 用 Result 项模拟「部分成功、部分失败」：转发是透明的，错误语义由调用方解释。
        let mut reader = interruptible_stream_events(
            vec![Ok(1u8), Err(2u8), Ok(3u8)].into_iter(),
            None::<String>,
        );
        let (items, error) = collect(&mut reader);
        assert!(error.is_none());
        assert_eq!(items.len(), 3);
        assert_eq!(items[1], Err(2));
        assert_eq!(items[2], Ok(3));
    }

    #[test]
    fn drop_sets_abandoned_flag() {
        let reader = interruptible_stream_events(0..100usize, None::<String>);
        let flag = reader.abandoned_flag();
        assert!(!reader.is_abandoned());
        drop(reader);
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn abandon_is_visible_before_drop() {
        let reader = interruptible_stream_events(0..100usize, None::<String>);
        reader.abandon();
        assert!(reader.is_abandoned());
    }

    #[test]
    fn poll_duration_converts_and_clamps() {
        assert_eq!(poll_duration(0.05), DEFAULT_POLL);
        assert_eq!(poll_duration(0.0), Duration::from_millis(1));
        assert_eq!(poll_duration(0.1), Duration::from_millis(100));
        assert_eq!(poll_duration(-1.0), Duration::from_millis(1));
    }

    #[test]
    fn opener_form_builds_stream_in_background() {
        struct CounterOpener {
            started: Arc<AtomicBool>,
        }
        impl StreamOpener for CounterOpener {
            type Item = u32;
            type Error = String;
            type Events = std::vec::IntoIter<u32>;

            fn open_stream(
                &mut self,
                _abandoned: AbandonedFlag,
            ) -> (Self::Events, Option<Self::Error>) {
                self.started.store(true, Ordering::Relaxed);
                (vec![10, 20].into_iter(), None)
            }
        }
        let started = Arc::new(AtomicBool::new(false));
        let mut reader = interruptible_stream_events_with_opener(CounterOpener {
            started: Arc::clone(&started),
        });
        // 打开器在后台线程跑，轮询等待 started 置位而不是假设时序。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !started.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (items, error) = collect(&mut reader);
        assert!(started.load(Ordering::Relaxed));
        assert_eq!(items, vec![10, 20]);
        assert!(error.is_none());
    }

    #[test]
    fn opener_error_before_events() {
        struct FailingOpener;
        impl StreamOpener for FailingOpener {
            type Item = u32;
            type Error = String;
            type Events = std::vec::IntoIter<u32>;

            fn open_stream(
                &mut self,
                _abandoned: AbandonedFlag,
            ) -> (Self::Events, Option<Self::Error>) {
                (Vec::new().into_iter(), Some("连接被拒".to_string()))
            }
        }
        let mut reader = interruptible_stream_events_with_opener(FailingOpener);
        let (items, error) = collect(&mut reader);
        assert!(items.is_empty());
        assert_eq!(error.as_deref(), Some("连接被拒"));
    }

    #[test]
    fn default_poll_matches_python_constant() {
        assert_eq!(DEFAULT_POLL_SECONDS, 0.05);
        assert_eq!(DEFAULT_POLL, poll_duration(DEFAULT_POLL_SECONDS));
    }
}
