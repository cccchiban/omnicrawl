//! 会话生命周期与 artifact 路由（`omnicrawl/api/routes/sessions.py` 的移植）。
//!
//! 已搬的是「文件面」：列表、诊断、事件读取、重命名、删除、导出与 artifact 读取。
//! 要动内核上下文的三条（新建、恢复、压缩/归档当前会话）见 crate README 的迁移表。

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::ApiState;
use crate::error::{data, ApiError};

use super::query::{body_string_field, body_text, query_bool, query_int};

/// 会话标题上限（Python `RenameRequest.title` 的 `max_length`）。
const MAX_TITLE_CHARS: usize = 200;
/// 导出 Markdown 上限（Python `ExportRequest.markdown` 的 `max_length`）。
const MAX_EXPORT_CHARS: usize = 2_000_000;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions", post(new_session))
        .route("/sessions/diagnostics", get(session_diagnostics_overview))
        .route(
            "/sessions/{session_id}/diagnostics",
            get(session_diagnostics),
        )
        .route("/sessions/{session_id}/events", get(session_events))
        .route("/sessions/{session_id}/resume", post(resume_session))
        .route("/sessions/{session_id}", delete(delete_session))
        .route("/sessions/current", patch(rename_session))
        .route("/sessions/current/archive", post(archive_session))
        .route("/sessions/current/compact", post(compact_session))
        .route("/sessions/current/export", post(export_session))
        .route(
            "/sessions/{session_id}/artifacts/{*artifact_path}",
            get(session_artifact),
        )
}

/// `POST /sessions`：开一条新会话（内核按空 session_id 新建并报出新 ID）。
async fn new_session(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    let session_id = service.switch_session("", None)?;
    Ok(data(json!({"session_id": session_id})))
}

/// `POST /sessions/{id}/resume`：恢复指定会话并把它的运行守护残留回给客户端。
async fn resume_session(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    // 先读状态视图：未知会话在这里就返回 404，不会去动内核。
    let payload = service.session_state_payload(&session_id)?;
    service.switch_session(&session_id, None)?;
    Ok(data(payload))
}

/// `POST /sessions/current/archive`：归档当前会话并立刻开一条新会话。
async fn archive_session(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    let archived = service.archive_current_session()?;
    let payload = service.session_state_payload(&archived.session_id)?;
    service.switch_session("", None)?;
    Ok(data(payload))
}

/// `POST /sessions/current/compact`：显式压缩当前会话，返回摘要。
async fn compact_session(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    let result = service.compact_current_session()?;
    let summary = result
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(data(json!({"summary": summary})))
}

#[derive(Debug, Deserialize)]
struct SessionsQuery {
    limit: Option<String>,
    archived: Option<String>,
}

async fn list_sessions(
    State(state): State<ApiState>,
    Query(params): Query<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query_int("limit", params.limit.as_deref(), 20, 1, 100)?;
    let archived = query_bool("archived", params.archived.as_deref(), false)?;
    let entries = state.service()?.list_sessions(limit as usize, archived)?;
    Ok(data(Value::Array(
        entries.iter().map(|entry| entry.to_dict()).collect(),
    )))
}

async fn session_diagnostics_overview(
    State(state): State<ApiState>,
) -> Result<Json<Value>, ApiError> {
    Ok(data(state.service()?.session_diagnostics(None)?))
}

async fn session_diagnostics(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(data(
        state.service()?.session_diagnostics(Some(&session_id))?,
    ))
}

async fn session_events(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let events = state.service()?.session_events(&session_id)?;
    Ok(data(Value::Array(
        events.iter().map(|event| event.to_dict()).collect(),
    )))
}

async fn rename_session(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let title = body_text(
        "title",
        body_string_field(&body, "title")?.as_deref(),
        MAX_TITLE_CHARS,
    )?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    Ok(data(service.rename_current_session(&title)?.to_dict()))
}

async fn delete_session(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    service.delete_session(&session_id)?;
    Ok(data(json!({"deleted": true, "session_id": session_id})))
}

async fn export_session(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let markdown = body_text(
        "markdown",
        body_string_field(&body, "markdown")?.as_deref(),
        MAX_EXPORT_CHARS,
    )?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    let path = service.export_current_session_markdown(&markdown)?;
    Ok(data(json!({"path": path.to_string_lossy()})))
}

/// `GET /sessions/{id}/artifacts/{path}`：读 HTML 正文，路径越界与缺失分开报。
async fn session_artifact(
    State(state): State<ApiState>,
    Path((session_id, artifact_path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    if artifact_path.is_empty()
        || artifact_path
            .split(['/', '\\'])
            .any(|segment| segment == "..")
    {
        return Err(ApiError::bad_request(
            "INVALID_ARTIFACT_PATH",
            "artifact 路径不合法。",
        ));
    }
    let text = match state
        .service()?
        .read_session_artifact_text(&session_id, &artifact_path)
    {
        Ok(text) => text,
        Err(error) => {
            // 底层异常文案可能带路径与会话细节，不进对外响应；只把脱敏后的原因写日志。
            eprintln!(
                "[api] 读取会话 artifact 失败 {} / {}：{}",
                session_id,
                artifact_path,
                omnicrawl_session::redact_sensitive_text(&error.message)
            );
            return Err(ApiError::new(
                "ARTIFACT_NOT_FOUND",
                "会话 artifact 不存在。",
                StatusCode::NOT_FOUND,
                None,
            ));
        }
    };
    let mut response = text.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Ok(response)
}
