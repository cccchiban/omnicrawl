//! 协议版本与协商。

use std::fmt;

/// 内核实现的协议版本。
pub const PROTOCOL_VERSION: &str = "1.0";

/// 内核支持的主版本号。
pub const SUPPORTED_MAJOR: u64 = 1;

/// 版本协商失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionError {
    /// 宿主主版本与内核不支持的任何主版本都不匹配。
    UnsupportedVersion { host_version: String },
}

impl fmt::Display for VersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { host_version } => write!(
                formatter,
                "宿主协议版本 {host_version} 与内核支持的主版本 {SUPPORTED_MAJOR} 不匹配。"
            ),
        }
    }
}

impl std::error::Error for VersionError {}

/// 协商握手：宿主在 `initialize` 里声明版本，内核返回自己使用的版本。
///
/// 只比较主版本；次版本约定为「只增不改语义」，宿主可用任意次版本与同主版本内核通信。
pub fn negotiate_version(host_version: &str) -> Result<&'static str, VersionError> {
    match major_of(host_version) {
        Some(major) if major == SUPPORTED_MAJOR => Ok(PROTOCOL_VERSION),
        _ => Err(VersionError::UnsupportedVersion {
            host_version: host_version.to_string(),
        }),
    }
}

fn major_of(version: &str) -> Option<u64> {
    version.trim().split('.').next()?.trim().parse().ok()
}
