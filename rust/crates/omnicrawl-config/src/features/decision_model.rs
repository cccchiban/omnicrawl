//! 结构化决策模型配置（`decision_models.toml`）。
//!
//! 结构化决策模型是独立于对话模型的第二类服务：它不生成文本，而是对一段 `state`
//! 加若干**有类型的提问**（choice / score / noul）返回**校准过的答案**。
//!
//! 两种请求方式（`mode`）：
//! * `jev`（默认）：`POST {base_url}/v1/decide`，`state` + `questions` 直接进请求体，
//!   响应体的 `answers` 直接就是答案。Bearer 鉴权，模型名形如 `jev-latest` / `jev-1.13.0`。
//! * `chat_completions`：`POST {base_url}/chat/completions`（OpenAI 兼容），把同一份
//!   `state` + `questions` 当作 user 消息的 JSON 文本发出，并要求结构化 JSON 输出；
//!   答案从 `choices[0].message.content` 里解析出同一形状的 `answers`，因此上层读答案的
//!   代码两种方式共用。该方式的 `base_url` 按 OpenAI 兼容口径填到 `/v1`（同 `[image_gen]`
//!   与 `[tts_api]`），因此只追加资源路径。
//!
//! 与 `models.toml` 分开成独立文件是刻意的：决策渠道与对话渠道是两套互不相干的服务地址
//! 与模型命名空间，混进 `models.toml` 会被对话侧的目录与能力解析当成候选模型。
//!
//! 本模块只做配置的读写与校验；请求的构造与发送留给后续接入方使用
//! [`DecisionChannelConfig::decide_url`] 与 [`DecisionChannelConfig::resolve_api_key`]。
//! 决策渠道**不并入** `llm.profiles`：它在 `initialize` 里没有对映字段，不应被宿主当成
//! 对话模型注入内核。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::core::runtime::{
    atomic_write_text, dump_toml_text, get_section, load_config_data, resolve_decision_models_path,
    resolve_decision_models_write_path, ConfigEnvironment,
};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

/// 决策服务类型：Jev 原生接口（`POST {base_url}/v1/decide`）。
pub const DECISION_MODE_JEV: &str = "jev";

/// 决策服务类型：OpenAI 兼容的 Chat Completions 接口（`POST {base_url}/chat/completions`，
/// 基地址按 OpenAI 兼容口径填到 `/v1`）。
pub const DECISION_MODE_CHAT_COMPLETIONS: &str = "chat_completions";

/// 决策服务类型：OneJev 本地自部署（`POST {base_url}/v1/systemone`，TypeSafe 兼容接口）。
///
/// 与云端 `jev` 是同一个协议的两种部署：`/v1/systemone` 的请求/响应形状与
/// `/v1/decide` 一致（`state` + `questions` 进请求体，顶层 `answers` 出答案），
/// 只是由本机 `qev serve` 提供，因此这里单列一种方式：路径不同，且可指向本地服务。
pub const DECISION_MODE_ONEJEV: &str = "onejev";

/// 可选的服务类型（界面「请求方式」字段的候选）。
pub const DECISION_MODES: [&str; 3] = [
    DECISION_MODE_JEV,
    DECISION_MODE_CHAT_COMPLETIONS,
    DECISION_MODE_ONEJEV,
];

/// 自部署（`onejev`）的默认基地址：本机 `qev serve` 的监听地址。
pub const DEFAULT_LOCAL_DECISION_BASE_URL: &str = "http://127.0.0.1:8766";
/// 自部署渠道的默认凭据环境变量名（本地服务不校验凭据，但仍需要一个非空占位）。
pub const DEFAULT_LOCAL_DECISION_API_KEY_ENV: &str = "ONEJEV_API_KEY";
/// 自部署渠道的默认 key（界面「切换为自部署」时用它）。
pub const DEFAULT_LOCAL_DECISION_CHANNEL_KEY: &str = "onejev-local";

/// Jev 决策接口的基础地址（不含 `/v1/decide`）。
pub const DEFAULT_DECISION_BASE_URL: &str = "https://jevtypesafeai.com/api";
/// 默认模型：跟随最新版本；生产环境建议改成固定版本（如 `jev-1.13.0`）。
pub const DEFAULT_DECISION_MODEL: &str = "jev-latest";
/// 默认凭据环境变量名。
pub const DEFAULT_DECISION_API_KEY_ENV: &str = "JEV_API_KEY";
/// 首次生成配置时使用的默认渠道 key。
pub const DEFAULT_DECISION_CHANNEL_KEY: &str = "jev-main";

/// 一个决策渠道：一套服务地址 + 凭据 + 模型。
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionChannelConfig {
    pub key: String,
    pub name: String,
    /// 决策服务类型，取 [`DECISION_MODES`] 之一。
    pub mode: String,
    pub base_url: String,
    /// 内联凭据。与 `config.toml` 的渠道 profile 同口径：可以为空，此时走 `api_key_env`。
    pub api_key: String,
    pub api_key_env: String,
    /// 决策模型名（`jev-latest` 或固定版本）。
    pub model: String,
    pub enabled: bool,
}

impl Default for DecisionChannelConfig {
    fn default() -> Self {
        Self {
            key: DEFAULT_DECISION_CHANNEL_KEY.to_string(),
            name: "Jev 主渠道".to_string(),
            mode: DECISION_MODE_JEV.to_string(),
            base_url: DEFAULT_DECISION_BASE_URL.to_string(),
            api_key: String::new(),
            api_key_env: DEFAULT_DECISION_API_KEY_ENV.to_string(),
            model: DEFAULT_DECISION_MODEL.to_string(),
            enabled: true,
        }
    }
}

