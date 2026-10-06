//! 与 Python `omnicrawl/extensions/` 真实现的对照测试。
//!
//! 数据集是冻结的对照契约：同一批输入喂给真实现，
//! 这里用同一份输入重放 Rust 实现并逐字段比对。改了任一侧都要重跑生成脚本。

use omnicrawl_extensions::install;
use omnicrawl_extensions::models::*;
use omnicrawl_extensions::registry::{self, ManifestEntry};
use omnicrawl_extensions::skill::{self, SkillManager, SkillMeta};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("extensions_parity.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
    serde_json::from_str(&text).expect("数据集必须是合法 JSON")
}

fn cases<'a>(data: &'a Value, group: &str) -> &'a Vec<Value> {
    data.get(group)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("数据集缺少分组：{group}"))
}

/// 比对一条用例：期望可能是返回值，也可能是错误文案。
fn expect(entry: &Value, actual: Result<Value, String>) {
    if let Some(expected) = entry.get("error").and_then(Value::as_str) {
        match actual {
            Err(error) => assert_eq!(error, expected, "错误文案不一致：{entry}"),
            Ok(value) => panic!("期望错误 {expected}，实际 {value}"),
        }
        return;
    }
    let expected = entry.get("result").cloned().unwrap_or(Value::Null);
    match actual {
        Ok(value) => assert_eq!(value, expected, "返回值不一致：{entry}"),
        Err(error) => panic!("期望 {expected}，实际错误 {error}"),
    }
}

fn handler_json(item: &ResolvedHandler) -> Value {
    let mut map = Map::new();
    map.insert("key".to_string(), Value::from(item.key.clone()));
    map.insert(
        "plugin_name".to_string(),
        Value::from(item.plugin_name.clone()),
    );
    map.insert(
        "plugin_version".to_string(),
        Value::from(item.plugin_version.clone()),
    );
    map.insert(
        "handler_id".to_string(),
        Value::from(item.handler_id.clone()),
    );
    map.insert("hook".to_string(), Value::from(item.hook.clone()));
    map.insert("mode".to_string(), Value::from(item.mode.clone()));
    map.insert("priority".to_string(), Value::from(item.priority));
    map.insert("scope".to_string(), Value::from(item.scope.clone()));
    map.insert("timeout_ms".to_string(), Value::from(item.timeout_ms));
    map.insert("replaces".to_string(), Value::from(item.replaces.clone()));
    map.insert(
        "integrity_prefix".to_string(),
        Value::from(item.integrity_prefix.clone()),
    );
    map.insert(
        "local_path".to_string(),
        Value::from(item.local_path.clone()),
    );
    map.insert(
        "permissions".to_string(),
        Value::from(item.permissions.clone()),
    );
    Value::Object(map)
}

fn registration_json(item: &HandlerRegistration) -> Value {
    let mut map = Map::new();
    map.insert("id".to_string(), Value::from(item.id.clone()));
    map.insert("hook".to_string(), Value::from(item.hook.clone()));
    map.insert("mode".to_string(), Value::from(item.mode.clone()));
    map.insert("priority".to_string(), Value::from(item.priority));
    map.insert("replaces".to_string(), Value::from(item.replaces.clone()));
    map.insert(
        "event_version".to_string(),
        match &item.event_version {
            Some(value) => Value::from(value.clone()),
            None => Value::Null,
        },
    );
    map.insert(
        "timeout_ms".to_string(),
        match item.timeout_ms {
            Some(value) => Value::from(value),
            None => Value::Null,
        },
    );
    Value::Object(map)
}

fn custom_event_json(item: &CustomEventDeclaration) -> Value {
    let mut map = Map::new();
    map.insert("name".to_string(), Value::from(item.name.clone()));
    map.insert("version".to_string(), Value::from(item.version));
    map.insert(
        "visibility".to_string(),
        Value::from(item.visibility.clone()),
    );
    map.insert("schema".to_string(), Value::Object(item.schema.clone()));
    Value::Object(map)
}

