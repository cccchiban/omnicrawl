//! 多平台连接器共用的运行态部件。
//!
//! 三个连接器（QQ / 飞书 / Telegram）的回合驱动语义必须一致，各自维护一份实现会让行为
//! 在无人察觉的情况下漂移——本项目已经出现过「一端的 `/stop` 能中断审批、另两端不能」。
//! 凡是**语义必须永远相同**的部分都放在这里，平台差异只留在平台目录内。
//!
//! 判定标准：同一段逻辑在三个平台里只是字段名/字段类型不同（`chat_id` 对 `receive_id`），
//! 那它属于这里；若某个平台的语义真的更严格（例如飞书额外校验发送者），该约束应当作为
//! 参数由平台传入，而不是复制一份实现。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// 等待期间的轮询间隔：既够细（停止指令能及时生效）又不至于空转烧 CPU。
const STOP_POLL: Duration = Duration::from_millis(50);

/// `/stop` 与「是否展示思考」两个全局开关。
pub struct BotSwitches {
    stopped: AtomicBool,
    show_thinking: AtomicBool,
}

impl BotSwitches {
    pub fn new(show_thinking: bool) -> BotSwitches {
        BotSwitches {
            stopped: AtomicBool::new(false),
            show_thinking: AtomicBool::new(show_thinking),
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// 置停止标志。**不**解除已阻塞的确认/提问等待：解除动作依赖各平台的槽位字段，
    /// 由平台在自己的 `request_stop` 里显式调用 [`PendingSlot::release`]。
    pub fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking.load(Ordering::SeqCst)
    }

    pub fn set_show_thinking(&self, value: bool) {
        self.show_thinking.store(value, Ordering::SeqCst);
    }

    /// 带停止检查的睡眠：返回 `true` 表示期间收到了停止请求。
    ///
    /// 退避重连、限速等待都用它，因此 `/stop` 不会等到退避结束才生效。
    pub fn sleep_until_stopped(&self, seconds: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        while Instant::now() < deadline {
            if self.is_stopped() {
                return true;
            }
            std::thread::sleep(STOP_POLL);
        }
        self.is_stopped()
    }
}

/// `confirm` 与 `ask_user` 的共享等待槽。
///
/// 平台把本槽位登记进自己的运行态，用户回复时调用 [`PendingSlot::resolve`] 唤醒等待方；
/// 超时返回 `None`，由平台决定「按拒绝」还是「按未作答」继续。
pub struct PendingSlot<T> {
    value: Mutex<SlotState<T>>,
    signal: Condvar,
}

struct SlotState<T> {
    value: Option<T>,
    released: bool,
}

impl<T: Clone> Default for PendingSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> PendingSlot<T> {
    pub fn new() -> PendingSlot<T> {
        PendingSlot {
            value: Mutex::new(SlotState {
                value: None,
                released: false,
            }),
            signal: Condvar::new(),
        }
    }

    pub fn wait(&self, timeout: Duration) -> Option<T> {
        let guard = self.value.lock().expect("等待槽状态锁中毒");
        let (guard, _) = self
            .signal
            .wait_timeout_while(guard, timeout, |state| {
                state.value.is_none() && !state.released
            })
            .expect("等待槽状态锁中毒");
        guard.value.clone()
    }

    /// 只接受第一次答复：重复回复不会覆盖已作出的决定。
    ///
    /// 传入 `None` 表示「放弃这次提问」——同样解除等待，但不产生答复。
    pub fn resolve(&self, value: Option<T>) {
        let mut guard = self.value.lock().expect("等待槽状态锁中毒");
        if guard.value.is_none() && !guard.released {
            match value {
                Some(value) => guard.value = Some(value),
                None => guard.released = true,
            }
        }
        self.signal.notify_all();
    }

    /// 解除等待但**不记录答复**（停止时用）：等待方按未答复处理，与超时同路。
    pub fn release(&self) {
        let mut guard = self.value.lock().expect("等待槽状态锁中毒");
        guard.released = true;
        self.signal.notify_all();
    }

    /// 是否已经拿到答复（用于区分「超时」与「已答复」两种收尾路径）。
    pub fn is_resolved(&self) -> bool {
        self.value
            .lock()
            .expect("等待槽状态锁中毒")
            .value
            .is_some()
    }

    /// 是否已被停止指令解除（未作答）。
    pub fn is_released(&self) -> bool {
        self.value
            .lock()
            .expect("等待槽状态锁中毒")
            .released
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn slot_resolves_once_and_keeps_first_answer() {
        let slot: PendingSlot<bool> = PendingSlot::new();
        slot.resolve(Some(true));
        slot.resolve(Some(false));
        assert_eq!(slot.wait(Duration::from_millis(10)), Some(true));
        assert!(slot.is_resolved());
    }

    #[test]
    fn released_slot_returns_none_without_becoming_answered() {
        let slot: PendingSlot<String> = PendingSlot::new();
        slot.release();
        assert_eq!(slot.wait(Duration::from_millis(10)), None);
        // 停止 ≠ 作答：飞书据此区分「超时提示」与「已收到回答」，不能混淆。
        assert!(!slot.is_resolved());
        assert!(slot.is_released());
    }

    #[test]
    fn release_wakes_a_blocked_waiter() {
        let slot: Arc<PendingSlot<bool>> = Arc::new(PendingSlot::new());
        let waiter = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || slot.wait(Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(50));
        slot.release();
        assert_eq!(waiter.join().expect("等待线程不应 panic"), None);
    }

    #[test]
    fn stopped_switches_report_stop() {
        let switches = BotSwitches::new(false);
        assert!(!switches.is_stopped());
        assert!(!switches.sleep_until_stopped(0.2));
        switches.mark_stopped();
        assert!(switches.is_stopped());
        // 停止后睡眠立即返回 true，而不是等满时长。
        assert!(switches.sleep_until_stopped(5.0));
    }

    #[test]
    fn thinking_switch_round_trips() {
        let switches = BotSwitches::new(false);
        assert!(!switches.show_thinking());
        switches.set_show_thinking(true);
        assert!(switches.show_thinking());
    }
}
