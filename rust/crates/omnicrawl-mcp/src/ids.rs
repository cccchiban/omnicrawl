//! 会话/审计标识生成。
//!
//! Python 用 `uuid4().hex[:12]`；Rust 侧不引入随机数依赖，用「时间 + 进程 + 计数器」
//! 混合出的伪随机十六进制串顶替：形态一致（12 位小写十六进制），取值本身不可复现。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 返回 `chars` 位小写十六进制串（Python 侧为 `uuid4().hex[:chars]`）。
pub fn random_hex(chars: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut state = mix(nanos
        ^ (std::process::id() as u64).rotate_left(32)
        ^ mix(counter.wrapping_add(0x9E37_79B9)));
    let mut out = String::with_capacity(chars + 16);
    while out.len() < chars {
        state = mix(state);
        out.push_str(&format!("{state:016x}"));
    }
    out.truncate(chars);
    out
}

/// 审计 ID：`mcp-<12 位十六进制>`。
pub fn audit_id() -> String {
    format!("mcp-{}", random_hex(12))
}

/// 会话 ID：`session-<12 位十六进制>`。
pub fn session_id() -> String {
    format!("session-{}", random_hex(12))
}

/// splitmix64：把弱熵打散成看起来随机的字。
fn mix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_expected_shape() {
        let session = session_id();
        assert_eq!(session.len(), "session-".len() + 12);
        assert!(session.starts_with("session-"));
        assert!(session["session-".len()..]
            .chars()
            .all(|ch| ch.is_ascii_hexdigit()));

        let audit = audit_id();
        assert!(audit.starts_with("mcp-"));
        assert_eq!(audit.len(), "mcp-".len() + 12);
    }

    #[test]
    fn ids_do_not_repeat_in_a_row() {
        let first = random_hex(12);
        let second = random_hex(12);
        assert_ne!(first, second);
    }
}
