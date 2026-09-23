//! `omnicrawl/extensions/plugin_models.py` 的 Rust 移植。
//!
//! 纯数据结构、常量与校验：不启动 Worker、不碰磁盘注册表，这样内核、CLI 与测试
//! 共享同一份契约，Agent 里不再散落魔法字符串。
//!
//! 与 Python 的已知差异见 crate README「已知差异」；两侧靠
//! `rust/tools/gen_extensions_models_fixture.py` 生成的数据集逐字段对照。

use crate::error::{PluginError, PluginManifestError, PluginProtocolError};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const HOOK_API_VERSION: &str = "1";
/// 需与 pyproject.toml 的 version 保持同步（与 Python 侧 `OMNICRAWL_VERSION` 同源）。
pub const OMNICRAWL_VERSION: &str = "0.1.66";
pub const PLUGIN_SCHEMA_VERSION: i64 = 1;

pub const HANDLER_MODE_OBSERVE: &str = "observe";
pub const HANDLER_MODE_TRANSFORM: &str = "transform";
pub const HANDLER_MODE_GUARD: &str = "guard";
pub const HANDLER_MODE_NOTIFY: &str = "notify";

/// 四种 Handler 模式；顺序与 Python 的 `HANDLER_MODES` 一致（集合成员相等即可）。
pub const HANDLER_MODES: [&str; 4] = [
    HANDLER_MODE_OBSERVE,
    HANDLER_MODE_TRANSFORM,
    HANDLER_MODE_GUARD,
    HANDLER_MODE_NOTIFY,
];

pub const JSON_PATCH_OPS: [&str; 3] = ["add", "replace", "remove"];

pub const DEFAULT_OBSERVE_TIMEOUT_MS: i64 = 500;
pub const DEFAULT_TRANSFORM_TIMEOUT_MS: i64 = 2000;
pub const DEFAULT_GUARD_TIMEOUT_MS: i64 = 2000;
pub const DEFAULT_NOTIFY_TIMEOUT_MS: i64 = 500;
pub const DEFAULT_MAX_TIMEOUT_MS: i64 = 5000;
pub const DEFAULT_FAILURE_THRESHOLD: i64 = 3;
pub const DEFAULT_MAX_MESSAGE_BYTES: i64 = 1_048_576;
pub const DEFAULT_CUSTOM_EVENT_MAX_DEPTH: i64 = 4;

/// Core Hook 名称。插件只能订阅，不能删除这些阶段。
pub const CORE_HOOKS: [&str; 29] = [
    "app.start.before",
    "app.start.after",
    "app.stop.before",
    "app.stop.after",
    "workspace.switch.before",
    "workspace.switch.after",
    "workspace.switch.error",
    "session.start.after",
    "session.resume.before",
    "session.resume.after",
    "session.resume.error",
    "session.close.before",
    "session.close.after",
    "turn.start",
    "turn.end",
    "turn.error",
    "turn.cancelled",
    "context.build.before",
    "context.build.after",
    "context.compaction.after_turn",
    "model.request.before",
    "model.response.after",
    "model.request.error",
    "tool.call.before",
    "tool.approval.before",
    "tool.approval.after",
    "tool.execute.before",
    "tool.execute.after",
    "tool.execute.error",
];

/// 每个 Hook 允许的 Handler 模式。
pub const HOOK_ALLOWED_MODES: &[(&str, &[&str])] = &[
    (
        "app.start.before",
        &[HANDLER_MODE_OBSERVE, HANDLER_MODE_GUARD],
    ),
    ("app.start.after", &[HANDLER_MODE_NOTIFY]),
    ("app.stop.before", &[HANDLER_MODE_OBSERVE]),
    ("app.stop.after", &[HANDLER_MODE_NOTIFY]),
    ("workspace.switch.before", &[HANDLER_MODE_GUARD]),
    ("workspace.switch.after", &[HANDLER_MODE_NOTIFY]),
    ("workspace.switch.error", &[HANDLER_MODE_NOTIFY]),
    ("session.start.after", &[HANDLER_MODE_NOTIFY]),
    ("session.resume.before", &[HANDLER_MODE_GUARD]),
    ("session.resume.after", &[HANDLER_MODE_NOTIFY]),
    ("session.resume.error", &[HANDLER_MODE_NOTIFY]),
    ("session.close.before", &[HANDLER_MODE_OBSERVE]),
    ("session.close.after", &[HANDLER_MODE_NOTIFY]),
    ("turn.start", &[HANDLER_MODE_TRANSFORM, HANDLER_MODE_GUARD]),
    ("turn.end", &[HANDLER_MODE_NOTIFY]),
    ("turn.error", &[HANDLER_MODE_NOTIFY]),
    ("turn.cancelled", &[HANDLER_MODE_NOTIFY]),
    ("context.build.before", &[HANDLER_MODE_TRANSFORM]),
    ("context.build.after", &[HANDLER_MODE_OBSERVE]),
    ("context.compaction.after_turn", &[HANDLER_MODE_NOTIFY]),
    (
        "model.request.before",
        &[HANDLER_MODE_TRANSFORM, HANDLER_MODE_GUARD],
    ),
    ("model.response.after", &[HANDLER_MODE_OBSERVE]),
    ("model.request.error", &[HANDLER_MODE_NOTIFY]),
    (
        "tool.call.before",
        &[HANDLER_MODE_TRANSFORM, HANDLER_MODE_GUARD],
    ),
    (
        "tool.approval.before",
        &[HANDLER_MODE_OBSERVE, HANDLER_MODE_GUARD],
    ),
    ("tool.approval.after", &[HANDLER_MODE_NOTIFY]),
    ("tool.execute.before", &[HANDLER_MODE_GUARD]),
    (
        "tool.execute.after",
        &[HANDLER_MODE_OBSERVE, HANDLER_MODE_TRANSFORM],
    ),
    ("tool.execute.error", &[HANDLER_MODE_NOTIFY]),
];

