//! 按资源分组的资源路由集合（对应 `omnicrawl/api/routes/`）。
//!
//! 已搬模块：`system` / `runs` / `support` / `projects` / `sessions` / `settings` / `monitors` /
//! `configuration` / `subagents`——`api/routes/*` 全部落地；`shared_store.py` 的四张跨进程表
//! 也已在 `shared_store.rs` 对齐，`/docs` 与 `/openapi.json` 见 `openapi.rs`。

use axum::http::StatusCode;

use crate::error::ApiError;

pub mod configuration;
pub mod monitors;
pub mod projects;
pub mod query;
pub mod runs;
pub mod sessions;
pub mod settings;
pub mod subagents;
pub mod support;
pub mod system;

/// 配置构造/校验异常 → 400，文案与 Python `_as_invalid_setting` 同形。
pub(super) fn invalid_setting(error: omnicrawl_config::ConfigError) -> ApiError {
    ApiError::new(
        "INVALID_SETTING",
        format!("配置值无效：{}", error.message()),
        StatusCode::BAD_REQUEST,
        None,
    )
}
