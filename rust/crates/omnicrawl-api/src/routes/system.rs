//! 系统运行时路由（`omnicrawl/api/routes/system.py` 的移植）。

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;

use crate::app::ApiState;
use crate::error::{data, ApiError};

pub fn router() -> Router<ApiState> {
    Router::new().route("/runtime", get(runtime))
}

async fn runtime(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    Ok(data(state.service()?.runtime_snapshot()))
}
