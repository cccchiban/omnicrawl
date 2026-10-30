//! 清单与注册表的数据结构：插件记录、版本引用、Handler 与事件声明。
//!
//! 从 `models.rs` 按职责拆出。

use super::*;

/// config.toml 中的 plugins 段（对应 Python 的 `PluginsConfig`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub default_timeout_ms: i64,
    pub max_timeout_ms: i64,
    pub failure_threshold: i64,
    pub max_message_bytes: i64,
    pub custom_event_max_depth: i64,
    /// 默认关闭网络安装，降低供应链攻击面；需要时在 config 中显式开启。
    pub allow_network_install: bool,
    pub audit_log_enabled: bool,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_timeout_ms: 1000,
            max_timeout_ms: DEFAULT_MAX_TIMEOUT_MS,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            custom_event_max_depth: DEFAULT_CUSTOM_EVENT_MAX_DEPTH,
            allow_network_install: false,
            audit_log_enabled: true,
        }
    }
}

impl PluginsConfig {
    /// 区间校验；文案与 Python 的 `__post_init__` 一致。
    pub fn validate(self) -> Result<Self, PluginError> {
        if self.default_timeout_ms < 50 || self.default_timeout_ms > self.max_timeout_ms {
            return Err(PluginError::new(
                "plugins.default_timeout_ms 必须在 50 与 max_timeout_ms 之间。",
            ));
        }
        if self.max_timeout_ms < 100 || self.max_timeout_ms > 60_000 {
            return Err(PluginError::new(
                "plugins.max_timeout_ms 必须在 100 到 60000 之间。",
            ));
        }
        if self.failure_threshold < 1 || self.failure_threshold > 20 {
            return Err(PluginError::new(
                "plugins.failure_threshold 必须在 1 到 20 之间。",
            ));
        }
        if self.max_message_bytes < 1024 || self.max_message_bytes > 8_388_608 {
            return Err(PluginError::new(
                "plugins.max_message_bytes 必须在 1KiB 到 8MiB 之间。",
            ));
        }
        if self.custom_event_max_depth < 1 || self.custom_event_max_depth > 16 {
            return Err(PluginError::new(
                "plugins.custom_event_max_depth 必须在 1 到 16 之间。",
            ));
        }
        Ok(self)
    }
}

/// 插件声明的一个 Handler（对应 Python 的 `HandlerRegistration`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerRegistration {
    pub id: String,
    pub hook: String,
    pub mode: String,
    pub priority: i64,
    pub replaces: Vec<String>,
    pub event_version: Option<String>,
    pub timeout_ms: Option<i64>,
}

impl HandlerRegistration {
}

/// 插件静态声明的自定义事件（对应 Python 的 `CustomEventDeclaration`）。
#[derive(Debug, Clone, PartialEq)]
pub struct CustomEventDeclaration {
    pub name: String,
    pub version: i64,
    pub visibility: String,
    pub schema: Map<String, Value>,
}

/// package.json + omnicrawl 字段解析后的完整 manifest（对应 Python 的 `PluginManifest`）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub entry: String,
    pub permissions: Vec<String>,
    pub hooks: Vec<HandlerRegistration>,
    pub engines_omnicrawl: String,
    pub engines_node: String,
    pub timeout_ms: Option<i64>,
    pub custom_events: Vec<CustomEventDeclaration>,
    pub agents: Vec<String>,
    pub package_type: String,
    pub source_path: String,
}

impl PluginManifest {
}

/// 注册表中的精确版本指针（对应 Python 的 `PluginVersionRef`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginVersionRef {
    pub version: String,
    pub integrity: String,
    pub lockfile_hash: String,
    pub source: String,
    pub store_path: String,
    pub content_hash: String,
}

impl PluginVersionRef {
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("version".to_string(), Value::from(self.version.clone()));
        data.insert("integrity".to_string(), Value::from(self.integrity.clone()));
        data.insert(
            "lockfileHash".to_string(),
            Value::from(self.lockfile_hash.clone()),
        );
        data.insert("source".to_string(), Value::from(self.source.clone()));
        if !self.store_path.is_empty() {
            data.insert(
                "storePath".to_string(),
                Value::from(self.store_path.clone()),
            );
        }
        if !self.content_hash.is_empty() {
            data.insert(
                "contentHash".to_string(),
                Value::from(self.content_hash.clone()),
            );
        }
        data
    }

    /// 空对象或缺失时返回 `None`（与 Python 的 `if not data: return None` 一致）。
    pub fn from_dict(data: Option<&Value>) -> Option<Self> {
        let map = data?.as_object()?;
        if map.is_empty() {
            return None;
        }
        Some(Self {
            version: text_of(map.get("version")),
            integrity: text_of(map.get("integrity")),
            lockfile_hash: alias_text(map, "lockfileHash", "lockfile_hash"),
            source: text_of(map.get("source")),
            store_path: alias_text(map, "storePath", "store_path"),
            content_hash: alias_text(map, "contentHash", "content_hash"),
        })
    }
}

