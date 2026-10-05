//! HTTP 应用装配：路由、统一响应信封与框架级错误响应。
//!
//! 结构对齐本地 API（`omnicrawl-api`）：成功走 `{"data": ...}`，失败走
//! `{"error": {code, message}}`。
//!
//! 与本地 API 的差别有两处：
//! * 这里**不做鉴权**——任何能访问监听地址的调用方都能直接调用。因此配置只接受回环地址，
//!   「只有本机程序能调用」是唯一的准入控制（见 `DecisionApiConfig::normalize`）。
//! * 服务是否监听只取决于 `[api] enabled` 与是否有可用渠道。

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use omnicrawl_config::core::runtime::ConfigEnvironment;

use crate::config::DecisionSettings;
use crate::routes::{self, DecisionContext};

/// 服务共享状态：运行期配置 + 已组装的决策上下文。
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<DecisionSettings>,
    /// 决策上下文；服务就绪时必然存在（没有可用渠道就不该监听）。
    context: Option<Arc<DecisionContext>>,
}

impl AppState {
    /// 按配置与进程环境装配；脱敏旁路走 `[desensitization]`。
    pub fn new(settings: DecisionSettings, environment: &ConfigEnvironment) -> Self {
        let context = settings
            .channel
            .as_ref()
            .map(|channel| Arc::new(routes::runtime_from_config(channel, environment)));
        Self {
            settings: Arc::new(settings),
            context,
        }
    }

    /// 决策上下文；服务未就绪时按不可用报错。
    pub fn context(&self) -> Result<&Arc<DecisionContext>, ApiError> {
        self.context
            .as_ref()
            .ok_or_else(ApiError::service_unavailable)
    }
}

/// 接口层错误：`code` + 可直接展示的 `message` + HTTP 状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub status: StatusCode,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, status: StatusCode) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
        }
    }

    /// 请求体语义不合法。
    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, StatusCode::BAD_REQUEST)
    }

    /// 服务未就绪（没有可用决策渠道）。
    pub fn service_unavailable() -> Self {
        Self::new(
            "SERVICE_UNAVAILABLE",
            "决策服务尚未就绪：请检查 decision_models.toml 的 [api] 与渠道配置。",
            StatusCode::SERVICE_UNAVAILABLE,
        )
    }

    /// 上游决策服务不可用（连接、超时、HTTP 错误码）。
    pub fn upstream(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, StatusCode::BAD_GATEWAY)
    }

    /// 上游响应不可解析。
    pub fn unparsable(message: impl Into<String>) -> Self {
        Self::new("DECISION_UNPARSABLE", message, StatusCode::BAD_GATEWAY)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {"code": self.code, "message": self.message}
        });
        (self.status, Json(body)).into_response()
    }
}

/// 成功响应信封。
pub fn data(value: Value) -> Json<Value> {
    Json(json!({ "data": value }))
}

/// 完整应用：所有端点直接可调用（不做鉴权）。
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/decide", post(routes::decide))
        .route("/v1/choice", post(routes::choice))
        .route("/v1/rank", post(routes::rank))
        .route("/v1/review", post(routes::review))
        .route("/v1/status", get(routes::status))
        .fallback(not_found)
        .with_state(state)
}

/// `build_app` 与 `build_router` 同义；保留别名与本地 API 的入口名一致。
pub fn build_app(state: AppState) -> Router {
    build_router(state)
}

/// 健康检查。
async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "omnicrawl-decision"}))
}

async fn not_found() -> Response {
    ApiError::new("NOT_FOUND", "Not Found", StatusCode::NOT_FOUND).into_response()
}
