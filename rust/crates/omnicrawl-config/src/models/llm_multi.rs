//! 多模型 Profile / `active_model` 解析（对应 `omnicrawl/config/models/llm_multi.py`）。
//!
//! 判定与文案全部在内核；读文件与读环境变量经 [`ConfigEnvironment`] 注入。

use std::collections::BTreeMap;

use crate::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::models::llm::{
    read_optional_config_text, LlmConfig, DEFAULT_REASONING_EFFORT, DEFAULT_THINKING_TYPE,
};
use crate::models::model_store::{
    load_model_store, provider_from_protocol, CustomModelRecord, ModelDescriptor, ModelStore,
};
use crate::models::vision::{parse_native_vision, resolve_native_vision, NATIVE_VISION_FIELD};
use crate::models::ProviderProfile;
use crate::toml::{Table, Value};
use crate::value::python_str;

/// Python `value if isinstance(value, dict) else {}`：段不是对象时按空段处理。
fn loose_section(table: &Table, key: &str) -> Table {
    match table.get(key) {
        Some(Value::Table(inner)) => inner.clone(),
        _ => Table::new(),
    }
}

/// `llm.profiles` 或 `llm.active_model` 存在时按多模型配置处理。
pub fn is_multi_model_section(section: &Table) -> bool {
    matches!(section.get("profiles"), Some(Value::Table(_)))
        || matches!(section.get("active_model"), Some(Value::Table(_)))
}

/// `load_multi_model_llm_config` 各解析阶段之间的中间态。
#[derive(Debug, Clone, PartialEq)]
struct ModelSource {
    source: String,
    profile_id: String,
    protocol: String,
    model_id: String,
    catalog_key: String,
    context_window: i64,
    model_context_explicit: bool,
    max_output_tokens: i64,
    temperature: Option<f64>,
    native_vision: Option<bool>,
    provider_options: Table,
}

impl ModelSource {
    fn new(source: &str) -> Self {
        Self {
            source: source.to_string(),
            profile_id: String::new(),
            protocol: String::new(),
            model_id: String::new(),
            catalog_key: String::new(),
            context_window: 128_000,
            model_context_explicit: false,
            max_output_tokens: 0,
            temperature: None,
            native_vision: None,
            provider_options: Table::new(),
        }
    }

    /// 自定义模型条目的字段投影（`_resolve_model_source` 的两条 custom 分支共用）。
    fn apply_record(&mut self, record: &CustomModelRecord) {
        self.source = "custom".to_string();
        self.catalog_key = record.key.clone();
        self.profile_id = record.profile.clone();
        self.protocol = record.protocol.as_str().to_string();
        self.model_id = record.model_id.clone();
        if record.context_window_tokens > 0 {
            self.context_window = record.context_window_tokens;
        }
        self.model_context_explicit = record.context_window_tokens > 0;
        self.max_output_tokens = record.max_output_tokens;
        self.temperature = record.temperature;
        self.provider_options = record.provider_options.clone();
        self.native_vision = record.native_vision;
    }
}

/// 按 `OMNICRAWL_MODEL` / `active_model.source` 判定当前模型来源并填充中间态。
fn resolve_model_source(
    env_model: &str,
    env_profile: &str,
    active_raw: &Table,
    store: &ModelStore,
    source_default: &str,
) -> Result<ModelSource, ConfigError> {
    let mut state = ModelSource::new(source_default);
    if !env_model.is_empty() && !env_model.contains('/') {
        match store.resolve_alias(env_model)? {
            Some(record) => state.apply_record(record),
            None => {
                state.model_id = env_model.to_string();
                state.source = "detected".to_string();
            }
        }
    } else if !env_model.is_empty() && env_model.contains('/') {
        let (profile_id, model_id) = env_model.split_once('/').unwrap_or(("", ""));
        state.profile_id = profile_id.to_string();
        state.model_id = model_id.to_string();
        state.source = "detected".to_string();
        if !env_profile.is_empty() {
            state.profile_id = env_profile.to_string();
        }
    } else if state.source == "custom" {
        let catalog_key = python_str(active_raw.get("key")).trim().to_string();
        if catalog_key.is_empty() {
            return Err(ConfigError::new(
                "llm.active_model.source=custom 时必须提供 key。",
            ));
        }
        match store.resolve_alias(&catalog_key)? {
            None => {
                return Err(ConfigError::new(format!(
                    "active_model 引用了不存在的自定义模型 key：{catalog_key}。\
                     请检查 models.toml，或重新选择模型。"
                )))
            }
            Some(record) => state.apply_record(record),
        }
    } else {
        state.profile_id = python_str(active_raw.get("profile")).trim().to_string();
        state.model_id = python_str(active_raw.get("model_id")).trim().to_string();
        state.protocol = python_str(active_raw.get("protocol")).trim().to_string();
        if state.profile_id.is_empty() || state.model_id.is_empty() {
            return Err(ConfigError::new(
                "llm.active_model.source=detected 时必须提供 profile 与 model_id。",
            ));
        }
    }

    if !env_profile.is_empty() {
        state.profile_id = env_profile.to_string();
    }
    Ok(state)
}

