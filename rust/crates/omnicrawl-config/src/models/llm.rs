//! LLM 配置模型、读写与规范化。
//!
//! 对应 `omnicrawl/config/models/llm.py`。环境默认值（`OPENAI_API_KEY` 等）由
//! [`LlmConfig::with_environment`] 提供，等价于 Python dataclass 的 `default_factory`。

use std::path::{Path, PathBuf};

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::models::llm_multi::{is_multi_model_section, load_multi_model_llm_config};
use crate::toml::{Table, Value};

pub const DEFAULT_THINKING_TYPE: &str = "disabled";
pub const DEFAULT_REASONING_EFFORT: &str = "";
pub const KNOWN_AVAILABLE_MODELS: [&str; 5] = [
    "qwen3.6-plus",
    "glm-5.1",
    "deepseek-v4-flash",
    "gpt-5.2",
    "gpt-5.4-mini",
];
pub const VALID_REASONING_EFFORTS: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: i64 = 128_000;
pub const DEFAULT_MAX_HISTORY_TURNS: i64 = 8;
pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: i64 = 180;
pub const DEFAULT_REQUEST_RETRY_COUNT: i64 = 5;
pub const DEFAULT_SYSTEM_PROMPT: &str = "你是一个通过语音和用户对话的中文 AI 助手。\
回答要自然、简洁、适合被朗读；遇到不确定内容要明确说明。";

/// 报错文案里列出的可选值（Python 是 `sorted([*VALID_REASONING_EFFORTS, "disabled"])`）。
const REASONING_EFFORT_ALLOWED: &str = "disabled, high, low, max, medium, none, xhigh";

const REASONING_EFFORT_ALIASES: [(&str, &str); 14] = [
    ("", ""),
    ("disabled", "disabled"),
    ("off", "disabled"),
    ("none", "none"),
    ("low", "low"),
    ("medium", "medium"),
    ("med", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
    ("x_high", "xhigh"),
    ("extra_high", "xhigh"),
    ("very_high", "xhigh"),
    ("max", "max"),
    ("maximum", "max"),
];

/// 面向 Agent / UI 的当前模型运行视图。
#[derive(Debug, Clone, PartialEq)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub thinking_type: String,
    pub reasoning_effort: String,
    pub context_window_tokens: i64,
    pub max_output_tokens: i64,
    /// 模型原生视觉：显式开关或「未配置」。
    pub native_vision: Option<bool>,
    pub temperature: Option<f64>,
    pub system_prompt: String,
    pub max_history_turns: i64,
    pub profile_id: String,
    pub provider: String,
    pub protocol: String,
    pub catalog_key: String,
    /// legacy | custom | detected
    pub model_source: String,
    /// Provider 能力声明：`prompt_cache=true` 时允许给非 GPT 系列也下发
    /// `prompt_cache_key`（内核 `should_send_prompt_cache_key` 的门禁）。
    ///
    /// 只由自定义模型条目的 `capabilities.prompt_cache` 填充；detected / legacy
    /// 路径保持 `None`（对应 Python `ModelCapabilities.prompt_cache` 未声明，
    /// 仅 GPT 系列会尝试）。
    pub prompt_cache: Option<bool>,
    pub api_key_env: String,
    pub user_agent: String,
    pub request_timeout_seconds: i64,
    pub request_retry_count: i64,
    pub provider_options: Table,
}

/// JSON 对象 → `provider_options` 的 TOML 表。
///
/// `provider_options` 在配置文件里是 TOML 表，而界面与协议交换的是 JSON；
/// 界面要把填好的值写回配置、或按 JSON 形状构造一份表来做等价比对时，用这里
/// 做一次平移，避免每一处各写一遍类型映射（日期没有 JSON 对应形状，落成字符串）。
pub fn provider_options_from_json(value: &serde_json::Value) -> Table {
    let serde_json::Value::Object(entries) = value else {
        return Table::new();
    };
    entries
        .iter()
        .map(|(key, entry)| (key.clone(), json_to_toml(entry)))
        .collect()
}

