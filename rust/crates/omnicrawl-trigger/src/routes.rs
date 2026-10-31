//! 端点：创建、列表、详情、事件流、转录、取消、状态。
//!
//! 事件流与本地 API 的 `/runs/{id}/events` 同形（`id` / `event` / `data`），客户端可以复用
//! 同一套 SSE 解析。区别只有一处：无人值守下不会出现 `confirmation.required` 与
//! `ask_user.required`——工具表里没有会等人工的入口。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use omnicrawl_session::SessionStore;

use crate::app::{ApiError, AppState, data};
use crate::records::{NewTrigger, TriggerState};
use crate::request::{self, build_prompt};

/// SSE 空闲多久发一次 keep-alive。
const KEEP_ALIVE_SECONDS: u64 = 15;
/// SSE 生产者的事件轮询粒度。
const STREAM_POLL: Duration = Duration::from_millis(100);
/// `wait_seconds` 轮询粒度。
const WAIT_POLL: Duration = Duration::from_millis(100);
/// 列表缺省条数。
const DEFAULT_LIST_LIMIT: usize = 20;

#[derive(Debug, Deserialize)]
struct ListQuery {
    state: Option<String>,
    tag: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    follow: Option<String>,
}

/// `GET /v1/status`：配置、并发与队列现状；**不回传凭据**。
pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let pool = &state.pool;
    let config = pool.config();
    let store = pool.store();
    let running = store.running_ids().len();
    let queued = store.list(Some(TriggerState::Queued), None, 1024).len();
    Ok(data(json!({
        "ready": pool.is_alive(),
        "max_concurrency": config.max_concurrency,
        "running": running,
        "queued": queued,
        "session_mode": config.session_mode,
        "tools": config.tools,
        "default_timeout_seconds": config.default_timeout_seconds,
        "unavailable_reason": Value::Null,
    })))
}

/// `POST /v1/triggers`：受理一个触发，返回记录摘要。
///
/// 响应 `202`（已排队）或 `200`（`wait_seconds` 内已终态）。去重命中时返回原记录并置
/// `duplicate = true`——调用方可以安全重试。
pub async fn create(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Response, ApiError> {
    let pool = state.pool.clone();
    if !pool.is_alive() {
        return Err(ApiError::service_unavailable(
            "触发接口正在关闭，不再受理新触发。",
        ));
    }
    let config = pool.config().clone();
    let parsed = request::parse_create(&payload, &config.session_mode)?;

    let workspace = if parsed.workspace.is_empty() {
        config.default_workspace.clone()
    } else {
        parsed.workspace.clone()
    };
    let timeout_seconds = parsed
        .timeout_seconds
        .unwrap_or(config.default_timeout_seconds);
    let prompt = build_prompt(
        &parsed.title,
        &parsed.message,
        parsed.data.as_ref(),
        chrono::Utc::now().timestamp(),
    );
    let spec = NewTrigger {
        trigger_id: new_trigger_id(),
        title: parsed.title,
        prompt,
        raw_message: parsed.message,
        workspace,
        timeout_seconds,
        session_key: parsed.session_key,
        idempotency_key: parsed.idempotency_key,
        tags: parsed.tags,
        notify: parsed.notify,
    };

    let store = Arc::clone(pool.store());
    let (record, duplicate) = store.submit(spec).map_err(|error| error.to_api_error())?;

    // `wait_seconds > 0` 时等到终态再回；否则立刻返回排队视图。
    let wait = Duration::from_secs(parsed.wait_seconds.max(0) as u64);
    if !wait.is_zero() {
        let trigger_id = record.trigger_id.clone();
        // 闭包拿一份克隆：`store` 后面还要读终态与入队位置。
        let waiter = Arc::clone(&store);
        tokio::task::spawn_blocking(move || wait_terminal(&waiter, &trigger_id, wait))
            .await
            .map_err(|_| ApiError::service_unavailable("等待触发收尾失败。"))?;
    }

    let current = store
        .get(&record.trigger_id)
        .map_err(|error| error.to_api_error())?;
    let mut body = current.summary();
    if let Some(map) = body.as_object_mut() {
        map.insert("duplicate".to_string(), json!(duplicate));
        if current.state == TriggerState::Queued {
            map.insert(
                "queue_position".to_string(),
                json!(store.queue_position(&record.trigger_id)),
            );
        }
    }
    let status = if current.state.is_terminal() {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, data(body)).into_response())
}

