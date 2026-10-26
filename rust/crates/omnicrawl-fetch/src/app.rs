//! HTTP 应用装配：路由、统一响应信封与框架级错误响应。
//!
//! 结构对齐决策接口（`omnicrawl-decision`）：成功走 `{"data": ...}`，失败走
//! `{"error": {code, message}}`。与本地 API 的差别在于**不做鉴权**——任何能访问监听
//! 地址的调用方都能直接调用，因此配置只接受回环地址。
//!
//! 抓取参数**不做类型校验**：交给 `fetch_batch` 用的那套宽松归一化（数字字符串、
//! `"true"`/`"1"` 之类的布尔写法都认）。批量脚本写起来顺手比严格更重要，
//! 而抓取本体在 `fetch_batch` 里已按既有口径收口。

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use omnicrawl_config::core::runtime::ConfigEnvironment;

use crate::config::FetchSettings;
use crate::routes;

/// 服务共享状态：运行期配置（含抓取执行体）。
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<FetchSettings>,
}

impl AppState {
    pub fn new(settings: FetchSettings, _environment: &ConfigEnvironment) -> Self {
        Self {
            settings: Arc::new(settings),
        }
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

    /// 请求里没有可抓的地址（唯一的请求级拒绝）。
    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, StatusCode::BAD_REQUEST)
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
        .route("/v1/fetch", post(routes::fetch))
        .route("/v1/status", get(routes::status))
        .fallback(not_found)
        .with_state(state)
}

/// `build_app` 与 `build_router` 同义；保留别名与其它本地服务的入口名一致。
pub fn build_app(state: AppState) -> Router {
    build_router(state)
}

/// 健康检查。
async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "omnicrawl-fetch"}))
}

async fn not_found() -> Response {
    ApiError::new("NOT_FOUND", "Not Found", StatusCode::NOT_FOUND).into_response()
}