/// transform 白名单路径（RFC 6902 风格，`*` 表示单段通配）。
pub const HOOK_PATCH_ALLOWLIST: &[(&str, &[&str])] = &[
    (
        "turn.start",
        &["/payload/userText", "/payload/tags", "/payload/tags/-"],
    ),
    ("context.build.before", &["/payload/additionalContext"]),
    (
        "model.request.before",
        &[
            "/payload/messages/*/content",
            "/payload/temperature",
            "/payload/top_p",
            "/payload/max_tokens",
        ],
    ),
    (
        "tool.call.before",
        &["/payload/arguments", "/payload/arguments/*"],
    ),
    (
        "tool.execute.after",
        &["/payload/displayText", "/payload/annotations"],
    ),
];

/// 不能被 replace / tombstone 的 sealed Handler 前缀。
pub const SEALED_HANDLER_PREFIX: &str = "core/";

const POLICY_VALUES: [&str; 3] = ["reject-operation", "ignore", "skip-handler"];

/// 单个 Core Hook 的失败与 deny 策略（对应 Python 的 `HookPolicy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookPolicy {
    pub on_deny: &'static str,
    pub on_timeout: &'static str,
    pub on_protocol_error: &'static str,
    pub on_handler_error: &'static str,
    pub disable_plugin_on_error: bool,
}

impl Default for HookPolicy {
    fn default() -> Self {
        Self {
            on_deny: "reject-operation",
            on_timeout: "skip-handler",
            on_protocol_error: "skip-handler",
            on_handler_error: "skip-handler",
            disable_plugin_on_error: false,
        }
    }
}

impl HookPolicy {
    /// 校验四个策略字段的取值；越界文案与 Python 的 `__post_init__` 一致。
    pub fn validate(self) -> Result<Self, PluginError> {
        for (field, value) in [
            ("on_deny", self.on_deny),
            ("on_timeout", self.on_timeout),
            ("on_protocol_error", self.on_protocol_error),
            ("on_handler_error", self.on_handler_error),
        ] {
            if !POLICY_VALUES.contains(&value) {
                return Err(PluginError::new(format!(
                    "HookPolicy.{field} 非法：{value}"
                )));
            }
        }
        Ok(self)
    }
}

/// 设计文档 §5.2：显式 deny 与故障分开处理。未列出的 Hook 没有策略（与 Python 的查表一致）。
pub fn hook_policy(hook: &str) -> Option<HookPolicy> {
    let reject_all = HookPolicy {
        on_deny: "reject-operation",
        on_timeout: "reject-operation",
        on_protocol_error: "reject-operation",
        on_handler_error: "reject-operation",
        disable_plugin_on_error: false,
    };
    let ignore_deny = HookPolicy {
        on_deny: "ignore",
        on_timeout: "skip-handler",
        on_protocol_error: "skip-handler",
        on_handler_error: "skip-handler",
        disable_plugin_on_error: false,
    };
    Some(match hook {
        "app.start.before" => HookPolicy {
            on_deny: "reject-operation",
            disable_plugin_on_error: true,
            ..ignore_deny
        },
        "app.start.after" | "app.stop.before" | "app.stop.after" => ignore_deny,
        "workspace.switch.before" | "session.resume.before" => reject_all,
        "workspace.switch.after"
        | "workspace.switch.error"
        | "session.start.after"
        | "session.resume.after"
        | "session.resume.error"
        | "session.close.before"
        | "session.close.after"
        | "turn.end"
        | "turn.error"
        | "turn.cancelled"
        | "context.build.before"
        | "context.build.after"
        | "context.compaction.after_turn"
        | "model.response.after"
        | "model.request.error"
        | "tool.approval.after"
        | "tool.execute.after"
        | "tool.execute.error" => ignore_deny,
        "turn.start"
        | "model.request.before"
        | "tool.call.before"
        | "tool.approval.before"
        | "tool.execute.before" => reject_all,
        _ => return None,
    })
}

/// 每个 Hook 允许的 Handler 模式；未知 Hook 返回空（与 Python 的 `frozenset()` 回落一致）。
pub fn hook_allowed_modes(hook: &str) -> &'static [&'static str] {
    HOOK_ALLOWED_MODES
        .iter()
        .find(|(name, _)| *name == hook)
        .map(|(_, modes)| *modes)
        .unwrap_or(&[])
}

/// Hook 的 transform 白名单；未知 Hook 或未声明白名单时返回空。
pub fn hook_patch_allowlist(hook: &str) -> &'static [&'static str] {
    HOOK_PATCH_ALLOWLIST
        .iter()
        .find(|(name, _)| *name == hook)
        .map(|(_, paths)| *paths)
        .unwrap_or(&[])
}

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
    pub fn is_custom_hook(&self) -> bool {
        self.hook.starts_with("plugin.")
    }
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
    pub fn handler_keys(&self) -> Vec<String> {
        self.hooks
            .iter()
            .map(|item| format!("{}/{}", self.name, item.id))
            .collect()
    }
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

    /// `trace` 的默认值：`{"depth": 0}`。
    pub fn default_trace() -> Map<String, Value> {
        let mut trace = Map::new();
        trace.insert("depth".to_string(), Value::from(0));
        trace
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

    pub fn raise_if_denied(&self) -> Result<(), crate::error::PluginDispatchError> {
        if self.denied {
            let reason = if self.deny_reason.is_empty() {
                format!("Hook {} 被拒绝。", self.hook)
            } else {
                self.deny_reason.clone()
            };
            return Err(crate::error::PluginDispatchError::new(reason));
        }
        Ok(())
    }
}

/// ISO-8601（UTC、秒精度、`Z` 结尾）：对应
/// `datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")`。
pub fn utc_now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// 32 位十六进制事件 id，与 Python 的 `uuid.uuid4().hex` 同形。
///
/// 这里不是密码学随机源：事件 id 只需要在一台机器上不撞车，
/// 所以用「时间戳 + 进程 id + 计数器」做散列（与会话层的 `random_event_id` 同一手法）。
pub fn new_event_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = Sha256::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(std::thread::current().name().unwrap_or_default().as_bytes());
    let digest = hasher.finalize();
    hex(&digest[..16])
}

