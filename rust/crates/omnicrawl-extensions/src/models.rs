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

// 按职责拆出的子模块；所有公开项在此重新导出，`crate::models::X` 与 `models::*` 的用法不变。
mod constants;
mod hook_result;
mod hooks;
mod manifest;
mod parsing;
mod patch;
mod util;
mod validation;

pub use constants::{
    CORE_HOOKS, DEFAULT_CUSTOM_EVENT_MAX_DEPTH, DEFAULT_FAILURE_THRESHOLD,
    DEFAULT_GUARD_TIMEOUT_MS, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_MAX_TIMEOUT_MS,
    DEFAULT_NOTIFY_TIMEOUT_MS, DEFAULT_OBSERVE_TIMEOUT_MS, DEFAULT_TRANSFORM_TIMEOUT_MS,
    HANDLER_MODES, HANDLER_MODE_GUARD, HANDLER_MODE_NOTIFY, HANDLER_MODE_OBSERVE,
    HANDLER_MODE_TRANSFORM, HOOK_ALLOWED_MODES, HOOK_API_VERSION, HOOK_PATCH_ALLOWLIST,
    JSON_PATCH_OPS, OMNICRAWL_VERSION, PLUGIN_SCHEMA_VERSION, SEALED_HANDLER_PREFIX,
};
pub use hook_result::{
    parse_hook_result,
};
pub use hooks::{
    HookPolicy, hook_allowed_modes, hook_patch_allowlist, hook_policy,
};
pub use manifest::{
    CustomEventDeclaration, DispatchOutcome, HandlerRegistration, HookEvent, HookResult,
    PluginManifest, PluginRecord, PluginRegistryDocument, PluginVersionRef, PluginsConfig,
    ResolvedHandler,
};
pub use parsing::{
    parse_custom_event, parse_handler_registration, parse_plugin_manifest, parse_plugins_config,
};
pub use patch::{
    apply_json_patch, validate_json_patch,
};
pub use util::{
    default_timeout_for_mode, hex, is_valid_npm_name, is_valid_semver, json_dumps_stable,
    mode_rank, new_event_id, normalize_plugin_namespace, scope_rank, sort_handlers, stable_hash,
    utc_now_iso,
};
// 兄弟子模块靠 `use super::*` 取这些内部辅助，因此必须在父模块重导出为可见。
pub(crate) use constants::POLICY_VALUES;
pub(crate) use util::{
    alias_text, bool_alias, bool_of, decode_utf8_sig, int_alias, int_of, is_falsy,
    is_valid_custom_event, is_valid_handler_id, option_value, string_list_alias, text_of,
    version_ref_value,
};
pub use validation::{
    path_matches_allowlist, validate_payload_against_schema,
};