fn manifest_json(item: &PluginManifest) -> Value {
    let mut map = Map::new();
    map.insert("name".to_string(), Value::from(item.name.clone()));
    map.insert("version".to_string(), Value::from(item.version.clone()));
    map.insert(
        "api_version".to_string(),
        Value::from(item.api_version.clone()),
    );
    map.insert("entry".to_string(), Value::from(item.entry.clone()));
    map.insert(
        "permissions".to_string(),
        Value::from(item.permissions.clone()),
    );
    map.insert(
        "hooks".to_string(),
        Value::Array(item.hooks.iter().map(registration_json).collect()),
    );
    map.insert(
        "engines_omnicrawl".to_string(),
        Value::from(item.engines_omnicrawl.clone()),
    );
    map.insert(
        "engines_node".to_string(),
        Value::from(item.engines_node.clone()),
    );
    map.insert(
        "timeout_ms".to_string(),
        match item.timeout_ms {
            Some(value) => Value::from(value),
            None => Value::Null,
        },
    );
    map.insert(
        "custom_events".to_string(),
        Value::Array(item.custom_events.iter().map(custom_event_json).collect()),
    );
    map.insert("agents".to_string(), Value::from(item.agents.clone()));
    map.insert(
        "package_type".to_string(),
        Value::from(item.package_type.clone()),
    );
    map.insert(
        "source_path".to_string(),
        Value::from(item.source_path.clone()),
    );
    Value::Object(map)
}

fn config_json(item: &PluginsConfig) -> Value {
    let mut map = Map::new();
    map.insert("enabled".to_string(), Value::from(item.enabled));
    map.insert(
        "default_timeout_ms".to_string(),
        Value::from(item.default_timeout_ms),
    );
    map.insert(
        "max_timeout_ms".to_string(),
        Value::from(item.max_timeout_ms),
    );
    map.insert(
        "failure_threshold".to_string(),
        Value::from(item.failure_threshold),
    );
    map.insert(
        "max_message_bytes".to_string(),
        Value::from(item.max_message_bytes),
    );
    map.insert(
        "custom_event_max_depth".to_string(),
        Value::from(item.custom_event_max_depth),
    );
    map.insert(
        "allow_network_install".to_string(),
        Value::from(item.allow_network_install),
    );
    map.insert(
        "audit_log_enabled".to_string(),
        Value::from(item.audit_log_enabled),
    );
    Value::Object(map)
}

fn hook_result_json(item: &HookResult) -> Value {
    let mut map = Map::new();
    map.insert("action".to_string(), Value::from(item.action.clone()));
    map.insert("reason".to_string(), Value::from(item.reason.clone()));
    map.insert("code".to_string(), Value::from(item.code.clone()));
    map.insert(
        "patch".to_string(),
        Value::Array(
            item.patch
                .iter()
                .map(|entry| Value::Object(entry.clone()))
                .collect(),
        ),
    );
    map.insert(
        "annotations".to_string(),
        Value::Object(item.annotations.clone()),
    );
    map.insert(
        "handler_key".to_string(),
        Value::from(item.handler_key.clone()),
    );
    map.insert("elapsed_ms".to_string(), Value::from(item.elapsed_ms));
    map.insert("status".to_string(), Value::from(item.status.clone()));
    Value::Object(map)
}

