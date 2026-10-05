//! 决策端点：通用 `decide`、选优 `choice`、排序 `rank`、审查 `review` 与状态 `status`。
//!
//! 请求体一律是 JSON。四个决策端点的形状与语义见 `omnicrawl://docs/decision_api.md`；
//! 出站调用是阻塞式 HTTP，因此每个处理函数都经 `spawn_blocking` 承接，不阻塞 axum 的
//! 异步 worker。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Map, Value};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_host::review::{masking_from_config, DecisionReviewOptions, ReviewMasking};

use crate::app::{data, ApiError, AppState};
pub use crate::upstream::DecisionContext;

use crate::upstream::{
    decide as upstream_decide, decide_choice as upstream_choice, decide_rank as upstream_rank,
    decide_review as upstream_review, UpstreamError, DEFAULT_QUESTION_ID, DEFAULT_TIMEOUT_SECONDS,
    MAX_TIMEOUT_SECONDS,
};

/// 请求带来的 `state` 上限（字符数）：决策只需要判断用的语料。
const MAX_STATE_CHARS: usize = 64_000;
/// 一次请求里候选项/候选描述的数量上限。
const MAX_CANDIDATES: usize = 64;

/// 按服务配置与进程环境组装决策上下文。
///
/// 脱敏旁路按 `[desensitization]` 装配（与工具调用审查、检索重排、提问托管同源）；
/// 装配失败时由下游中止请求——本接口是出站请求的发起方，绝不外发未脱敏的原文。
pub fn runtime_from_config(channel: &DecisionReviewOptions, environment: &ConfigEnvironment) -> DecisionContext {
    let masking: Option<Arc<ReviewMasking>> = masking_from_config(environment).map(Arc::new);
    DecisionContext {
        channel: channel.clone(),
        masking,
        timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
    }
}

/// 按请求体里的 `timeout_seconds` 覆盖超时（越界按上限收口）。
fn with_timeout(context: &DecisionContext, requested: Option<f64>) -> DecisionContext {
    let timeout_seconds = requested
        .map(|value| value.clamp(1.0, MAX_TIMEOUT_SECONDS as f64) as u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    DecisionContext {
        channel: context.channel.clone(),
        masking: context.masking.clone(),
        timeout_seconds,
    }
}

/// 把上游失败映射成接口错误。
fn upstream_error(error: UpstreamError) -> ApiError {
    match error.code {
        "INVALID_REQUEST" => ApiError::bad_request(error.code, error.message),
        "DECISION_UNPARSABLE" => ApiError::unparsable(error.message),
        _ => ApiError::upstream(error.code, error.message),
    }
}

/// `POST /v1/decide`：`{state, questions, timeout_seconds?}` → `{answers, raw}`。
pub async fn decide(State(state): State<AppState>, body: Json<Value>) -> Result<Json<Value>, ApiError> {
    let context = state.context()?.clone();
    let body = body.0;
    let payload = require_object(&body, "state")?;
    let questions = require_object(&body, "questions")?;
    let context = with_timeout(&context, optional_number(&body, "timeout_seconds")?);
    let outcome = tokio::task::spawn_blocking(move || upstream_decide(&context, &payload, &questions))
        .await
        .map_err(join_error)?
        .map_err(upstream_error)?;
    Ok(data(json!({
        "answers": outcome.answers,
        "raw": outcome.raw,
    })))
}

/// `POST /v1/choice`：从候选项里选一个，回 `{index, answers}`。
pub async fn choice(State(state): State<AppState>, body: Json<Value>) -> Result<Json<Value>, ApiError> {
    let context = state.context()?.clone();
    let body = body.0;
    let payload = require_object(&body, "state")?;
    let options = require_text_list(&body, "options")?;
    let instructions = optional_text(&body, "instructions")?.unwrap_or_default();
    let question_id = question_id(&body)?;
    let context = with_timeout(&context, optional_number(&body, "timeout_seconds")?);
    let outcome = tokio::task::spawn_blocking(move || {
        upstream_choice(&context, &payload, &instructions, &options, &question_id)
    })
    .await
    .map_err(join_error)?
    .map_err(upstream_error)?;
    Ok(data(json!({
        "index": outcome.index,
        "answers": outcome.answers,
    })))
}

/// `POST /v1/rank`：按相关度重排候选项，回 `{order, count}`（`order` 是输入下标的新顺序）。
pub async fn rank(State(state): State<AppState>, body: Json<Value>) -> Result<Json<Value>, ApiError> {
    let context = state.context()?.clone();
    let body = body.0;
    let payload = require_object(&body, "state")?;
    let candidates = require_text_list(&body, "candidates")?;
    let instructions = optional_text(&body, "instructions")?.unwrap_or_default();
    let question_id = question_id(&body)?;
    let context = with_timeout(&context, optional_number(&body, "timeout_seconds")?);
    let order = tokio::task::spawn_blocking(move || {
        upstream_rank(
            &context,
            &payload,
            &instructions,
            &candidates,
            &question_id,
        )
    })
    .await
    .map_err(join_error)?
    .map_err(upstream_error)?;
    let count = order.len();
    Ok(data(json!({"order": order, "count": count})))
}

/// `POST /v1/review`：判定一份待审查负载，回 `{approved, reason, detail, confidence}`。
pub async fn review(State(state): State<AppState>, body: Json<Value>) -> Result<Json<Value>, ApiError> {
    let context = state.context()?.clone();
    let body = body.0;
    let payload = require_object(&body, "payload")?;
    // 缺省 fail-closed：脱敏不可用时拒绝判定（与工具调用审查同一语义）。
    let fail_closed = optional_bool(&body, "fail_closed")?.unwrap_or(true);
    let context = with_timeout(&context, optional_number(&body, "timeout_seconds")?);
    let verdict = tokio::task::spawn_blocking(move || {
        upstream_review(&context, &payload, fail_closed)
    })
    .await
    .map_err(join_error)?
    .map_err(upstream_error)?;
    let mut result = Map::new();
    result.insert("approved".to_string(), Value::Bool(verdict.approved));
    result.insert(
        "reason".to_string(),
        verdict.reason.map(Value::String).unwrap_or(Value::Null),
    );
    result.insert("detail".to_string(), Value::String(verdict.detail));
    result.insert(
        "confidence".to_string(),
        verdict
            .confidence
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
    );
    Ok(data(Value::Object(result)))
}

/// `GET /v1/status`：服务与渠道的自检信息（不含凭据）。
pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let settings = &state.settings;
    let channel = settings.channel.as_ref().map(|channel| {
        json!({
            "mode": channel.mode,
            "model": channel.model,
            "base_url": channel.base_url,
            "api_key_env": channel.api_key_env,
            "api_key_configured": !channel.resolve_api_key().is_empty(),
        })
    });
    Ok(data(json!({
        "ready": settings.ready(),
        "listen": settings.address(),
        "channel": channel,
        "unavailable_reason": settings.unavailable_reason(),
    })))
}

