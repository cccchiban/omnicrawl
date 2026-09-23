//! 运行设置路由（`omnicrawl/api/routes/settings.py` 的移植）：`GET /settings` 全量只读，
//! `PUT /settings/<域>` 增量写。
//!
//! 写端点按域接收白名单字段，未传字段保持当前值；敏感字段（api_key / token / 渠道凭据）
//! 不回传明文，只回 `has_*` 标记，也不接受明文写入。
//!
//! 与 Python 的差别只有一处：Python 先改 Agent 运行态、再落盘，落盘失败时把运行态回滚；
//! Rust 宿主没有常驻 Agent 配置，等价物是「先构造并校验新配置，再原子落盘」——落盘失败
//! 时磁盘不会被改坏，但也不存在需要回滚的运行态。写端点与 Python 一样不做「运行中禁改」
//! 拦截：设置属运行时配置，改完即时生效（下一次读取即生效）。

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::{json, Map, Value};

use omnicrawl_config::core::settings::{
    load_feature_enabled, load_show_thinking, save_context_compaction_trigger_percent,
    save_context_window_tokens, save_feature_enabled, save_show_thinking,
};
use omnicrawl_config::features::advisor::load_advisor_config;
use omnicrawl_config::features::agent_workspace::{
    load_agent_workspace_config, save_agent_workspace_config,
};
use omnicrawl_config::features::context_compaction::load_context_compaction_config;
use omnicrawl_config::features::image_gen::{
    load_image_gen_configuration, save_image_gen_configuration,
};
use omnicrawl_config::features::run_guard::{load_run_guard_config, save_run_guard_config};
use omnicrawl_config::features::subagents::load_subagent_config;
use omnicrawl_config::features::tools::{load_tool_switches, save_tool_switches};
use omnicrawl_config::features::tts::{load_tts_configuration, save_tts_configuration};
use omnicrawl_config::models::channels::load_channel_configuration;
use omnicrawl_config::models::llm::{load_llm_config, ActiveModelRef};
use omnicrawl_config::models::vision::{
    load_vision_configuration, save_vision_configuration, VisionConfiguration,
};
use omnicrawl_config::ConfigError;
use omnicrawl_controllers::settings::{SUBAGENT_ADVANCED_SETTING_KEYS, TOOL_SWITCH_KEYS};
use omnicrawl_mcp::load_mcp_config;

use crate::app::ApiState;
use crate::error::{data, ApiError};

use super::invalid_setting;
use super::query::{
    body_int, body_number, body_object, body_required_bool, body_required_int, body_string_field,
    body_text_list,
};

/// 上下文窗口上限（Python `ContextWindowSetting.window_tokens` 的 `le`）。
const MAX_CONTEXT_WINDOW_TOKENS: i64 = 1_000_000_000;
/// `ImageGenSetting.n` 的取值范围。
const IMAGE_GEN_N_RANGE: (i64, i64) = (1, 10);
/// `ImageGenSetting.timeout_seconds` 的取值范围。
const IMAGE_GEN_TIMEOUT_RANGE: (i64, i64) = (1, 600);
/// `TtsSetting.thread_count` 的取值范围。
const TTS_THREAD_RANGE: (i64, i64) = (1, 32);
/// 设置里的自由文本上限（Python 侧这些字段没有长度约束，这里只防住异常大包）。
const MAX_SETTING_TEXT_CHARS: usize = 32_768;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/settings", get(get_settings))
        .route("/settings/context", put(put_context))
        .route("/settings/context_compaction", put(put_context_compaction))
        .route("/settings/show_thinking", put(put_show_thinking))
        .route("/settings/features", put(put_features))
        .route("/settings/run_guard", put(put_run_guard))
        .route("/settings/agent_workspace", put(put_agent_workspace))
        .route("/settings/vision", put(put_vision))
        .route("/settings/image_gen", put(put_image_gen))
        .route("/settings/tts", put(put_tts))
        .route("/settings/tools", put(put_tools))
}

// ---------- GET /settings ----------