impl DecisionChannelConfig {
    /// 取值域校验与归一化（错误文案带渠道 key，便于在界面上直接读）。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        self.key = self.key.trim().to_string();
        if self.key.is_empty() {
            return Err(ConfigError::new("决策渠道的 key 不能为空。"));
        }
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(ConfigError::new(format!(
                "决策渠道 {} 的名称不能为空。",
                self.key
            )));
        }
        self.mode = self.mode.trim().to_lowercase();
        if !DECISION_MODES.contains(&self.mode.as_str()) {
            return Err(ConfigError::new(format!(
                "决策渠道 {} 的请求方式仅支持 {}，当前值：{}。",
                self.key,
                DECISION_MODES.join("、"),
                self.mode
            )));
        }
        let base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ConfigError::new(format!(
                "决策渠道 {} 的基地址不能为空。",
                self.key
            )));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(ConfigError::new(format!(
                "决策渠道 {} 的基地址必须以 http:// 或 https:// 开头。",
                self.key
            )));
        }
        self.base_url = base_url;
        self.model = self.model.trim().to_string();
        if self.model.is_empty() {
            return Err(ConfigError::new(format!(
                "决策渠道 {} 的模型 ID 不能为空。",
                self.key
            )));
        }
        self.api_key = self.api_key.trim().to_string();
        let api_key_env = self.api_key_env.trim().to_string();
        self.api_key_env = if api_key_env.is_empty() {
            DEFAULT_DECISION_API_KEY_ENV.to_string()
        } else {
            api_key_env
        };
        Ok(self)
    }

    /// 决策接口的完整地址：按 `mode` 选路径（基地址末尾斜杠已在归一化时去掉）。
    ///
    /// 上层只认这一个入口，因此三种请求方式对调用方是同一件事。`chat_completions` 的基地址
    /// 已含 `/v1`（OpenAI 兼容口径），只追加资源路径；`jev` / `onejev` 的基地址是站点根下的
    /// API 前缀，各自补 `/v1/...`。
    pub fn decide_url(&self) -> String {
        match self.mode.as_str() {
            DECISION_MODE_CHAT_COMPLETIONS => {
                format!("{}/chat/completions", self.base_url)
            }
            DECISION_MODE_ONEJEV => format!("{}/v1/systemone", self.base_url),
            _ => format!("{}/v1/decide", self.base_url),
        }
    }

    /// 渠道是否指向本机回环地址（自部署的 OneJev 服务）。
    ///
    /// `onejev` 是「本机 qev 服务」的协议形状，但云端网关（new-api 一类）也提供同一形状的
    /// 接口，因此判断「要不要拉起本机服务」不能只看 `mode`，还要看地址确实落在回环上。
    pub fn is_local_service(&self) -> bool {
        let Some((_, rest)) = self.base_url.split_once("://") else {
            return false;
        };
        // 去掉路径/查询/片段，只剩 authority（可能带 userinfo 与端口）。
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let authority = authority
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(authority);
        let host = if let Some(rest) = authority.strip_prefix('[') {
            // IPv6 字面量：取方括号内的部分。
            rest.split(']').next().unwrap_or("")
        } else {
            authority.split(':').next().unwrap_or("")
        };
        DECISION_API_LOOPBACK_HOSTS.contains(&host.to_lowercase().as_str())
    }

    /// 生效的内联凭据：只认配置里的明文 `api_key`。
    pub fn resolve_api_key(&self, _env: &ConfigEnvironment) -> String {
        self.api_key.trim().to_string()
    }
}

/// 决策模型功能开关的配置段名。
pub const DECISION_FEATURES_SECTION: &str = "features";

/// 自部署配置的段名（`decision_models.toml` 的 `[local]`）。
pub const DECISION_LOCAL_SECTION: &str = "local";

/// 决策 REST 接口的段名（`decision_models.toml` 的 `[api]`）。
pub const DECISION_API_SECTION: &str = "api";

/// 决策 REST 服务的默认监听端口。
///
/// 与本地 API（8765）和 OneJev 的 `qev serve`（8766）都错开：三者常常同时在本机跑。
pub const DEFAULT_DECISION_API_PORT: i64 = 8767;

/// 决策 REST 服务只接受回环地址（它持有决策渠道凭据，是本地代理）。
pub const DECISION_API_LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// 决策 REST 接口的运行期配置。
///
/// 与决策渠道分开：渠道是「决策服务在哪」，本段是「把决策能力以什么地址暴露给本机其它程序」。
/// 接口不做鉴权，因此只允许回环地址——本机任意进程都能调用，这是刻意的取舍。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionApiConfig {
    /// 是否随宿主启动常驻服务。
    pub enabled: bool,
    pub host: String,
    pub port: i64,
}

impl Default for DecisionApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: DEFAULT_DECISION_API_PORT,
        }
    }
}

impl DecisionApiConfig {
    /// 取值域校验与归一化（错误文案带 `api` 前缀，与段名一致）。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let host = self.host.trim().to_string();
        if !DECISION_API_LOOPBACK_HOSTS.contains(&host.to_lowercase().as_str()) {
            return Err(ConfigError::new(
                "决策接口 api.host 仅允许回环地址（127.0.0.1 / localhost / ::1）。",
            ));
        }
        self.host = host;
        if !(1..=65535).contains(&self.port) {
            return Err(ConfigError::new(
                "决策接口 api.port 必须是 1 到 65535 的整数。",
            ));
        }
        Ok(self)
    }

    /// 服务能否真正监听（开关打开）。
    pub fn usable(&self) -> bool {
        self.enabled
    }

    /// 监听地址文本。
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 读取 `decision_models.toml` 的 `[api]` 段；缺段或读不出来时返回默认值（未启用）。
pub fn load_decision_api_configuration(
    env: &ConfigEnvironment,
    decision_models_path: Option<&Path>,
) -> DecisionApiConfig {
    let Ok(target) = resolve_decision_models_path(env, decision_models_path) else {
        return DecisionApiConfig::default();
    };
    let Ok(data) = load_config_data(env, Some(&target)) else {
        return DecisionApiConfig::default();
    };
    let Ok(section) = get_section(&data, DECISION_API_SECTION) else {
        return DecisionApiConfig::default();
    };
    let defaults = DecisionApiConfig::default();
    DecisionApiConfig {
        enabled: bool_field(&section, "enabled", defaults.enabled),
        host: text_field(&section, "host", &defaults.host),
        port: match section.get("port") {
            Some(Value::Integer(value)) => *value,
            _ => defaults.port,
        },
    }
    // 校验失败（例如手改坏了监听地址）时不阻断决策功能：回落到默认值。
    .normalize()
    .unwrap_or(defaults)
}

