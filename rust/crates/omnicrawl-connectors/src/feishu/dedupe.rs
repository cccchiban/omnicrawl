//! 飞书事件去重：进程内短期去重与跨重启指纹。
//!
//! 语义基准是 Python `omnicrawl/connectors/fsapp.py` 的 `_claim_message_once` /
//! `_inbox_dedupe_key`。持久入站队列（`feishu_inbox.py`）尚未搬：这里只做进程内去重，
//! 跨重启的 pending 重放与 24 小时窗口去重仍由 Python 侧承担。

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// 进程内去重窗口：WebSocket 重连后的事件重投递都在这个范围内。
pub const DEDUP_TTL_SECONDS: f64 = 10.0 * 60.0;

/// 去重表上限，超出后按时间淘汰最旧的一半以上。
pub const DEDUP_MAX_ENTRIES: usize = 2000;

/// 进程内已见消息表（调用方负责加锁）。
pub struct SeenMessages {
    seen: BTreeMap<String, f64>,
}

impl Default for SeenMessages {
    fn default() -> Self {
        Self::new()
    }
}

impl SeenMessages {
    pub fn new() -> SeenMessages {
        SeenMessages {
            seen: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// 首次见到返回 true；重复、空 id 与过期语义与 Python 一致（空 id 视为首次）。
    pub fn claim(&mut self, message_id: &str, now: f64) -> bool {
        if message_id.is_empty() {
            return true;
        }
        let expired: Vec<String> = self
            .seen
            .iter()
            .filter(|(_key, timestamp)| now - **timestamp > DEDUP_TTL_SECONDS)
            .map(|(key, _timestamp)| key.clone())
            .collect();
        for key in expired {
            self.seen.remove(&key);
        }
        if self.seen.len() >= DEDUP_MAX_ENTRIES {
            let mut entries: Vec<(String, f64)> = self
                .seen
                .iter()
                .map(|(key, timestamp)| (key.clone(), *timestamp))
                .collect();
            entries.sort_by(|left, right| left.1.total_cmp(&right.1));
            let drop_count = 1.max(entries.len() - DEDUP_MAX_ENTRIES + 1);
            for (key, _timestamp) in entries.into_iter().take(drop_count) {
                self.seen.remove(&key);
            }
        }
        if self.seen.contains_key(message_id) {
            return false;
        }
        self.seen.insert(message_id.to_string(), now);
        true
    }
}

/// 跨重启去重键：文本用 sender + chat + create_time + 内容哈希，媒体回退到 message_id。
pub fn inbox_dedupe_key(
    message_type: &str,
    message_id: &str,
    create_time: &str,
    chat_id: &str,
    open_id: &str,
    user_text: &str,
) -> String {
    if message_type != "text" {
        return if message_id.is_empty() {
            format!("type:{message_type}")
        } else {
            message_id.to_string()
        };
    }
    if create_time.is_empty() || chat_id.is_empty() || open_id.is_empty() || user_text.is_empty() {
        return if message_id.is_empty() {
            "text:unknown".to_string()
        } else {
            message_id.to_string()
        };
    }
    let digest = Sha256::digest(user_text.as_bytes());
    let hex = format!("{digest:x}");
    format!("text:{open_id}:{chat_id}:{create_time}:{}", &hex[..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_message_is_claimed_once() {
        let mut seen = SeenMessages::new();
        assert!(seen.claim("m1", 1_000.0));
        assert!(!seen.claim("m1", 1_000.0));
        assert!(seen.claim("", 1_000.0));
        assert!(seen.claim("", 1_000.0));
    }

    #[test]
    fn entries_expire_after_window() {
        let mut seen = SeenMessages::new();
        assert!(seen.claim("m1", 0.0));
        assert!(!seen.claim("m1", DEDUP_TTL_SECONDS - 1.0));
        assert!(seen.claim("m1", DEDUP_TTL_SECONDS + 1.0));
    }

    #[test]
    fn table_is_capped() {
        let mut seen = SeenMessages::new();
        for index in 0..(DEDUP_MAX_ENTRIES + 5) {
            assert!(seen.claim(&format!("m{index}"), 1_000.0));
        }
        assert!(seen.len() <= DEDUP_MAX_ENTRIES);
        // 最新的一条仍在表里，最旧的已被淘汰。
        let newest = format!("m{}", DEDUP_MAX_ENTRIES + 4);
        assert!(!seen.claim(&newest, 1_000.0));
        assert!(seen.claim("m0", 1_000.0));
    }

    #[test]
    fn text_key_uses_content_digest() {
        let key = inbox_dedupe_key("text", "m1", "1700000000", "oc_1", "ou_1", "你好");
        assert!(key.starts_with("text:ou_1:oc_1:1700000000:"), "{key}");
        assert_eq!(key.len(), "text:ou_1:oc_1:1700000000:".len() + 32);
        assert_eq!(
            key,
            inbox_dedupe_key("text", "m2", "1700000000", "oc_1", "ou_1", "你好")
        );
        assert_ne!(
            key,
            inbox_dedupe_key("text", "m1", "1700000000", "oc_1", "ou_1", "再见")
        );
    }

    #[test]
    fn media_and_incomplete_keys_fall_back_to_message_id() {
        assert_eq!(inbox_dedupe_key("image", "m9", "", "", "", ""), "m9");
        assert_eq!(inbox_dedupe_key("image", "", "", "", "", ""), "type:image");
        assert_eq!(
            inbox_dedupe_key("text", "m9", "", "oc_1", "ou_1", "hi"),
            "m9"
        );
        assert_eq!(inbox_dedupe_key("text", "", "", "", "", ""), "text:unknown");
    }
}