/// 模型切换时按模型/渠道覆盖重建原生视觉开关；读取失败按未配置处理。
fn selection_native_vision(
    env: &ConfigEnvironment,
    catalog_key: &str,
    profile_id: &str,
) -> Option<bool> {
    resolve_native_vision(env, catalog_key, profile_id, None, None)
        .ok()
        .and_then(|setting| setting.value)
}

/// 校验 Profile 存在/启用，并推导最终 provider 与 protocol。
fn resolve_profile(
    profile_id: &str,
    protocol: &str,
    profiles_raw: &Table,
) -> Result<(String, String, Table), ConfigError> {
    let profile_data = match profiles_raw.get(profile_id) {
        Some(Value::Table(table)) => table.clone(),
        _ => {
            return Err(ConfigError::new(format!(
                "Profile 不存在或未启用：{profile_id}"
            )))
        }
    };
    if matches!(profile_data.get("enabled"), Some(Value::Boolean(false))) {
        return Err(ConfigError::new(format!("Profile 已禁用：{profile_id}")));
    }

    let provider = match profile_data.get("provider") {
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        _ => "openai".to_string(),
    };
    let mut protocol = protocol.to_string();
    if protocol.is_empty() {
        protocol = python_str(profile_data.get("default_protocol"))
            .trim()
            .to_string();
        if protocol.is_empty() {
            protocol = match provider.as_str() {
                "openai" => "openai_chat_completions".to_string(),
                "anthropic" => "anthropic_messages".to_string(),
                "gemini" => "gemini_generate_content".to_string(),
                _ => return Err(ConfigError::new(format!("未知 Provider：{provider}"))),
            };
        }
    }

    let protocol_provider = provider_from_protocol(&protocol)?;
    if protocol_provider.as_str() != provider {
        return Err(ConfigError::new(format!(
            "模型协议与 Profile Provider 不匹配：Profile {profile_id} 为 {provider}，\
             协议 {protocol} 属于 {}。",
            protocol_provider.as_str()
        )));
    }
    Ok((provider, protocol, profile_data))
}

/// 解析 Profile 凭据：api_key / base_url / user_agent。
fn resolve_credentials(
    env: &ConfigEnvironment,
    profile_id: &str,
    provider: &str,
    protocol: &str,
    profile_data: &Table,
) -> (String, String, String, String, ProviderProfile) {
    let api_key_env = python_str(profile_data.get("api_key_env"))
        .trim()
        .to_string();
    let api_key_plain = python_str(profile_data.get("api_key")).trim().to_string();
    let user_agent = python_str(profile_data.get("user_agent"))
        .trim()
        .to_string();
    let mut base_url = python_str(profile_data.get("base_url")).trim().to_string();
    let profile = ProviderProfile {
        id: profile_id.to_string(),
        provider: provider.to_string(),
        enabled: true,
        base_url: base_url.clone(),
        api_key: api_key_plain,
        api_key_env: api_key_env.clone(),
        user_agent: user_agent.clone(),
        default_protocol: protocol.to_string(),
        discovery_enabled: true,
        default_context_window_tokens: 0,
        provider_options: Table::new(),
        request_timeout_seconds: 180.0,
        request_retry_count: 5,
        discovery_timeout_seconds: 10.0,
    };
    let mut api_key = profile.resolve_api_key(env);
    if api_key.is_empty() && provider == "openai" {
        api_key = env.get_trimmed("OPENAI_API_KEY");
    }
    if base_url.is_empty() && provider == "openai" {
        base_url = env.get_trimmed("OPENAI_BASE_URL");
    }
    (api_key, base_url, api_key_env, user_agent, profile)
}