/// `GET /v1/triggers`：按创建时间倒序，可按状态与标签过滤。
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, 100);
    let state_filter = match query.state.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(TriggerState::parse(raw).ok_or_else(|| {
            ApiError::bad_request(
                "INVALID_REQUEST",
                "state 只能是 queued / running / completed / failed / cancelled。",
            )
        })?),
    };
    let tag = match query.tag.as_deref() {
        None | Some("") => None,
        Some(raw) => {
            let (key, value) = raw
                .split_once('=')
                .ok_or_else(|| ApiError::bad_request("INVALID_REQUEST", "tag 必须写成 k=v。"))?;
            if key.trim().is_empty() {
                return Err(ApiError::bad_request(
                    "INVALID_REQUEST",
                    "tag 的键不能为空。",
                ));
            }
            Some((key.trim().to_string(), value.trim().to_string()))
        }
    };
    let records = state.pool.store().list(
        state_filter,
        tag.as_ref()
            .map(|(key, value)| (key.as_str(), value.as_str())),
        limit,
    );
    Ok(data(Value::Array(
        records.iter().map(|record| record.summary()).collect(),
    )))
}

/// `GET /v1/triggers/{id}`：单条状态与结果。
pub async fn detail(
    State(state): State<AppState>,
    Path(trigger_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .pool
        .store()
        .get(&trigger_id)
        .map_err(|error| error.to_api_error())?;
    Ok(data(record.summary()))
}

/// `POST /v1/triggers/{id}/cancel`：幂等取消。
pub async fn cancel(
    State(state): State<AppState>,
    Path(trigger_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .pool
        .store()
        .request_cancel(&trigger_id)
        .map_err(|error| error.to_api_error())?;
    Ok(data(record.summary()))
}

/// `GET /v1/triggers/{id}/transcript`：会话转录投影。
///
/// 直接读会话 JSONL（触发接口专用的会话根），按事件原样返回，与本地 API 的
/// `GET /sessions/{id}/events` 同形。
pub async fn transcript(
    State(state): State<AppState>,
    Path(trigger_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = state
        .pool
        .store()
        .get(&trigger_id)
        .map_err(|error| error.to_api_error())?;
    if record.session_id.is_empty() {
        return Ok(data(json!({
            "trigger_id": trigger_id,
            "session_id": Value::Null,
            "events": [],
            "note": "本次触发还没有可用会话（尚未开始或内核未报出会话 ID）。",
        })));
    }
    let root = state.pool.config().session_root.clone();
    let session_id = record.session_id.clone();
    let read = tokio::task::spawn_blocking(move || {
        let store = SessionStore::open(root);
        store.read_events(&session_id)
    })
    .await
    .map_err(|_| ApiError::service_unavailable("读取会话转录失败。"))?;
    match read {
        Ok(events) => Ok(data(json!({
            "trigger_id": trigger_id,
            "session_id": record.session_id,
            "events": events.iter().map(|event| event.to_dict()).collect::<Vec<Value>>(),
        }))),
        Err(error) => Err(ApiError::new(
            "TRANSCRIPT_UNAVAILABLE",
            format!("读取会话转录失败：{error:?}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

/// `GET /v1/triggers/{id}/events`：SSE 事件流。
///
/// 生产者跑在阻塞线程上（事件来自内存记录），客户端断开时通道关闭，生产者下一次写入失败
/// 自然收尾。
pub async fn events(
    State(state): State<AppState>,
    Path(trigger_id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let store = Arc::clone(state.pool.store());
    // 先校验记录存在：不存在的触发要 404，而不是开一条空流。
    store
        .get(&trigger_id)
        .map_err(|error| error.to_api_error())?;
    let cursor = parse_last_event_id(&headers)?;
    let follow = parse_follow(query.follow.as_deref())?;

    let (sender, receiver) = mpsc::channel::<Result<Bytes, Infallible>>(64);
    tokio::task::spawn_blocking(move || produce_events(store, trigger_id, cursor, follow, sender));

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

/// 逐条推送事件，直到终态、客户端断开或游标过期。
fn produce_events(
    store: Arc<crate::records::TriggerStore>,
    trigger_id: String,
    cursor: u64,
    follow: bool,
    sender: mpsc::Sender<Result<Bytes, Infallible>>,
) {
    let mut last = cursor;
    let mut idle = Duration::ZERO;
    loop {
        match store.events_after(&trigger_id, last) {
            Ok(events) => {
                for event in events {
                    last = event.id;
                    idle = Duration::ZERO;
                    if sender
                        .blocking_send(Ok(Bytes::from(event.to_sse())))
                        .is_err()
                    {
                        return;
                    }
                }
                let terminal = store
                    .get(&trigger_id)
                    .map(|record| record.state.is_terminal())
                    .unwrap_or(true);
                if terminal {
                    // 终态后再读一次，确保最后一条终态事件已经送出。
                    if let Ok(events) = store.events_after(&trigger_id, last) {
                        for event in events {
                            last = event.id;
                            if sender
                                .blocking_send(Ok(Bytes::from(event.to_sse())))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    return;
                }
            }
            Err(_) => {
                // 游标过期或记录被回收：发一条错误事件后收流，让客户端知道要重新拉状态。
                let payload = json!({"code": "EVENT_CURSOR_EXPIRED", "earliest_event_id": last});
                let frame = format!("event: stream.error\ndata: {payload}\n\n");
                let _ = sender.blocking_send(Ok(Bytes::from(frame)));
                return;
            }
        }
        if !follow {
            return;
        }
        if store.wait_for_events(&trigger_id, last, STREAM_POLL) {
            idle = Duration::ZERO;
            continue;
        }
        idle += STREAM_POLL;
        if idle >= Duration::from_secs(KEEP_ALIVE_SECONDS) {
            idle = Duration::ZERO;
            if sender
                .blocking_send(Ok(Bytes::from(": keep-alive\n\n")))
                .is_err()
            {
                return;
            }
        }
    }
}

/// 等触发进入终态，最多 `limit`。
fn wait_terminal(store: &crate::records::TriggerStore, trigger_id: &str, limit: Duration) {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        match store.get(trigger_id) {
            Ok(record) if record.state.is_terminal() => return,
            Ok(_) => {}
            // 记录不在（保留期回收）：没有可等的事实。
            Err(_) => return,
        }
        std::thread::sleep(WAIT_POLL);
    }
}

/// `Last-Event-ID` → 游标；负数按 0 处理。
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

/// `follow` 查询参数；缺省为真。
fn parse_follow(raw: Option<&str>) -> Result<bool, ApiError> {
    let Some(text) = raw else {
        return Ok(true);
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("follow 必须是布尔值：{other}"),
        )),
    }
}

/// 触发 ID：`trg_` + 12 位十六进制（时间 + 计数器），同一毫秒内也不会撞。
fn new_trigger_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("trg_{:012x}", (nanos ^ (seq << 20)) & 0xffff_ffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_ids_are_unique_within_a_burst() {
        let ids: Vec<String> = (0..1000).map(|_| new_trigger_id()).collect();
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        assert!(ids.iter().all(|id| id.starts_with("trg_")));
    }

    #[test]
    fn follow_parsing_accepts_common_spellings() {
        assert!(parse_follow(None).expect("缺省为真"));
        assert!(parse_follow(Some("yes")).expect("真"));
        assert!(!parse_follow(Some("0")).expect("假"));
        assert!(parse_follow(Some("maybe")).is_err());
    }
}
