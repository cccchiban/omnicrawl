//! 模型、推理强度与审批模式配置路由（`omnicrawl/api/routes/configuration.py` 的移植）。
//!
//! 三件持久化的事（选中的模型、推理强度、审批模式）都先落盘再下发内核：落盘失败时内核
//! 保持旧配置（与 Python「持久化失败则运行时保持旧模型」同义）。内核侧只有一条
//! `session.settings` 通道，且内核主循环在跑回合时被占用，因此回合在途时这些端点会回
//! `409 RUN_ACTIVE` 而不是排队等待（Python 侧 Agent 在进程内，可以即时生效）。

use axum::extract::State;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use omnicrawl_config::features::approval::{
    normalize_approval_mode, save_approval_mode, APPROVAL_MODE_AUTO,
};
use omnicrawl_config::models::llm::{
    load_llm_config, normalize_reasoning_effort, save_active_model_ref, save_reasoning_effort,
    ActiveModelRef,
};
use omnicrawl_config::models::llm_multi::apply_model_selection;
use omnicrawl_config::models::model_catalog::{model_options_to_ui, save_llm_model, CatalogModel};
use omnicrawl_host::approval::ApprovalMode;

use crate::app::ApiState;
use crate::error::{data, ApiError};

use super::invalid_setting;
use super::query::{body_string_field, body_text, optional_body_text};

/// 模型字段上限（Python `ModelChangeRequest` 的 `max_length`）。
const MAX_MODEL_FIELD_CHARS: usize = 300;
/// `source` 字段上限。
const MAX_SOURCE_CHARS: usize = 30;
/// `protocol` 字段上限。
const MAX_PROTOCOL_CHARS: usize = 100;
/// 推理强度字段上限（`ReasoningChangeRequest.effort`）。
const MAX_EFFORT_CHARS: usize = 30;
/// 审批模式字段上限（`ApprovalChangeRequest.mode`）。
const MAX_MODE_CHARS: usize = 30;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/models", get(list_models))
        .route("/models/catalog", get(get_model_catalog))
        .route("/models/refresh", post(refresh_model_catalog))
        .route("/models/current", put(set_model))
        .route("/reasoning", put(set_reasoning))
        .route("/approval", put(set_approval))
}

/// `GET /models`：兼容旧扁平模型列表（`id` / `name` / `provider`）。
async fn list_models(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let options = state.service()?.model_options()?;
    let rows = model_options_to_ui(&options);
    Ok(data(serde_json::to_value(rows).unwrap_or(Value::Null)))
}

/// `GET /models/catalog`：双列目录（custom + detected + diagnostics）。
async fn get_model_catalog(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let catalog = state.service()?.model_catalog(false)?;
    Ok(data(json!({
        "current": to_json(&catalog.current),
        "custom": catalog.custom.iter().map(|item| catalog_row(item, false)).collect::<Vec<Value>>(),
        "detected": catalog.detected.iter().map(|item| catalog_row(item, true)).collect::<Vec<Value>>(),
        "diagnostics": to_json(&catalog.diagnostics),
    })))
}

/// `POST /models/refresh`：清空发现缓存后强制重扫远端模型列表。
async fn refresh_model_catalog(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let catalog = state.service()?.model_catalog(true)?;
    Ok(data(json!({
        "refreshed": true,
        "detected_count": catalog.detected.len(),
        "custom_count": catalog.custom.len(),
        "diagnostics": to_json(&catalog.diagnostics),
    })))
}

/// `PUT /models/current`：切换并保存当前模型。
async fn set_model(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let selection = resolve_model_selection(&body)?;
    let service = state.service()?;
    let env = service.environment();
    // 先解析出新模型视图（非法选择在这里就失败，不会写盘），再落盘，最后下发内核。
    let current = load_llm_config(env).map_err(invalid_setting)?;
    let next = apply_model_selection(env, &current, &selection.model).map_err(invalid_setting)?;
    // 落盘返回路径，这里只要副作用；两个分支都不是表达式值，避免类型外泄到 match。
    match &selection.reference {
        Some(reference) => {
            save_active_model_ref(env, reference, None).map_err(invalid_setting)?;
        }
        None => {
            save_llm_model(env, &selection.model, None).map_err(invalid_setting)?;
        }
    }
    service.apply_model_switch(&next)?;
    Ok(data(json!({
        "model": service.current_model(),
        "source": selection.source,
        "key": selection.key,
        "profile": selection.profile,
        "protocol": selection.protocol,
    })))
}

/// `PUT /reasoning`：切换并保存推理强度。
async fn set_reasoning(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let raw = body_string_field(&body, "effort")?;
    let effort = body_text("effort", raw.as_deref(), MAX_EFFORT_CHARS)?;
    let normalized = normalize_reasoning_effort(&effort).map_err(invalid_setting)?;
    let service = state.service()?;
    save_reasoning_effort(service.environment(), normalized, None).map_err(invalid_setting)?;
    service.apply_reasoning_effort(normalized)?;
    Ok(data(json!({"reasoning_effort": normalized})))
}