fn handler_from_json(value: &Value) -> ResolvedHandler {
    let map = value.as_object().expect("handler 必须是对象");
    let text = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let list = |key: &str| {
        map.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    ResolvedHandler {
        key: text("key"),
        plugin_name: text("plugin_name"),
        plugin_version: text("plugin_version"),
        handler_id: text("handler_id"),
        hook: text("hook"),
        mode: text("mode"),
        priority: map.get("priority").and_then(Value::as_i64).unwrap_or(0),
        scope: text("scope"),
        timeout_ms: map.get("timeout_ms").and_then(Value::as_i64).unwrap_or(0),
        replaces: list("replaces"),
        integrity_prefix: text("integrity_prefix"),
        local_path: text("local_path"),
        permissions: list("permissions"),
    }
}

fn record_from_json(value: &Value) -> PluginRecord {
    let map = value.as_object().expect("record 必须是对象");
    let name = map.get("name").and_then(Value::as_str).unwrap_or_default();
    PluginRecord::from_dict(name, value)
}

fn manifest_from_json(value: &Value) -> PluginManifest {
    let map = value.as_object().expect("manifest 必须是对象");
    let text = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let hooks = map
        .get("hooks")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let item = item.as_object().expect("handler 必须是对象");
                    HandlerRegistration {
                        id: item
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        hook: item
                            .get("hook")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        mode: item
                            .get("mode")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        priority: item.get("priority").and_then(Value::as_i64).unwrap_or(0),
                        replaces: item
                            .get("replaces")
                            .and_then(Value::as_array)
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|entry| entry.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        event_version: item
                            .get("event_version")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        timeout_ms: item.get("timeout_ms").and_then(Value::as_i64),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let custom_events = map
        .get("custom_events")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    let item = item.as_object().expect("custom event 必须是对象");
                    CustomEventDeclaration {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        version: item.get("version").and_then(Value::as_i64).unwrap_or(1),
                        visibility: item
                            .get("visibility")
                            .and_then(Value::as_str)
                            .unwrap_or("private")
                            .to_string(),
                        schema: item
                            .get("schema")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    PluginManifest {
        name: text("name"),
        version: text("version"),
        api_version: text("api_version"),
        entry: text("entry"),
        permissions: map
            .get("permissions")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        hooks,
        engines_omnicrawl: text("engines_omnicrawl"),
        engines_node: text("engines_node"),
        timeout_ms: map.get("timeout_ms").and_then(Value::as_i64),
        custom_events,
        agents: map
            .get("agents")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        package_type: {
            let value = text("package_type");
            if value.is_empty() {
                "module".to_string()
            } else {
                value
            }
        },
        source_path: text("source_path"),
    }
}

#[test]
fn semver_matches_python() {
    let data = fixture();
    for entry in cases(&data, "semver") {
        let version = entry
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expected = entry.get("valid").and_then(Value::as_bool).unwrap_or(false);
        assert_eq!(
            is_valid_semver(version),
            expected,
            "semver 判定不一致：{entry}"
        );
    }
}

#[test]
fn npm_name_matches_python() {
    let data = fixture();
    for entry in cases(&data, "npm_name") {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expected = entry.get("valid").and_then(Value::as_bool).unwrap_or(false);
        assert_eq!(is_valid_npm_name(name), expected, "包名判定不一致：{entry}");
    }
}

#[test]
fn namespace_matches_python() {
    let data = fixture();
    for entry in cases(&data, "namespace") {
        let package = entry
            .get("package")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expected = entry
            .get("expect")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            normalize_plugin_namespace(package),
            expected,
            "命名空间归一化不一致：{entry}"
        );
    }
}

#[test]
fn plugins_config_matches_python() {
    let data = fixture();
    for entry in cases(&data, "plugins_config") {
        let input = entry.get("input").cloned().unwrap_or(Value::Null);
        let actual = parse_plugins_config(Some(&input))
            .map(|item| config_json(&item))
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn handler_registration_matches_python() {
    let data = fixture();
    for entry in cases(&data, "handler_registration") {
        let input = entry.get("input").cloned().unwrap_or(Value::Null);
        let actual = parse_handler_registration(&input)
            .map(|item| registration_json(&item))
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn custom_event_matches_python() {
    let data = fixture();
    for entry in cases(&data, "custom_event") {
        let input = entry.get("input").cloned().unwrap_or(Value::Null);
        let actual = parse_custom_event(&input, "@omnicrawl/demo")
            .map(|item| custom_event_json(&item))
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn plugin_manifest_matches_python() {
    let data = fixture();
    for entry in cases(&data, "plugin_manifest") {
        let input = entry.get("input").cloned().unwrap_or(Value::Null);
        let actual = parse_plugin_manifest(&input, "/tmp/demo/package.json")
            .map(|item| manifest_json(&item))
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn payload_schema_matches_python() {
    let data = fixture();
    for entry in cases(&data, "payload_schema") {
        let payload = entry.get("payload").cloned().unwrap_or(Value::Null);
        let schema = entry
            .get("schema")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let schema_option = if entry.get("schema").map(Value::is_null).unwrap_or(true) {
            None
        } else {
            Some(&schema)
        };
        let actual = validate_payload_against_schema(&payload, schema_option)
            .map(|_| Value::Null)
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn validate_patch_matches_python() {
    let data = fixture();
    for entry in cases(&data, "validate_patch") {
        let hook = entry
            .get("hook")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let patch = entry.get("patch").cloned().unwrap_or(Value::Null);
        let actual = validate_json_patch(&patch, hook)
            .map(|items| Value::Array(items.into_iter().map(Value::Object).collect()))
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn apply_patch_matches_python() {
    let data = fixture();
    for entry in cases(&data, "apply_patch") {
        let document = entry
            .get("document")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let patch: Vec<Map<String, Value>> = entry
            .get("patch")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_object().cloned())
                    .collect()
            })
            .unwrap_or_default();
        let actual = apply_json_patch(&document, &patch)
            .map(Value::Object)
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn sort_handlers_matches_python() {
    let data = fixture();
    for entry in cases(&data, "sort_handlers") {
        let handlers: Vec<ResolvedHandler> = entry
            .get("handlers")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(handler_from_json).collect())
            .unwrap_or_default();
        let sorted: Vec<Value> = sort_handlers(&handlers).iter().map(handler_json).collect();
        let expected = entry
            .get("expect")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            Value::Array(sorted),
            Value::Array(expected),
            "排序不一致：{entry}"
        );
    }
}

#[test]
fn hook_result_matches_python() {
    let data = fixture();
    for entry in cases(&data, "hook_result") {
        let raw = entry.get("raw").cloned().unwrap_or(Value::Null);
        let raw_option = if entry.get("raw").map(Value::is_null).unwrap_or(true) {
            None
        } else {
            Some(&raw)
        };
        let actual = parse_hook_result(raw_option, "demo/h1", 12.5)
            .map(|item| hook_result_json(&item))
            .map_err(|error| error.to_string());
        expect(entry, actual);
    }
}

#[test]
fn stable_hash_matches_python() {
    let data = fixture();
    for entry in cases(&data, "stable_hash") {
        let value = entry.get("value").cloned().unwrap_or(Value::Null);
        let expected = entry
            .get("expect")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(stable_hash(&value), expected, "稳定哈希不一致：{entry}");
    }
}

#[test]
fn timeout_for_mode_matches_python() {
    let data = fixture();
    for entry in cases(&data, "timeout_for_mode") {
        let mode = entry
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expected = entry.get("expect").and_then(Value::as_i64).unwrap_or(0);
        assert_eq!(
            default_timeout_for_mode(mode),
            expected,
            "默认超时不一致：{entry}"
        );
    }
}

#[test]
fn merge_registry_matches_python() {
    let data = fixture();
    for entry in cases(&data, "merge_registry") {
        let user = PluginRegistryDocument::from_dict(entry.get("user").unwrap_or(&Value::Null))
            .expect("user 文档必须合法");
        let project =
            PluginRegistryDocument::from_dict(entry.get("project").unwrap_or(&Value::Null))
                .expect("project 文档必须合法");
        let merged = registry::merge_registry_documents(&user, &project);
        let expected = entry.get("expect").cloned().unwrap_or(Value::Null);
        assert_eq!(
            Value::Object(merged.to_dict()),
            expected,
            "合并结果不一致：{entry}"
        );
    }
}

#[test]
fn execution_plan_matches_python() {
    let data = fixture();
    for entry in cases(&data, "execution_plan") {
        let entries: Vec<ManifestEntry> = entry
            .get("manifests")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let manifest =
                            manifest_from_json(item.get("manifest").unwrap_or(&Value::Null));
                        let scope = item
                            .get("scope")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let record = record_from_json(item.get("record").unwrap_or(&Value::Null));
                        ManifestEntry {
                            manifest,
                            scope,
                            record,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let disabled: Vec<String> = entry
            .get("disabled")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let max_timeout = entry
            .get("max_timeout_ms")
            .and_then(Value::as_i64)
            .unwrap_or(5000);
        let plan = registry::build_execution_plan(&entries, &disabled, max_timeout);
        let actual: Vec<Value> = plan.iter().map(handler_json).collect();
        let expected = entry
            .get("expect")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            Value::Array(actual),
            Value::Array(expected),
            "执行计划不一致：{entry}"
        );
    }
}

#[test]
fn resolve_replacements_matches_python() {
    let data = fixture();
    for entry in cases(&data, "resolve_replacements") {
        let handlers: Vec<ResolvedHandler> = entry
            .get("handlers")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(handler_from_json).collect())
            .unwrap_or_default();
        let keys: Vec<Value> = registry::resolve_replacements(handlers)
            .iter()
            .map(|item| Value::from(item.key.clone()))
            .collect();
        let expected = entry
            .get("expect")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            Value::Array(keys),
            Value::Array(expected),
            "替换解析不一致：{entry}"
        );
    }
}

#[test]
fn skill_name_matches_python() {
    let data = fixture();
    for entry in cases(&data, "skill_name") {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expected = entry.get("errors").cloned().unwrap_or(Value::Null);
        assert_eq!(
            Value::from(skill::validate_skill_name(name)),
            expected,
            "名称校验不一致：{entry}"
        );
    }
}

#[test]
fn skill_description_matches_python() {
    let data = fixture();
    for entry in cases(&data, "skill_description") {
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expected = entry.get("errors").cloned().unwrap_or(Value::Null);
        assert_eq!(
            Value::from(skill::validate_skill_description(description)),
            expected,
            "描述校验不一致：{entry}"
        );
    }
}

#[test]
fn frontmatter_matches_python() {
    let data = fixture();
    for entry in cases(&data, "frontmatter") {
        let content = entry
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (frontmatter, body) = skill::parse_frontmatter(content);
        let mut actual = Map::new();
        actual.insert("content".to_string(), Value::from(content));
        actual.insert("frontmatter".to_string(), Value::Object(frontmatter));
        actual.insert("body".to_string(), Value::from(body));
        assert_eq!(
            Value::Object(actual),
            *entry,
            "frontmatter 解析不一致：{entry}"
        );
    }
}

#[test]
fn infer_description_matches_python() {
    let data = fixture();
    for entry in cases(&data, "infer_description") {
        let raw = entry.get("raw").and_then(Value::as_str).unwrap_or_default();
        let body = entry
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let expected = entry
            .get("expect")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            skill::infer_description(raw, body),
            expected,
            "描述推断不一致：{entry}"
        );
    }
}

#[test]
fn skill_prompt_matches_python() {
    let data = fixture();
    for entry in cases(&data, "skill_prompt") {
        let metas: Vec<SkillMeta> = entry
            .get("skills")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| SkillMeta {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        description: item
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        source_path: PathBuf::from(
                            item.get("source_path")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ),
                        base_dir: PathBuf::from(
                            item.get("base_dir")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ),
                        scope: item
                            .get("scope")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        disable_model_invocation: item
                            .get("disable_model_invocation")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let expected = entry
            .get("expect")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            SkillManager::format_skills_for_prompt(&metas),
            expected,
            "渐进式披露输出不一致：{entry}"
        );
    }
}

#[test]
fn install_helpers_match_python() {
    // 纯判定面：包规格解析与 registry URL 拼接（不触网）。
    let spec = install::parse_package_spec("@omnicrawl/demo@1.2.3").expect("合法 spec");
    assert_eq!(spec.name, "@omnicrawl/demo");
    assert_eq!(spec.version.as_deref(), Some("1.2.3"));
    assert_eq!(spec.raw, "@omnicrawl/demo@1.2.3");

    let plain = install::parse_package_spec("demo@latest").expect("合法 spec");
    assert_eq!(plain.name, "demo");
    assert_eq!(plain.version.as_deref(), Some("latest"));

    for bad in [
        "",
        "git+https://x/y",
        "https://x/y",
        "./local",
        "/abs/path",
        "a@b@c",
    ] {
        assert!(
            install::parse_package_spec(bad).is_err(),
            "应当拒绝的 spec 被接受：{bad}"
        );
    }

    assert_eq!(install::quote_path_segment("a b"), "a%20b");
    assert_eq!(install::quote_path_segment("@scope/name"), "@scope%2Fname");
}
