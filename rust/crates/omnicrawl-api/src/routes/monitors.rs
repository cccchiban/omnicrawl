//! 后台 Monitor 任务路由（`omnicrawl/api/routes/monitors.py` 的移植）。
//!
//! 三个端点都只读：启动与停止后台任务仍然只走 Agent 的 `monitor` 内置工具。任务由宿主
//! 的 `MonitorManager` 持有，与工具表共享同一个实例，因此这里看到的就是工具启动的那批。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::app::ApiState;
use crate::error::{data, ApiError};
use crate::runs::RunEvent;
use crate::service::AgentService;

use super::query::{query_bool, query_int};

/// SSE 空闲多久发一次 keep-alive（Python 的 `wait_for_monitor_events(..., 15.0)`）。
const KEEP_ALIVE_SECONDS: u64 = 15;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/monitors", get(list_monitors))
        .route("/monitors/{monitor_id}", get(get_monitor))
        .route("/monitors/{monitor_id}/events", get(stream_monitor_events))
}

async fn list_monitors(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    Ok(data(Value::Array(state.service()?.list_monitors()?)))
}

async fn get_monitor(
    State(state): State<ApiState>,
    Path(monitor_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(data(state.service()?.monitor_task(&monitor_id)?))
}

#[derive(Debug, Deserialize)]
struct MonitorEventsQuery {
    follow: Option<String>,
    max_events: Option<String>,
    cursor: Option<String>,
}

/// `GET /monitors/{id}/events`：后台任务日志的 SSE。
///
/// 生产者跑在阻塞线程上（事件来自宿主内存里的环形缓冲），客户端断开时通道关闭，
/// 生产者下一次写入失败自然收尾；任务落入终态或任务被清理时也会收尾。
async fn stream_monitor_events(
    State(state): State<ApiState>,
    Path(monitor_id): Path<String>,
    Query(params): Query<MonitorEventsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let follow = query_bool("follow", params.follow.as_deref(), true)?;
    let max_events = query_int("max_events", params.max_events.as_deref(), 100, 1, 200)? as usize;
    let cursor = query_int("cursor", params.cursor.as_deref(), 0, 0, i64::MAX)? as u64;
    // Python：`max(0, int(last_event_id_header or cursor))`——空头按未提供处理。
    let start = last_event_id(&headers)?.unwrap_or(cursor);
    let service = Arc::clone(state.service()?);
    // 先校验任务存在：不存在的任务要 404，而不是开一条空流。
    service.monitor_task(&monitor_id)?;

    let (sender, receiver) = mpsc::channel::<Result<Bytes, Infallible>>(64);
    let producer_service = Arc::clone(&service);
    let producer_id = monitor_id.clone();
    tokio::task::spawn_blocking(move || {
        produce_monitor_events(
            producer_service,
            producer_id,
            start,
            follow,
            max_events,
            sender,
        )
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

/// 事件生产：先补发游标之后的存量日志，再跟随到任务终态；空闲满 15 秒发一行 keep-alive。
fn produce_monitor_events(
    service: Arc<AgentService>,
    monitor_id: String,
    mut cursor: u64,
    follow: bool,
    max_events: usize,
    sender: mpsc::Sender<Result<Bytes, Infallible>>,
) {
    loop {
        let Ok(result) = service.poll_monitor_events(&monitor_id, cursor, max_events) else {
            // 任务已被清理：不能再改状态码，直接收尾，客户端重新拉任务列表。
            return;
        };
        for event in &result.events {
            cursor = event.sequence;
            let name = if event.stream == "stdout" || event.stream == "stderr" {
                "monitor.output"
            } else {
                "monitor.status"
            };
            let frame = RunEvent {
                id: event.sequence,
                event: name.to_string(),
                data: json!({
                    "monitor_id": monitor_id,
                    "sequence": event.sequence,
                    "created_at": event.created_at,
                    "stream": event.stream,
                    "text": event.text,
                    "status": result.snapshot.status,
                    "exit_code": result.snapshot.exit_code,
                }),
            };
            if sender
                .blocking_send(Ok(Bytes::from(frame.to_sse())))
                .is_err()
            {
                return;
            }
        }
        if !follow || result.snapshot.status != "running" {
            return;
        }
        service.wait_for_monitor_events(
            &monitor_id,
            cursor,
            Duration::from_secs(KEEP_ALIVE_SECONDS),
        );
        if result.events.is_empty()
            && sender
                .blocking_send(Ok(Bytes::from(": keep-alive\n\n")))
                .is_err()
        {
            return;
        }
    }
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
