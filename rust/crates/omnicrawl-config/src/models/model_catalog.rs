//! 模型目录（对应 `omnicrawl/config/models/model_catalog.py`）。
//!
//! 双列目录：`custom`（`models.toml` 里启用的条目）与 `detected`（各 Profile 的
//! `/models` 发现结果）。网络、时钟与发现缓存都由端口注入——内核只保留聚合、
//! 缓存 TTL/LRU 与文案判定。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use omnicrawl_llm::{DiscoveryResult, DiscoveryStatus, ModelCapabilities};
use omnicrawl_protocol::{ModelIdentity, Protocol, Provider};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::models::llm::{load_llm_config, save_active_model_ref, ActiveModelRef, LlmConfig};
use crate::models::llm_multi::parse_profiles;
use crate::models::model_store::{
    load_model_store, provider_from_protocol, CustomModelRecord, ModelDescriptor,
};
use crate::models::ProviderProfile;
use crate::toml::{Table, Value};

pub const MODEL_LIST_TIMEOUT_SECONDS: f64 = 10.0;
pub const MAX_MODEL_LIST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_MODELS_PER_PROFILE: usize = 500;
pub const DEFAULT_DISCOVERY_CACHE_TTL_SECONDS: f64 = 300.0;
/// 条目上限：渠道 / 自定义模型目录频繁变化时不让发现缓存无界增长。
pub const MAX_DISCOVERY_CACHE_ENTRIES: usize = 32;

/// 可供 TUI 或 API 客户端展示和切换的模型项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
    pub provider: String,
}

impl ModelOption {
    pub fn to_ui_table(&self) -> Table {
        let mut table = Table::new();
        table.insert("id".to_string(), Value::String(self.id.clone()));
        table.insert("name".to_string(), Value::String(self.name.clone()));
        table.insert("provider".to_string(), Value::String(self.provider.clone()));
        table
    }
}

/// 目录中的一条模型（custom / detected / current_missing）。
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogModel {
    pub source: String,
    pub key: String,
    pub profile_id: String,
    pub provider: String,
    pub protocol: String,
    pub model_id: String,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    pub context_window_tokens: i64,
    pub availability: String,
    pub matched_custom_key: String,
    pub diagnostic: String,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub sort_order: i64,
}

impl CatalogModel {
    pub fn triple(&self) -> (String, String, String) {
        (
            self.profile_id.clone(),
            self.protocol.clone(),
            self.model_id.clone(),
        )
    }

    pub fn to_option(&self) -> ModelOption {
        let option_id = if self.source == "custom" && !self.key.is_empty() {
            self.key.clone()
        } else {
            self.model_id.clone()
        };
        ModelOption {
            id: option_id.clone(),
            name: if self.display_name.is_empty() {
                option_id
            } else {
                self.display_name.clone()
            },
            provider: if self.provider.is_empty() {
                detect_model_provider(&self.model_id).to_string()
            } else {
                self.provider.clone()
            },
        }
    }
}

/// 发现缓存：按 Profile 保序（最近使用在末尾），只缓存成功结果。
#[derive(Debug, Default)]
pub struct DiscoveryCache {
    entries: Mutex<Vec<(String, DiscoveryResult, Instant)>>,
}

impl DiscoveryCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&self) {
        lock_entries(&self.entries).clear();
    }

    fn get(
        &self,
        profile_id: &str,
        refresh: bool,
        now: Instant,
        ttl: f64,
    ) -> Option<DiscoveryResult> {
        if refresh {
            return None;
        }
        let mut entries = lock_entries(&self.entries);
        let index = entries
            .iter()
            .position(|(id, _, _)| id.as_str() == profile_id)?;
        let fresh = now
            .saturating_duration_since(entries[index].2)
            .as_secs_f64()
            < ttl;
        if !fresh {
            return None;
        }
        let entry = entries.remove(index);
        let result = entry.1.clone();
        entries.push(entry);
        Some(result)
    }

    fn store(&self, profile_id: &str, result: &DiscoveryResult, now: Instant) {
        let mut entries = lock_entries(&self.entries);
        if result.status == DiscoveryStatus::Ok {
            entries.retain(|(id, _, _)| id.as_str() != profile_id);
            entries.push((profile_id.to_string(), result.clone(), now));
            while entries.len() > MAX_DISCOVERY_CACHE_ENTRIES {
                entries.remove(0);
            }
        } else {
            // 网络或网关故障通常是短暂的；缓存失败会让服务恢复后仍持续展示旧诊断。
            entries.retain(|(id, _, _)| id.as_str() != profile_id);
        }
    }
}