/// 把决策 REST 接口配置写回 `decision_models.toml` 的 `[api]` 段（保留渠道、开关与 `version`）。
pub fn save_decision_api_configuration(
    env: &ConfigEnvironment,
    configuration: &DecisionApiConfig,
    decision_models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let normalized = configuration.clone().normalize()?;
    let source = resolve_decision_models_path(env, decision_models_path)?;
    let target = resolve_decision_models_write_path(env, decision_models_path)?;
    let mut data = if source.exists() {
        load_config_data(env, Some(&source))?
    } else {
        Table::new()
    };
    if !data.contains_key("version") {
        data.insert("version".to_string(), Value::Integer(1));
    }
    let mut section = Table::new();
    section.insert(
        "enabled".to_string(),
        Value::Boolean(normalized.enabled),
    );
    section.insert("host".to_string(), Value::String(normalized.host.clone()));
    section.insert("port".to_string(), Value::Integer(normalized.port));
    data.insert(DECISION_API_SECTION.to_string(), Value::Table(section));
    atomic_write_text(&target, &dump_toml_text(&data))?;
    Ok(target)
}

/// 可自部署的 OneJev 尺寸键（与 `omnicrawl-onejev` 的尺寸清单同源，由该 crate 的测试对账）。
///
/// 放在配置域是因为校验发生在这里；清单的其余信息（仓库、体积、显存）留在负责下载与
/// 启动的那个 crate，避免配置文件反向依赖推理子系统。
pub const ONEJEV_SIZE_KEYS: [&str; 5] = ["0.8B", "4B", "9B", "27B-FP8", "27B"];

/// 自部署的推理设备取值。
pub const LOCAL_DEVICE_OPTIONS: [&str; 3] = ["auto", "cuda", "cpu"];

/// 默认健康检查等待上限（秒）：首次加载权重并捕获 CUDA 图，大尺寸会久一些。
pub const DEFAULT_LOCAL_HEALTH_TIMEOUT_SECONDS: i64 = 600;

/// 自部署（OneJev）的运行期配置：尺寸、设备、权重目录与服务参数。
///
/// 与对话模型、决策渠道都无关：这是一份「本机跑哪一档 OneJev、跑在哪个设备」的设置，
/// 由 `mode = "onejev"` 的决策渠道消费（本地 `qev serve` 的地址由渠道的 `base_url` 给）。
#[derive(Debug, Clone, PartialEq)]
pub struct LocalDeploymentConfig {
    /// 当前部署的尺寸键，取 [`ONEJEV_SIZE_KEYS`] 之一。
    pub size: String,
    /// `auto` / `cuda` / `cpu`；`auto` 按 CUDA 处理（引擎自身在无 GPU 时回退）。
    pub device: String,
    /// 数据根目录（权重、虚拟环境、缓存都在这下面）；空值表示默认目录。
    pub model_dir: String,
    /// 决策渠道选了自部署时是否随实例启动自动准备本地服务（已在跑则复用）。
    pub auto_start: bool,
    /// 启动后等待健康检查的上限（秒）。
    pub health_timeout_seconds: i64,
}

impl Default for LocalDeploymentConfig {
    fn default() -> Self {
        Self {
            // 默认最小的一档：能在没有独显的机器上先把链路跑通。
            size: "0.8B".to_string(),
            device: "auto".to_string(),
            model_dir: String::new(),
            auto_start: true,
            health_timeout_seconds: DEFAULT_LOCAL_HEALTH_TIMEOUT_SECONDS,
        }
    }
}

impl LocalDeploymentConfig {
    /// 取值域校验与归一化。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let size = self.size.trim().to_string();
        if !ONEJEV_SIZE_KEYS
            .iter()
            .any(|key| key.eq_ignore_ascii_case(&size))
        {
            return Err(ConfigError::new(format!(
                "自部署尺寸仅支持 {}，当前值：{}。",
                ONEJEV_SIZE_KEYS.join("、"),
                size
            )));
        }
        self.size = ONEJEV_SIZE_KEYS
            .iter()
            .find(|key| key.eq_ignore_ascii_case(&size))
            .map(|key| (*key).to_string())
            .unwrap_or_else(|| ONEJEV_SIZE_KEYS[0].to_string());
        let device = self.device.trim().to_lowercase();
        if !LOCAL_DEVICE_OPTIONS.contains(&device.as_str()) {
            return Err(ConfigError::new(format!(
                "自部署设备仅支持 {}，当前值：{}。",
                LOCAL_DEVICE_OPTIONS.join("、"),
                device
            )));
        }
        self.device = device;
        self.model_dir = self.model_dir.trim().to_string();
        if self.health_timeout_seconds <= 0 {
            return Err(ConfigError::new("自部署健康检查超时必须为正数（秒）。"));
        }
        Ok(self)
    }
}

/// 读取 `decision_models.toml` 的 `[local]` 段；缺段或读不出来时返回默认值。
pub fn load_local_deployment(
    env: &ConfigEnvironment,
    decision_models_path: Option<&Path>,
) -> LocalDeploymentConfig {
    let Ok(target) = resolve_decision_models_path(env, decision_models_path) else {
        return LocalDeploymentConfig::default();
    };
    let Ok(data) = load_config_data(env, Some(&target)) else {
        return LocalDeploymentConfig::default();
    };
    let Ok(section) = get_section(&data, DECISION_LOCAL_SECTION) else {
        return LocalDeploymentConfig::default();
    };
    let defaults = LocalDeploymentConfig::default();
    LocalDeploymentConfig {
        size: text_field(&section, "size", &defaults.size),
        device: text_field(&section, "device", &defaults.device),
        model_dir: python_str(section.get("model_dir")),
        auto_start: bool_field(&section, "auto_start", defaults.auto_start),
        health_timeout_seconds: match section.get("health_timeout_seconds") {
            Some(Value::Integer(value)) => *value,
            _ => defaults.health_timeout_seconds,
        },
    }
    // 校验失败（例如手改坏了尺寸名）时不阻断决策功能：回落到默认值。
    .normalize()
    .unwrap_or(defaults)
}

/// 把自部署配置写回 `decision_models.toml` 的 `[local]` 段（保留渠道、开关与 `version`）。
pub fn save_local_deployment(
    env: &ConfigEnvironment,
    configuration: &LocalDeploymentConfig,
    decision_models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let normalized = configuration.clone().normalize()?;
    let source = resolve_decision_models_path(env, decision_models_path)?;
    let target = resolve_decision_models_write_path(env, decision_models_path)?;
    let mut data = if source.exists() {
        load_config_data(env, Some(&source))?
    } else {
        Table::new()
    };
    if !data.contains_key("version") {
        data.insert("version".to_string(), Value::Integer(1));
    }
    let mut section = Table::new();
    section.insert("size".to_string(), Value::String(normalized.size.clone()));
    section.insert(
        "device".to_string(),
        Value::String(normalized.device.clone()),
    );
    section.insert(
        "model_dir".to_string(),
        Value::String(normalized.model_dir.clone()),
    );
    section.insert(
        "auto_start".to_string(),
        Value::Boolean(normalized.auto_start),
    );
    section.insert(
        "health_timeout_seconds".to_string(),
        Value::Integer(normalized.health_timeout_seconds),
    );
    data.insert(DECISION_LOCAL_SECTION.to_string(), Value::Table(section));
    atomic_write_text(&target, &dump_toml_text(&data))?;
    Ok(target)
}

