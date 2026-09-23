//! 统一响应结构：成功走 `{"data": ...}`，失败走 `{"error": {code, message, details}}`。
//!
//! 对应 `omnicrawl/api/deps.py` 的 `data` / `error_response` 与 `models.py::APIServiceError`。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// 可稳定映射为 HTTP 错误结构的服务异常（对应 `APIServiceError`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub status: StatusCode,
    /// 装箱是为把错误体压到 clippy 的 `result_large_err` 阈值之下：
    /// 每个处理函数都返回 `Result<_, ApiError>`，让它的体积保持小更划算。
    pub details: Option<Box<Value>>,
}

impl ApiError {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        status: StatusCode,
        details: Option<Value>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
            details: details.map(Box::new),
        }
    }

    /// 请求语义不合法（400）。
    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, message, StatusCode::BAD_REQUEST, None)
    }

    /// 令牌缺失或错误（401）。
    pub fn unauthorized() -> Self {
        Self::new(
            "UNAUTHORIZED",
            "缺少或无效的 Bearer Token。",
            StatusCode::UNAUTHORIZED,
            None,
        )
    }

    /// Agent 服务尚未就绪（503）。
    pub fn service_unavailable() -> Self {
        Self::new(
            "SERVICE_UNAVAILABLE",
            "Agent 服务尚未就绪。",
            StatusCode::SERVICE_UNAVAILABLE,
            None,
        )
    }

    /// 请求参数校验失败（422），`details` 给出逐字段原因。
    pub fn validation(details: Value) -> Self {
        Self::new(
            "VALIDATION_ERROR",
            "请求参数校验失败。",
            StatusCode::UNPROCESSABLE_ENTITY,
            Some(details),
        )
    }

    /// 兜底内部错误：不把异常正文暴露给客户端。
    pub fn internal() -> Self {
        Self::new(
            "INTERNAL_ERROR",
            "服务器内部错误。",
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
        )
    }

    /// `{"error": {"code", "message", "details"}}`；`details` 恒在，缺省为 `null`。
    pub fn body(&self) -> Value {
        json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "details": self.details,
            }
        })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

/// 成功响应信封。
pub fn data(value: Value) -> Json<Value> {
    Json(json!({ "data": value }))
}

/// 框架级错误体（未匹配路径 / 方法不允许），形状与 Starlette 默认响应一致。
pub fn framework_detail(detail: &str, status: StatusCode) -> Response {
    (status, Json(json!({ "detail": detail }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_always_carries_details_key() {
        let error = ApiError::bad_request("RUN_NOT_FOUND", "生成任务不存在：x");
        assert_eq!(
            error.body(),
            json!({
                "error": {
                    "code": "RUN_NOT_FOUND",
                    "message": "生成任务不存在：x",
                    "details": Value::Null,
                }
            })
        );
    }

    #[test]
    fn data_envelope_wraps_value() {
        assert_eq!(
            data(json!({"status": "ok"})).0,
            json!({"data": {"status": "ok"}})
        );
    }
}