/// 对任意 JSON 可序列化值计算稳定短哈希，用于审计而非完整性锁。
pub fn stable_hash(value: &Value) -> String {
    let encoded = json_dumps_stable(value);
    let digest = Sha256::digest(encoded.as_bytes());
    hex(&digest[..8])
}

/// 对应 `json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`。
///
/// `serde_json` 保插入序，所以要先把对象键排成字典序再序列化；
/// 转义与分隔符风格与 Python 一致（UTF-8 原样输出、无空格分隔）。
pub fn json_dumps_stable(value: &Value) -> String {
    serde_json::to_string(&sort_value(value)).unwrap_or_default()
}

fn sort_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), sort_value(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// `[0-9a-f]` 十六进制小写写法（与 Python 的 `bytes.hex()` 同形）。
pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// 把 NPM 包名规范化为自定义 Hook 命名空间片段。
pub fn normalize_plugin_namespace(package_name: &str) -> String {
    let mut text = package_name.trim().to_lowercase();
    if let Some(rest) = text.strip_prefix('@') {
        text = rest.to_string();
    }
    text = text.replace('/', "-");
    let filtered: String = text
        .chars()
        .map(|ch| {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let mut collapsed = String::with_capacity(filtered.len());
    let mut previous_dash = false;
    for ch in filtered.chars() {
        if ch == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        collapsed.push(ch);
    }
    let trimmed = collapsed.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "plugin".to_string()
    } else {
        trimmed
    }
}

/// 各模式的默认超时（对应 Python 的 `default_timeout_for_mode`）。
pub fn default_timeout_for_mode(mode: &str) -> i64 {
    match mode {
        HANDLER_MODE_TRANSFORM => DEFAULT_TRANSFORM_TIMEOUT_MS,
        HANDLER_MODE_GUARD => DEFAULT_GUARD_TIMEOUT_MS,
        HANDLER_MODE_NOTIFY => DEFAULT_NOTIFY_TIMEOUT_MS,
        _ => DEFAULT_OBSERVE_TIMEOUT_MS,
    }
}

/// 执行顺序：guard → transform → observe/notify。
pub fn mode_rank(mode: &str) -> i64 {
    match mode {
        HANDLER_MODE_GUARD => 0,
        HANDLER_MODE_TRANSFORM => 1,
        HANDLER_MODE_OBSERVE | HANDLER_MODE_NOTIFY => 2,
        _ => 9,
    }
}

/// project 高于 user：排序时更小的 rank 先执行。
pub fn scope_rank(scope: &str) -> i64 {
    if scope == "project" {
        0
    } else {
        1
    }
}

/// 稳定排序：模式 → 优先级降序 → 作用域 → 插件名 → Handler id。
pub fn sort_handlers(handlers: &[ResolvedHandler]) -> Vec<ResolvedHandler> {
    let mut sorted = handlers.to_vec();
    sorted.sort_by(|left, right| {
        mode_rank(&left.mode)
            .cmp(&mode_rank(&right.mode))
            .then((-left.priority).cmp(&(-right.priority)))
            .then(scope_rank(&left.scope).cmp(&scope_rank(&right.scope)))
            .then(left.plugin_name.cmp(&right.plugin_name))
            .then(left.handler_id.cmp(&right.handler_id))
    });
    sorted
}

fn version_ref_value(reference: Option<&PluginVersionRef>) -> Value {
    match reference {
        Some(item) => Value::Object(item.to_dict()),
        None => Value::Null,
    }
}

fn option_value(value: &Option<String>) -> Value {
    match value {
        Some(text) => Value::from(text.clone()),
        None => Value::Null,
    }
}

/// 与 Python `str()` 的差异见 crate README：非字符串一律取原样文本，`None` 回落空串。
pub(crate) fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => if *flag { "True" } else { "False" }.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// `data.get(primary, data.get(fallback, ""))`：primary 存在就用它（含 null）。
pub(crate) fn alias_text(map: &Map<String, Value>, primary: &str, fallback: &str) -> String {
    match map.get(primary) {
        Some(value) => text_of(Some(value)),
        None => text_of(map.get(fallback)),
    }
}

pub(crate) fn bool_of(value: Option<&Value>, default: bool) -> bool {
    match value {
        None => default,
        Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

pub(crate) fn bool_alias(
    map: &Map<String, Value>,
    primary: &str,
    fallback: &str,
    default: bool,
) -> bool {
    match map.get(primary) {
        Some(value) => bool_of(Some(value), false),
        None => bool_of(map.get(fallback), default),
    }
}

pub(crate) fn int_of(value: Option<&Value>, default: i64) -> i64 {
    match value {
        None | Some(Value::Null) => default,
        Some(Value::Bool(flag)) => i64::from(*flag),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item as i64))
            .unwrap_or(default),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(default),
        _ => default,
    }
}

pub(crate) fn int_alias(
    map: &Map<String, Value>,
    primary: &str,
    fallback: &str,
    default: i64,
) -> i64 {
    match map.get(primary) {
        Some(value) => int_of(Some(value), default),
        None => int_of(map.get(fallback), default),
    }
}

/// 字符串数组读取；非数组一律按空数组处理（Python 对字符串会逐字符迭代，这里不复现该怪癖）。
pub(crate) fn string_list_alias(
    map: &Map<String, Value>,
    primary: &str,
    fallback: &str,
) -> Vec<String> {
    let raw = match map.get(primary) {
        Some(value) => value,
        None => match map.get(fallback) {
            Some(value) => value,
            None => return Vec::new(),
        },
    };
    match raw {
        Value::Array(items) => items.iter().map(|item| text_of(Some(item))).collect(),
        _ => Vec::new(),
    }
}

/// Python 的真值测试：空容器、0、空串、null、false 都是假。
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !*flag,
        Value::Number(number) => number.as_f64().map(|item| item == 0.0).unwrap_or(false),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// `^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$`
fn is_valid_handler_id(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || chars.len() > 64 {
        return false;
    }
    if !chars[0].is_ascii_alphanumeric() {
        return false;
    }
    chars[1..]
        .iter()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// `^plugin\.[a-z0-9][a-z0-9-]{0,62}(?:\.[a-z0-9][a-z0-9-]{0,62})+$`
fn is_valid_custom_event(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() < 3 || parts[0] != "plugin" {
        return false;
    }
    parts[1..].iter().all(|part| {
        let chars: Vec<char> = part.chars().collect();
        if chars.is_empty() || chars.len() > 63 {
            return false;
        }
        let first = chars[0];
        (first.is_ascii_lowercase() || first.is_ascii_digit())
            && chars[1..]
                .iter()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-')
    })
}

/// `utf-8-sig`：读到 BOM 就剥掉，其余按 UTF-8 解码（非法字节按替换字符处理）。
pub(crate) fn decode_utf8_sig(bytes: &[u8]) -> String {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(body).to_string()
}

/// SemVer 2.0.0 的官方正则（Python 侧 `_SEMVER_RE`）的手写等价物。
pub fn is_valid_semver(version: &str) -> bool {
    let text = version.trim();
    let main = match text.split_once('+') {
        Some((left, right)) => {
            if !is_valid_build_metadata(right) {
                return false;
            }
            left
        }
        None => text,
    };
    let core = match main.split_once('-') {
        Some((left, right)) => {
            if !is_valid_prerelease(right) {
                return false;
            }
            left
        }
        None => main,
    };
    is_valid_core_version(core)
}

fn is_valid_core_version(core: &str) -> bool {
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| is_zero_or_positive(part))
}

/// `(0|[1-9]\d*)`
fn is_zero_or_positive(text: &str) -> bool {
    if text == "0" {
        return true;
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_digit() && first != '0' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_digit())
}

/// `(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.…)*`
fn is_valid_prerelease(text: &str) -> bool {
    !text.is_empty() && text.split('.').all(is_valid_prerelease_identifier)
}

fn is_valid_prerelease_identifier(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    if is_zero_or_positive(text) {
        return true;
    }
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index >= bytes.len() {
        return false;
    }
    let marker = bytes[index];
    if !(marker.is_ascii_alphabetic() || marker == b'-') {
        return false;
    }
    bytes[index..]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

/// `(?:[0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*)`
fn is_valid_build_metadata(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
}

/// `^(?:@[a-z0-9-~][a-z0-9-._~]*/)?[a-z0-9-~][a-z0-9-._~]*$`
pub fn is_valid_npm_name(name: &str) -> bool {
    let text = name.trim();
    let remainder = match text.strip_prefix('@') {
        Some(after) => match after.split_once('/') {
            Some((scope, rest)) => {
                if !is_valid_npm_segment(scope) {
                    return false;
                }
                rest
            }
            None => return false,
        },
        None => text,
    };
    is_valid_npm_segment(remainder)
}

fn is_valid_npm_segment(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first)
            if first.is_ascii_lowercase()
                || first.is_ascii_digit()
                || first == '-'
                || first == '~' => {}
        _ => return false,
    }
    chars.all(|ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '.' | '_' | '~')
    })
}

