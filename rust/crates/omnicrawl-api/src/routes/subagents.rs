//! 当前会话后台 SubAgent 任务与事件控制流路由（`omnicrawl/api/routes/subagents.py` 的移植）。
//!
//! 控制面不提供创建任务：远程客户端不能经 HTTP 创建或重新分发任务，任务与事件流始终用当前
//! 会话范围。任务本体归内核 `SubAgentTaskManager`，宿主经协议 `subagent.query` 转发查询与取消；
//! 会话级事件流是宿主侧投影（回合内由回合线程喂、回合外由事件泵排空内核通知）。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::app::ApiState;
use crate::error::{data, ApiError};
use crate::service::AgentService;

use super::query::query_bool;

/// SSE 空闲多久发一次 keep-alive（Python 的 `wait_for_subagent_events(..., 15.0)`）。
const KEEP_ALIVE_SECONDS: u64 = 15;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/subagents/events", get(stream_subagent_events))
        .route("/subagents", get(list_subagents))
        .route("/subagents/{task_id}", get(get_subagent))
        .route("/subagents/{task_id}/cancel", post(cancel_subagent))
}

/// `GET /subagents/events`：当前会话的后台任务/审批事件流，不绑定某个已结束的父 Run。
async fn stream_subagent_events(
    State(state): State<ApiState>,
    Query(params): Query<SubagentEventsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let follow = query_bool("follow", params.follow.as_deref(), true)?;
    let cursor = last_event_id(&headers)?.unwrap_or(0);
    let service = Arc::clone(state.service()?);
    // 冻结当前会话：产生事件的 Run 可能由别的 worker 持有，流内不再随会话切换漂移。
    let session_id = service.current_subagent_session_id();

    let (sender, receiver) = mpsc::channel::<Result<Bytes, Infallible>>(64);
    tokio::task::spawn_blocking(move || {
        produce_subagent_events(service, session_id, cursor, follow, sender)
    });

    let stream = ReceiverStream::new(receiver);
    let mut response = Body::from_stream(stream).into_response();
    let response_headers = response.headers_mut();
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response_headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

/// `GET /subagents`：当前会话可见的后台任务。
async fn list_subagents(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let tasks = state.service()?.list_subagent_tasks()?;
    Ok(data(Value::Array(tasks)))
}

/// `GET /subagents/{task_id}`：其他会话的任务统一不可见。
async fn get_subagent(
    State(state): State<ApiState>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.service()?.get_subagent_task(&task_id)? {
        Some(task) => Ok(data(task)),
        None => Err(subagent_not_found()),
    }
}

/// `POST /subagents/{task_id}/cancel`：只请求取消，不创建也不重启任务。
async fn cancel_subagent(
    State(state): State<ApiState>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.service()?.cancel_subagent_task(&task_id)? {
        Some(result) => Ok(data(result)),
        None => Err(subagent_not_found()),
    }
}

#[derive(Debug, Deserialize)]
struct SubagentEventsQuery {
    follow: Option<String>,
}

/// 事件生产：先补发游标之后的存量事件，再跟随；空闲满 15 秒发一行 keep-alive。
fn produce_subagent_events(
    service: Arc<AgentService>,
    session_id: String,
    mut cursor: u64,
    follow: bool,
    sender: mpsc::Sender<Result<Bytes, Infallible>>,
) {
    loop {
        for event in service.subagent_events_after(&session_id, cursor) {
            cursor = event.id;
            if sender
                .blocking_send(Ok(Bytes::from(event.to_sse())))
                .is_err()
            {
                return;
            }
        }
        if !follow {
            return;
        }
        service.wait_for_subagent_events(
            &session_id,
            cursor,
            Duration::from_secs(KEEP_ALIVE_SECONDS),
        );
        if service
            .subagent_events_after(&session_id, cursor)
            .is_empty()
            && sender
                .blocking_send(Ok(Bytes::from(": keep-alive\n\n")))
                .is_err()
        {
            return;
        }
    }
}

/// 当前会话找不到该任务（含跨会话访问）。
fn subagent_not_found() -> ApiError {
    ApiError::new(
        "SUBAGENT_NOT_FOUND",
        "未找到当前会话的 SubAgent 任务。",
        StatusCode::NOT_FOUND,
        None,
    )
}

/// `Last-Event-ID` → 游标；空头按未提供处理，负数按 0 处理。
fn last_event_id(headers: &HeaderMap) -> Result<Option<u64>, ApiError> {
    let Some(raw) = headers.get("last-event-id") else {
        return Ok(None);
    };
    let text = raw.to_str().unwrap_or_default().trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.parse::<i64>() {
        Ok(value) => Ok(Some(value.max(0) as u64)),
        Err(_) => Err(ApiError::bad_request(
            "INVALID_EVENT_ID",
            "Last-Event-ID 必须是整数。",
        )),
    }
}