/// 单个插件在 registry 中的记录（对应 Python 的 `PluginRecord`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginRecord {
    pub name: String,
    pub enabled: bool,
    pub active: Option<PluginVersionRef>,
    pub candidate: Option<PluginVersionRef>,
    pub previous: Option<PluginVersionRef>,
    pub approved_permissions: Vec<String>,
    pub local_path: String,
    pub dev_mode: bool,
}

impl PluginRecord {
    /// 注意：与 Python 一致，`to_dict()` 不含 `name`（名字是外层字典的键）。
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert("enabled".to_string(), Value::from(self.enabled));
        data.insert(
            "active".to_string(),
            version_ref_value(self.active.as_ref()),
        );
        data.insert(
            "candidate".to_string(),
            version_ref_value(self.candidate.as_ref()),
        );
        data.insert(
            "previous".to_string(),
            version_ref_value(self.previous.as_ref()),
        );
        data.insert(
            "approvedPermissions".to_string(),
            Value::from(self.approved_permissions.clone()),
        );
        data.insert(
            "localPath".to_string(),
            Value::from(self.local_path.clone()),
        );
        data.insert("devMode".to_string(), Value::from(self.dev_mode));
        data
    }

    pub fn from_dict(name: &str, data: &Value) -> Self {
        let Some(map) = data.as_object() else {
            return Self {
                name: name.to_string(),
                ..Self::default()
            };
        };
        Self {
            name: name.to_string(),
            enabled: bool_of(map.get("enabled"), false),
            active: PluginVersionRef::from_dict(map.get("active")),
            candidate: PluginVersionRef::from_dict(map.get("candidate")),
            previous: PluginVersionRef::from_dict(map.get("previous")),
            approved_permissions: string_list_alias(
                map,
                "approvedPermissions",
                "approved_permissions",
            ),
            local_path: alias_text(map, "localPath", "local_path"),
            dev_mode: bool_alias(map, "devMode", "dev_mode", false),
        }
    }
}

/// user/project 注册表文档（对应 Python 的 `PluginRegistryDocument`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRegistryDocument {
    pub schema_version: i64,
    pub plugins: Vec<PluginRecord>,
    pub disabled_handlers: Vec<String>,
}

impl Default for PluginRegistryDocument {
    fn default() -> Self {
        Self {
            schema_version: PLUGIN_SCHEMA_VERSION,
            plugins: Vec::new(),
            disabled_handlers: Vec::new(),
        }
    }
}

impl PluginRegistryDocument {
    /// `plugins` 按名字排序后写出（与 Python 的 `sorted(self.plugins.items())` 一致）。
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut records = self.plugins.clone();
        records.sort_by(|left, right| left.name.cmp(&right.name));
        let mut plugins = Map::new();
        for record in &records {
            plugins.insert(record.name.clone(), Value::Object(record.to_dict()));
        }
        let mut data = Map::new();
        data.insert(
            "schemaVersion".to_string(),
            Value::from(self.schema_version),
        );
        data.insert("plugins".to_string(), Value::Object(plugins));
        data.insert(
            "disabledHandlers".to_string(),
            Value::from(self.disabled_handlers.clone()),
        );
        data
    }

    pub fn from_dict(data: &Value) -> Result<Self, PluginError> {
        let Some(map) = data.as_object() else {
            return Err(PluginError::new("registry 顶层必须是对象。"));
        };
        let raw_plugins = map.get("plugins").cloned().unwrap_or(Value::Null);
        let plugins_value = if raw_plugins.is_null() {
            Value::Object(Map::new())
        } else {
            raw_plugins
        };
        let Some(plugins_map) = plugins_value.as_object() else {
            return Err(PluginError::new("registry.plugins 必须是对象。"));
        };
        let mut plugins: Vec<PluginRecord> = Vec::new();
        for (name, value) in plugins_map {
            if !value.is_object() {
                continue;
            }
            plugins.push(PluginRecord::from_dict(name, value));
        }
        let disabled_handlers = string_list_alias(map, "disabledHandlers", "disabled_handlers");
        let schema_version = int_alias(
            map,
            "schemaVersion",
            "schema_version",
            PLUGIN_SCHEMA_VERSION,
        );
        Ok(Self {
            schema_version,
            plugins,
            disabled_handlers,
        })
    }

    pub fn get(&self, name: &str) -> Option<&PluginRecord> {
        self.plugins.iter().find(|record| record.name == name)
    }
}

