//! 抓取端点：`POST /v1/fetch` 与 `GET /v1/status`。
//!
//! 请求体一律是 JSON。抓取是阻塞式网络调用（并行抓取用线程，JS 沙箱也在本线程里跑），
//! 因此处理函数经 `spawn_blocking` 承接，不阻塞 axum 的异步 worker。
//!
//! 字段口径直接对映 `fetcher` 工具的参数：`urls` / `max_chars` / `max_html` / `timeout` /
//! `parallel` / `insecure` / `impersonate` / `run_scripts` / `force_scripts`。
//! 接口**不做类型校验**，全部交给 `fetch_batch` 的宽松归一化——批量脚本传
//! `"max_chars": "20000"` 也照常生效。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Map, Value};

use omnicrawl_host::tools::cookies::{CookieJar, SharedJar};
use omnicrawl_host::tools::fetcher::fetch_batch;
use omnicrawl_host::tools::FetcherOptions;

use crate::app::{data, ApiError, AppState};
use crate::config::FetchSettings;

/// 抓取参数的透传白名单：这几个键原样交给 `fetch_batch`。
///
/// 白名单而不是全量透传，是为了让拼错的字段尽早显形——静默忽略会让调用方以为参数生效了。
const PASSTHROUGH_KEYS: [&str; 7] = [
    "max_chars",
    "max_html",
    "timeout",
    "parallel",
    "insecure",
    "run_scripts",
    "force_scripts",
];

/// `POST /v1/fetch`：`{urls, ...}` → `{count, elapsed_seconds, results}`。
pub async fn fetch(
    State(state): State<AppState>,
    body: Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let settings = state.settings.clone();
    let arguments = arguments_from_body(&body.0, &settings)?;
    let options = Arc::new(options_for_request(&settings.fetcher));
    let outcome = tokio::task::spawn_blocking(move || fetch_batch(options.as_ref(), &arguments))
        .await
        .map_err(|error| {
            ApiError::new(
                "INTERNAL_ERROR",
                format!("抓取任务异常结束：{error}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?
        .map_err(|error| {
            ApiError::bad_request(
                error.code.unwrap_or_else(|| "INVALID_REQUEST".to_string()),
                error.message,
            )
        })?;

    let results: Vec<Value> = outcome
        .entries
        .iter()
        .map(|entry| {
            json!({
                "url": entry.url,
                "error": entry.error,
                "status": entry.status,
                "final_url": entry.final_url,
                "title": entry.title,
                "content": entry.content,
            })
        })
        .collect();
    Ok(data(json!({
        "count": results.len(),
        "elapsed_seconds": outcome.elapsed_seconds,
        "results": results,
    })))
}

/// `GET /v1/status`：服务自检（不回传任何凭据，本服务也没有凭据）。
pub async fn status(State(state): State<AppState>) -> Json<Value> {
    let settings = &state.settings;
    data(json!({
        "ready": settings.ready(),
        "listen": settings.address(),
        "max_urls": settings.url_limit(),
        "impersonate": if settings.api.impersonate.is_empty() {
            Value::Null
        } else {
            Value::String(settings.api.impersonate.clone())
        },
        "unavailable_reason": settings.unavailable_reason(),
    }))
}

/// 把请求体转成 `fetch_batch` 的参数表。
///
/// `urls` 与 `url` 都接：单地址写成 `{"url": "..."}` 比 `{"urls": ["..."]}` 顺手，
/// 两者同时出现时以 `urls` 为准。
fn arguments_from_body(
    body: &Value,
    settings: &FetchSettings,
) -> Result<Map<String, Value>, ApiError> {
    let Some(object) = body.as_object() else {
        return Err(ApiError::bad_request(
            "INVALID_REQUEST",
            "请求体必须是 JSON 对象，例如 {\"urls\": [\"https://example.com\"]}。",
        ));
    };
    let Some(urls) = object.get("urls").or_else(|| object.get("url")) else {
        return Err(ApiError::bad_request(
            "INVALID_REQUEST",
            "缺少字段 urls（或 url）：请提供至少一个要抓取的地址。",
        ));
    };

    let mut arguments = Map::new();
    // 数组按上限截断（与工具侧同一口径：超出部分静默丢弃而不是整单失败）。
    let limit = settings.url_limit();
    arguments.insert(
        "urls".to_string(),
        match urls {
            Value::Array(items) => Value::Array(items.iter().take(limit).cloned().collect()),
            other => other.clone(),
        },
    );
    for key in PASSTHROUGH_KEYS {
        if let Some(value) = object.get(key) {
            arguments.insert(key.to_string(), value.clone());
        }
    }
    // 配置里固定了指纹族时以配置为准：批量脚本不必逐条重挑身份。
    if !settings.api.impersonate.is_empty() {
        arguments.insert(
            "impersonate".to_string(),
            Value::String(settings.api.impersonate.clone()),
        );
    } else if let Some(value) = object.get("impersonate") {
        arguments.insert("impersonate".to_string(), value.clone());
    }
    Ok(arguments)
}

/// 单次请求的执行体：`FetcherOptions` 的每请求副本。
///
/// 只有 cookie 罐是新的：不同调用方（不同脚本、不同 Python 进程）不该共享会话 cookie，
/// 否则一个脚本登录后的身份会漏给另一个。其余（传输、UA、代理、会话缓存）照旧共用。
fn options_for_request(base: &FetcherOptions) -> FetcherOptions {
    let jar: SharedJar = Arc::new(CookieJar::new());
    FetcherOptions {
        cookie_jar: Some(jar),
        ..base.clone()
    }
}
