//! HTTP 应用装配：路由、统一响应信封与错误响应。
//!
//! 结构与决策接口、抓取接口一致：成功走 `{"data": ...}`，失败走
//! `{"error": {code, message}}`。与本地 API 的差别是**不做鉴权**——任何能访问监听地址的
//! 本机程序都能拉起 loop，因此配置只接受回环地址，工具白名单默认收窄（见配置域）。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::pool::TriggerPool;
use crate::routes;

/// 服务共享状态。
#[derive(Clone)]
pub struct AppState {
    pub pool: TriggerPool,
}

impl AppState {
    pub fn new(pool: TriggerPool) -> Self {
        Self { pool }
    }
}

/// 接口层错误：`code` + 可直接展示的 `message` + HTTP 状态（`details` 只给结构化补充）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub status: StatusCode,
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, status: StatusCode) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
            details: None,
        }
    }

    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, StatusCode::BAD_REQUEST)
    }

    pub fn not_found(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, StatusCode::NOT_FOUND)
    }

    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            "SERVICE_UNAVAILABLE",
            message,
            StatusCode::SERVICE_UNAVAILABLE,
        )
    }

    /// SSE 游标早于保留窗口：客户端应当重新拉一次状态而不是重连。
    pub fn cursor_expired(earliest: u64) -> Self {
        Self::new(
            "EVENT_CURSOR_EXPIRED",
            "请求的事件游标已超出保留窗口，请重新获取触发状态。",
            StatusCode::CONFLICT,
        )
        .with_details(json!({"earliest_event_id": earliest}))
    }

    fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "error": {"code": self.code, "message": self.message}
        });
        if let Some(details) = self.details {
            if let Some(error) = body
                .get_mut("error")
                .and_then(serde_json::Value::as_object_mut)
            {
                error.insert("details".to_string(), details);
            }
        }
        (self.status, Json(body)).into_response()
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
        .route("/v1/status", get(routes::status))
        .route("/v1/triggers", post(routes::create).get(routes::list))
        .route("/v1/triggers/{trigger_id}", get(routes::detail))
        .route("/v1/triggers/{trigger_id}/events", get(routes::events))
        .route(
            "/v1/triggers/{trigger_id}/transcript",
            get(routes::transcript),
        )
        .route("/v1/triggers/{trigger_id}/cancel", post(routes::cancel))
        .fallback(not_found)
        .with_state(state)
}

/// `build_app` 与 `build_router` 同义；保留别名与其它本地服务的入口名一致。
pub fn build_app(state: AppState) -> Router {
    build_router(state)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "omnicrawl-trigger"}))
}

async fn not_found() -> Response {
    ApiError::not_found("NOT_FOUND", "Not Found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_keeps_code_and_message() {
        let error = ApiError::bad_request("INVALID_REQUEST", "message 不能为空。");
        assert_eq!(error.code, "INVALID_REQUEST");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.message, "message 不能为空。");
    }

    #[test]
    fn queue_full_is_429_and_cursor_expiry_is_409() {
        let full = ApiError::new("QUEUE_FULL", "满了", StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(full.status, StatusCode::TOO_MANY_REQUESTS);
        let expired = ApiError::cursor_expired(7);
        assert_eq!(expired.status, StatusCode::CONFLICT);
        // 最早可用事件号放在结构化 details 里，不拼进给人看的 message。
        assert_eq!(
            expired
                .details
                .as_ref()
                .and_then(|details| details.get("earliest_event_id")),
            Some(&json!(7))
        );
        assert!(!expired.message.contains("earliest_event_id"));
    }
}
