//! 会话存储错误：消息与 Python `SessionStoreError` 逐字对齐。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStoreError {
    message: String,
}

impl SessionStoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 错误文案。UI 与日志直接展示它，因此不允许在这里做二次包装。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionStoreError {}
