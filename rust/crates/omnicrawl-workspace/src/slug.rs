//! Slug 安全验证（对应 `omnicrawl/workspace/slug.py`）：校验名称能否安全用作单一路径段。
//!
//! 任何把外部/不可信名称拼进文件系统路径的地方（目录名、元数据文件名、git 分支名等）
//! 都应先过 [`validate_slug`] 或 [`is_safe_slug`]。
//!
//! 规则（fail-closed）：非空；只允许 `[A-Za-z0-9_-]`（不含 `.`，因此不可能出现 `..`，
//! 也不含路径分隔符、空白与控制字符）；长度 1..max_length。

use omnicrawl_config::toml::Value as TomlValue;
use omnicrawl_config::value::python_repr;

/// 默认长度上限。
pub const DEFAULT_MAX_LENGTH: usize = 64;

/// 名称不是安全 slug（消息里带具体原因，便于上层包装成业务错误）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlugSafetyError {
    message: String,
}

impl SlugSafetyError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for SlugSafetyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SlugSafetyError {}

/// 判断名称是否为安全 slug（可用于拼接单一路径段）。
///
/// 与 Python 一致：空白检查用 `strip()` 的**真值**，但长度与字符集判定用**原始值**，
/// 因此带首尾空白的名称不会因为 strip 后合法而通过。
pub fn is_safe_slug(name: &str, max_length: usize) -> bool {
    if name.trim().is_empty() {
        return false;
    }
    if name.chars().count() > max_length {
        return false;
    }
    is_slug_shape(name)
}

/// 校验并返回规范化后的 slug；不安全时抛 [`SlugSafetyError`]。
///
/// 只做去除首尾空白与校验，不做替换/截断：调用方传入不可信名称时应校验失败而不是
/// 静默改写，避免「净化后仍被拼错」的歧义。
pub fn validate_slug(
    name: &str,
    field: &str,
    max_length: usize,
) -> Result<String, SlugSafetyError> {
    let raw = name.trim().to_string();
    if raw.is_empty() {
        return Err(SlugSafetyError::new(format!("{field}不能为空。")));
    }
    let length = raw.chars().count();
    if length > max_length {
        return Err(SlugSafetyError::new(format!(
            "{field}长度超过 {max_length} 字符（当前 {length} 字符）。"
        )));
    }
    if !is_slug_shape(&raw) {
        return Err(SlugSafetyError::new(format!(
            "{field}只能包含字母、数字、下划线与连字符，且不能包含点、路径\
             分隔符、空格等字符。实际值：{}",
            python_repr(&TomlValue::String(raw))
        )));
    }
    Ok(raw)
}

/// `^[A-Za-z0-9_-]+$`：每个字符都必须是 ASCII 字母 / 数字 / 下划线 / 连字符。
fn is_slug_shape(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}