fn json_to_toml(value: &serde_json::Value) -> toml::Value {
    match value {
        serde_json::Value::Null => toml::Value::String(String::new()),
        serde_json::Value::Bool(flag) => toml::Value::Boolean(*flag),
        serde_json::Value::Number(number) => match number.as_i64() {
            Some(integer) => toml::Value::Integer(integer),
            None => toml::Value::Float(number.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(text) => toml::Value::String(text.clone()),
        serde_json::Value::Array(items) => {
            toml::Value::Array(items.iter().map(json_to_toml).collect())
        }
        serde_json::Value::Object(entries) => {
            let mut nested = Table::new();
            for (key, entry) in entries {
                nested.insert(key.clone(), json_to_toml(entry));
            }
            toml::Value::Table(nested)
        }
    }
}

impl LlmConfig {
    /// 空配置视图：所有字段取常量默认值，凭据一律来自配置段。
    pub fn with_environment(_env: &ConfigEnvironment) -> Self {
        Self {
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
            thinking_type: DEFAULT_THINKING_TYPE.to_string(),
            reasoning_effort: String::new(),
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            max_output_tokens: 0,
            native_vision: None,
            temperature: None,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            max_history_turns: DEFAULT_MAX_HISTORY_TURNS,
            profile_id: String::new(),
            provider: "openai".to_string(),
            protocol: "openai_chat_completions".to_string(),
            catalog_key: String::new(),
            model_source: "legacy".to_string(),
            prompt_cache: None,
            api_key_env: String::new(),
            user_agent: String::new(),
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
            request_retry_count: DEFAULT_REQUEST_RETRY_COUNT,
            provider_options: Table::new(),
        }
    }

    /// Python `LLMConfig.__post_init__`：就地归一化并校验。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        self.api_key = self.api_key.trim().to_string();
        self.base_url = self.base_url.trim().to_string();
        self.model = self.model.trim().to_string();
        self.user_agent = self.user_agent.trim().to_string();
        if self.user_agent.contains('\r') || self.user_agent.contains('\n') {
            return Err(ConfigError::new("配置项 llm.user_agent 不能包含换行。"));
        }
        let thinking = self.thinking_type.trim().to_lowercase();
        self.thinking_type = if thinking.is_empty() {
            DEFAULT_THINKING_TYPE.to_string()
        } else {
            thinking
        };
        self.reasoning_effort = normalize_reasoning_effort(&self.reasoning_effort)?.to_string();
        if self.context_window_tokens <= 0 {
            return Err(ConfigError::new(
                "配置项 llm.context_window_tokens 必须大于 0。",
            ));
        }
        if self.model_source == "legacy" {
            require_non_empty("api_key", &self.api_key)?;
            require_non_empty("base_url", &self.base_url)?;
            require_non_empty("model", &self.model)?;
        } else {
            if self.model.trim().is_empty() {
                return Err(ConfigError::new("缺少当前模型 model_id。"));
            }
            if self.api_key.trim().is_empty() {
                return Err(ConfigError::new(
                    "缺少 API Key，请在 Profile 中配置 api_key。",
                ));
            }
        }
        Ok(self)
    }

    /// 是否向 Provider 下发推理（思考）开关。
    pub fn thinking_enabled(&self) -> bool {
        if self.reasoning_effort == "none" || self.reasoning_effort == "disabled" {
            return false;
        }
        if !self.reasoning_effort.is_empty() {
            return true;
        }
        !(self.thinking_type.is_empty() || self.thinking_type == "disabled")
    }
}

/// 当前模型选择（写回 `llm.active_model` / `llm.model`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActiveModelRef {
    /// custom | detected
    pub source: String,
    pub key: String,
    pub profile: String,
    pub model_id: String,
    pub protocol: String,
}

impl ActiveModelRef {
    /// 对应 `ActiveModelRef.to_dict()`。
    pub fn to_table(&self) -> Table {
        let mut table = Table::new();
        if self.source == "custom" {
            table.insert("source".to_string(), Value::String("custom".to_string()));
            table.insert("key".to_string(), Value::String(self.key.clone()));
            return table;
        }
        table.insert("source".to_string(), Value::String("detected".to_string()));
        table.insert("profile".to_string(), Value::String(self.profile.clone()));
        table.insert("model_id".to_string(), Value::String(self.model_id.clone()));
        table.insert("protocol".to_string(), Value::String(self.protocol.clone()));
        table
    }
}

fn require_non_empty(key: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::new(format!(
            "缺少配置 llm.{key}，请在 config.toml 中填写 llm.{key}。"
        )));
    }
    Ok(())
}

/// 推理强度归一化；不认识的值直接报错（不静默降级）。
pub fn normalize_reasoning_effort(value: &str) -> Result<&'static str, ConfigError> {
    let normalized = value.trim().to_lowercase().replace(['-', ' '], "_");
    if let Some((_, effort)) = REASONING_EFFORT_ALIASES
        .iter()
        .find(|(alias, _)| *alias == normalized)
    {
        return Ok(effort);
    }
    Err(ConfigError::new(format!(
        "llm.reasoning_effort 仅支持 {REASONING_EFFORT_ALLOWED}，当前值：{value}。"
    )))
}

