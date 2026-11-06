//! 网络审批站：把代理线程的"要不要放行"接到界面上。
//!
//! 为什么需要一个独立的站，而不是复用工具审批面板（`host::Waiting::Approval`）：
//!
//! 1. **发问的是代理线程，不是模型**。工具审批由批次推进产生、随批次结束消失；
//!    网络审批由沙盒内某条命令的一次连接产生，与工具批次没有对应关系。
//! 2. **代理线程必须阻塞**。客户端正卡在 `CONNECT` 上等我们的 `200`/`403`，
//!    没法"稍后再说"；而界面绝不能因此阻塞，所以这里是线程间等待（`Condvar`），
//!    不是同一条调用栈上的回调。
//! 3. **失败方向必须是关闭**。超时、界面不在、用户没看见，一律拒绝（设计文档 §7.3）。
//!
//! 没有界面接管时（本地 API、无人值守触发、CLI）[`ApprovalStation::ask`] **立即拒绝**，
//! 而不是空等一个完整超时：否则每条命令都会被一个没人会回答的问题拖住
//! `net_approval_timeout_seconds` 秒，表现为"沙盒一开就卡死"。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::proxy::{ApprovalRequest, Approver};

/// 一条等待界面回答的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// 回答时要带回来的编号。
    pub id: u64,
    pub host: String,
    pub port: u16,
    pub scheme: &'static str,
    /// 过期时刻：界面据此显示剩余时间，也据此丢弃已失效的条目。
    pub deadline: Instant,
}

impl PendingApproval {
    /// 面板上的展示形式。
    pub fn target(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }

    /// 剩余秒数（向上取整，最少 0）。
    pub fn remaining_seconds(&self, now: Instant) -> u64 {
        self.deadline
            .checked_duration_since(now)
            .map(|left| left.as_secs() + u64::from(left.subsec_nanos() > 0))
            .unwrap_or(0)
    }
}

/// 请求被拒绝的原因，用于写进拒绝记录与诊断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 用户点了拒绝。
    Denied,
    /// 等了 `net_approval_timeout_seconds` 秒没人回答。
    TimedOut,
    /// 没有界面接管（API / 无人值守），按拒绝处理。
    NoInterface,
}

impl Refusal {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Denied => "用户拒绝",
            Self::TimedOut => "审批超时",
            Self::NoInterface => "没有审批界面，按拒绝处理",
        }
    }
}

#[derive(Default)]
struct State {
    /// 当前等待回答的请求（界面按此渲染）。
    queue: Vec<PendingApproval>,
    /// 已回答的请求：编号 → 是否放行。
    answers: HashMap<u64, bool>,
    next_id: u64,
}

/// 网络审批站。由 `Sandbox` 持有，界面按需接管。
#[derive(Default)]
pub struct ApprovalStation {
    state: Mutex<State>,
    /// 请求被回答或队列变化时唤醒阻塞中的代理线程。
    signal: Condvar,
    /// 是否有界面接管。默认 `false`：无人值守场景下 ask 立即拒绝。
    interactive: AtomicBool,
    /// 累计拒绝数（诊断用）。
    refusals: AtomicU64,
}

impl std::fmt::Debug for ApprovalStation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApprovalStation")
            .field("interactive", &self.interactive.load(Ordering::Relaxed))
            .field("refusals", &self.refusals.load(Ordering::Relaxed))
            .finish()
    }
}

impl ApprovalStation {
    pub fn new() -> Self {
        Self::default()
    }

    /// 交给代理的审批回调。
    ///
    /// 超时随配置走（`[sandbox.net] approval_timeout_seconds` 每次判定时重新读取，
    /// 因此界面里改设置无需重启代理）。
    pub fn approver(self: &Arc<Self>, timeout: Duration) -> Approver {
        let station = Arc::clone(self);
        Arc::new(move |request: &ApprovalRequest| {
            station.ask_with_timeout(request, Some(timeout))
        })
    }

    /// 由界面在启动时打开：打开之前 `ask` 一律立即拒绝。
    pub fn set_interactive(&self, interactive: bool) {
        self.interactive.store(interactive, Ordering::Relaxed);
        if !interactive {
            // 关掉时把等待中的请求一并唤醒，让代理线程按拒绝退出，不留在原地等超时。
            let mut state = self.lock();
            state.queue.clear();
            self.signal.notify_all();
        }
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive.load(Ordering::Relaxed)
    }

    /// 当前等待回答的请求（界面每帧取一次）。
    pub fn pending(&self) -> Vec<PendingApproval> {
        self.lock().queue.clone()
    }

    /// 是否有请求在等人：界面的轮询节拍据此切到活动档。
    pub fn has_pending(&self) -> bool {
        !self.lock().queue.is_empty()
    }