/// `PUT /approval`：切换并保存审批模式。
async fn set_approval(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let raw = body_string_field(&body, "mode")?;
    let mode = body_text("mode", raw.as_deref(), MAX_MODE_CHARS)?;
    let normalized = normalize_approval_mode(&mode).map_err(invalid_setting)?;
    let service = state.service()?;
    save_approval_mode(service.environment(), normalized, None).map_err(invalid_setting)?;
    // 宿主只区分「自动」与「人工」；`review`（自动审查）按人工处理，与启动时同一套映射。
    let runtime = if normalized == APPROVAL_MODE_AUTO {
        ApprovalMode::Auto
    } else {
        ApprovalMode::Manual
    };
    service.set_runtime_approval_mode(runtime)?;
    Ok(data(json!({"approval_mode": normalized})))
}

/// 一次切换目标：请求体解析结果与响应字段。
struct Selection {
    model: String,
    source: String,
    key: String,
    profile: String,
    protocol: String,
    /// `legacy`（裸 model）时为空，落盘走 `save_llm_model`。
    reference: Option<ActiveModelRef>,
}

/// 把规范字段或旧 `model` 字段解析为可切换目标（与 Python `_resolve_model_selection` 同序）。
fn resolve_model_selection(body: &Value) -> Result<Selection, ApiError> {
    let source = checked(body, "source", MAX_SOURCE_CHARS)?
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let model = checked(body, "model", MAX_MODEL_FIELD_CHARS)?;
    let key = checked(body, "key", MAX_MODEL_FIELD_CHARS)?;
    let profile = checked(body, "profile", MAX_MODEL_FIELD_CHARS)?;
    let model_id = checked(body, "model_id", MAX_MODEL_FIELD_CHARS)?;
    let protocol = checked(body, "protocol", MAX_PROTOCOL_CHARS)?;

    if source == "custom" {
        let key = key
            .or_else(|| model.clone())
            .unwrap_or_default()
            .trim()
            .to_string();
        if key.is_empty() {
            return Err(ApiError::bad_request(
                "INVALID_MODEL",
                "source=custom 时必须提供 key。",
            ));
        }
        return Ok(Selection {
            model: key.clone(),
            source: "custom".to_string(),
            key: key.clone(),
            profile: String::new(),
            protocol: String::new(),
            reference: Some(ActiveModelRef {
                source: "custom".to_string(),
                key,
                profile: String::new(),
                model_id: String::new(),
                protocol: String::new(),
            }),
        });
    }

    if source == "detected" {
        let profile = profile.unwrap_or_default().trim().to_string();
        let model_id = model_id
            .or_else(|| model.clone())
            .unwrap_or_default()
            .trim()
            .to_string();
        let protocol = protocol.unwrap_or_default().trim().to_string();
        if profile.is_empty() || model_id.is_empty() {
            return Err(ApiError::bad_request(
                "INVALID_MODEL",
                "source=detected 时必须提供 profile 与 model_id。",
            ));
        }
        let token = format!("{profile}/{model_id}");
        return Ok(Selection {
            model: token,
            source: "detected".to_string(),
            key: String::new(),
            profile: profile.clone(),
            protocol: protocol.clone(),
            reference: Some(ActiveModelRef {
                source: "detected".to_string(),
                key: String::new(),
                profile,
                model_id,
                protocol,
            }),
        });
    }

    let model = model
        .or(model_id)
        .or(key)
        .unwrap_or_default()
        .trim()
        .to_string();
    if model.is_empty() {
        return Err(ApiError::bad_request(
            "INVALID_MODEL",
            "请提供 model，或使用 source/key 或 source/profile/model_id。",
        ));
    }
    Ok(Selection {
        model,
        source: "legacy".to_string(),
        key: String::new(),
        profile: String::new(),
        protocol: String::new(),
        reference: None,
    })
}

/// 取一个可选字符串字段并校验上限；缺字段返回 `None`。
fn checked(body: &Value, name: &str, max_length: usize) -> Result<Option<String>, ApiError> {
    match body_string_field(body, name)? {
        None => Ok(None),
        Some(text) => optional_body_text(name, Some(text.as_str()), "", max_length).map(Some),
    }
}

/// 目录条目 → 客户端行：模型项字段 + 目录来源字段。
fn catalog_row(item: &CatalogModel, detected: bool) -> Value {
    let mut row = match serde_json::to_value(item.to_option().to_ui_table()) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    row.insert("source".to_string(), json!(item.source));
    row.insert("key".to_string(), json!(item.key));
    row.insert("profile".to_string(), json!(item.profile_id));
    row.insert("protocol".to_string(), json!(item.protocol));
    row.insert("model_id".to_string(), json!(item.model_id));
    row.insert("display_name".to_string(), json!(item.display_name));
    row.insert("availability".to_string(), json!(item.availability));
    if detected {
        row.insert(
            "matched_custom_key".to_string(),
            json!(item.matched_custom_key),
        );
    }
    Value::Object(row)
}

/// 配置侧的 TOML 视图转成响应 JSON；这些值都可序列化，失败只可能是内部错误。
fn to_json<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}