/// 「工具调用审查走决策模型」开关的配置键。
pub const DECISION_SWITCH_TOOL_REVIEW: &str = "tool_call_review";

/// 「记忆搜索走决策模型排序」开关的配置键。
pub const DECISION_SWITCH_MEMORY_SEARCH: &str = "memory_search_rerank";

/// 「知识库检索走决策模型排序」开关的配置键。
pub const DECISION_SWITCH_KB_SEARCH: &str = "kb_search_rerank";

/// 「提问自动托管」开关的配置键。
pub const DECISION_SWITCH_ASK_USER_CUSTODY: &str = "ask_user_custody";

/// 「老一轮工具调用按需淘汰」开关的配置键。
pub const DECISION_SWITCH_TOOL_PRUNE: &str = "tool_call_prune";

/// 一个决策模型功能开关：配置键、界面文案、默认值。
///
/// 开关表是决策模型页下方那一组开关的唯一来源（读盘、写盘、界面、测试都读它）；
/// 后续新增开关只需往 [`DECISION_SWITCHES`] 里加一项。
pub struct DecisionSwitchSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub default: bool,
}

/// 已落地的决策模型功能开关（顺序即界面顺序）。
pub const DECISION_SWITCHES: [DecisionSwitchSpec; 5] = [
    DecisionSwitchSpec {
        key: DECISION_SWITCH_TOOL_REVIEW,
        label: "工具调用审查使用决策模型",
        default: false,
    },
    DecisionSwitchSpec {
        key: DECISION_SWITCH_MEMORY_SEARCH,
        label: "记忆搜索使用决策模型排序",
        default: false,
    },
    DecisionSwitchSpec {
        key: DECISION_SWITCH_KB_SEARCH,
        label: "知识库检索使用决策模型排序",
        default: false,
    },
    DecisionSwitchSpec {
        key: DECISION_SWITCH_ASK_USER_CUSTODY,
        label: "提问由决策模型自动作答",
        default: false,
    },
    DecisionSwitchSpec {
        key: DECISION_SWITCH_TOOL_PRUNE,
        label: "旧一轮工具调用按需淘汰",
        default: false,
    },
];

/// 决策模型功能开关的当前值（键 → 值）。
pub type DecisionSwitches = BTreeMap<String, bool>;

/// 默认开关值（缺段、缺键时的回落）。
pub fn default_decision_switches() -> DecisionSwitches {
    DECISION_SWITCHES
        .iter()
        .map(|spec| (spec.key.to_string(), spec.default))
        .collect()
}

/// 单个开关的默认值；未知键回落到 `false`。
pub fn decision_switch_default(key: &str) -> bool {
    DECISION_SWITCHES
        .iter()
        .find(|spec| spec.key == key)
        .map(|spec| spec.default)
        .unwrap_or(false)
}

/// 从 `decision_models.toml` 的 `[features]` 段读功能开关。
///
/// 读不到配置（文件缺失、段缺失、值类型不对）时返回默认值：开关只是加速手段，
/// 读不出来时按「不启用决策模型审查」（即原来的对话模型审查）处理。
pub fn load_decision_switches(
    env: &ConfigEnvironment,
    decision_models_path: Option<&Path>,
) -> DecisionSwitches {
    let mut switches = default_decision_switches();
    let Ok(target) = resolve_decision_models_path(env, decision_models_path) else {
        return switches;
    };
    let Ok(data) = load_config_data(env, Some(&target)) else {
        return switches;
    };
    let Ok(section) = get_section(&data, DECISION_FEATURES_SECTION) else {
        return switches;
    };
    for spec in DECISION_SWITCHES {
        if let Some(value) = section.get(spec.key) {
            switches.insert(spec.key.to_string(), truthy(value));
        }
    }
    switches
}

/// 把一个开关写回 `decision_models.toml` 的 `[features]` 段（保留其他键与其他段）。
pub fn save_decision_switch(
    env: &ConfigEnvironment,
    key: &str,
    enabled: bool,
    decision_models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if !DECISION_SWITCHES.iter().any(|spec| spec.key == key) {
        return Err(ConfigError::new(format!(
            "未知的决策模型开关：{key}。"
        )));
    }
    let source = resolve_decision_models_path(env, decision_models_path)?;
    let target = resolve_decision_models_write_path(env, decision_models_path)?;
    let mut data = if source.exists() {
        load_config_data(env, Some(&source))?
    } else {
        Table::new()
    };
    if !data.contains_key("version") {
        data.insert("version".to_string(), Value::Integer(1));
    }
    let mut section = match data.get(DECISION_FEATURES_SECTION) {
        Some(Value::Table(inner)) => inner.clone(),
        _ => Table::new(),
    };
    // 已知开关都补齐：第一次写开关时把整张表落全，读盘口径与界面口径一致。
    for spec in DECISION_SWITCHES {
        let value = if spec.key == key {
            enabled
        } else {
            truthy_or(section.get(spec.key), spec.default)
        };
        section.insert(spec.key.to_string(), Value::Boolean(value));
    }
    data.insert(
        DECISION_FEATURES_SECTION.to_string(),
        Value::Table(section),
    );
    atomic_write_text(&target, &dump_toml_text(&data))?;
    Ok(target)
}

/// Python `bool(...)` 在缺失时的回落写法。
fn truthy_or(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        None => fallback,
        Some(value) => truthy(value),
    }
}

/// 决策渠道集合与默认渠道 key。
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionModelConfiguration {
    pub channels: Vec<DecisionChannelConfig>,
    pub default_key: String,
}

impl Default for DecisionModelConfiguration {
    fn default() -> Self {
        Self {
            channels: vec![DecisionChannelConfig::default()],
            default_key: DEFAULT_DECISION_CHANNEL_KEY.to_string(),
        }
    }
}