    /// 累计拒绝数。
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }

    /// 回答一条请求；编号不存在（已超时或被撤销）时无操作。
    pub fn answer(&self, id: u64, approved: bool) {
        let mut state = self.lock();
        if state.queue.iter().any(|pending| pending.id == id) {
            state.answers.insert(id, approved);
            self.signal.notify_all();
        }
    }

    /// 撤销所有等待中的请求（会话结束、沙盒关闭时调用）。
    pub fn cancel_all(&self) {
        let mut state = self.lock();
        state.queue.clear();
        self.signal.notify_all();
    }

    /// 问用户要不要放行；返回 `true` 才放行。
    ///
    /// 本函数**在代理线程里阻塞**，最多 `timeout`。任何不确定的情况都拒绝。
    pub fn ask(&self, request: &ApprovalRequest) -> bool {
        self.ask_with_timeout(request, None)
    }

    /// 同 [`Self::ask`]，显式给出超时（来自 `[sandbox.net] approval_timeout_seconds`）。
    ///
    /// `None` 表示用默认值：配置没给时不该让一条连接无限等下去。
    pub fn ask_with_timeout(
        &self,
        request: &ApprovalRequest,
        timeout: Option<Duration>,
    ) -> bool {
        let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);
        if !self.is_interactive() {
            // 没有界面就别假装在等：立即拒绝，别把命令拖住一整个超时。
            self.refusals.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let deadline = Instant::now() + timeout;
        let id = {
            let mut state = self.lock();
            let id = state.next_id;
            state.next_id = state.next_id.wrapping_add(1);
            state.queue.push(PendingApproval {
                id,
                host: request.host.clone(),
                port: request.port,
                scheme: request.scheme,
                deadline,
            });
            self.signal.notify_all();
            id
        };

        let approved = {
            let mut state = self.lock();
            loop {
                if let Some(answer) = state.answers.remove(&id) {
                    break answer;
                }
                // 被 `cancel_all` / `set_interactive(false)` 撤销：队列里已没有我们。
                if !state.queue.iter().any(|pending| pending.id == id) {
                    break false;
                }
                let now = Instant::now();
                if now >= deadline {
                    break false;
                }
                let (guard, _) = self
                    .signal
                    .wait_timeout(state, deadline.saturating_duration_since(now))
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state = guard;
            }
        };

        // 无论结果如何都要把条目摘掉：界面不该继续显示一个已有答案的问题。
        let mut state = self.lock();
        state.queue.retain(|pending| pending.id != id);
        state.answers.remove(&id);
        drop(state);
        self.signal.notify_all();

        if !approved {
            self.refusals.fetch_add(1, Ordering::Relaxed);
        }
        approved
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 配置没给超时时的兜底：与 `[sandbox.net] approval_timeout_seconds` 的默认值一致。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            host: "example.com".to_string(),
            port: 443,
            scheme: "https",
        }
    }

    #[test]
    fn without_an_interface_ask_is_refused_at_once() {
        let station = Arc::new(ApprovalStation::new());
        assert!(!station.is_interactive());
        let started = Instant::now();
        assert!(!station.ask(&request()), "无人接管时必须拒绝");
        // 不能空等超时：这里要求"立即"（远小于默认 60 秒）。
        assert!(started.elapsed() < Duration::from_secs(1), "不该等超时");
        assert_eq!(station.refusals(), 1);
    }

    #[test]
    fn an_answer_moves_the_blocked_caller() {
        let station = Arc::new(ApprovalStation::new());
        station.set_interactive(true);
        let asker = Arc::clone(&station);
        let handle = std::thread::spawn(move || asker.ask(&request()));

        // 等请求进队列（界面就是这么看到的）。
        let deadline = Instant::now() + Duration::from_secs(5);
        let pending = loop {
            let pending = station.pending();
            if !pending.is_empty() {
                break pending;
            }
            assert!(Instant::now() < deadline, "请求应进入队列");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(pending[0].host, "example.com");
        assert_eq!(pending[0].target(), "https://example.com:443");

        station.answer(pending[0].id, true);
        assert!(handle.join().expect("线程应正常结束"), "批准后应放行");
        assert!(station.pending().is_empty(), "回答后队列应清空");
    }

    #[test]
    fn timeout_is_a_refusal_not_a_hang() {
        let station = Arc::new(ApprovalStation::new());
        station.set_interactive(true);
        let started = Instant::now();
        // 没人回答：到点必须自己拒绝，而不是永远等下去。
        assert!(!station.ask_with_timeout(&request(), Some(Duration::from_millis(80))));
        assert!(started.elapsed() >= Duration::from_millis(70));
        assert!(station.pending().is_empty(), "超时后条目要摘掉");
    }

    #[test]
    fn turning_the_interface_off_releases_waiters() {
        let station = Arc::new(ApprovalStation::new());
        station.set_interactive(true);
        let asker = Arc::clone(&station);
        let handle = std::thread::spawn(move || asker.ask(&request()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while station.pending().is_empty() {
            assert!(Instant::now() < deadline, "请求应进入队列");
            std::thread::sleep(Duration::from_millis(5));
        }
        station.set_interactive(false);
        assert!(!handle.join().expect("线程应正常结束"), "撤销后按拒绝处理");
    }

    #[test]
    fn answering_an_unknown_id_does_nothing() {
        let station = Arc::new(ApprovalStation::new());
        station.set_interactive(true);
        station.answer(12345, true);
        assert!(station.pending().is_empty());
    }
}