fn lock_entries(
    entries: &Mutex<Vec<(String, DiscoveryResult, Instant)>>,
) -> std::sync::MutexGuard<'_, Vec<(String, DiscoveryResult, Instant)>> {
    entries.lock().unwrap_or_else(|error| error.into_inner())
}

/// `/models` 探测请求（由内核构造，端口只负责发送）。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelListRequest {
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub timeout_seconds: f64,
}

/// 探测失败的分类（对齐 Python 的三种异常分支）。
#[derive(Debug, Clone, PartialEq)]
pub enum ModelListFailure {
    /// `urllib.error.HTTPError`：带状态码的失败响应。
    Http { status: Option<i64> },
    /// `urllib.error.URLError` / 超时。
    Connect { message: String },
    /// 其他 `OSError`。
    Io { message: String },
}

/// 探测结果：正文（最多 `MAX_MODEL_LIST_BYTES + 1` 字节）或失败。
#[derive(Debug, Clone, PartialEq)]
pub enum ModelListOutcome {
    Body(Vec<u8>),
    Failure(ModelListFailure),
}

/// 一次 `/models` 请求的执行端口。
pub type ModelListFetch<'a> = &'a dyn Fn(&ModelListRequest) -> ModelListOutcome;

/// 一次 Profile 发现的执行端口（`adapter.discover_models`）。
pub type DiscoverFn<'a> = &'a dyn Fn(&ProviderProfile, Protocol, f64) -> DiscoveryResult;

/// 目录构建需要的外部能力。
pub struct CatalogPorts<'a> {
    pub discover: DiscoverFn<'a>,
    pub cache: &'a DiscoveryCache,
}

/// 目录构建参数。
#[derive(Debug, Clone)]
pub struct CatalogRequest<'a> {
    pub config: Option<&'a LlmConfig>,
    pub refresh: bool,
    pub timeout_seconds: f64,
    pub include_custom: bool,
    pub include_detected: bool,
    pub config_data: Option<&'a Table>,
}

impl Default for CatalogRequest<'_> {
    fn default() -> Self {
        Self {
            config: None,
            refresh: false,
            timeout_seconds: MODEL_LIST_TIMEOUT_SECONDS,
            include_custom: true,
            include_detected: true,
            config_data: None,
        }
    }
}

/// 双列目录结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    pub current: Table,
    pub custom: Vec<CatalogModel>,
    pub detected: Vec<CatalogModel>,
    pub diagnostics: Vec<Table>,
}

/// 从当前 `llm.base_url` 的 OpenAI 兼容 `/models` 接口检测模型列表。
pub fn detect_model_options(
    config: &LlmConfig,
    timeout_seconds: f64,
    fetch: ModelListFetch<'_>,
) -> Result<Vec<ModelOption>, ConfigError> {
    let endpoint = models_endpoint(&config.base_url)?;
    let request = ModelListRequest {
        endpoint,
        headers: vec![
            ("Accept".to_string(), "application/json".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {}", config.api_key),
            ),
            ("User-Agent".to_string(), "ai-voice-agent/1.0".to_string()),
        ],
        timeout_seconds,
    };

    let raw_body = match fetch(&request) {
        ModelListOutcome::Body(body) => body,
        ModelListOutcome::Failure(ModelListFailure::Http { status }) => {
            return Err(ConfigError::new(format_http_error(status)))
        }
        ModelListOutcome::Failure(ModelListFailure::Connect { message }) => {
            return Err(ConfigError::new(format!("无法连接模型列表接口：{message}")))
        }
        ModelListOutcome::Failure(ModelListFailure::Io { message }) => {
            return Err(ConfigError::new(format!("读取模型列表失败：{message}")))
        }
    };

    if raw_body.len() > MAX_MODEL_LIST_BYTES {
        return Err(ConfigError::new("模型列表响应过大，已拒绝解析。"));
    }

    let bytes = raw_body
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&raw_body);
    let text = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => text,
        Err(_) => return Err(ConfigError::new("模型列表接口返回的内容不是 UTF-8 JSON。")),
    };
    let payload: serde_json::Value = match serde_json::from_str(&text) {
        Ok(payload) => payload,
        Err(error) => {
            return Err(ConfigError::new(format!(
                "模型列表接口返回的内容不是合法 JSON：第 {} 行。",
                error.line()
            )))
        }
    };

    let model_ids = extract_model_ids(&payload);
    if model_ids.is_empty() {
        let error_message = extract_error_message(&payload);
        if !error_message.is_empty() {
            return Err(ConfigError::new(format!(
                "模型列表接口返回错误：{error_message}"
            )));
        }
        return Err(ConfigError::new("模型列表接口没有返回可用模型。"));
    }

    Ok(model_ids
        .into_iter()
        .map(|model_id| ModelOption {
            name: model_id.clone(),
            provider: detect_model_provider(&model_id).to_string(),
            id: model_id,
        })
        .collect())
}