/// 从 config.toml 的 plugins 段构造配置；缺省时默认关闭插件。
pub fn parse_plugins_config(data: Option<&Value>) -> Result<PluginsConfig, PluginError> {
    let value = match data {
        Some(item) if !is_falsy(item) => item,
        _ => return Ok(PluginsConfig::default()),
    };
    let Some(map) = value.as_object() else {
        return Err(PluginError::new("plugins 配置必须是 JSON 对象。"));
    };
    PluginsConfig {
        enabled: bool_of(map.get("enabled"), false),
        default_timeout_ms: int_alias(map, "default_timeout_ms", "defaultTimeoutMs", 1000),
        max_timeout_ms: int_alias(
            map,
            "max_timeout_ms",
            "maxTimeoutMs",
            DEFAULT_MAX_TIMEOUT_MS,
        ),
        failure_threshold: int_alias(
            map,
            "failure_threshold",
            "failureThreshold",
            DEFAULT_FAILURE_THRESHOLD,
        ),
        max_message_bytes: int_alias(
            map,
            "max_message_bytes",
            "maxMessageBytes",
            DEFAULT_MAX_MESSAGE_BYTES,
        ),
        custom_event_max_depth: int_alias(
            map,
            "custom_event_max_depth",
            "customEventMaxDepth",
            DEFAULT_CUSTOM_EVENT_MAX_DEPTH,
        ),
        allow_network_install: bool_alias(
            map,
            "allow_network_install",
            "allowNetworkInstall",
            false,
        ),
        audit_log_enabled: bool_alias(map, "audit_log_enabled", "auditLogEnabled", true),
    }
    .validate()
}

/// 解析一个 Handler 声明。
pub fn parse_handler_registration(raw: &Value) -> Result<HandlerRegistration, PluginManifestError> {
    let Some(map) = raw.as_object() else {
        return Err(PluginManifestError::new("hooks 项必须是对象。"));
    };
    let handler_id = text_of(map.get("id")).trim().to_string();
    let hook = text_of(map.get("hook")).trim().to_string();
    let mode = text_of(map.get("mode")).trim().to_string();
    if !is_valid_handler_id(&handler_id) {
        return Err(PluginManifestError::new(format!(
            "非法 Handler ID：{handler_id}"
        )));
    }
    if !HANDLER_MODES.contains(&mode.as_str()) {
        return Err(PluginManifestError::new(format!(
            "Handler {handler_id} mode 非法：{mode}"
        )));
    }
    if CORE_HOOKS.contains(&hook.as_str()) {
        if !hook_allowed_modes(&hook).contains(&mode.as_str()) {
            return Err(PluginManifestError::new(format!(
                "Hook {hook} 不支持 mode={mode}"
            )));
        }
    } else if hook.starts_with("plugin.") {
        if !is_valid_custom_event(&hook) {
            return Err(PluginManifestError::new(format!(
                "自定义 Hook 名称非法：{hook}"
            )));
        }
    } else {
        return Err(PluginManifestError::new(format!("未知 Hook：{hook}")));
    }

    let priority = int_of(map.get("priority"), 0);
    let replaces = match map.get("replaces") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| text_of(Some(item)).trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        Some(_) => {
            return Err(PluginManifestError::new(format!(
                "Handler {handler_id} replaces 必须是数组。"
            )))
        }
    };
    for target in &replaces {
        if target.starts_with(SEALED_HANDLER_PREFIX) {
            return Err(PluginManifestError::new(format!(
                "不能替换 sealed Handler：{target}"
            )));
        }
        if !target.contains('/') {
            return Err(PluginManifestError::new(format!(
                "replaces 必须是 package/handler 形式：{target}"
            )));
        }
    }

    let timeout_ms = match map.get("timeoutMs").or_else(|| map.get("timeout_ms")) {
        Some(value) if !value.is_null() => Some(int_of(Some(value), 0)),
        _ => None,
    };
    let event_version = match map.get("eventVersion").or_else(|| map.get("event_version")) {
        Some(value) if !value.is_null() => Some(text_of(Some(value))),
        _ => None,
    };
    Ok(HandlerRegistration {
        id: handler_id,
        hook,
        mode,
        priority,
        replaces,
        event_version,
        timeout_ms,
    })
}

