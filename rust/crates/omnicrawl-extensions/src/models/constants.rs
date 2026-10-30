//! 契约常量：版本、Handler 模式、超时默认值与 29 个核心 Hook 名单。
//!
//! 从 `models.rs` 按职责拆出。

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

pub(crate) const POLICY_VALUES: [&str; 3] = ["reject-operation", "ignore", "skip-handler"];
