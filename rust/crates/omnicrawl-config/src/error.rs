//! 配置域的错误面。
//!
//! Python 侧 `RuntimeConfigError` 只承载文案（读取失败、后缀不符、类型不对都直接进 UI），
//! 因此 Rust 侧同样按字符串保真搬运，不做分类。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}