/// 解析一个自定义事件声明。
pub fn parse_custom_event(
    raw: &Value,
    package_name: &str,
) -> Result<CustomEventDeclaration, PluginManifestError> {
    let Some(map) = raw.as_object() else {
        return Err(PluginManifestError::new("customEvents 项必须是对象。"));
    };
    let name = text_of(map.get("name")).trim().to_string();
    let namespace = normalize_plugin_namespace(package_name);
    let expected_prefix = format!("plugin.{namespace}.");
    if !name.starts_with(&expected_prefix) {
        return Err(PluginManifestError::new(format!(
            "自定义事件必须位于本插件命名空间 {expected_prefix}*，当前：{name}"
        )));
    }
    if !is_valid_custom_event(&name) {
        return Err(PluginManifestError::new(format!(
            "自定义事件名称非法：{name}"
        )));
    }
    let version = int_of(map.get("version"), 1);
    if version < 1 {
        return Err(PluginManifestError::new(format!(
            "自定义事件版本必须 >= 1：{name}"
        )));
    }
    let visibility = match map.get("visibility") {
        Some(value) => text_of(Some(value)).trim().to_string(),
        None => "private".to_string(),
    };
    if visibility != "private" && visibility != "public" {
        return Err(PluginManifestError::new(format!(
            "自定义事件 visibility 非法：{visibility}"
        )));
    }
    let schema = match map.get("schema") {
        Some(Value::Object(value)) => value.clone(),
        None | Some(Value::Null) => {
            let mut default_schema = Map::new();
            default_schema.insert("type".to_string(), Value::from("object"));
            default_schema
        }
        Some(_) => {
            return Err(PluginManifestError::new(format!(
                "自定义事件 schema 必须是对象：{name}"
            )))
        }
    };
    Ok(CustomEventDeclaration {
        name,
        version,
        visibility,
        schema,
    })
}