impl DecisionModelConfiguration {
    /// 校验整份配置：至少一条渠道、key 唯一。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.channels.is_empty() {
            return Err(ConfigError::new("至少需要保留一个决策渠道。"));
        }
        let mut seen: Vec<&str> = Vec::new();
        for channel in &self.channels {
            let key = channel.key.trim();
            if key.is_empty() {
                return Err(ConfigError::new("决策渠道的 key 不能为空。"));
            }
            if seen.contains(&key) {
                return Err(ConfigError::new(format!("决策渠道 key 重复：{key}。")));
            }
            seen.push(key);
        }
        Ok(())
    }

    /// 当前生效的默认渠道：`default_key` 命中且启用时优先，否则取第一条启用的渠道。
    ///
    /// 这是后续接入方（决策工具、结构化决策控制器）取用配置的入口。
    pub fn active_channel(&self) -> Option<&DecisionChannelConfig> {
        let key = self.default_key.trim();
        if !key.is_empty() {
            if let Some(channel) = self
                .channels
                .iter()
                .find(|channel| channel.key == key && channel.enabled)
            {
                return Some(channel);
            }
        }
        self.channels.iter().find(|channel| channel.enabled)
    }

    /// 把默认渠道折算到存在的 key 上（写盘与读盘统一口径）。
    fn normalize_default_key(mut self) -> Self {
        let key = self.default_key.trim().to_string();
        let resolved = self
            .channels
            .iter()
            .find(|channel| channel.key == key && channel.enabled)
            .or_else(|| self.channels.iter().find(|channel| channel.enabled))
            .or_else(|| self.channels.first())
            .map(|channel| channel.key.clone())
            .unwrap_or_default();
        self.default_key = resolved;
        self
    }
}

/// 读取 `decision_models.toml`；文件或 `channels` 段缺失时返回默认的一条 Jev 渠道。
pub fn load_decision_model_configuration(
    env: &ConfigEnvironment,
    decision_models_path: Option<&Path>,
) -> Result<DecisionModelConfiguration, ConfigError> {
    let target = resolve_decision_models_path(env, decision_models_path)?;
    let data = load_config_data(env, Some(&target))?;
    let channels = match data.get("channels") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => {
            return Err(ConfigError::new(
                "配置段 decision_models.channels 必须是对象。",
            ))
        }
    };
    if channels.is_empty() {
        return Ok(DecisionModelConfiguration::default());
    }
    let mut parsed: Vec<DecisionChannelConfig> = Vec::new();
    for (key, value) in &channels {
        let Some(section) = value.as_table() else {
            return Err(ConfigError::new(format!("决策渠道 {key} 必须是对象。")));
        };
        parsed.push(parse_channel(key, section)?);
    }
    let default_key = python_str(data.get("default_key")).trim().to_string();
    let configuration = DecisionModelConfiguration {
        channels: parsed,
        default_key,
    }
    .normalize_default_key();
    configuration.validate()?;
    Ok(configuration)
}

/// 把一份决策渠道配置写回 `decision_models.toml`（保留其他段与 `version`）。
pub fn save_decision_model_configuration(
    env: &ConfigEnvironment,
    configuration: &DecisionModelConfiguration,
    decision_models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    configuration.validate()?;
    let mut channels: Vec<DecisionChannelConfig> = Vec::new();
    for channel in &configuration.channels {
        channels.push(channel.clone().normalize()?);
    }
    let normalized = DecisionModelConfiguration {
        channels,
        default_key: configuration.default_key.clone(),
    }
    .normalize_default_key();
    normalized.validate()?;

    let source = resolve_decision_models_path(env, decision_models_path)?;
    let target = resolve_decision_models_write_path(env, decision_models_path)?;
    let mut data = if source.exists() {
        load_config_data(env, Some(&source))?
    } else {
        Table::new()
    };
    if !data.contains_key("version") {
        data.insert("version".to_string(), Value::Integer(1));
    }
    data.insert(
        "default_key".to_string(),
        Value::String(normalized.default_key.clone()),
    );
    let mut section = Table::new();
    for channel in &normalized.channels {
        let mut entry = Table::new();
        entry.insert("name".to_string(), Value::String(channel.name.clone()));
        entry.insert("mode".to_string(), Value::String(channel.mode.clone()));
        entry.insert(
            "base_url".to_string(),
            Value::String(channel.base_url.clone()),
        );
        entry.insert("api_key".to_string(), Value::String(channel.api_key.clone()));
        entry.insert(
            "api_key_env".to_string(),
            Value::String(channel.api_key_env.clone()),
        );
        entry.insert("model".to_string(), Value::String(channel.model.clone()));
        entry.insert("enabled".to_string(), Value::Boolean(channel.enabled));
        section.insert(channel.key.clone(), Value::Table(entry));
    }
    data.insert("channels".to_string(), Value::Table(section));
    atomic_write_text(&target, &dump_toml_text(&data))?;
    Ok(target)
}

fn parse_channel(key: &str, section: &Table) -> Result<DecisionChannelConfig, ConfigError> {
    DecisionChannelConfig {
        key: key.to_string(),
        name: text_field(section, "name", key),
        mode: text_field(section, "mode", DECISION_MODE_JEV),
        base_url: text_field(section, "base_url", DEFAULT_DECISION_BASE_URL),
        api_key: python_str(section.get("api_key")),
        api_key_env: text_field(section, "api_key_env", DEFAULT_DECISION_API_KEY_ENV),
        model: text_field(section, "model", DEFAULT_DECISION_MODEL),
        enabled: bool_field(section, "enabled", true),
    }
    .normalize()
}

fn bool_field(section: &Table, name: &str, default: bool) -> bool {
    match section.get(name) {
        None => default,
        Some(value) => truthy(value),
    }
}