/// 构建双列目录：custom / detected / diagnostics / current。
pub fn build_catalog(
    env: &ConfigEnvironment,
    request: &CatalogRequest<'_>,
    ports: &CatalogPorts<'_>,
) -> Result<Catalog, ConfigError> {
    let owned_config = if request.config.is_none() {
        load_llm_config(env).ok()
    } else {
        None
    };
    let config: Option<&LlmConfig> = request.config.or(owned_config.as_ref());

    let owned_data = if request.config_data.is_none() {
        Some(load_config_data(env, None)?)
    } else {
        None
    };
    let data: &Table = match request.config_data {
        Some(data) => data,
        None => owned_data.as_ref().expect("已加载配置数据"),
    };
    let llm_section = get_section(data, "llm")?;

    let mut profiles = parse_profiles(&llm_section);
    if profiles.is_empty() {
        if let Some(config) = config {
            let id = if config.profile_id.is_empty() {
                "default-openai".to_string()
            } else {
                config.profile_id.clone()
            };
            profiles.insert(
                id.clone(),
                ProviderProfile {
                    id,
                    provider: if config.provider.is_empty() {
                        "openai".to_string()
                    } else {
                        config.provider.clone()
                    },
                    enabled: true,
                    base_url: config.base_url.clone(),
                    api_key: config.api_key.clone(),
                    api_key_env: config.api_key_env.clone(),
                    user_agent: config.user_agent.clone(),
                    default_protocol: if config.protocol.is_empty() {
                        "openai_chat_completions".to_string()
                    } else {
                        config.protocol.clone()
                    },
                    discovery_enabled: true,
                    default_context_window_tokens: 0,
                    provider_options: Table::new(),
                    request_timeout_seconds: 180.0,
                    request_retry_count: 5,
                    discovery_timeout_seconds: request.timeout_seconds,
                },
            );
        }
    }

    let mut custom_items: Vec<CatalogModel> = Vec::new();
    let mut custom_index: Vec<((String, String, String), String)> = Vec::new();
    if request.include_custom {
        let store = load_model_store(env, None)?;
        for record in &store.models {
            if !record.enabled {
                continue;
            }
            let provider = match profiles.get(&record.profile) {
                Some(profile) => profile.provider.clone(),
                None => provider_from_protocol(record.protocol.as_str())?
                    .as_str()
                    .to_string(),
            };
            let item = catalog_model_from_record(record, provider);
            custom_index.push((item.triple(), record.key.clone()));
            custom_items.push(item);
        }
    }

    let mut detected_items: Vec<CatalogModel> = Vec::new();
    let mut diagnostics: Vec<Table> = Vec::new();
    if request.include_detected {
        for (profile_id, profile) in &profiles {
            if !profile.discovery_enabled {
                continue;
            }
            let timeout_seconds = if request.timeout_seconds != 0.0 {
                request.timeout_seconds
            } else {
                profile.discovery_timeout_seconds
            };
            let result = discover_for_profile(
                profile,
                request.refresh,
                timeout_seconds,
                ports.discover,
                ports.cache,
            );
            if result.status != DiscoveryStatus::Ok {
                let mut diagnostic = Table::new();
                diagnostic.insert("profile".to_string(), Value::String(profile_id.clone()));
                diagnostic.insert(
                    "status".to_string(),
                    Value::String(discovery_status_text(result.status).to_string()),
                );
                diagnostic.insert(
                    "message".to_string(),
                    Value::String(if result.message.is_empty() {
                        "模型列表发现失败，自定义模型仍可使用。".to_string()
                    } else {
                        result.message.clone()
                    }),
                );
                diagnostics.push(diagnostic);
                continue;
            }
            for model in result.models.iter().take(MAX_MODELS_PER_PROFILE) {
                let triple = (
                    model.profile_id.clone(),
                    model.protocol.as_str().to_string(),
                    model.model_id.clone(),
                );
                let matched = custom_index
                    .iter()
                    .find(|(key, _)| key == &triple)
                    .map(|(_, key)| key.clone())
                    .unwrap_or_default();
                if !matched.is_empty() {
                    for custom in custom_items.iter_mut() {
                        if custom.key == matched {
                            custom.availability = "available".to_string();
                            custom.matched_custom_key = custom.key.clone();
                            break;
                        }
                    }
                }
                let display_name = if model.display_name.is_empty() {
                    model.model_id.clone()
                } else {
                    model.display_name.clone()
                };
                detected_items.push(CatalogModel {
                    source: "detected".to_string(),
                    key: format!("{}/{}", model.profile_id, model.model_id),
                    profile_id: model.profile_id.clone(),
                    provider: model.provider.clone(),
                    protocol: model.protocol.as_str().to_string(),
                    model_id: model.model_id.clone(),
                    display_name,
                    capabilities: model.capabilities,
                    // Python：`model.context_window_tokens or profile.default_context_window_tokens`。
                    context_window_tokens: if model.context_window_tokens != 0 {
                        model.context_window_tokens
                    } else {
                        profile.default_context_window_tokens
                    },
                    availability: "available".to_string(),
                    matched_custom_key: matched,
                    diagnostic: String::new(),
                    tags: Vec::new(),
                    aliases: Vec::new(),
                    sort_order: 0,
                });
            }
        }
    }

    Ok(Catalog {
        current: current_catalog_view(config, &custom_items, &detected_items),
        custom: custom_items,
        detected: detected_items,
        diagnostics,
    })
}