async fn get_settings(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let service = state.service()?;
    let env = service.environment();
    let llm = load_llm_config(env).map_err(invalid_setting)?;
    let context_window = llm.context_window_tokens;
    let compaction = load_context_compaction_config(env, None).map_err(invalid_setting)?;
    let run_guard = load_run_guard_config(env, None).map_err(invalid_setting)?;
    let workspace = load_agent_workspace_config(env, None).map_err(invalid_setting)?;
    let vision = load_vision_configuration(env, None).map_err(invalid_setting)?;
    let image_gen = load_image_gen_configuration(env, None).map_err(invalid_setting)?;
    let tts = load_tts_configuration(env, None).map_err(invalid_setting)?;
    let subagents = load_subagent_config(env, None).map_err(invalid_setting)?;
    let tool_switches = load_tool_switches(env, None).map_err(invalid_setting)?;
    // 与 Python 一致：这三项读不出来时给空载荷，而不是让整个快照失败。
    let advisor = load_advisor_config(env, None).unwrap_or_default();
    let channels = load_channel_configuration(env, None, None).ok();
    let mcp = load_mcp_config(env, None).ok();

    let channel_payload = match channels {
        Some(configuration) => json!({
            "default_key": configuration.default_key,
            "channels": configuration
                .channels
                .iter()
                .map(|channel| json!({
                    "name": channel.name,
                    "provider": channel.provider,
                    "enabled": channel.enabled,
                    "has_api_key": !channel.api_key.is_empty(),
                    "api_key_env": channel.api_key_env,
                    "base_url": channel.base_url,
                }))
                .collect::<Vec<Value>>(),
        }),
        None => json!({"default_key": "", "channels": []}),
    };
    let mcp_payload = match mcp {
        Some(configuration) => json!({
            "enabled": configuration.enabled,
            "default_timeout_seconds": configuration.default_timeout_seconds,
            "servers": configuration
                .servers
                .iter()
                .map(|(name, server)| json!({
                    "name": name,
                    "enabled": server.enabled,
                    "transport": server.transport,
                    "command": server.command.clone().unwrap_or_default(),
                    "url": server.url.clone().unwrap_or_default(),
                    "timeout_seconds": server.timeout_seconds,
                    "risk_level": server.risk_level,
                }))
                .collect::<Vec<Value>>(),
            "policy": {
                "require_confirmation_for_write": configuration.policy.require_confirmation_for_write,
                "require_confirmation_for_command": configuration.policy.require_confirmation_for_command,
                "allow_external_network_tools": configuration.policy.allow_external_network_tools,
                "audit_log_enabled": configuration.policy.audit_log_enabled,
            },
        }),
        None => json!({
            "enabled": false,
            "default_timeout_seconds": 30,
            "servers": [],
            "policy": {},
        }),
    };

    Ok(data(json!({
        "model": {
            "current": service.current_model(),
            "source": llm.model_source,
            "catalog_key": llm.catalog_key,
            "context_window_tokens": context_window,
            "reasoning_effort": service.reasoning_effort(),
        },
        "approval": service.approval_mode(),
        "context": {"window_tokens": context_window},
        "context_compaction": {
            "trigger_percent": compaction.trigger_context_percent.unwrap_or(80),
            "trigger_tokens": compaction.trigger_context_tokens,
        },
        "show_thinking": load_show_thinking(env, None).unwrap_or(true),
        // 插件运行态：与 `features.plugins` 的区别在于这里给的是运行期实际装上的
        // Worker、执行计划与诊断，供设置页与排障展示。
        "plugins": service.plugins_status(),
        "features": {
            "memory": load_feature_enabled(env, "memory", false, None, None).unwrap_or(false),
            "plugins": service.plugins_enabled(),
            "subagents": load_feature_enabled(env, "subagents", false, None, None).unwrap_or(false),
            "mcp": service.mcp_enabled(),
        },
        "run_guard": serialize_run_guard(&run_guard),
        "agent_workspace": serialize_agent_workspace(&workspace),
        "vision": serialize_vision(&vision),
        "image_gen": serialize_image_gen(&image_gen),
        "tts": serialize_tts(&tts),
        "advisor": {
            "enabled": advisor.enabled,
            "model_key": advisor.model_key,
            "effort": advisor.effort,
            "disabled_for_models": advisor.disabled_for_models,
        },
        "channels": channel_payload,
        "tools": {
            "switches": tool_switches,
            "keys": TOOL_SWITCH_KEYS,
        },
        "mcp": mcp_payload,
        "subagents_advanced": {
            "values": subagent_advanced_values(&subagents),
            "keys": SUBAGENT_ADVANCED_SETTING_KEYS,
        },
    })))
}

// ---------- PUT /settings/context ----------