/// Python `str(section.get(name) or fallback)`。
fn text_field(section: &Table, name: &str, fallback: &str) -> String {
    let text = python_str(section.get(name));
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("oc-decision-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join(crate::core::runtime::USER_CONFIG_DIRNAME))
            .expect("建立临时配置目录");
        path
    }

    fn env_for(root: &Path) -> ConfigEnvironment {
        ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
    }

    #[test]
    fn missing_file_yields_one_default_jev_channel() {
        let root = temp_root("empty");
        let configuration =
            load_decision_model_configuration(&env_for(&root), None).expect("缺文件时用默认值");
        assert_eq!(configuration.channels.len(), 1);
        let channel = &configuration.channels[0];
        assert_eq!(channel.key, DEFAULT_DECISION_CHANNEL_KEY);
        assert_eq!(channel.mode, DECISION_MODE_JEV);
        assert_eq!(channel.base_url, DEFAULT_DECISION_BASE_URL);
        assert_eq!(channel.model, DEFAULT_DECISION_MODEL);
        assert_eq!(channel.api_key_env, DEFAULT_DECISION_API_KEY_ENV);
        assert!(channel.enabled);
        assert_eq!(
            channel.decide_url(),
            "https://jevtypesafeai.com/api/v1/decide",
            "接口地址按 base_url + /v1/decide 拼"
        );
        assert_eq!(
            configuration.active_channel().map(|item| item.key.as_str()),
            Some(DEFAULT_DECISION_CHANNEL_KEY),
            "默认渠道即唯一那条"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decide_url_follows_the_request_mode() {
        let native = DecisionChannelConfig::default();
        assert_eq!(native.mode, DECISION_MODE_JEV);
        assert_eq!(
            native.decide_url(),
            "https://jevtypesafeai.com/api/v1/decide",
            "原生方式走 /v1/decide"
        );

        let chat = DecisionChannelConfig {
            mode: DECISION_MODE_CHAT_COMPLETIONS.to_string(),
            // 对话补全的基地址按 OpenAI 兼容口径填到 /v1（同 image_gen / tts_api）。
            base_url: "https://api.openlux.ai/v1".to_string(),
            ..DecisionChannelConfig::default()
        };
        assert_eq!(
            chat.decide_url(),
            "https://api.openlux.ai/v1/chat/completions",
            "对话补全方式在基地址上追加 /chat/completions，不再重复拼 /v1"
        );

        let local = DecisionChannelConfig {
            mode: DECISION_MODE_ONEJEV.to_string(),
            base_url: DEFAULT_LOCAL_DECISION_BASE_URL.to_string(),
            ..DecisionChannelConfig::default()
        };
        assert_eq!(
            local.decide_url(),
            "http://127.0.0.1:8766/v1/systemone",
            "自部署方式走 OneJev 服务的 /v1/systemone"
        );
    }

    #[test]
    fn only_loopback_onejev_channels_count_as_local_service() {
        // 本机自部署：回环地址，算本地服务。
        let local = DecisionChannelConfig {
            mode: DECISION_MODE_ONEJEV.to_string(),
            base_url: DEFAULT_LOCAL_DECISION_BASE_URL.to_string(),
            ..DecisionChannelConfig::default()
        };
        assert!(local.is_local_service(), "回环 + onejev 即本机服务");

        // 云端网关也提供 onejev 形状的接口（如 new-api 的 /v1/systemone）：
        // 不能因为 mode 相同就去拉起本机 qev 进程。
        let cloud = DecisionChannelConfig {
            mode: DECISION_MODE_ONEJEV.to_string(),
            base_url: "https://api.openlux.ai".to_string(),
            ..DecisionChannelConfig::default()
        };
        assert_eq!(
            cloud.decide_url(),
            "https://api.openlux.ai/v1/systemone",
            "云端走同一协议路径"
        );
        assert!(!cloud.is_local_service(), "非回环地址不是本机服务");

        // 其它写法：带端口、userinfo、路径、IPv6 字面量与 localhost。
        for (base_url, expected) in [
            ("http://localhost:8766", true),
            ("http://user:pass@127.0.0.1:8766/prefix", true),
            ("http://[::1]:8766", true),
            ("http://192.168.1.10:8766", false),
            ("http://127.0.0.1.example.com", false),
        ] {
            let channel = DecisionChannelConfig {
                mode: DECISION_MODE_ONEJEV.to_string(),
                base_url: base_url.to_string(),
                ..DecisionChannelConfig::default()
            };
            assert_eq!(channel.is_local_service(), expected, "base_url = {base_url}");
        }
    }

    #[test]
    fn local_deployment_defaults_and_round_trips() {
        let root = temp_root("local");
        let env = env_for(&root);
        // 缺文件、缺段都按默认值（0.8B + auto + 默认目录）。
        let defaults = load_local_deployment(&env, None);
        assert_eq!(defaults.size, "0.8B");
        assert_eq!(defaults.device, "auto");
        assert!(defaults.model_dir.is_empty(), "空目录表示用默认目录");
        assert!(defaults.auto_start);
        assert_eq!(
            defaults.health_timeout_seconds,
            DEFAULT_LOCAL_HEALTH_TIMEOUT_SECONDS
        );

        let configuration = LocalDeploymentConfig {
            size: "9B".to_string(),
            device: "cuda".to_string(),
            model_dir: "D:/onejev".to_string(),
            auto_start: false,
            health_timeout_seconds: 900,
        };
        let path = save_local_deployment(&env, &configuration, None).expect("写盘");
        assert!(path.exists(), "自部署配置写到默认位置：{path:?}");
        let reloaded = load_local_deployment(&env, None);
        assert_eq!(reloaded.size, "9B");
        assert_eq!(reloaded.device, "cuda");
        assert_eq!(reloaded.model_dir, "D:/onejev");
        assert!(!reloaded.auto_start);
        assert_eq!(reloaded.health_timeout_seconds, 900);
        // 尺寸大小写不敏感，归一化后落到清单里的规范写法。
        let mixed = LocalDeploymentConfig {
            size: "27b-fp8".to_string(),
            ..LocalDeploymentConfig::default()
        }
        .normalize()
        .expect("尺寸写法不敏感");
        assert_eq!(mixed.size, "27B-FP8");

        // 写自部署配置不能碰渠道段（与写开关同一口径）。
        save_decision_model_configuration(&env, &DecisionModelConfiguration::default(), None)
            .expect("写渠道");
        save_local_deployment(&env, &configuration, None).expect("再写自部署");
        let channels = load_decision_model_configuration(&env, None).expect("读回渠道");
        assert_eq!(channels.channels.len(), 1);
        assert_eq!(channels.default_key, DEFAULT_DECISION_CHANNEL_KEY);

        // 非法值被拒绝并给出可读原因。
        let bad_size = LocalDeploymentConfig {
            size: "13B".to_string(),
            ..LocalDeploymentConfig::default()
        };
        let error = bad_size.normalize().expect_err("未知尺寸应被拒绝");
        assert!(error.message().contains("13B"), "{}", error.message());
        let bad_device = LocalDeploymentConfig {
            device: "tpu".to_string(),
            ..LocalDeploymentConfig::default()
        };
        let error = bad_device.normalize().expect_err("未知设备应被拒绝");
        assert!(error.message().contains("设备"), "{}", error.message());

        // 磁盘上被手改坏的值读回来时回落到默认，不阻断决策功能。
        std::fs::write(
            path,
            "version = 1\n[local]\nsize = \"13B\"\ndevice = \"tpu\"\n",
        )
        .expect("写坏配置");
        let fallback = load_local_deployment(&env, None);
        assert_eq!(fallback.size, LocalDeploymentConfig::default().size);
        assert_eq!(fallback.device, LocalDeploymentConfig::default().device);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_then_load_round_trips_every_field() {
        let root = temp_root("roundtrip");
        let env = env_for(&root);
        let configuration = DecisionModelConfiguration {
            channels: vec![
                DecisionChannelConfig {
                    key: "jev-main".to_string(),
                    name: "Jev 主渠道".to_string(),
                    mode: DECISION_MODE_JEV.to_string(),
                    base_url: "https://jevtypesafeai.com/api/".to_string(),
                    api_key: String::new(),
                    api_key_env: "JEV_API_KEY".to_string(),
                    model: "jev-1.13.0".to_string(),
                    enabled: true,
                },
                DecisionChannelConfig {
                    key: "self-hosted".to_string(),
                    name: "自建决策服务".to_string(),
                    mode: DECISION_MODE_JEV.to_string(),
                    base_url: "http://127.0.0.1:8080".to_string(),
                    api_key: "jv_live_test".to_string(),
                    api_key_env: String::new(),
                    model: "jev-latest".to_string(),
                    enabled: true,
                },
            ],
            default_key: "self-hosted".to_string(),
        };
        let path = save_decision_model_configuration(&env, &configuration, None).expect("写盘");
        assert!(path.exists(), "文件应写到默认位置：{path:?}");

        let reloaded = load_decision_model_configuration(&env, None).expect("读回");
        assert_eq!(reloaded.default_key, "self-hosted");
        assert_eq!(reloaded.channels.len(), 2);
        assert_eq!(reloaded.channels[0].base_url, "https://jevtypesafeai.com/api");
        assert_eq!(reloaded.channels[0].model, "jev-1.13.0");
        // 空的环境变量名回落到默认名。
        assert_eq!(reloaded.channels[1].api_key_env, DEFAULT_DECISION_API_KEY_ENV);
        assert_eq!(
            reloaded.active_channel().map(|item| item.key.as_str()),
            Some("self-hosted"),
            "默认渠道按 key 命中"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_key_falls_back_to_first_enabled_channel() {
        let root = temp_root("default-key");
        let env = env_for(&root);
        let configuration = DecisionModelConfiguration {
            channels: vec![
                DecisionChannelConfig {
                    key: "off".to_string(),
                    name: "关掉的".to_string(),
                    mode: DECISION_MODE_JEV.to_string(),
                    base_url: DEFAULT_DECISION_BASE_URL.to_string(),
                    api_key: String::new(),
                    api_key_env: DEFAULT_DECISION_API_KEY_ENV.to_string(),
                    model: DEFAULT_DECISION_MODEL.to_string(),
                    enabled: false,
                },
                DecisionChannelConfig {
                    key: "on".to_string(),
                    name: "开着的".to_string(),
                    mode: DECISION_MODE_JEV.to_string(),
                    base_url: DEFAULT_DECISION_BASE_URL.to_string(),
                    api_key: String::new(),
                    api_key_env: DEFAULT_DECISION_API_KEY_ENV.to_string(),
                    model: DEFAULT_DECISION_MODEL.to_string(),
                    enabled: true,
                },
            ],
            default_key: String::new(),
        };
        save_decision_model_configuration(&env, &configuration, None).expect("写盘");
        let reloaded = load_decision_model_configuration(&env, None).expect("读回");
        assert_eq!(reloaded.default_key, "on", "空默认值落到第一条启用的渠道");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_channel_is_rejected_with_a_readable_message() {
        let channel = DecisionChannelConfig {
            key: "bad".to_string(),
            name: String::new(),
            mode: DECISION_MODE_JEV.to_string(),
            base_url: DEFAULT_DECISION_BASE_URL.to_string(),
            api_key: String::new(),
            api_key_env: DEFAULT_DECISION_API_KEY_ENV.to_string(),
            model: DEFAULT_DECISION_MODEL.to_string(),
            enabled: true,
        };
        let error = channel.normalize().expect_err("空名称应被拒绝");
        assert!(error.message().contains("bad"), "{}", error.message());

        let bad_url = DecisionChannelConfig {
            key: "bad-url".to_string(),
            name: "坏地址".to_string(),
            mode: DECISION_MODE_JEV.to_string(),
            base_url: "jevtypesafeai.com".to_string(),
            api_key: String::new(),
            api_key_env: DEFAULT_DECISION_API_KEY_ENV.to_string(),
            model: DEFAULT_DECISION_MODEL.to_string(),
            enabled: true,
        };
        let error = bad_url.normalize().expect_err("缺少 scheme 应被拒绝");
        assert!(error.message().contains("http"), "{}", error.message());

        let unsupported_mode = DecisionChannelConfig {
            mode: "openai".to_string(),
            ..DecisionChannelConfig::default()
        };
        let error = unsupported_mode.normalize().expect_err("未知服务类型应被拒绝");
        assert!(error.message().contains("请求方式"), "{}", error.message());
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let configuration = DecisionModelConfiguration {
            channels: vec![
                DecisionChannelConfig::default(),
                DecisionChannelConfig::default(),
            ],
            default_key: DEFAULT_DECISION_CHANNEL_KEY.to_string(),
        };
        let error = configuration.validate().expect_err("重复 key 应被拒绝");
        assert!(error.message().contains("重复"), "{}", error.message());
    }

    #[test]
    fn decision_switches_default_off_and_round_trip() {
        let root = temp_root("switches");
        let env = env_for(&root);
        // 缺文件、缺段、缺键都按默认值（工具调用审查不启用决策模型）。
        let switches = load_decision_switches(&env, None);
        assert_eq!(
            switches.get(DECISION_SWITCH_TOOL_REVIEW).copied(),
            Some(false)
        );
        assert!(!decision_switch_default(DECISION_SWITCH_TOOL_REVIEW));
        // 两个检索重排开关同样默认关闭。
        assert_eq!(
            switches.get(DECISION_SWITCH_MEMORY_SEARCH).copied(),
            Some(false)
        );
        assert_eq!(switches.get(DECISION_SWITCH_KB_SEARCH).copied(), Some(false));
        // 提问托管开关同样默认关闭。
        assert_eq!(
            switches.get(DECISION_SWITCH_ASK_USER_CUSTODY).copied(),
            Some(false)
        );
        assert!(!decision_switch_default(DECISION_SWITCH_ASK_USER_CUSTODY));

        let path = save_decision_switch(&env, DECISION_SWITCH_TOOL_REVIEW, true, None)
            .expect("写开关");
        assert!(path.exists(), "开关写到默认位置：{path:?}");
        let reloaded = load_decision_switches(&env, None);
        assert_eq!(
            reloaded.get(DECISION_SWITCH_TOOL_REVIEW).copied(),
            Some(true)
        );
        // 写一个开关会把整张表落全：其余开关按当前值（默认）补齐，不是被写错。
        assert_eq!(
            reloaded.get(DECISION_SWITCH_MEMORY_SEARCH).copied(),
            Some(false)
        );
        assert_eq!(reloaded.get(DECISION_SWITCH_KB_SEARCH).copied(), Some(false));
        assert_eq!(
            reloaded.get(DECISION_SWITCH_ASK_USER_CUSTODY).copied(),
            Some(false)
        );

        // 新开关各自独立写回，互不影响。
        save_decision_switch(&env, DECISION_SWITCH_KB_SEARCH, true, None).expect("写知识库开关");
        save_decision_switch(&env, DECISION_SWITCH_ASK_USER_CUSTODY, true, None)
            .expect("写提问托管开关");
        let reloaded = load_decision_switches(&env, None);
        assert_eq!(reloaded.get(DECISION_SWITCH_KB_SEARCH).copied(), Some(true));
        assert_eq!(
            reloaded.get(DECISION_SWITCH_ASK_USER_CUSTODY).copied(),
            Some(true)
        );
        assert_eq!(
            reloaded.get(DECISION_SWITCH_TOOL_REVIEW).copied(),
            Some(true),
            "写一个开关不该改动另一个的值"
        );

        // 写开关不能碰渠道段：两条渠道与默认 key 都要原样保留。
        let configuration = DecisionModelConfiguration::default();
        save_decision_model_configuration(&env, &configuration, None).expect("写渠道");
        save_decision_switch(&env, DECISION_SWITCH_TOOL_REVIEW, false, None).expect("再写开关");
        let reloaded_channels = load_decision_model_configuration(&env, None).expect("读回渠道");
        assert_eq!(reloaded_channels.channels.len(), 1);
        assert_eq!(
            reloaded_channels.default_key,
            DEFAULT_DECISION_CHANNEL_KEY,
            "写开关不应改默认渠道"
        );
        assert_eq!(
            load_decision_switches(&env, None)
                .get(DECISION_SWITCH_TOOL_REVIEW)
                .copied(),
            Some(false)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_decision_switch_is_rejected() {
        let root = temp_root("unknown-switch");
        let env = env_for(&root);
        let error = save_decision_switch(&env, "not_a_switch", true, None)
            .expect_err("未知开关应被拒绝");
        assert!(error.message().contains("not_a_switch"), "{}", error.message());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decision_api_defaults_to_disabled() {
        let root = temp_root("api-defaults");
        let env = env_for(&root);
        // 缺文件、缺段都按默认值：默认关闭。
        let defaults = load_decision_api_configuration(&env, None);
        assert!(!defaults.enabled);
        assert!(!defaults.usable(), "默认关闭时接口不可用");
        assert_eq!(defaults.host, "127.0.0.1");
        assert_eq!(defaults.port, DEFAULT_DECISION_API_PORT);
        assert_eq!(defaults.address(), "127.0.0.1:8767");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decision_api_round_trips_and_keeps_other_sections() {
        let root = temp_root("api-round-trip");
        let env = env_for(&root);
        let configuration = DecisionApiConfig {
            enabled: true,
            host: "localhost".to_string(),
            port: 9100,
        };
        let path = save_decision_api_configuration(&env, &configuration, None).expect("写盘");
        assert!(path.exists(), "接口配置写到默认位置：{path:?}");
        let reloaded = load_decision_api_configuration(&env, None);
        assert!(reloaded.enabled);
        assert!(reloaded.usable());
        assert_eq!(reloaded.host, "localhost");
        assert_eq!(reloaded.port, 9100);

        // 写接口段不能碰渠道段（与写开关、写自部署同一口径）。
        save_decision_model_configuration(&env, &DecisionModelConfiguration::default(), None)
            .expect("写渠道");
        save_decision_api_configuration(&env, &configuration, None).expect("再写接口段");
        let channels = load_decision_model_configuration(&env, None).expect("读回渠道");
        assert_eq!(channels.channels.len(), 1);
        assert_eq!(channels.default_key, DEFAULT_DECISION_CHANNEL_KEY);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decision_api_rejects_non_loopback_and_bad_port() {
        let bad_host = DecisionApiConfig {
            enabled: true,
            host: "0.0.0.0".to_string(),
            ..DecisionApiConfig::default()
        };
        let error = bad_host.normalize().expect_err("非回环地址应被拒绝");
        assert!(error.message().contains("回环"), "{}", error.message());

        let bad_port = DecisionApiConfig {
            port: 70000,
            ..DecisionApiConfig::default()
        };
        let error = bad_port.normalize().expect_err("越界端口应被拒绝");
        assert!(error.message().contains("api.port"), "{}", error.message());

        // 手改坏了监听地址时不阻断决策功能：读回落到默认值。
        let root = temp_root("api-bad-host");
        let env = env_for(&root);
        std::fs::write(
            root.join(crate::core::runtime::USER_CONFIG_DIRNAME)
                .join(crate::core::runtime::DEFAULT_DECISION_MODELS_FILENAME),
            "[api]\nenabled = true\nhost = \"0.0.0.0\"\nport = 8767\n",
        )
        .expect("写坏配置");
        let reloaded = load_decision_api_configuration(&env, None);
        assert!(!reloaded.enabled, "坏配置回落到默认值");
        assert_eq!(reloaded.host, "127.0.0.1");
        let _ = std::fs::remove_dir_all(&root);
    }
}