/// 解析并校验 package.json 中的 omnicrawl 扩展元数据。
pub fn parse_plugin_manifest(
    package_data: &Value,
    source_path: &str,
) -> Result<PluginManifest, PluginManifestError> {
    let Some(data) = package_data.as_object() else {
        return Err(PluginManifestError::new("package.json 顶层必须是对象。"));
    };
    let name = text_of(data.get("name")).trim().to_string();
    let version = text_of(data.get("version")).trim().to_string();
    if !is_valid_npm_name(&name) {
        return Err(PluginManifestError::new(format!("非法 NPM 包名：{name}")));
    }
    if !is_valid_semver(&version) {
        return Err(PluginManifestError::new(format!("非法 SemVer：{version}")));
    }

    let Some(omnicrawl) = data.get("omnicrawl").and_then(Value::as_object) else {
        return Err(PluginManifestError::new("缺少 omnicrawl 扩展字段。"));
    };

    let api_version = alias_text(omnicrawl, "apiVersion", "api_version")
        .trim()
        .to_string();
    if api_version != HOOK_API_VERSION {
        return Err(PluginManifestError::new(format!(
            "不支持的 apiVersion：{api_version}，当前仅接受 {HOOK_API_VERSION}。"
        )));
    }

    let engines = match omnicrawl.get("engines") {
        Some(Value::Object(value)) => value.clone(),
        None | Some(Value::Null) => Map::new(),
        Some(_) => {
            return Err(PluginManifestError::new("omnicrawl.engines 必须是对象。"));
        }
    };
    let engines_omnicrawl = text_of(engines.get("omnicrawl")).trim().to_string();
    let engines_node = text_of(engines.get("node")).trim().to_string();
    if engines_omnicrawl.is_empty() {
        return Err(PluginManifestError::new(
            "缺少 omnicrawl.engines.omnicrawl。",
        ));
    }
    if engines_node.is_empty() {
        return Err(PluginManifestError::new("缺少 omnicrawl.engines.node。"));
    }

    let entry = match omnicrawl.get("entry") {
        Some(value) => text_of(Some(value)).trim().to_string(),
        None => text_of(data.get("main")).trim().to_string(),
    };
    if entry.is_empty() {
        return Err(PluginManifestError::new("缺少 omnicrawl.entry。"));
    }
    let normalized_entry = entry.replace('\\', "/");
    if entry.starts_with('/')
        || entry.starts_with('\\')
        || normalized_entry.split('/').any(|part| part == "..")
    {
        return Err(PluginManifestError::new(format!(
            "entry 路径非法或不允许逃逸：{entry}"
        )));
    }

    let permissions = match omnicrawl.get("permissions") {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| text_of(Some(item)).trim().to_string())
            .filter(|item| !item.is_empty())
            .collect::<Vec<String>>(),
        _ => {
            return Err(PluginManifestError::new(
                "omnicrawl.permissions 必须是非空数组。",
            ));
        }
    };

    let agents_raw = match omnicrawl.get("agents") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => {
            return Err(PluginManifestError::new(
                "omnicrawl.agents 必须是包内 Markdown 相对路径数组。",
            ));
        }
    };
    if !agents_raw.iter().all(Value::is_string) {
        return Err(PluginManifestError::new(
            "omnicrawl.agents 必须是包内 Markdown 相对路径数组。",
        ));
    }
    let mut agents: Vec<String> = Vec::new();
    for raw_path in &agents_raw {
        let raw_text = raw_path.as_str().unwrap_or_default();
        let normalized = raw_text.trim().replace('\\', "/");
        let parts: Vec<&str> = normalized.split('/').collect();
        let invalid = normalized.is_empty()
            || normalized.starts_with('/')
            || parts[0].contains(':')
            || parts.contains(&"..")
            || !normalized.to_lowercase().ends_with(".md");
        if invalid {
            return Err(PluginManifestError::new(format!(
                "omnicrawl.agents 路径非法或不允许逃逸：{raw_text}"
            )));
        }
        if agents.contains(&normalized) {
            return Err(PluginManifestError::new(format!(
                "omnicrawl.agents 不允许重复路径：{normalized}"
            )));
        }
        agents.push(normalized);
    }
    if !agents.is_empty() && !permissions.iter().any(|item| item == "agent:definitions") {
        return Err(PluginManifestError::new(
            "声明 omnicrawl.agents 需要 agent:definitions 权限。",
        ));
    }

    let hooks_raw = match omnicrawl.get("hooks") {
        Some(Value::Array(items)) => items.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => {
            return Err(PluginManifestError::new("omnicrawl.hooks 必须是数组。"));
        }
    };
    if hooks_raw.is_empty() && agents.is_empty() {
        return Err(PluginManifestError::new(
            "omnicrawl.hooks 必须是非空数组，或声明 omnicrawl.agents。",
        ));
    }
    let mut hooks: Vec<HandlerRegistration> = Vec::new();
    for item in &hooks_raw {
        hooks.push(parse_handler_registration(item)?);
    }
    let mut handler_ids: Vec<&str> = hooks.iter().map(|item| item.id.as_str()).collect();
    handler_ids.sort_unstable();
    for pair in handler_ids.windows(2) {
        if pair[0] == pair[1] {
            return Err(PluginManifestError::new("Handler ID 在包内必须唯一。"));
        }
    }

    for handler in &hooks {
        let required_perm = format!("hook:{}", handler.hook);
        if handler.hook.starts_with("plugin.") {
            // 订阅自定义 Hook 使用 hook:custom-subscribe；发布权限另计。
            if !permissions
                .iter()
                .any(|item| item == "hook:custom-subscribe" || *item == required_perm)
            {
                return Err(PluginManifestError::new(format!(
                    "Handler {} 订阅自定义 Hook 需要 hook:custom-subscribe 权限。",
                    handler.id
                )));
            }
        } else if !permissions.contains(&required_perm) {
            return Err(PluginManifestError::new(format!(
                "Handler {} 注册 {} 需要权限 {required_perm}。",
                handler.id, handler.hook
            )));
        }
    }

    let custom_events_raw = match omnicrawl
        .get("customEvents")
        .or_else(|| omnicrawl.get("custom_events"))
    {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => {
            return Err(PluginManifestError::new(
                "omnicrawl.customEvents 必须是数组。",
            ));
        }
    };
    let mut custom_events: Vec<CustomEventDeclaration> = Vec::new();
    if !custom_events_raw.is_empty() {
        if !permissions.iter().any(|item| item == "hook:custom-emit") {
            return Err(PluginManifestError::new(
                "声明 customEvents 需要 hook:custom-emit 权限。",
            ));
        }
        for item in &custom_events_raw {
            custom_events.push(parse_custom_event(item, &name)?);
        }
    }

    let timeout_ms = match omnicrawl
        .get("timeoutMs")
        .or_else(|| omnicrawl.get("timeout_ms"))
    {
        Some(value) if !value.is_null() => Some(int_of(Some(value), 0)),
        _ => None,
    };
    let package_type = {
        let text = text_of(data.get("type")).trim().to_string();
        if text.is_empty() {
            "module".to_string()
        } else {
            text
        }
    };

    Ok(PluginManifest {
        name,
        version,
        api_version,
        entry: normalized_entry,
        permissions,
        hooks,
        engines_omnicrawl,
        engines_node,
        timeout_ms,
        custom_events,
        agents,
        package_type,
        source_path: source_path.to_string(),
    })
}