async fn put_context(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let tokens = body_required_int(
        "window_tokens",
        body.get("window_tokens"),
        1,
        MAX_CONTEXT_WINDOW_TOKENS,
    )?;
    let service = state.service()?;
    let env = service.environment();
    let llm = load_llm_config(env).map_err(invalid_setting)?;
    let percent = load_context_compaction_config(env, None)
        .map_err(invalid_setting)?
        .trigger_context_percent
        .unwrap_or(80);
    let path =
        save_context_window_tokens(env, tokens, &llm.model_source, &llm.catalog_key, None, None)
            .map_err(invalid_setting)?;
    // 联动压缩阈值：窗口变了，按同一百分比重算阈值 Token。
    save_context_compaction_trigger_percent(env, percent, tokens, None).map_err(invalid_setting)?;
    Ok(data(json!({
        "window_tokens": tokens,
        "trigger_percent": percent,
        "trigger_tokens": tokens * percent / 100,
        "saved_path": path.to_string_lossy(),
    })))
}

// ---------- PUT /settings/context_compaction ----------

async fn put_context_compaction(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let percent = body_required_int("trigger_percent", body.get("trigger_percent"), 1, 100)?;
    let service = state.service()?;
    let env = service.environment();
    let llm = load_llm_config(env).map_err(invalid_setting)?;
    let path =
        save_context_compaction_trigger_percent(env, percent, llm.context_window_tokens, None)
            .map_err(invalid_setting)?;
    Ok(data(json!({
        "trigger_percent": percent,
        "trigger_tokens": llm.context_window_tokens * percent / 100,
        "window_tokens": llm.context_window_tokens,
        "saved_path": path.to_string_lossy(),
    })))
}

// ---------- PUT /settings/show_thinking ----------

async fn put_show_thinking(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let enabled = body_required_bool("enabled", body.get("enabled"))?;
    let env = state.service()?.environment();
    let path = save_show_thinking(env, enabled, None).map_err(invalid_setting)?;
    Ok(data(json!({
        "show_thinking": enabled,
        "saved_path": path.to_string_lossy(),
    })))
}

// ---------- PUT /settings/features ----------

