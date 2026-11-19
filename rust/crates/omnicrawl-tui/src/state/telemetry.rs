//! 遥测与速率估计：按 token 增量与耗时滑动窗口估算速度，并在待机时冻结。
//!
//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

/// 输出速度估计：累计估算 token 数与真正的连续输出时长，间隔超过 [`IDLE_GAP`] 不计时。
#[derive(Debug, Clone, Default)]
pub struct RateEstimator {
    pub(super) tokens: f64,
    pub(super) active: Duration,
    pub(super) last: Option<Instant>,
    pub(super) value: Option<f64>,
}

impl RateEstimator {
    pub fn record(&mut self, text: &str, now: Instant) {
        self.tokens += estimated_tokens(text) as f64;
        if let Some(last) = self.last {
            let gap = now.saturating_duration_since(last);
            if gap <= IDLE_GAP {
                self.active += gap;
            }
        }
        self.last = Some(now);
        if self.active > Duration::from_millis(200) {
            self.value = Some(self.tokens / self.active.as_secs_f64());
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    /// 模型流中断回滚：撤销本回合累计的量，重新计时。
    pub fn rollback(&mut self) {
        self.tokens = 0.0;
        self.active = Duration::ZERO;
        self.last = None;
        self.value = None;
    }
}

#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub context_window: Option<u64>,
    pub rate: RateEstimator,
}

/// Token 估算：CJK 字符按 1 token，其他字符按 4 字符 1 token。
pub fn estimated_tokens(text: &str) -> u64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}