/// 对自定义事件 payload 做 V1 轻量 schema 校验；不引入 jsonschema 依赖，避免安装面扩大。
pub fn validate_payload_against_schema(
    payload: &Value,
    schema: Option<&Map<String, Value>>,
) -> Result<(), PluginManifestError> {
    let schema = match schema {
        Some(value) if !value.is_empty() => value,
        _ => {
            if !payload.is_null() && !payload.is_object() {
                return Err(PluginManifestError::new(
                    "自定义事件 payload 默认必须是对象。",
                ));
            }
            return Ok(());
        }
    };

    let expected_type = text_of(schema.get("type"));
    if expected_type == "object" {
        let Some(payload_map) = payload.as_object() else {
            return Err(PluginManifestError::new("自定义事件 payload 必须是对象。"));
        };
        // Python 里 properties 缺省是 `{}`、显式 null 是 `None`，两者的 additionalProperties
        // 行为不同：前者把未声明字段全判为多余，后者跳过检查。
        let properties = match schema.get("properties") {
            Some(Value::Object(value)) => Some(value.clone()),
            Some(Value::Null) => None,
            None => Some(Map::new()),
            Some(_) => {
                return Err(PluginManifestError::new("schema.properties 必须是对象。"));
            }
        };
        let required = match schema.get("required") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(_) => {
                return Err(PluginManifestError::new("schema.required 必须是数组。"));
            }
        };
        for key in &required {
            let key_text = text_of(Some(key));
            if !payload_map.contains_key(&key_text) {
                return Err(PluginManifestError::new(format!(
                    "自定义事件 payload 缺少必填字段：{key_text}"
                )));
            }
        }
        let additional = bool_of(schema.get("additionalProperties"), true);
        if !additional {
            if let Some(declared) = &properties {
                let mut extras: Vec<String> = payload_map
                    .keys()
                    .filter(|key| !declared.contains_key(*key))
                    .cloned()
                    .collect();
                if !extras.is_empty() {
                    extras.sort();
                    return Err(PluginManifestError::new(format!(
                        "自定义事件 payload 含未声明字段：{}",
                        extras.join(", ")
                    )));
                }
            }
        }
        if let Some(declared) = &properties {
            for (key, child_schema) in declared {
                let Some(value) = payload_map.get(key) else {
                    continue;
                };
                let Some(child_map) = child_schema.as_object() else {
                    continue;
                };
                let child_type = text_of(child_map.get("type"));
                let mismatch = match child_type.as_str() {
                    "string" => !value.is_string(),
                    "number" => !value.is_number(),
                    "integer" => value.as_i64().is_none(),
                    "boolean" => !value.is_boolean(),
                    "object" => !value.is_object(),
                    "array" => !value.is_array(),
                    _ => false,
                };
                if mismatch {
                    return Err(PluginManifestError::new(format!(
                        "字段 {key} 必须是 {child_type}"
                    )));
                }
            }
        }
        return Ok(());
    }
    if expected_type == "array" {
        if !payload.is_array() {
            return Err(PluginManifestError::new("自定义事件 payload 必须是数组。"));
        }
        return Ok(());
    }
    if expected_type == "string" {
        if !payload.is_string() {
            return Err(PluginManifestError::new(
                "自定义事件 payload 必须是 string。",
            ));
        }
        return Ok(());
    }
    // 未识别 type 时仅要求可 JSON 序列化对象/标量。
    if !payload.is_null()
        && !payload.is_object()
        && !payload.is_array()
        && !payload.is_string()
        && !payload.is_number()
        && !payload.is_boolean()
    {
        return Err(PluginManifestError::new(
            "自定义事件 payload 类型不受支持。",
        ));
    }
    Ok(())
}

/// 判断 JSON Pointer 是否命中 allowlist；支持单段 `*` 通配。
pub fn path_matches_allowlist(path: &str, allowlist: &[&str]) -> bool {
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    allowlist
        .iter()
        .any(|pattern| pointer_match(&normalized, pattern))
}

fn pointer_match(path: &str, pattern: &str) -> bool {
    let path_parts: Vec<&str> = path.split('/').skip(1).collect();
    let pattern_parts: Vec<&str> = pattern.split('/').skip(1).collect();
    if path_parts.len() != pattern_parts.len() {
        // 允许数组尾插：/payload/tags/- 与 /payload/tags 同级规则由 allowlist 显式列出。
        return false;
    }
    path_parts
        .iter()
        .zip(pattern_parts.iter())
        .all(|(actual, expected)| *expected == "*" || actual == expected)
}

/// 校验 transform Patch，并返回规范化后的操作列表。
pub fn validate_json_patch(
    patch: &Value,
    hook: &str,
) -> Result<Vec<Map<String, Value>>, PluginManifestError> {
    let allowlist = hook_patch_allowlist(hook);
    if allowlist.is_empty() {
        return Err(PluginManifestError::new(format!(
            "Hook {hook} 不允许 transform Patch。"
        )));
    }
    let Some(items) = patch.as_array() else {
        return Err(PluginManifestError::new("patch 必须是数组。"));
    };
    let mut normalized: Vec<Map<String, Value>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(map) = item.as_object() else {
            return Err(PluginManifestError::new(format!(
                "patch[{index}] 必须是对象。"
            )));
        };
        let op = text_of(map.get("op")).trim().to_string();
        let path = text_of(map.get("path")).trim().to_string();
        if !JSON_PATCH_OPS.contains(&op.as_str()) {
            return Err(PluginManifestError::new(format!(
                "patch[{index}] 不支持 op={op}，V1 仅允许 add/replace/remove。"
            )));
        }
        if !path.starts_with('/') {
            return Err(PluginManifestError::new(format!(
                "patch[{index}].path 必须是 JSON Pointer：{path}"
            )));
        }
        if !path_matches_allowlist(&path, allowlist) {
            return Err(PluginManifestError::new(format!(
                "patch[{index}].path 不在 Hook {hook} 白名单：{path}"
            )));
        }
        let mut entry = Map::new();
        entry.insert("op".to_string(), Value::from(op.clone()));
        entry.insert("path".to_string(), Value::from(path));
        if op == "add" || op == "replace" {
            let Some(value) = map.get("value") else {
                return Err(PluginManifestError::new(format!(
                    "patch[{index}] 缺少 value。"
                )));
            };
            entry.insert("value".to_string(), value.clone());
        }
        normalized.push(entry);
    }
    Ok(normalized)
}

/// 在深拷贝上应用受限 JSON Patch：只实现 V1 需要的 add/replace/remove，以及数组尾插 `-`。
pub fn apply_json_patch(
    document: &Map<String, Value>,
    patch: &[Map<String, Value>],
) -> Result<Map<String, Value>, PluginManifestError> {
    let mut result = Value::Object(document.clone());
    for operation in patch {
        let op = text_of(operation.get("op"));
        let path = text_of(operation.get("path"));
        let parts: Vec<String> = path.split('/').skip(1).map(decode_pointer_token).collect();
        if parts.is_empty() {
            return Err(PluginManifestError::new("不允许 Patch 根文档。"));
        }
        let mut cursor = &mut result;
        for part in &parts[..parts.len() - 1] {
            cursor = descend(cursor, part)?;
        }
        let last = parts[parts.len() - 1].clone();
        apply_operation(cursor, &last, &op, operation.get("value"), &path)?;
    }
    match result {
        Value::Object(map) => Ok(map),
        _ => Err(PluginManifestError::new("Patch 结果不是对象。")),
    }
}

