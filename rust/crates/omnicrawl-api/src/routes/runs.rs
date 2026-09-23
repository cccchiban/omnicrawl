//! 生成任务与人工确认路由（`omnicrawl/api/routes/runs.py` 的移植）。

use std::convert::Infallible;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::app::ApiState;
use crate::error::{data, ApiError};
use crate::runs::RunStatus;
use crate::service::AgentService;

/// SSE 空闲多久发一次 keep-alive（Python 的 `wait_for_events(..., 15.0)`）。
const KEEP_ALIVE_SECONDS: u64 = 15;
/// SSE 生产者的事件轮询粒度。
const STREAM_POLL: Duration = Duration::from_millis(100);

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/runs", post(create_run))
        .route("/runs/{run_id}", get(get_run))
        .route("/runs/{run_id}/events", get(stream_events))
        .route("/runs/{run_id}/cancel", post(cancel_run))
        .route(
            "/runs/{run_id}/confirmations/{confirmation_id}",
            post(confirm_run),
        )
        .route(
            "/runs/{run_id}/questions/{question_id}",
            post(answer_user_question),
        )
}

#[derive(Debug, Deserialize)]
struct RunRequest {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConfirmationDecision {
    approved: bool,
}

#[derive(Debug, Deserialize)]
struct UserQuestionAnswer {
    answer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    follow: Option<String>,
}

fn service_of(state: &ApiState) -> Result<&Arc<AgentService>, ApiError> {
    state.service()
}

async fn create_run(
    State(state): State<ApiState>,
    Json(payload): Json<RunRequest>,
) -> Result<Response, ApiError> {
    let service = service_of(&state)?;
    let Some(message) = payload.message else {
        return Err(ApiError::validation(json!([{
            "loc": ["body", "message"],
            "msg": "Field required",
            "type": "missing",
        }])));
    };
    if message.is_empty() {
        return Err(ApiError::validation(json!([{
            "loc": ["body", "message"],
            "msg": "String should have at least 1 character",
            "type": "string_too_short",
        }])));
    }
    let run = service.start_run(&message)?;
    Ok((StatusCode::ACCEPTED, data(run.summary())).into_response())
}

async fn get_run(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(data(service_of(&state)?.summary(&run_id)?))
}

async fn cancel_run(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(data(service_of(&state)?.cancel_run(&run_id)?))
}

async fn confirm_run(
    State(state): State<ApiState>,
    Path((run_id, confirmation_id)): Path<(String, String)>,
    Json(payload): Json<ConfirmationDecision>,
) -> Result<Json<Value>, ApiError> {
    Ok(data(service_of(&state)?.decide_confirmation(
        &run_id,
        &confirmation_id,
        payload.approved,
    )?))
}

async fn answer_user_question(
    State(state): State<ApiState>,
    Path((run_id, question_id)): Path<(String, String)>,
    Json(payload): Json<UserQuestionAnswer>,
) -> Result<Json<Value>, ApiError> {
    let Some(answer) = payload.answer else {
        return Err(ApiError::validation(json!([{
            "loc": ["body", "answer"],
            "msg": "Field required",
            "type": "missing",
        }])));
    };
    Ok(data(service_of(&state)?.decide_question(
        &run_id,
        &question_id,
        &answer,
    )?))
}

/// `GET /runs/{id}/events`：SSE 事件流。
///
/// 生产者跑在阻塞线程上（回合事件来自内核子进程与运行记录），客户端断开时通道关闭，
/// 生产者下一次写入失败自然收尾。
async fn stream_events(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let service = Arc::clone(service_of(&state)?);
    // 先校验任务存在：不存在的任务要 404，而不是开一条空流。
    service.get_run(&run_id)?;
    let cursor = parse_last_event_id(&headers)?;
    let follow = parse_follow(query.follow.as_deref())?;

    let (sender, receiver) = mpsc::channel::<Result<Bytes, Infallible>>(64);
    let producer_service = Arc::clone(&service);
    let producer_run = run_id.clone();
    tokio::task::spawn_blocking(move || {
        produce_events(producer_service, producer_run, cursor, follow, sender)
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

/// `Last-Event-ID` → 游标；负数按 0 处理（Python 的 `max(0, int(...))`）。
fn parse_last_event_id(headers: &HeaderMap) -> Result<u64, ApiError> {
    let Some(raw) = headers.get("last-event-id") else {
        return Ok(0);
    };
    let text = raw.to_str().unwrap_or_default().trim();
    match text.parse::<i64>() {
        Ok(value) => Ok(value.max(0) as u64),
        Err(_) => Err(ApiError::bad_request(
            "INVALID_EVENT_ID",
            "Last-Event-ID 必须是整数。",
        )),
    }
}

/// `follow` 查询参数；缺省为真，取值非法按校验失败处理。
fn parse_follow(raw: Option<&str>) -> Result<bool, ApiError> {
    let Some(text) = raw else {
        return Ok(true);
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(ApiError::validation(json!([{
            "loc": ["query", "follow"],
            "msg": "Input should be a valid boolean",
            "type": "bool_parsing",
            "input": other,
        }]))),
    }
}

/// 事件生产：先补发游标之后的存量事件，再跟随到终态；空闲满 15 秒发一行 keep-alive。
fn produce_events(
    service: Arc<AgentService>,
    run_id: String,
    cursor: u64,
    follow: bool,
    sender: mpsc::Sender<Result<Bytes, Infallible>>,
) {
    let mut cursor = cursor;
    loop {
        let Ok(run) = service.get_run(&run_id) else {
            return;
        };
        match service.events_after(&run_id, cursor) {
            Ok(events) => {
                for event in events {
                    cursor = event.id;
                    let text = event.to_sse();
                    if sender.blocking_send(Ok(Bytes::from(text))).is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                // 游标过期等错误无法再改状态码：记日志并收尾，客户端应重新拉任务状态。
                eprintln!("[api] SSE 读取事件失败：{}", error.message);
                return;
            }
        }
        if !follow || run.status.is_terminal() {
            return;
        }
        if wait_for_new_events(&service, &run_id, cursor) {
            continue;
        }
        if sender
            .blocking_send(Ok(Bytes::from(": keep-alive\n\n")))
            .is_err()
        {
            return;
        }
    }
}

/// 等最多 `KEEP_ALIVE_SECONDS`；有新事件或任务进终态返回真。
fn wait_for_new_events(service: &AgentService, run_id: &str, cursor: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(KEEP_ALIVE_SECONDS);
    while Instant::now() < deadline {
        thread::sleep(STREAM_POLL);
        match service.events_after(run_id, cursor) {
            Ok(events) if !events.is_empty() => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
        match service.get_run(run_id) {
            Ok(run) if run.status.is_terminal() => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
    }
    false
}

/// 终态判定给测试与生产者共用（`docs/API.md`：终态后流自动结束）。
pub fn is_terminal(status: RunStatus) -> bool {
    status.is_terminal()
}
