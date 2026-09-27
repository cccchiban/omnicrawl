//! 进程级定时器精度：Windows 上把系统计时器粒度抬到 1ms。
//!
//! 事件循环用 `event::poll(timeout)` 等待终端事件，Windows 侧最终落到
//! `WaitForMultipleObjects`——它的超时精度受系统计时器粒度限制，默认约 15.6ms。
//! 也就是说「睡 20ms」实际可能睡 31ms，这段白等直接叠加在「回车 → 首字」与
//! 「首字到达 → 上屏」上。
//!
//! `timeBeginPeriod(1)` 把粒度降到 1ms，是播放器/终端类进程的常规做法（crossterm、
//! ratatui 生态里的应用同样这么做）。代价是系统在低功耗场景下更频繁唤醒，
//! 因此进程退出时用 `timeEndPeriod` 恢复：`TimerResolutionGuard` 负责这件事。
//!
//! 非 Windows 平台不需要这个调用（`poll` 直接走 `poll(2)`/`select(2)`，精度由内核给）。

/// 提升后的定时器粒度（毫秒）。
const TIMER_PERIOD_MS: u32 = 1;

#[cfg(windows)]
mod platform {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }

    pub fn begin(period: u32) -> bool {
        // SAFETY: 两个入口只接受一个整数参数，不涉及内存所有权。
        unsafe { timeBeginPeriod(period) == 0 }
    }

    pub fn end(period: u32) {
        // SAFETY: 同上；即使在未成功提升时调用也是安全的。
        unsafe {
            timeEndPeriod(period);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn begin(_period: u32) -> bool {
        false
    }

    pub fn end(_period: u32) {}
}

/// 定时器精度守卫：创建时尝试提升，丢弃时恢复。
///
/// 提升失败（极少数受限环境）不报错：事件循环照常工作，只是等待精度回到系统默认。
pub struct TimerResolutionGuard {
    period: u32,
    raised: bool,
}

impl TimerResolutionGuard {
    /// 尝试把本进程的定时器精度提升到 [`TIMER_PERIOD_MS`]。
    pub fn acquire() -> Self {
        let raised = platform::begin(TIMER_PERIOD_MS);
        Self {
            period: TIMER_PERIOD_MS,
            raised,
        }
    }

    /// 是否真的提升成功（诊断用）。
    pub fn is_raised(&self) -> bool {
        self.raised
    }
}

impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        if self.raised {
            platform::end(self.period);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_is_safe_to_acquire_and_drop() {
        // 成功与否都取决于环境，这里只要求「能获取、能释放」不 panic。
        let guard = TimerResolutionGuard::acquire();
        let _ = guard.is_raised();
    }
}