fn descend<'a>(cursor: &'a mut Value, key: &str) -> Result<&'a mut Value, PluginManifestError> {
    if let Value::Array(items) = cursor {
        let index: usize = key
            .parse()
            .map_err(|_| PluginManifestError::new(format!("Patch 路径的数组下标非法：{key}")))?;
        if index >= items.len() {
            return Err(PluginManifestError::new(format!(
                "Patch 路径的数组下标越界：{key}"
            )));
        }
        return Ok(&mut items[index]);
    }
    if !cursor.is_object() {
        *cursor = Value::Object(Map::new());
    }
    let map = cursor.as_object_mut().expect("已归一化为对象");
    let needs_container = !matches!(map.get(key), Some(Value::Object(_)) | Some(Value::Array(_)));
    if needs_container {
        map.insert(key.to_string(), Value::Object(Map::new()));
    }
    Ok(map.get_mut(key).expect("刚刚写入"))
}

fn apply_operation(
    parent: &mut Value,
    last: &str,
    op: &str,
    value: Option<&Value>,
    path: &str,
) -> Result<(), PluginManifestError> {
    match op {
        "add" => {
            let value = value.cloned().unwrap_or(Value::Null);
            if let Value::Array(items) = parent {
                if last == "-" {
                    items.push(value);
                } else {
                    let index: usize = last.parse().map_err(|_| {
                        PluginManifestError::new(format!("Patch 路径的数组下标非法：{last}"))
                    })?;
                    // Python 的 list.insert 越界会追加到末尾，这里保持同一语义。
                    if index >= items.len() {
                        items.push(value);
                    } else {
                        items.insert(index, value);
                    }
                }
            } else {
                let map = parent.as_object_mut().expect("非数组即对象");
                map.insert(last.to_string(), value);
            }
        }
        "replace" => {
            let value = value.cloned().unwrap_or(Value::Null);
            if let Value::Array(items) = parent {
                let index: usize = last.parse().map_err(|_| {
                    PluginManifestError::new(format!("Patch 路径的数组下标非法：{last}"))
                })?;
                if index >= items.len() {
                    return Err(PluginManifestError::new(format!(
                        "replace 目标不存在：{path}"
                    )));
                }
                items[index] = value;
            } else {
                let map = parent.as_object_mut().expect("非数组即对象");
                if !map.contains_key(last) {
                    return Err(PluginManifestError::new(format!(
                        "replace 目标不存在：{path}"
                    )));
                }
                map.insert(last.to_string(), value);
            }
        }
        "remove" => {
            if let Value::Array(items) = parent {
                let index: usize = last.parse().map_err(|_| {
                    PluginManifestError::new(format!("Patch 路径的数组下标非法：{last}"))
                })?;
                if index >= items.len() {
                    return Err(PluginManifestError::new(format!(
                        "remove 目标不存在：{path}"
                    )));
                }
                items.remove(index);
            } else {
                let map = parent.as_object_mut().expect("非数组即对象");
                if map.remove(last).is_none() {
                    return Err(PluginManifestError::new(format!(
                        "remove 目标不存在：{path}"
                    )));
                }
            }
        }
        other => {
            return Err(PluginManifestError::new(format!(
                "不支持的 Patch op：{other}"
            )));
        }
    }
    Ok(())
}

fn decode_pointer_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// 把 Worker 返回值规范化为 `HookResult`。
pub fn parse_hook_result(
    raw: Option<&Value>,
    handler_key: &str,
    elapsed_ms: f64,
) -> Result<HookResult, PluginProtocolError> {
    let Some(raw) = raw else {
        return Ok(HookResult::continue_result(
            handler_key,
            Map::new(),
            elapsed_ms,
            "continue",
        ));
    };
    if raw.is_null() {
        return Ok(HookResult::continue_result(
            handler_key,
            Map::new(),
            elapsed_ms,
            "continue",
        ));
    }
    let Some(map) = raw.as_object() else {
        return Err(PluginProtocolError::new(format!(
            "Handler {handler_key} 返回值必须是对象。"
        )));
    };
    let action = match map.get("action") {
        Some(value) => text_of(Some(value)).trim().to_string(),
        None => "continue".to_string(),
    };
    // Python 的 `raw.get("annotations") or {}`：空列表、空串、null 都退化成空对象。
    let annotations = match map.get("annotations") {
        None => Map::new(),
        Some(value) if is_falsy(value) => Map::new(),
        Some(Value::Object(value)) => value.clone(),
        Some(_) => {
            return Err(PluginProtocolError::new(format!(
                "Handler {handler_key} annotations 必须是对象。"
            )));
        }
    };
    match action.as_str() {
        "continue" => Ok(HookResult::continue_result(
            handler_key,
            annotations,
            elapsed_ms,
            "continue",
        )),
        "approve" => Err(PluginProtocolError::new(format!(
            "Handler {handler_key} 返回了非法 action=approve；插件只能 deny，不能批准。"
        ))),
        "deny" => {
            let reason = {
                let text = text_of(map.get("reason")).trim().to_string();
                if text.is_empty() {
                    "插件拒绝操作".to_string()
                } else {
                    text
                }
            };
            let code = text_of(map.get("code")).trim().to_string();
            Ok(HookResult::deny_result(
                reason.as_str(),
                &code,
                handler_key,
                elapsed_ms,
            ))
        }
        "patch" => {
            let patch = match map.get("patch") {
                Some(Value::Array(items)) => items.clone(),
                None | Some(Value::Null) => Vec::new(),
                Some(_) => {
                    return Err(PluginProtocolError::new(format!(
                        "Handler {handler_key} patch 必须是数组。"
                    )));
                }
            };
            let mut normalized: Vec<Map<String, Value>> = Vec::new();
            for item in &patch {
                let Some(value) = item.as_object() else {
                    return Err(PluginProtocolError::new(format!(
                        "Handler {handler_key} patch 必须是数组。"
                    )));
                };
                normalized.push(value.clone());
            }
            Ok(HookResult::patch_result(
                normalized,
                handler_key,
                annotations,
                elapsed_ms,
            ))
        }
        other => Err(PluginProtocolError::new(format!(
            "Handler {handler_key} action 非法：{other}"
        ))),
    }
}
