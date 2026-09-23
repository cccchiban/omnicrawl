//! 历史、Skill、MCP 与 Memory 支持路由（`omnicrawl/api/routes/support.py` 的移植）。

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::ApiState;
use crate::error::{data, ApiError};

use super::query::{query_bool, query_int};

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/history", get(prompt_history))
        .route("/skills", get(skills))
        .route("/mcp", get(mcp_status))
        .route("/memory/clean", post(clean_memory))
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    query: Option<String>,
    limit: Option<String>,
    current_session_only: Option<String>,
}

async fn prompt_history(
    State(state): State<ApiState>,
    Query(params): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query_int("limit", params.limit.as_deref(), 20, 1, 100)?;
    let current_only = query_bool(
        "current_session_only",
        params.current_session_only.as_deref(),
        false,
    )?;
    let entries = state.service()?.search_prompt_history(
        params.query.as_deref().unwrap_or(""),
        limit,
        current_only,
    )?;
    Ok(data(Value::Array(
        entries.iter().map(|entry| entry.to_dict()).collect(),
    )))
}

async fn skills(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let items: Vec<Value> = state
        .service()?
        .skills()
        .into_iter()
        .map(Value::Object)
        .collect();
    Ok(data(Value::Array(items)))
}

async fn mcp_status(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    Ok(data(json!({"status": state.service()?.mcp_status()})))
}

async fn clean_memory(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let service = state.service()?;
    service.ensure_mutation_allowed()?;
    let deleted = service.clean_memory()?;
    Ok(data(json!({
        "deleted": deleted,
        "count": deleted.len(),
    })))
}
