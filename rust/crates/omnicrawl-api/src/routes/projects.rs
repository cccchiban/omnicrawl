//! 项目与工作区切换路由（`omnicrawl/api/routes/projects.py` 的移植）。
//!
//! 请求体字段与限长按 Python 侧的 Pydantic 模型实现：`ProjectRequest`（name 1..200、
//! path 0..32768）、`ProjectRenameRequest` / `ProjectPinRequest`（path 1..32768）。
//! `POST /projects/switch` 要切内核工作区，见 crate README 的迁移表。

use axum::extract::{Query, State};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::ApiState;
use crate::error::{data, ApiError};

use super::query::{
    body_bool, body_string_field, body_text, optional_body_text, query_text_required,
};

/// 项目路径字段上限（Python `ProjectRequest.path` 的 `max_length`）。
const MAX_PROJECT_PATH_CHARS: usize = 32_768;
/// 项目名上限（Python `ProjectRequest.name` 的 `max_length`）。
const MAX_PROJECT_NAME_CHARS: usize = 200;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/projects", get(list_projects))
        .route("/projects", post(create_project))
        .route("/projects", patch(rename_project))
        .route("/projects", delete(remove_project))
        .route("/projects/overview", get(list_project_overview))
        .route("/projects/import", post(import_project))
        .route("/projects/pin", post(pin_project))
        .route("/projects/switch", post(switch_project))
}

#[derive(Debug, Deserialize)]
struct RemoveProjectQuery {
    path: Option<String>,
}

async fn list_projects(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let entries = state.service()?.list_projects()?;
    Ok(data(Value::Array(
        entries.iter().map(|entry| entry.to_value()).collect(),
    )))
}

async fn list_project_overview(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let overview = state.service()?.project_overview()?;
    Ok(data(json_serialize(&overview)))
}

async fn create_project(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let name = body_text(
        "name",
        body_string_field(&body, "name")?.as_deref(),
        MAX_PROJECT_NAME_CHARS,
    )?;
    let path = optional_body_text(
        "path",
        body_string_field(&body, "path")?.as_deref(),
        "",
        MAX_PROJECT_PATH_CHARS,
    )?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    Ok(data(service.create_project(&name, &path)?.to_value()))
}

async fn import_project(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let name = body_text(
        "name",
        body_string_field(&body, "name")?.as_deref(),
        MAX_PROJECT_NAME_CHARS,
    )?;
    let path = optional_body_text(
        "path",
        body_string_field(&body, "path")?.as_deref(),
        "",
        MAX_PROJECT_PATH_CHARS,
    )?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    Ok(data(service.import_project(&name, &path)?.to_value()))
}

async fn rename_project(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let path = body_text(
        "path",
        body_string_field(&body, "path")?.as_deref(),
        MAX_PROJECT_PATH_CHARS,
    )?;
    let name = body_text(
        "name",
        body_string_field(&body, "name")?.as_deref(),
        MAX_PROJECT_NAME_CHARS,
    )?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    Ok(data(service.rename_project(&path, &name)?.to_value()))
}

async fn pin_project(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let path = body_text(
        "path",
        body_string_field(&body, "path")?.as_deref(),
        MAX_PROJECT_PATH_CHARS,
    )?;
    let pinned = body_bool("pinned", body.get("pinned"), true)?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    Ok(data(service.pin_project(&path, pinned)?.to_value()))
}

async fn remove_project(
    State(state): State<ApiState>,
    Query(params): Query<RemoveProjectQuery>,
) -> Result<Json<Value>, ApiError> {
    let path = query_text_required("path", params.path.as_deref())?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    service.remove_project(&path)?;
    Ok(data(json!({"removed": true, "path": path})))
}

/// `POST /projects/switch`：切换工作区。会话不绑工作区，切换后仍是同一条会话。
async fn switch_project(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let path = body_text(
        "path",
        body_string_field(&body, "path")?.as_deref(),
        MAX_PROJECT_PATH_CHARS,
    )?;
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    // 与 Python 同序：先严格解析目标（不存在或不是目录直接失败），再重起内核换工作区。
    let candidate = match std::fs::canonicalize(&path) {
        Ok(resolved) => resolved,
        Err(error) => {
            return Err(ApiError::bad_request(
                "INVALID_WORKSPACE",
                format!("工作区切换失败：{path} 无法解析，{error}"),
            ))
        }
    };
    if !candidate.is_dir() {
        return Err(ApiError::bad_request(
            "INVALID_WORKSPACE",
            format!("工作区切换失败：{} 不是目录。", candidate.display()),
        ));
    }
    let session_id = service.current_session_id();
    let switched = service.switch_session(&session_id, Some(&candidate))?;
    Ok(data(json!({
        "workspace_root": candidate.to_string_lossy(),
        "session_id": switched,
    })))
}

/// 项目总览条目（`ProjectOverview`）转成 JSON 数组。
fn json_serialize(overview: &[omnicrawl_session::ProjectOverview]) -> Value {
    match serde_json::to_value(overview) {
        Ok(value) => value,
        Err(_) => Value::Null,
    }
}
