//! TLS 会话票据缓存：让同一站点的第二次连接走恢复，而不是完整握手。
//!
//! 为什么需要：真实浏览器第二次访问同一站点会带票据（1.2）或 PSK（1.3），握手只需
//! 一个 RTT；不做恢复则每次都完整握手，服务端看到的是「同一个 IP 每次都是全新客户端」。
//! 这不只是像不像的问题——每页多一个 RTT，且行为上明显不同于真实用户。
//!
//! 缓存粒度是 `host:port`，因为票据只对签发它的服务端有效。过期（`ticket_lifetime`）
//! 与容量都按需清理：抓取工具的会话通常很短，但缓存不能无界增长。
//!
//! 失败必须能回退：票据被服务端拒绝（配置变更、密钥轮换、限流）是常态，调用方拿到
//! `None` 就走完整握手。这里不做重试——重试会让一次抓取变成两次连接。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 一条可恢复的会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// 服务端下发的票据本体。
    pub ticket: Vec<u8>,
    /// 由 `resumption_master_secret` 与票据 nonce 派生的 PSK。
    pub psk: Vec<u8>,
    /// 票据的 `ticket_age_add`：客户端上报票据年龄时要加上的混淆量。
    pub ticket_age_add: u32,
    /// 签发时刻（Unix 秒），用于算票据年龄与判断过期。
    pub issued_at: i64,
    /// 有效期（秒）。为 0 表示不可恢复。
    pub lifetime: u32,
    /// 协商出的密码套件（恢复时 PSK 必须与套件匹配）。
    pub suite: u16,
    /// ALPN 协商结果：换协议的会话不能恢复。
    pub alpn: Option<String>,
}

impl Session {
    /// 票据是否已过期；`lifetime` 为 0 的票据一律视为不可用。
    pub fn is_expired(&self, now: i64) -> bool {
        if self.lifetime == 0 {
            return true;
        }
        now.saturating_sub(self.issued_at) >= i64::from(self.lifetime)
    }

    /// 上报给服务端的票据年龄（`obfuscated_ticket_age`）。
    ///
    /// RFC 8446 §4.2.11.1：客户端上报「毫秒」年龄再与 `ticket_age_add` 相加取模 2^32。
    /// 单位是毫秒，不是秒——用错单位会被服务端判为票据无效。
    pub fn obfuscated_age(&self, now: i64) -> u32 {
        let elapsed_ms = now
            .saturating_sub(self.issued_at)
            .max(0)
            .saturating_mul(1000);
        (elapsed_ms as u32).wrapping_add(self.ticket_age_add)
    }
}

/// 按 `host:port` 缓存的会话表。
#[derive(Debug, Default)]
pub struct SessionCache {
    entries: Mutex<HashMap<String, Session>>,
}

impl SessionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 共享句柄：一次抓取里的多个连接共用一份缓存。
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// 取一条仍有效的会话；没有或已过期时返回 `None`。
    ///
    /// 过期的条目顺手删掉：抓取工具通常不会主动清理，攒着只是占内存。
    pub fn get(&self, key: &str) -> Option<Session> {
        let now = now_seconds();
        let mut entries = self.lock();
        match entries.get(key) {
            Some(session) if !session.is_expired(now) => Some(session.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// 存入一条会话；`lifetime` 为 0 时直接忽略（服务端明确说这条票据不可恢复）。
    pub fn put(&self, key: impl Into<String>, session: Session) {
        if session.lifetime == 0 {
            return;
        }
        self.lock().insert(key.into(), session);
    }

    /// 丢弃一条会话（票据被拒绝时调用，避免下次再拿它试）。
    pub fn remove(&self, key: &str) {
        self.lock().remove(key);
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// 清空（测试用）。
    #[cfg(test)]
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// 缓存键：票据只对签发它的 `host:port` 有效。
pub fn cache_key(host: &str, port: u16) -> String {
    format!("{}:{}", host.to_ascii_lowercase(), port)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(lifetime: u32, issued_at: i64) -> Session {
        Session {
            ticket: vec![1, 2, 3],
            psk: vec![4; 32],
            ticket_age_add: 0,
            issued_at,
            lifetime,
            suite: 0x1301,
            alpn: Some("h2".to_string()),
        }
    }

    #[test]
    fn a_stored_session_comes_back() {
        let cache = SessionCache::new();
        let key = cache_key("Example.COM", 443);
        assert_eq!(key, "example.com:443", "键必须归一化主机名");
        cache.put(key.clone(), session(7200, now_seconds()));
        assert_eq!(cache.get(&key).map(|item| item.ticket), Some(vec![1, 2, 3]));
    }

    #[test]
    fn different_ports_do_not_share_sessions() {
        let cache = SessionCache::new();
        cache.put(cache_key("example.com", 443), session(7200, now_seconds()));
        assert!(cache.get(&cache_key("example.com", 8443)).is_none());
    }

    /// 过期条目必须拿不到，且顺手被删掉（抓取工具不会主动清理）。
    #[test]
    fn an_expired_session_is_dropped() {
        let cache = SessionCache::new();
        let key = cache_key("example.com", 443);
        let now = now_seconds();
        cache.put(key.clone(), session(10, now - 11));
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.len(), 0, "过期条目应当被删除");
    }

    /// `lifetime = 0` 是服务端明确说「别恢复」，连存都不该存。
    #[test]
    fn a_zero_lifetime_session_is_not_stored() {
        let cache = SessionCache::new();
        cache.put(cache_key("example.com", 443), session(0, now_seconds()));
        assert!(cache.is_empty());
    }

    /// 票据年龄必须用**毫秒**：用秒会让服务端算出完全不同的年龄而拒绝票据。
    #[test]
    fn the_obfuscated_age_is_in_milliseconds() {
        let issued = 1_000_000;
        let item = Session {
            ticket_age_add: 100,
            ..session(7200, issued)
        };
        // 过了 3 秒 = 3000 毫秒，加混淆量。
        assert_eq!(item.obfuscated_age(issued + 3), 3100);
        // 时钟回退（now < issued）不该下溢。
        assert_eq!(item.obfuscated_age(issued - 5), 100);
    }

    /// 年龄是 u32 语义：溢出必须回绕而不是 panic。
    #[test]
    fn the_obfuscated_age_wraps_around() {
        let item = Session {
            ticket_age_add: u32::MAX,
            ..session(7200, 0)
        };
        assert_eq!(item.obfuscated_age(0), u32::MAX);
        // 再加 1 秒（1000 毫秒）应当回绕。
        assert_eq!(item.obfuscated_age(1), 999);
    }

    #[test]
    fn remove_drops_the_session() {
        let cache = SessionCache::new();
        let key = cache_key("example.com", 443);
        cache.put(key.clone(), session(7200, now_seconds()));
        cache.remove(&key);
        assert!(cache.get(&key).is_none());
    }
}
