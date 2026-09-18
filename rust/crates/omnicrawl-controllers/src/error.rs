//! 控制器域的错误面。
//!
//! Python 侧统一抛 `AgentError`，文案直接进 UI 与模型上下文，因此 Rust 侧同样按
//! 字符串保真搬运：错误只承载文案，不做分类。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    message: String,
}

impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentError {}

impl From<crate::undo::SnapshotError> for AgentError {
    fn from(value: crate::undo::SnapshotError) -> Self {
        Self::new(value.to_string())
    }
}