async fn put_features(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let mut changes: Vec<(&'static str, bool)> = Vec::new();
    for key in ["memory", "plugins", "subagents", "show_thinking"] {
        if let Some(raw) = provided(&body, key) {
            changes.push((key, body_required_bool(key, Some(raw))?));
        }
    }
    let service = state.service()?;
    let env = service.environment();
    let mut diagnostics: Vec<String> = Vec::new();
    for (key, value) in changes {
        let saved = if key == "show_thinking" {
            save_show_thinking(env, value, None)
        } else {
            save_feature_enabled(env, key, value, None, None)
        };
        saved.map_err(|error| {
            ApiError::new(
                "SETTINGS_PARTIAL_FAILED",
                format!("功能开关保存失败：{}", error.message()),
                StatusCode::BAD_GATEWAY,
                None,
            )
        })?;
        // 插件总开关写盘之后立刻切运行期：Worker 与执行计划当场重建，
        // 不必重启服务（与 Python 设置面板的事务式重建同一语义）。
        if key == "plugins" {
            diagnostics = service.set_plugins_enabled(value)?;
        }
    }
    Ok(data(json!({
        "features": {
            "memory": load_feature_enabled(env, "memory", false, None, None).unwrap_or(false),
            "plugins": service.plugins_enabled(),
            "subagents": load_feature_enabled(env, "subagents", false, None, None).unwrap_or(false),
            "show_thinking": load_show_thinking(env, None).unwrap_or(true),
        },
        "plugins": service.plugins_status(),
        "diagnostics": diagnostics,
    })))
}

// ---------- PUT /settings/run_guard ----------

async fn put_run_guard(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let env = state.service()?.environment();
    let mut next = load_run_guard_config(env, None).map_err(invalid_setting)?;
    if let Some(raw) = provided(&body, "enabled") {
        next.enabled = body_required_bool("enabled", Some(raw))?;
    }
    if let Some(guard) = body_object(&body, "guard") {
        if let Some(raw) = provided(&guard, "enabled") {
            next.guard.enabled = body_required_bool("guard.enabled", Some(raw))?;
        }
        for (field, target) in [
            ("window_chars", &mut next.guard.window_chars),
            ("substr_len", &mut next.guard.substr_len),
            ("check_every", &mut next.guard.check_every),
            ("max_blocks", &mut next.guard.max_blocks),
            ("max_chars", &mut next.guard.max_chars),
            ("max_guard_retries", &mut next.guard.max_guard_retries),
        ] {
            let name = format!("guard.{field}");
            if let Some(value) = body_int(&name, guard.get(field), i64::MIN, i64::MAX)? {
                *target = value;
            }
        }
        if let Some(value) = body_number("guard.repeat_ratio", guard.get("repeat_ratio"))? {
            next.guard.repeat_ratio = value;
        }
        if let Some(list) =
            body_text_list("guard.auto_retry_errors", guard.get("auto_retry_errors"))?
        {
            next.guard.auto_retry_errors = list;
        }
    }
    if let Some(continuation) = body_object(&body, "continuation") {
        if let Some(raw) = provided(&continuation, "enabled") {
            next.continuation.enabled = body_required_bool("continuation.enabled", Some(raw))?;
        }
        if let Some(value) = body_int(
            "continuation.max_auto_followups",
            continuation.get("max_auto_followups"),
            i64::MIN,
            i64::MAX,
        )? {
            next.continuation.max_auto_followups = value;
        }
    }
    let next = next.validate().map_err(invalid_setting)?;
    let path = save_run_guard_config(env, &next, None).map_err(invalid_setting)?;
    Ok(data(with_saved_path(
        serialize_run_guard(&next),
        &path.to_string_lossy(),
    )))
}

// ---------- PUT /settings/agent_workspace ----------

async fn put_agent_workspace(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let env = state.service()?.environment();
    let previous = load_agent_workspace_config(env, None).map_err(invalid_setting)?;
    let mut next = previous.clone();
    if let Some(raw) = provided(&body, "enabled") {
        next.enabled = body_required_bool("enabled", Some(raw))?;
    }
    if let Some(raw) = provided(&body, "detached") {
        next.detached = body_required_bool("detached", Some(raw))?;
    }
    if let Some(raw) = provided(&body, "apply_on_exit") {
        next.apply_on_exit = body_required_bool("apply_on_exit", Some(raw))?;
    }
    if let Some(raw) = provided(&body, "sync_uncommitted") {
        next.sync_uncommitted = body_required_bool("sync_uncommitted", Some(raw))?;
    }
    if let Some(text) = body_string_field(&body, "mode")? {
        next.mode = text;
    }
    if let Some(text) = body_string_field(&body, "base_branch")? {
        next.base_branch = text;
    }
    if let Some(text) = body_string_field(&body, "cleanup_on_exit")? {
        next.cleanup_on_exit = text;
    }
    let next = next.normalize().map_err(invalid_setting)?;
    let path = save_agent_workspace_config(env, &next, None).map_err(invalid_setting)?;
    Ok(data(with_saved_path(
        serialize_agent_workspace(&next),
        &path.to_string_lossy(),
    )))
}

// ---------- PUT /settings/vision ----------

async fn put_vision(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let enabled = body_required_bool("enabled", body.get("enabled"))?;
    let env = state.service()?.environment();
    let previous = load_vision_configuration(env, None).map_err(invalid_setting)?;
    let next = VisionConfiguration {
        enabled,
        models: previous.models,
    }
    .validate()
    .map_err(invalid_setting)?;
    let path = save_vision_configuration(env, &next, None).map_err(invalid_setting)?;
    Ok(data(with_saved_path(
        serialize_vision(&next),
        &path.to_string_lossy(),
    )))
}

// ---------- PUT /settings/image_gen ----------

async fn put_image_gen(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let env = state.service()?.environment();
    let mut next = load_image_gen_configuration(env, None).map_err(invalid_setting)?;
    if let Some(raw) = provided(&body, "enabled") {
        next.enabled = body_required_bool("enabled", Some(raw))?;
    }
    if provided(&body, "api_key").is_some() {
        let text = body_string_field(&body, "api_key")?.unwrap_or_default();
        // 凭据只走环境变量：这里只接受「清空」，明文一律拒绝。
        if !text.is_empty() {
            return Err(ApiError::bad_request(
                "INVALID_SETTING",
                "image_gen.api_key 不接受明文写入；请使用 api_key_env 环境变量。",
            ));
        }
        next.api_key = String::new();
    }
    if let Some(value) = body_int("n", body.get("n"), IMAGE_GEN_N_RANGE.0, IMAGE_GEN_N_RANGE.1)? {
        next.n = value;
    }
    if let Some(value) = body_int(
        "timeout_seconds",
        body.get("timeout_seconds"),
        IMAGE_GEN_TIMEOUT_RANGE.0,
        IMAGE_GEN_TIMEOUT_RANGE.1,
    )? {
        next.timeout_seconds = value;
    }
    for (field, target) in [
        ("base_url", &mut next.base_url),
        ("api_key_env", &mut next.api_key_env),
        ("model", &mut next.model),
        ("size", &mut next.size),
        ("quality", &mut next.quality),
        ("output_format", &mut next.output_format),
    ] {
        if let Some(text) = body_string_field(&body, field)? {
            if text.chars().count() > MAX_SETTING_TEXT_CHARS {
                return Err(invalid_setting(ConfigError::new(format!(
                    "image_gen.{field} 过长。"
                ))));
            }
            *target = text;
        }
    }
    let next = next.normalize().map_err(invalid_setting)?;
    let path = save_image_gen_configuration(env, &next, None).map_err(invalid_setting)?;
    Ok(data(with_saved_path(
        serialize_image_gen(&next),
        &path.to_string_lossy(),
    )))
}

// ---------- PUT /settings/tts ----------

async fn put_tts(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let env = state.service()?.environment();
    let mut next = load_tts_configuration(env, None).map_err(invalid_setting)?;
    if let Some(raw) = provided(&body, "enabled") {
        next.enabled = body_required_bool("enabled", Some(raw))?;
    }
    if let Some(raw) = provided(&body, "auto_play") {
        next.auto_play = body_required_bool("auto_play", Some(raw))?;
    }
    if let Some(raw) = provided(&body, "streaming") {
        next.streaming = body_required_bool("streaming", Some(raw))?;
    }
    if let Some(value) = body_int(
        "thread_count",
        body.get("thread_count"),
        TTS_THREAD_RANGE.0,
        TTS_THREAD_RANGE.1,
    )? {
        next.thread_count = value;
    }
    for (field, target) in [
        ("model_dir", &mut next.model_dir),
        ("voice", &mut next.voice),
        ("device", &mut next.device),
        ("output_dir", &mut next.output_dir),
    ] {
        if let Some(text) = body_string_field(&body, field)? {
            if text.chars().count() > MAX_SETTING_TEXT_CHARS {
                return Err(invalid_setting(ConfigError::new(format!(
                    "tts.{field} 过长。"
                ))));
            }
            *target = text;
        }
    }
    let next = next.normalize().map_err(invalid_setting)?;
    let path = save_tts_configuration(env, &next, None).map_err(invalid_setting)?;
    Ok(data(with_saved_path(
        serialize_tts(&next),
        &path.to_string_lossy(),
    )))
}

// ---------- PUT /settings/tools ----------

async fn put_tools(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    match provided(&body, "switches") {
        Some(Value::Object(switches)) => {
            for (name, raw) in switches {
                let Some(enabled) = raw.as_bool() else {
                    return Err(ApiError::bad_request(
                        "INVALID_SETTING",
                        format!("tools.{name} 必须是布尔值。"),
                    ));
                };
                entries.push((name.clone(), enabled));
            }
        }
        Some(_) => {
            return Err(ApiError::bad_request(
                "INVALID_SETTING",
                "tools 更新需要 {name, enabled} 或 {switches: {name: bool}}。",
            ))
        }
        None => {
            let name = body_string_field(&body, "name")?.unwrap_or_default();
            let name = name.trim().to_string();
            let Some(raw) = provided(&body, "enabled") else {
                return Err(ApiError::bad_request(
                    "INVALID_SETTING",
                    "tools 更新需要 {name, enabled} 或 {switches: {name: bool}}。",
                ));
            };
            let Some(enabled) = raw.as_bool() else {
                return Err(ApiError::bad_request(
                    "INVALID_SETTING",
                    "tools 更新需要 {name, enabled} 或 {switches: {name: bool}}。",
                ));
            };
            if name.is_empty() {
                return Err(ApiError::bad_request(
                    "INVALID_SETTING",
                    "tools 更新需要 {name, enabled} 或 {switches: {name: bool}}。",
                ));
            }
            entries.push((name, enabled));
        }
    }

    // 先整批校验名称，避免中途失败留下半份改动。
    let mut normalized: Vec<(String, bool)> = Vec::new();
    for (raw_name, enabled) in entries {
        let name = validate_tool_name(&raw_name)?;
        normalized.push((name, enabled));
    }
    let env = state.service()?.environment();
    if !normalized.is_empty() {
        save_tool_switches(env, &normalized, None).map_err(invalid_setting)?;
    }
    let switches = load_tool_switches(env, None).map_err(invalid_setting)?;
    let applied: Map<String, Value> = normalized
        .iter()
        .map(|(name, enabled)| (name.clone(), Value::Bool(*enabled)))
        .collect();
    Ok(data(json!({
        "switches": switches,
        "applied": Value::Object(applied),
    })))
}

// ---------- 序列化与工具 ----------

fn serialize_run_guard(config: &omnicrawl_config::features::run_guard::RunGuardConfig) -> Value {
    json!({
        "enabled": config.enabled,
        "guard": {
            "enabled": config.guard.enabled,
            "window_chars": config.guard.window_chars,
            "substr_len": config.guard.substr_len,
            "repeat_ratio": config.guard.repeat_ratio,
            "check_every": config.guard.check_every,
            "max_blocks": config.guard.max_blocks,
            "max_chars": config.guard.max_chars,
            "max_guard_retries": config.guard.max_guard_retries,
            "auto_retry_errors": config.guard.auto_retry_errors,
        },
        "continuation": {
            "enabled": config.continuation.enabled,
            "max_auto_followups": config.continuation.max_auto_followups,
        },
    })
}

fn serialize_agent_workspace(
    config: &omnicrawl_config::features::agent_workspace::AgentWorkspaceConfig,
) -> Value {
    json!({
        "enabled": config.enabled,
        "mode": config.mode,
        "base_branch": config.base_branch,
        "base_ref": config.base_ref,
        "detached": config.detached,
        "apply_on_exit": config.apply_on_exit,
        "cleanup_on_exit": config.cleanup_on_exit,
        "sync_uncommitted": config.sync_uncommitted,
        "copy_dirs": config.copy_dirs,
        "env_scripts": config.env_scripts,
    })
}

fn serialize_vision(config: &VisionConfiguration) -> Value {
    json!({
        "enabled": config.enabled,
        "models": config.models.iter().map(ActiveModelRef::to_table).collect::<Vec<_>>(),
    })
}

fn serialize_image_gen(
    config: &omnicrawl_config::features::image_gen::ImageGenConfiguration,
) -> Value {
    json!({
        "enabled": config.enabled,
        "base_url": config.base_url,
        "api_key_env": config.api_key_env,
        "has_api_key": !config.api_key.is_empty(),
        "model": config.model,
        "size": config.size,
        "quality": config.quality,
        "output_format": config.output_format,
        "n": config.n,
        "timeout_seconds": config.timeout_seconds,
    })
}

fn serialize_tts(config: &omnicrawl_config::features::tts::TtsConfiguration) -> Value {
    json!({
        "enabled": config.enabled,
        "model_dir": config.model_dir,
        "voice": config.voice,
        "auto_play": config.auto_play,
        "thread_count": config.thread_count,
        "device": config.device,
        "streaming": config.streaming,
        "output_dir": config.output_dir,
    })
}

/// 子代理进阶项的取值（键与顺序都取自控制器的常量表）。
fn subagent_advanced_values(
    config: &omnicrawl_config::features::subagents::SubAgentConfig,
) -> Value {
    let mut values = Map::new();
    for key in SUBAGENT_ADVANCED_SETTING_KEYS {
        let value = match key {
            "max_concurrency" => json!(config.max_concurrency),
            "max_tasks_per_batch" => json!(config.max_tasks_per_batch),
            "default_timeout_seconds" => json!(config.default_timeout_seconds),
            "model_request_concurrency" => json!(config.model_request_concurrency),
            "verify_command_timeout_seconds" => json!(config.verify_command_timeout_seconds),
            "task_retention_minutes" => json!(config.task_retention_minutes),
            _ => continue,
        };
        values.insert(key.to_string(), value);
    }
    Value::Object(values)
}

/// 把 `saved_path` 合进响应体（Python 的 `{**payload, "saved_path": ...}`）。
fn with_saved_path(mut payload: Value, path: &str) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert("saved_path".to_string(), Value::String(path.to_string()));
    }
    payload
}

/// 显式提供（非 null）的字段；与 Pydantic 的「Optional 字段传 null 等于保持原值」一致。
fn provided<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body.get(name).filter(|value| !value.is_null())
}

/// 工具开关名归一化；非法名称按 400 报（与 Python 的 `_as_invalid_setting` 同码）。
fn validate_tool_name(name: &str) -> Result<String, ApiError> {
    omnicrawl_config::features::tools::validate_tool_switch_name(name).map_err(invalid_setting)
}