/// 按常见模型名前缀给 UI 一个轻量 provider 分类。
pub fn detect_model_provider(model_id: &str) -> &'static str {
    let normalized = model_id.trim().to_lowercase();
    let starts_with_any =
        |prefixes: &[&str]| prefixes.iter().any(|item| normalized.starts_with(item));
    if starts_with_any(&["gpt-", "chatgpt-", "o1", "o3", "o4"]) {
        return "gpt";
    }
    if starts_with_any(&["claude-", "anthropic/claude"]) {
        return "claude";
    }
    if starts_with_any(&["gemini", "models/gemini"]) {
        return "gemini";
    }
    if starts_with_any(&["deepseek", "deepseek/"]) {
        return "deepseek";
    }
    if starts_with_any(&["qwen", "qwen/", "qwq"]) {
        return "qwen";
    }
    if starts_with_any(&["glm", "chatglm", "zhipu"]) {
        return "glm";
    }
    "other"
}

/// 把当前模型补进候选列表头部（列表里没有时）。
pub fn ensure_current_model_option(
    options: &[ModelOption],
    current_model: &str,
) -> Vec<ModelOption> {
    let current = current_model.trim();
    let mut result = options.to_vec();
    if current.is_empty() || result.iter().any(|option| option.id == current) {
        return result;
    }
    let mut head = vec![ModelOption {
        id: current.to_string(),
        name: current.to_string(),
        provider: detect_model_provider(current).to_string(),
    }];
    head.append(&mut result);
    head
}

/// 目录项 → UI 键值对列表。
pub fn model_options_to_ui(options: &[ModelOption]) -> Vec<Table> {
    options.iter().map(ModelOption::to_ui_table).collect()
}

/// 渲染候选列表（` *` 标记当前模型，超出 `limit` 时给出剩余条数）。
pub fn format_model_options(options: &[ModelOption], current_model: &str, limit: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (index, option) in options.iter().take(limit).enumerate() {
        let marker = if option.id == current_model { " *" } else { "" };
        lines.push(format!("{:>2}. {}{}", index + 1, option.id, marker));
    }
    let remaining = options.len().saturating_sub(limit);
    if remaining > 0 {
        lines.push(format!("... 还有 {remaining} 个模型未显示。"));
    }
    lines.join("\n")
}