/// 应用 Profile 默认窗口与 `llm.defaults` 的覆盖规则。
fn apply_window_overrides(
    source: &str,
    mut context_window: i64,
    model_context_explicit: bool,
    profile_data: &Table,
    defaults: &Table,
) -> i64 {
    if let Some(Value::Integer(value)) = profile_data.get("default_context_window_tokens") {
        if *value > 0 && (context_window <= 0 || source == "detected") {
            context_window = *value;
        }
    }
    if let Some(Value::Integer(value)) = defaults.get("context_window_tokens") {
        // 设置面板会把 detected 模型的用户选择写进 llm.defaults；它必须覆盖 Profile 提供的
        // 发现回退值，否则重启后设置会悄然恢复成 Profile 默认值。
        if *value > 0
            && (source == "detected"
                || (!model_context_explicit && (context_window <= 0 || context_window == 128_000)))
        {
            context_window = *value;
        }
    }
    context_window
}

/// 从 `llm.profiles` + `active_model` + `models.toml` 构建当前 LLM 运行视图。
pub fn load_multi_model_llm_config(
    env: &ConfigEnvironment,
    llm_section: &Table,
) -> Result<LlmConfig, ConfigError> {
    let defaults = loose_section(llm_section, "defaults");
    let profiles_raw = loose_section(llm_section, "profiles");
    if profiles_raw.is_empty() {
        return Err(ConfigError::new("多模型配置缺少 llm.profiles。"));
    }
    let env_model = {
        let primary = env.get_trimmed("OMNICRAWL_MODEL");
        if primary.is_empty() {
            env.get_trimmed("OPENAI_MODEL")
        } else {
            primary
        }
    };
    let env_profile = env.get_trimmed("OMNICRAWL_PROFILE");
    let active_raw = loose_section(llm_section, "active_model");
    let store = load_model_store(env, None)?;
    let source = {
        let raw = python_str(active_raw.get("source"));
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            "custom".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let state = resolve_model_source(&env_model, &env_profile, &active_raw, &store, &source)?;
    let (provider, protocol, profile_data) =
        resolve_profile(&state.profile_id, &state.protocol, &profiles_raw)?;
    let (api_key, base_url, api_key_env, user_agent, _profile) =
        resolve_credentials(env, &state.profile_id, &provider, &protocol, &profile_data);

    // `{**defaults, **llm_section}`：llm_section 覆盖 defaults。
    let mut merged = defaults.clone();
    for (key, value) in llm_section {
        merged.insert(key.clone(), value.clone());
    }
    let reasoning_effort = read_optional_config_text(
        env,
        &merged,
        "reasoning_effort",
        "REASONING_EFFORT",
        DEFAULT_REASONING_EFFORT,
    )?;
    let thinking_type = read_optional_config_text(
        env,
        &merged,
        "thinking_type",
        "OPENAI_THINKING_TYPE",
        DEFAULT_THINKING_TYPE,
    )?;
    let context_window = apply_window_overrides(
        &state.source,
        state.context_window,
        state.model_context_explicit,
        &profile_data,
        &defaults,
    );

    let timeout = match defaults.get("request_timeout_seconds") {
        Some(Value::Integer(value)) if *value > 0 => *value,
        _ => 180,
    };
    let retries = match defaults.get("request_retry_count") {
        Some(Value::Integer(value)) if *value > 0 => *value,
        _ => 5,
    };

    // 模型原生视觉：模型条目覆盖 > 渠道 Profile 覆盖；未配置时按运行时能力。
    let native_vision = match state.native_vision {
        Some(flag) => Some(flag),
        None => parse_native_vision(profile_data.get(NATIVE_VISION_FIELD)),
    };

    LlmConfig {
        api_key,
        base_url,
        model: state.model_id,
        thinking_type,
        reasoning_effort,
        context_window_tokens: if context_window > 0 {
            context_window
        } else {
            128_000
        },
        max_output_tokens: state.max_output_tokens,
        temperature: state.temperature,
        native_vision,
        profile_id: state.profile_id,
        provider,
        protocol,
        catalog_key: state.catalog_key,
        model_source: if state.source == "detected" {
            "detected".to_string()
        } else {
            "custom".to_string()
        },
        api_key_env,
        user_agent,
        request_timeout_seconds: timeout,
        request_retry_count: retries,
        provider_options: state.provider_options,
        ..LlmConfig::with_environment(env)
    }
    .normalize()
}

/// 解析 `llm.profiles` 为 Profile 表（跳过禁用项与缺 Provider 项）。
pub fn parse_profiles(llm_section: &Table) -> BTreeMap<String, ProviderProfile> {
    let mut result = BTreeMap::new();
    let raw = match llm_section.get("profiles") {
        Some(Value::Table(table)) => table,
        _ => return result,
    };
    let defaults = get_section(llm_section, "defaults").unwrap_or_default();
    for (profile_id, item) in raw {
        let table = match item {
            Value::Table(table) => table,
            _ => continue,
        };
        if matches!(table.get("enabled"), Some(Value::Boolean(false))) {
            continue;
        }
        let provider = python_str(table.get("provider")).trim().to_string();
        if provider.is_empty() {
            continue;
        }
        let discovery = loose_section(table, "discovery");
        let discovery_enabled = match discovery.get("enabled") {
            None => true,
            Some(value) => crate::value::truthy(value),
        };
        let default_context_window_tokens = match table.get("default_context_window_tokens") {
            Some(Value::Integer(value)) => *value,
            _ => 0,
        };
        result.insert(
            profile_id.clone(),
            ProviderProfile {
                id: profile_id.clone(),
                provider,
                enabled: true,
                base_url: python_str(table.get("base_url")).trim().to_string(),
                api_key: python_str(table.get("api_key")).trim().to_string(),
                api_key_env: python_str(table.get("api_key_env")).trim().to_string(),
                user_agent: python_str(table.get("user_agent")).trim().to_string(),
                default_protocol: python_str(table.get("default_protocol")).trim().to_string(),
                discovery_enabled,
                default_context_window_tokens,
                provider_options: loose_section(table, "provider_options"),
                request_timeout_seconds: float_or(defaults.get("request_timeout_seconds"), 180.0),
                request_retry_count: int_or(defaults.get("request_retry_count"), 5),
                discovery_timeout_seconds: float_or(
                    defaults.get("discovery_timeout_seconds"),
                    10.0,
                ),
            },
        );
    }
    result
}

/// 把 LLM 运行视图转换为 Runtime 所需的 Profile + Descriptor。
pub fn llm_config_to_profile_and_descriptor(
    config: &LlmConfig,
) -> (ProviderProfile, ModelDescriptor) {
    let profile_id = if config.profile_id.is_empty() {
        "default-openai".to_string()
    } else {
        config.profile_id.clone()
    };
    let protocol_text = if config.protocol.is_empty() {
        "openai_chat_completions".to_string()
    } else {
        config.protocol.clone()
    };
    let provider_text = if config.provider.is_empty() {
        "openai".to_string()
    } else {
        config.provider.clone()
    };
    let profile = ProviderProfile {
        id: profile_id.clone(),
        provider: provider_text.clone(),
        enabled: true,
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
        api_key_env: config.api_key_env.clone(),
        user_agent: config.user_agent.clone(),
        default_protocol: protocol_text.clone(),
        discovery_enabled: true,
        default_context_window_tokens: 0,
        provider_options: config.provider_options.clone(),
        request_timeout_seconds: config.request_timeout_seconds as f64,
        request_retry_count: config.request_retry_count,
        discovery_timeout_seconds: 10.0,
    };
    let provider = omnicrawl_protocol::Provider::parse(&provider_text)
        .unwrap_or(omnicrawl_protocol::Provider::Openai);
    let protocol = omnicrawl_protocol::Protocol::parse(&protocol_text)
        .unwrap_or(omnicrawl_protocol::Protocol::OpenaiChatCompletions);
    let mut identity = omnicrawl_protocol::ModelIdentity::new(
        profile_id,
        provider,
        protocol,
        config.model.clone(),
    );
    identity.catalog_key = config.catalog_key.clone();
    let capabilities = omnicrawl_llm::ModelCapabilities {
        streaming: Some(true),
        tools: Some(true),
        parallel_tool_calls: Some(true),
        reasoning: Some(config.thinking_enabled()),
        context_window_tokens: config.context_window_tokens,
        max_output_tokens: config.max_output_tokens,
        ..Default::default()
    };
    let descriptor = ModelDescriptor {
        identity,
        display_name: if config.catalog_key.is_empty() {
            config.model.clone()
        } else {
            config.catalog_key.clone()
        },
        capabilities,
        context_window_tokens: config.context_window_tokens,
        max_output_tokens: config.max_output_tokens,
        temperature: config.temperature,
        aliases: Vec::new(),
        description: String::new(),
        tags: Vec::new(),
        provider_options: config.provider_options.clone(),
        source: if config.model_source == "legacy" {
            "migrated".to_string()
        } else {
            config.model_source.clone()
        },
        sort_order: 0,
        enabled: true,
    };
    (profile, descriptor)
}

/// 把 selection 解析为新的运行时 LLM 视图：自定义 key/alias、`profile/model_id` 或裸 model_id。
pub fn apply_model_selection(
    env: &ConfigEnvironment,
    config: &LlmConfig,
    selection: &str,
) -> Result<LlmConfig, ConfigError> {
    let token = selection.trim();
    if token.is_empty() {
        return Err(ConfigError::new("模型选择不能为空。"));
    }

    // models.toml 损坏必须显式失败，不可静默降级。
    let store = load_model_store(env, None)?;
    if let Some(record) = store.resolve_alias(token)? {
        return config_from_custom_record(env, config, record);
    }

    if let Some((profile_id, model_id)) = token.split_once('/') {
        let profile_id = profile_id.trim();
        let model_id = model_id.trim();
        if profile_id.is_empty() || model_id.is_empty() {
            return Err(ConfigError::new("profile/model_id 格式无效。"));
        }
        return config_from_profile_model(env, config, profile_id, model_id);
    }

    // 裸 model_id：只替换模型，保留当前 Profile 凭据与协议；不继承上一自定义模型的
    // max_output/temperature。
    Ok(LlmConfig {
        model: token.to_string(),
        max_output_tokens: 0,
        temperature: None,
        native_vision: selection_native_vision(env, "", &config.profile_id),
        protocol: if config.protocol.is_empty() {
            "openai_chat_completions".to_string()
        } else {
            config.protocol.clone()
        },
        catalog_key: String::new(),
        ..config.clone()
    })
}

fn config_from_custom_record(
    env: &ConfigEnvironment,
    config: &LlmConfig,
    record: &CustomModelRecord,
) -> Result<LlmConfig, ConfigError> {
    let profiles = profiles_from_disk(env);
    let profile = match profiles.get(&record.profile) {
        Some(profile) => profile,
        None => {
            return Err(ConfigError::new(format!(
                "自定义模型 {} 引用了不存在或已禁用的 Profile：{}",
                record.key, record.profile
            )))
        }
    };
    let protocol_provider = provider_from_protocol(record.protocol.as_str())?;
    if protocol_provider
        != omnicrawl_protocol::Provider::parse(&profile.provider)
            .unwrap_or(omnicrawl_protocol::Provider::Openai)
    {
        return Err(ConfigError::new(format!(
            "自定义模型 {} 的协议 {} 属于 {}，但 Profile {} 为 {}。",
            record.key,
            record.protocol.as_str(),
            protocol_provider.as_str(),
            record.profile,
            profile.provider
        )));
    }

    let api_key = profile.resolve_api_key(env);
    if api_key.is_empty() {
        return Err(ConfigError::new(format!(
            "Profile {} 缺少 API Key；不同模型渠道不会复用当前模型凭据。",
            profile.id
        )));
    }
    let context_window_tokens = if record.context_window_tokens > 0 {
        record.context_window_tokens
    } else if profile.default_context_window_tokens > 0 {
        profile.default_context_window_tokens
    } else {
        config.context_window_tokens
    };
    Ok(LlmConfig {
        api_key,
        base_url: profile.base_url.clone(),
        model: record.model_id.clone(),
        context_window_tokens,
        max_output_tokens: record.max_output_tokens,
        temperature: record.temperature,
        native_vision: selection_native_vision(env, &record.key, &record.profile),
        profile_id: profile.id.clone(),
        provider: profile.provider.clone(),
        protocol: if record.protocol.as_str().is_empty() {
            profile.resolve_protocol("")?.as_str().to_string()
        } else {
            record.protocol.as_str().to_string()
        },
        catalog_key: record.key.clone(),
        model_source: "custom".to_string(),
        api_key_env: profile.api_key_env.clone(),
        user_agent: profile.user_agent.clone(),
        request_timeout_seconds: if profile.request_timeout_seconds > 0.0 {
            profile.request_timeout_seconds as i64
        } else {
            config.request_timeout_seconds
        },
        request_retry_count: if profile.request_retry_count > 0 {
            profile.request_retry_count
        } else {
            config.request_retry_count
        },
        provider_options: if !record.provider_options.is_empty() {
            record.provider_options.clone()
        } else {
            profile.provider_options.clone()
        },
        ..config.clone()
    })
}

fn config_from_profile_model(
    env: &ConfigEnvironment,
    config: &LlmConfig,
    profile_id: &str,
    model_id: &str,
) -> Result<LlmConfig, ConfigError> {
    let profiles = profiles_from_disk(env);
    let Some(profile) = profiles.get(profile_id) else {
        // 多模型配置存在但 profile 缺失：直接失败，避免静默回落到错误凭据。
        if !profiles.is_empty() {
            return Err(ConfigError::new(format!(
                "Profile 不存在或未启用：{profile_id}"
            )));
        }
        return Ok(LlmConfig {
            model: model_id.to_string(),
            max_output_tokens: 0,
            temperature: None,
            native_vision: selection_native_vision(env, "", profile_id),
            profile_id: profile_id.to_string(),
            provider: if config.provider.is_empty() {
                "openai".to_string()
            } else {
                config.provider.clone()
            },
            protocol: if config.protocol.is_empty() {
                "openai_chat_completions".to_string()
            } else {
                config.protocol.clone()
            },
            catalog_key: String::new(),
            model_source: "detected".to_string(),
            ..config.clone()
        });
    };

    let api_key = profile.resolve_api_key(env);
    if api_key.is_empty() {
        return Err(ConfigError::new(format!(
            "Profile {} 缺少 API Key；不同模型渠道不会复用当前模型凭据。",
            profile.id
        )));
    }
    let protocol = {
        let resolved = profile.resolve_protocol("")?.as_str().to_string();
        if !resolved.is_empty() {
            resolved
        } else if !config.protocol.is_empty() {
            config.protocol.clone()
        } else {
            "openai_chat_completions".to_string()
        }
    };
    Ok(LlmConfig {
        api_key,
        base_url: profile.base_url.clone(),
        model: model_id.to_string(),
        context_window_tokens: if profile.default_context_window_tokens > 0 {
            profile.default_context_window_tokens
        } else {
            config.context_window_tokens
        },
        max_output_tokens: 0,
        temperature: None,
        native_vision: selection_native_vision(env, "", &profile.id),
        profile_id: profile.id.clone(),
        provider: profile.provider.clone(),
        protocol,
        catalog_key: String::new(),
        model_source: "detected".to_string(),
        api_key_env: profile.api_key_env.clone(),
        user_agent: profile.user_agent.clone(),
        request_timeout_seconds: if profile.request_timeout_seconds > 0.0 {
            profile.request_timeout_seconds as i64
        } else {
            config.request_timeout_seconds
        },
        request_retry_count: if profile.request_retry_count > 0 {
            profile.request_retry_count
        } else {
            config.request_retry_count
        },
        provider_options: profile.provider_options.clone(),
        ..config.clone()
    })
}

/// 读盘上的 `llm.profiles`；任何读取/解析失败都按「没有 Profile」处理。
fn profiles_from_disk(env: &ConfigEnvironment) -> BTreeMap<String, ProviderProfile> {
    let Ok(data) = load_config_data(env, None) else {
        return BTreeMap::new();
    };
    let Ok(section) = get_section(&data, "llm") else {
        return BTreeMap::new();
    };
    parse_profiles(&section)
}

fn int_or(value: Option<&Value>, default: i64) -> i64 {
    match value {
        Some(Value::Integer(number)) => *number,
        Some(Value::Float(number)) => *number as i64,
        _ => default,
    }
}

fn float_or(value: Option<&Value>, default: f64) -> f64 {
    match value {
        Some(Value::Integer(number)) => *number as f64,
        Some(Value::Float(number)) => *number,
        _ => default,
    }
}