/// 合并 project/user 后、按稳定顺序排序的 Handler（对应 Python 的 `ResolvedHandler`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedHandler {
    pub key: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub handler_id: String,
    pub hook: String,
    pub mode: String,
    pub priority: i64,
    pub scope: String,
    pub timeout_ms: i64,
    pub replaces: Vec<String>,
    pub integrity_prefix: String,
    pub local_path: String,
    pub permissions: Vec<String>,
}

/// 统一事件信封（对应 Python 的 `HookEvent`）。
#[derive(Debug, Clone, PartialEq)]
pub struct HookEvent {
    pub api_version: String,
    pub event_id: String,
    pub hook: String,
    pub timestamp: String,
    pub sequence: i64,
    pub workspace: Map<String, Value>,
    pub payload: Map<String, Value>,
    pub deadline_ms: i64,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub trace: Map<String, Value>,
}

impl HookEvent {
    /// dataclass 字段名是 snake_case，协议用 camelCase；键序与 Python 的 `to_dict` 一致。
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = Map::new();
        data.insert(
            "apiVersion".to_string(),
            Value::from(self.api_version.clone()),
        );
        data.insert("eventId".to_string(), Value::from(self.event_id.clone()));
        data.insert("hook".to_string(), Value::from(self.hook.clone()));
        data.insert("timestamp".to_string(), Value::from(self.timestamp.clone()));
        data.insert("sequence".to_string(), Value::from(self.sequence));
        data.insert(
            "workspace".to_string(),
            Value::Object(self.workspace.clone()),
        );
        data.insert("sessionId".to_string(), option_value(&self.session_id));
        data.insert("turnId".to_string(), option_value(&self.turn_id));
        data.insert("payload".to_string(), Value::Object(self.payload.clone()));
        data.insert("deadlineMs".to_string(), Value::from(self.deadline_ms));
        data.insert("trace".to_string(), Value::Object(self.trace.clone()));
        data
    }

}

/// 单个 Handler 的规范化结果（对应 Python 的 `HookResult`）。
#[derive(Debug, Clone, PartialEq)]
pub struct HookResult {
    pub action: String,
    pub reason: String,
    pub code: String,
    pub patch: Vec<Map<String, Value>>,
    pub annotations: Map<String, Value>,
    pub handler_key: String,
    pub elapsed_ms: f64,
    pub status: String,
}

impl Default for HookResult {
    fn default() -> Self {
        Self {
            action: "continue".to_string(),
            reason: String::new(),
            code: String::new(),
            patch: Vec::new(),
            annotations: Map::new(),
            handler_key: String::new(),
            elapsed_ms: 0.0,
            status: "continue".to_string(),
        }
    }
}

impl HookResult {
    pub fn continue_result(
        handler_key: &str,
        annotations: Map<String, Value>,
        elapsed_ms: f64,
        status: &str,
    ) -> Self {
        Self {
            action: "continue".to_string(),
            annotations,
            handler_key: handler_key.to_string(),
            elapsed_ms,
            status: status.to_string(),
            ..Self::default()
        }
    }

    pub fn deny_result(reason: &str, code: &str, handler_key: &str, elapsed_ms: f64) -> Self {
        Self {
            action: "deny".to_string(),
            reason: reason.to_string(),
            code: code.to_string(),
            handler_key: handler_key.to_string(),
            elapsed_ms,
            status: "deny".to_string(),
            ..Self::default()
        }
    }

    pub fn patch_result(
        patch: Vec<Map<String, Value>>,
        handler_key: &str,
        annotations: Map<String, Value>,
        elapsed_ms: f64,
    ) -> Self {
        Self {
            action: "patch".to_string(),
            patch,
            annotations,
            handler_key: handler_key.to_string(),
            elapsed_ms,
            status: "patch".to_string(),
            ..Self::default()
        }
    }
}

/// 一次 Hook 分发的聚合结果（对应 Python 的 `DispatchOutcome`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchOutcome {
    pub hook: String,
    pub payload: Map<String, Value>,
    pub denied: bool,
    pub deny_reason: String,
    pub deny_code: String,
    pub results: Vec<HookResult>,
    pub annotations: Map<String, Value>,
}

impl DispatchOutcome {
    pub fn new(hook: &str, payload: Map<String, Value>) -> Self {
        Self {
            hook: hook.to_string(),
            payload,
            denied: false,
            deny_reason: String::new(),
            deny_code: String::new(),
            results: Vec::new(),
            annotations: Map::new(),
        }
    }

}