/// 从本地配置文件创建当前 LLM 运行视图。
pub fn load_llm_config(env: &ConfigEnvironment) -> Result<LlmConfig, ConfigError> {
    let data = load_config_data(env, None)?;
    let section = get_section(&data, "llm")?;
    if is_multi_model_section(&section) {
        return load_multi_model_llm_config(env, &section);
    }
    LlmConfig {
        api_key: read_required_config_text(&section, "api_key")?,
        base_url: read_required_config_text(&section, "base_url")?,
        model: read_required_config_text(&section, "model")?,
        thinking_type: read_optional_config_text(
            &section,
            "thinking_type",
            DEFAULT_THINKING_TYPE,
        )?,
        reasoning_effort: read_optional_config_text(
            &section,
            "reasoning_effort",
            DEFAULT_REASONING_EFFORT,
        )?,
        user_agent: read_optional_config_text(&section, "user_agent", "")?,
        context_window_tokens: read_context_window_tokens(&section, DEFAULT_CONTEXT_WINDOW_TOKENS)?,
        model_source: "legacy".to_string(),
        provider: "openai".to_string(),
        protocol: "openai_chat_completions".to_string(),
        ..LlmConfig::with_environment(env)
    }
    .normalize()
}

/// 把推理强度写回配置文件，并同步 `thinking_type`。
pub fn save_reasoning_effort(
    env: &ConfigEnvironment,
    effort: &str,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let normalized = normalize_reasoning_effort(effort)?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "llm")?;
    let thinking_type = if normalized == "none" || normalized == "disabled" {
        "disabled"
    } else {
        "enabled"
    };
    if is_multi_model_section(&section) {
        // Python：defaults 不是对象时按空段处理。
        let mut defaults = match section.get("defaults") {
            Some(Value::Table(inner)) => inner.clone(),
            _ => Table::new(),
        };
        defaults.insert(
            "reasoning_effort".to_string(),
            Value::String(normalized.to_string()),
        );
        defaults.insert(
            "thinking_type".to_string(),
            Value::String(thinking_type.to_string()),
        );
        section.insert("defaults".to_string(), Value::Table(defaults));
    } else {
        section.insert(
            "reasoning_effort".to_string(),
            Value::String(normalized.to_string()),
        );
        section.insert(
            "thinking_type".to_string(),
            Value::String(thinking_type.to_string()),
        );
    }
    data.insert("llm".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 持久化当前模型选择。
pub fn save_active_model_ref(
    env: &ConfigEnvironment,
    reference: &ActiveModelRef,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, "llm")?;
    if !is_multi_model_section(&section) {
        let value = if reference.model_id.is_empty() {
            reference.key.clone()
        } else {
            reference.model_id.clone()
        };
        section.insert("model".to_string(), Value::String(value));
    } else {
        section.insert(
            "active_model".to_string(),
            Value::Table(reference.to_table()),
        );
    }
    data.insert("llm".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 必填文本：只读配置项。
pub fn read_required_config_text(section: &Table, key: &str) -> Result<String, ConfigError> {
    let missing = || {
        ConfigError::new(format!("缺少配置 llm.{key}，请在配置文件中填写 llm.{key}。"))
    };
    match section.get(key) {
        None => Err(missing()),
        Some(Value::String(text)) if text.trim().is_empty() => Err(missing()),
        Some(Value::String(text)) => Ok(text.trim().to_string()),
        Some(_) => Err(ConfigError::new(format!("配置项 llm.{key} 必须是字符串。"))),
    }
}

/// 可选文本：空值回落默认。
pub fn read_optional_config_text(
    section: &Table,
    key: &str,
    default: &str,
) -> Result<String, ConfigError> {
    match section.get(key) {
        None => Ok(default.to_string()),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            Ok(if trimmed.is_empty() {
                default.to_string()
            } else {
                trimmed.to_string()
            })
        }
        Some(_) => Err(ConfigError::new(format!("配置项 llm.{key} 必须是字符串。"))),
    }
}

/// 上下文窗口：必须是正整数。
pub fn read_context_window_tokens(section: &Table, default: i64) -> Result<i64, ConfigError> {
    match section.get("context_window_tokens") {
        None => Ok(default),
        Some(Value::Integer(value)) if *value > 0 => Ok(*value),
        Some(_) => Err(ConfigError::new(
            "配置项 llm.context_window_tokens 必须是正整数。",
        )),
    }
}