/// 兼容旧接口：把当前模型写回配置。
pub fn save_llm_model(
    env: &ConfigEnvironment,
    model_id: &str,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let model = model_id.trim();
    if model.is_empty() {
        return Err(ConfigError::new("模型 ID 不能为空。"));
    }

    let mut data = load_config_data(env, config_path)?;
    let mut llm_section = get_section(&data, "llm")?;
    if !matches!(llm_section.get("profiles"), Some(Value::Table(_))) {
        llm_section.insert("model".to_string(), Value::String(model.to_string()));
        data.insert("llm".to_string(), Value::Table(llm_section));
        return save_config_data(env, &data, config_path);
    }

    let store = load_model_store(env, None)?;
    if let Some(record) = store.resolve_alias(model)? {
        let reference = ActiveModelRef {
            source: "custom".to_string(),
            key: record.key.clone(),
            profile: String::new(),
            model_id: record.model_id.clone(),
            protocol: String::new(),
        };
        return save_active_model_ref(env, &reference, config_path);
    }

    let mut profile_id = String::new();
    let mut model_name = model.to_string();
    if let Some((head, tail)) = model.split_once('/') {
        profile_id = head.to_string();
        model_name = tail.to_string();
    }
    let profiles = parse_profiles(&llm_section);
    if profile_id.is_empty() {
        let active = llm_section
            .get("active_model")
            .and_then(|value| value.as_table());
        profile_id = active
            .map(|active| {
                crate::value::python_str(active.get("profile"))
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        if profile_id.is_empty() {
            if let Some(first) = profiles.keys().next() {
                profile_id = first.clone();
            }
        }
    }
    let Some(profile) = profiles.get(&profile_id) else {
        return Err(ConfigError::new(format!(
            "无法解析 Profile：{}",
            if profile_id.is_empty() {
                "(空)".to_string()
            } else {
                profile_id
            }
        )));
    };
    let protocol = profile.resolve_protocol("")?;
    let reference = ActiveModelRef {
        source: "detected".to_string(),
        key: String::new(),
        profile: profile_id,
        model_id: model_name,
        protocol: protocol.as_str().to_string(),
    };
    save_active_model_ref(env, &reference, config_path)
}

/// 是否由环境变量接管模型选择。
///
/// 环境变量接管已移除，此函数恒为假，保留是为了让调用点继续自述语义。
pub fn model_env_override_active(_env: &ConfigEnvironment) -> bool {
    false
}

/// 目录项 → 可构建运行时的模型描述。
pub fn catalog_model_to_descriptor(item: &CatalogModel) -> Result<ModelDescriptor, ConfigError> {
    let provider = Provider::parse(&item.provider)
        .ok_or_else(|| ConfigError::new(format!("未知 Provider：{}", item.provider)))?;
    let protocol = Protocol::parse(&item.protocol)
        .ok_or_else(|| ConfigError::new(format!("不支持的协议：{}", item.protocol)))?;
    let mut identity = ModelIdentity::new(
        item.profile_id.clone(),
        provider,
        protocol,
        item.model_id.clone(),
    );
    if item.source == "custom" {
        identity.catalog_key = item.key.clone();
    }
    Ok(ModelDescriptor {
        identity,
        display_name: item.display_name.clone(),
        capabilities: item.capabilities,
        context_window_tokens: item.context_window_tokens,
        max_output_tokens: item.capabilities.max_output_tokens,
        temperature: None,
        aliases: item.aliases.clone(),
        description: String::new(),
        tags: item.tags.clone(),
        provider_options: Table::new(),
        source: item.source.clone(),
        sort_order: item.sort_order,
        enabled: true,
    })
}

fn catalog_model_from_record(record: &CustomModelRecord, provider: String) -> CatalogModel {
    CatalogModel {
        source: "custom".to_string(),
        key: record.key.clone(),
        profile_id: record.profile.clone(),
        provider,
        protocol: record.protocol.as_str().to_string(),
        model_id: record.model_id.clone(),
        display_name: record.display_name.clone(),
        capabilities: record.capabilities,
        context_window_tokens: record.context_window_tokens,
        availability: "unknown".to_string(),
        matched_custom_key: String::new(),
        diagnostic: String::new(),
        tags: record.tags.clone(),
        aliases: record.aliases.clone(),
        sort_order: record.sort_order,
    }
}

fn discover_for_profile(
    profile: &ProviderProfile,
    refresh: bool,
    timeout_seconds: f64,
    discover: DiscoverFn<'_>,
    cache: &DiscoveryCache,
) -> DiscoveryResult {
    let now = Instant::now();
    if let Some(cached) = cache.get(
        &profile.id,
        refresh,
        now,
        DEFAULT_DISCOVERY_CACHE_TTL_SECONDS,
    ) {
        return cached;
    }
    let result = match profile.resolve_protocol("") {
        Ok(protocol) => discover(profile, protocol, timeout_seconds),
        Err(error) => DiscoveryResult {
            profile_id: profile.id.clone(),
            status: DiscoveryStatus::Unavailable,
            message: format!("模型列表发现失败：{}", error.message()),
            models: Vec::new(),
        },
    };
    cache.store(&profile.id, &result, now);
    result
}

fn discovery_status_text(status: DiscoveryStatus) -> &'static str {
    match status {
        DiscoveryStatus::Ok => "ok",
        DiscoveryStatus::Unavailable => "unavailable",
        DiscoveryStatus::Unsupported => "unsupported",
    }
}

fn current_catalog_view(
    config: Option<&LlmConfig>,
    custom_items: &[CatalogModel],
    detected_items: &[CatalogModel],
) -> Table {
    let mut view = Table::new();
    let Some(config) = config else {
        return view;
    };
    let mut source = if config.model_source == "detected" {
        "detected".to_string()
    } else {
        "custom".to_string()
    };
    if !config.catalog_key.is_empty() {
        source = "custom".to_string();
    }
    view.insert("source".to_string(), Value::String(source));
    view.insert("key".to_string(), Value::String(config.catalog_key.clone()));
    view.insert(
        "profile".to_string(),
        Value::String(config.profile_id.clone()),
    );
    view.insert(
        "protocol".to_string(),
        Value::String(config.protocol.clone()),
    );
    view.insert("model_id".to_string(), Value::String(config.model.clone()));

    let triple = (
        config.profile_id.clone(),
        config.protocol.clone(),
        config.model.clone(),
    );
    let known = custom_items
        .iter()
        .chain(detected_items.iter())
        .any(|item| item.triple() == triple);
    if !config.profile_id.is_empty() && !known {
        let matched_current = !config.catalog_key.is_empty()
            && custom_items
                .iter()
                .any(|item| item.key == config.catalog_key);
        if !matched_current {
            view.insert("missing".to_string(), Value::Boolean(true));
        }
    }
    view
}

fn models_endpoint(base_url: &str) -> Result<String, ConfigError> {
    let base = base_url.trim();
    if base.is_empty() {
        return Err(ConfigError::new("缺少 llm.base_url，无法检测模型列表。"));
    }
    Ok(format!("{}/models", base.trim_end_matches('/')))
}

fn extract_model_ids(payload: &serde_json::Value) -> Vec<String> {
    let data = if payload.is_object() {
        payload.get("data").unwrap_or(&serde_json::Value::Null)
    } else {
        payload
    };
    let Some(items) = data.as_array() else {
        return Vec::new();
    };
    let mut seen: Vec<String> = Vec::new();
    let mut result: Vec<String> = Vec::new();
    for item in items {
        let model_id = read_model_id(item);
        if model_id.is_empty() || seen.contains(&model_id) {
            continue;
        }
        seen.push(model_id.clone());
        result.push(model_id);
    }
    result
}

fn read_model_id(item: &serde_json::Value) -> String {
    if let Some(text) = item.as_str() {
        return text.trim().to_string();
    }
    let Some(object) = item.as_object() else {
        return String::new();
    };
    for key in ["id", "model", "name"] {
        if let Some(text) = object.get(key).and_then(|value| value.as_str()) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

fn extract_error_message(payload: &serde_json::Value) -> String {
    let Some(object) = payload.as_object() else {
        return String::new();
    };
    let Some(error) = object.get("error") else {
        return String::new();
    };
    if let Some(nested) = error.as_object() {
        return nested
            .get("message")
            .and_then(|value| value.as_str())
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
    }
    error
        .as_str()
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn format_http_error(status: Option<i64>) -> String {
    match status {
        Some(401) => "模型列表接口鉴权失败（HTTP 401），请检查 API Key。".to_string(),
        Some(403) => "当前 API Key 没有读取模型列表的权限（HTTP 403）。".to_string(),
        Some(404) => {
            "模型列表接口不存在（HTTP 404），请检查 llm.base_url 是否指向 OpenAI 兼容 /v1 地址。"
                .to_string()
        }
        Some(429) => "模型列表接口触发限流（HTTP 429），请稍后重试。".to_string(),
        Some(code) if (500..=599).contains(&code) => {
            format!("模型列表服务暂时不可用（HTTP {code}）。")
        }
        Some(code) => format!("模型列表接口返回错误（HTTP {code}）。"),
        None => "模型列表接口返回错误。".to_string(),
    }
}