fn join_error(error: tokio::task::JoinError) -> ApiError {
    ApiError::new(
        "INTERNAL_ERROR",
        format!("决策任务异常结束：{error}"),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
}

/// 提问 ID：调用方没给就用默认值。
fn question_id(body: &Value) -> Result<String, ApiError> {
    Ok(optional_text(body, "question_id")?.unwrap_or_else(|| DEFAULT_QUESTION_ID.to_string()))
}

/// 必填对象字段；非对象按 400 报错。
fn require_object(body: &Value, name: &str) -> Result<Value, ApiError> {
    match body.get(name) {
        Some(value @ Value::Object(_)) => {
            let text = value.to_string();
            if text.chars().count() > MAX_STATE_CHARS {
                return Err(ApiError::bad_request(
                    "INVALID_REQUEST",
                    format!("{name} 过大（上限 {MAX_STATE_CHARS} 字符）。"),
                ));
            }
            Ok(value.clone())
        }
        Some(_) => Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("{name} 必须是 JSON 对象。"),
        )),
        None => Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("缺少字段 {name}。"),
        )),
    }
}

/// 必填的字符串数组字段（候选项 / 候选描述）。
fn require_text_list(body: &Value, name: &str) -> Result<Vec<String>, ApiError> {
    let Some(value) = body.get(name) else {
        return Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("缺少字段 {name}。"),
        ));
    };
    let Some(items) = value.as_array() else {
        return Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("{name} 必须是字符串数组。"),
        ));
    };
    if items.is_empty() {
        return Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("{name} 不能为空。"),
        ));
    }
    if items.len() > MAX_CANDIDATES {
        return Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("{name} 最多 {MAX_CANDIDATES} 项。"),
        ));
    }
    let mut texts: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(ApiError::bad_request(
                "INVALID_REQUEST",
                format!("{name} 的每一项都必须是字符串。"),
            ));
        };
        if text.trim().is_empty() {
            return Err(ApiError::bad_request(
                "INVALID_REQUEST",
                format!("{name} 的每一项都不能为空。"),
            ));
        }
        texts.push(text.to_string());
    }
    Ok(texts)
}

/// 可选字符串字段。
fn optional_text(body: &Value, name: &str) -> Result<Option<String>, ApiError> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("{name} 必须是字符串。"),
        )),
    }
}

/// 可选布尔字段。
fn optional_bool(body: &Value, name: &str) -> Result<Option<bool>, ApiError> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(ApiError::bad_request(
            "INVALID_REQUEST",
            format!("{name} 必须是布尔值。"),
        )),
    }
}

/// 可选数字字段。
fn optional_number(body: &Value, name: &str) -> Result<Option<f64>, ApiError> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_f64().map(Some).ok_or_else(|| {
            ApiError::bad_request("INVALID_REQUEST", format!("{name} 必须是数字。"))
        }),
    }
}
