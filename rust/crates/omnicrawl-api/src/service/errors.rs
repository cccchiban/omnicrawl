//! API 层的错误构造器。
//!
//! 全部只做「文案 + 状态码」装配，不含判定逻辑；改动理由一致（改接口错误语义时只动这里），
//! 因此从 `service.rs` 里独立出来。

use axum::http::StatusCode;

use crate::error::ApiError;
use omnicrawl_session::SessionStoreError;

/// 会话系统未启用。
pub(super) fn session_disabled() -> ApiError {
    ApiError::new(
        "SESSION_DISABLED",
        "会话系统未启用。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}
/// 后台任务管理器缺位。
pub(super) fn monitor_unavailable() -> ApiError {
    ApiError::new(
        "MONITOR_UNAVAILABLE",
        "后台监控不可用。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}
/// 未知后台任务。
pub(super) fn monitor_not_found() -> ApiError {
    ApiError::new(
        "MONITOR_NOT_FOUND",
        "后台任务不存在。",
        StatusCode::NOT_FOUND,
        None,
    )
}
/// 内核正忙（回合在途，运行期设置要等回合结束才能被内核处理）。
pub(super) fn kernel_busy(message: &str) -> ApiError {
    ApiError::new("RUN_ACTIVE", message, StatusCode::CONFLICT, None)
}
/// 内核重起失败（切换会话 / 工作区 / 压缩）。
pub(super) fn kernel_restart_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(
        "KERNEL_RESTART_FAILED",
        message.into(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}
/// 当前服务不持有内核启动信息（嵌入模式不支持运行期切换会话）。
pub(super) fn kernel_restart_unavailable() -> ApiError {
    ApiError::new(
        "KERNEL_RESTART_UNAVAILABLE",
        "当前服务未持有内核启动信息，不能在运行期切换会话或工作区。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}
/// 内核运行期命令失败（压缩等）。
pub(super) fn kernel_command_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(
        "KERNEL_COMMAND_FAILED",
        message.into(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}
/// 提示词装配失败（模式切换要求重新装配 system prompt）。
pub(super) fn prompt_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(
        "PROMPT_DISABLED",
        format!("提示词装配失败：{}", message.into()),
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}
/// SubAgent 后台任务不可用（功能未启用）。
pub(super) fn subagent_unavailable() -> ApiError {
    ApiError::new(
        "SUBAGENT_UNAVAILABLE",
        "SubAgent 后台任务不可用。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}
/// 模型列表探测失败（Python `MODEL_LIST_FAILED`）。
pub(super) fn model_list_failed(error: omnicrawl_config::ConfigError) -> ApiError {
    ApiError::new(
        "MODEL_LIST_FAILED",
        error.message(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}
/// 双列目录构建失败（Python `MODEL_CATALOG_FAILED`）。
pub(super) fn model_catalog_failed(error: omnicrawl_config::ConfigError) -> ApiError {
    ApiError::new(
        "MODEL_CATALOG_FAILED",
        error.message(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}
/// 会话与项目存储错误 → API 错误。
///
/// Python 侧这些异常会穿过 `APIServiceError` 处理器落到通用 500；这里按「资源缺失 → 404、
/// 其余 → 400」收紧，错误文案保持原样（已知差异见 crate README）。
pub(super) fn session_store_error(error: SessionStoreError) -> ApiError {
    let message = error.message().to_string();
    let missing = message.starts_with("未找到会话")
        || message.starts_with("project 不存在")
        || message.starts_with("artifact 不存在")
        || message.starts_with("会话不存在");
    if missing {
        ApiError::new("NOT_FOUND", message, StatusCode::NOT_FOUND, None)
    } else {
        ApiError::new("INVALID_REQUEST", message, StatusCode::BAD_REQUEST, None)
    }
}
