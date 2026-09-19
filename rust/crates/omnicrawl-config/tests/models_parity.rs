//! 模型配置（llm / llm_multi / model_store / vision）的对照测试。
//!
//! 期望值来自 Python 真实现（生成器 `rust/tools/gen_config_models_fixture.py`）。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::models::llm::{self as llm_mod, ActiveModelRef, LlmConfig};
use omnicrawl_config::models::llm_multi as multi;
use omnicrawl_config::models::model_store::{self as store, CustomModelRecord, ModelStore};
use omnicrawl_config::models::vision;
use omnicrawl_config::models::ProviderProfile;
use omnicrawl_config::toml::{Table, Value as TomlValue};
use omnicrawl_config::value::{json_to_toml, toml_to_json, toml_to_json_object};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/config_models_parity.json");

const MANAGED_HOME: &str = "C:\\oc-models\\home";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-models-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn env_for(case: &Value, home: &str) -> R::ConfigEnvironment {
    let mut env = R::ConfigEnvironment::new(home, "win32");
    if let Some(map) = case.get("env").and_then(|item| item.as_object()) {
        for (name, value) in map {
            if let Some(text) = value.as_str() {
                env = env.with_env_value(name, text);
            }
        }
    }
    env
}

fn case_root(root: &Path, index: usize) -> PathBuf {
    let dir = root.join(format!("case-{index}"));
    std::fs::create_dir_all(&dir).expect("建立用例目录");
    dir
}

fn user_dir(case_root: &Path) -> PathBuf {
    let dir = case_root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&dir).expect("建立用户配置目录");
    dir
}

fn write_config(dir: &Path, name: &str, data: &Value) {
    let table = json_to_table(data);
    std::fs::write(
        dir.join(name),
        omnicrawl_config::toml::dump_document(&table),
    )
    .expect("写配置");
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn json_to_table(value: &Value) -> Table {
    match value {
        Value::Object(_) => omnicrawl_config::value::json_object_to_table(value),
        _ => Table::new(),
    }
}

fn llm_config_to_value(config: &LlmConfig) -> Value {
    json!({
        "api_key": config.api_key,
        "base_url": config.base_url,
        "model": config.model,
        "thinking_type": config.thinking_type,
        "reasoning_effort": config.reasoning_effort,
        "context_window_tokens": config.context_window_tokens,
        "max_output_tokens": config.max_output_tokens,
        "native_vision": config.native_vision,
        "temperature": config.temperature,
        "system_prompt": config.system_prompt,
        "max_history_turns": config.max_history_turns,
        "profile_id": config.profile_id,
        "provider": config.provider,
        "protocol": config.protocol,
        "catalog_key": config.catalog_key,
        "model_source": config.model_source,
        "api_key_env": config.api_key_env,
        "user_agent": config.user_agent,
        "request_timeout_seconds": config.request_timeout_seconds,
        "request_retry_count": config.request_retry_count,
        "provider_options": toml_to_json(&TomlValue::Table(config.provider_options.clone())),
    })
}

fn record_to_value(record: &CustomModelRecord) -> Value {
    json!({
        "key": record.key,
        "display_name": record.display_name,
        "profile": record.profile,
        "model_id": record.model_id,
        "protocol": record.protocol.as_str(),
        "enabled": record.enabled,
        "aliases": record.aliases,
        "description": record.description,
        "tags": record.tags,
        "context_window_tokens": record.context_window_tokens,
        "max_output_tokens": record.max_output_tokens,
        "temperature": record.temperature,
        "native_vision": record.native_vision,
        "capabilities": json!({
            "streaming": record.capabilities.streaming,
            "tools": record.capabilities.tools,
            "parallel_tool_calls": record.capabilities.parallel_tool_calls,
            "reasoning": record.capabilities.reasoning,
            "vision": record.capabilities.vision,
            "model_discovery": record.capabilities.model_discovery,
            "prompt_cache": record.capabilities.prompt_cache,
            "context_window_tokens": record.capabilities.context_window_tokens,
            "max_output_tokens": record.capabilities.max_output_tokens,
        }),
        "provider_options": toml_to_json_object(&record.provider_options),
        "sort_order": record.sort_order,
    })
}

fn profile_to_value(profile: &ProviderProfile) -> Value {
    json!({
        "id": profile.id,
        "provider": profile.provider,
        "enabled": profile.enabled,
        "base_url": profile.base_url,
        "api_key": profile.api_key,
        "api_key_env": profile.api_key_env,
        "user_agent": profile.user_agent,
        "default_protocol": profile.default_protocol,
        "discovery_enabled": profile.discovery_enabled,
        "default_context_window_tokens": profile.default_context_window_tokens,
        "provider_options": toml_to_json_object(&profile.provider_options),
        "request_timeout_seconds": profile.request_timeout_seconds,
        "request_retry_count": profile.request_retry_count,
        "discovery_timeout_seconds": profile.discovery_timeout_seconds,
    })
}

fn ref_to_value(reference: &ActiveModelRef) -> Value {
    json!({
        "source": reference.source,
        "key": reference.key,
        "profile": reference.profile,
        "model_id": reference.model_id,
        "protocol": reference.protocol,
    })
}

fn apply_llm_input(mut config: LlmConfig, input: &Value) -> LlmConfig {
    let Some(map) = input.as_object() else {
        return config;
    };
    for (key, value) in map {
        match key.as_str() {
            "api_key" => config.api_key = value.as_str().unwrap_or_default().to_string(),
            "base_url" => config.base_url = value.as_str().unwrap_or_default().to_string(),
            "model" => config.model = value.as_str().unwrap_or_default().to_string(),
            "thinking_type" => {
                config.thinking_type = value.as_str().unwrap_or_default().to_string()
            }
            "reasoning_effort" => {
                config.reasoning_effort = value.as_str().unwrap_or_default().to_string()
            }
            "context_window_tokens" => {
                config.context_window_tokens = value.as_i64().unwrap_or_default()
            }
            "max_output_tokens" => config.max_output_tokens = value.as_i64().unwrap_or_default(),
            "native_vision" => config.native_vision = value.as_bool(),
            "temperature" => config.temperature = value.as_f64(),
            "user_agent" => config.user_agent = value.as_str().unwrap_or_default().to_string(),
            "model_source" => config.model_source = value.as_str().unwrap_or_default().to_string(),
            "api_key_env" => config.api_key_env = value.as_str().unwrap_or_default().to_string(),
            "provider" => config.provider = value.as_str().unwrap_or_default().to_string(),
            "protocol" => config.protocol = value.as_str().unwrap_or_default().to_string(),
            "profile_id" => config.profile_id = value.as_str().unwrap_or_default().to_string(),
            "catalog_key" => config.catalog_key = value.as_str().unwrap_or_default().to_string(),
            "system_prompt" => {
                config.system_prompt = value.as_str().unwrap_or_default().to_string()
            }
            "max_history_turns" => config.max_history_turns = value.as_i64().unwrap_or_default(),
            "request_timeout_seconds" => {
                config.request_timeout_seconds = value.as_i64().unwrap_or_default()
            }
            "request_retry_count" => {
                config.request_retry_count = value.as_i64().unwrap_or_default()
            }
            "provider_options" => config.provider_options = json_to_table(value),
            other => panic!("数据集出现未知字段：{other}"),
        }
    }
    config
}

fn store_from_value(value: &Value) -> ModelStore {
    let version = value
        .get("version")
        .and_then(|item| item.as_i64())
        .unwrap_or(1);
    let mut models: Vec<CustomModelRecord> = Vec::new();
    if let Some(items) = value.get("models").and_then(|item| item.as_array()) {
        for item in items {
            models.push(record_from_value(item));
        }
    }
    ModelStore {
        version,
        models,
        path: None,
    }
}

fn record_from_value(value: &Value) -> CustomModelRecord {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(|item| item.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let number = |key: &str| value.get(key).and_then(|item| item.as_i64()).unwrap_or(0);
    CustomModelRecord {
        key: text("key"),
        display_name: text("display_name"),
        profile: text("profile"),
        model_id: text("model_id"),
        protocol: omnicrawl_protocol::Protocol::parse(
            value["protocol"].as_str().unwrap_or_default(),
        )
        .unwrap_or(omnicrawl_protocol::Protocol::OpenaiChatCompletions),
        enabled: value
            .get("enabled")
            .and_then(|item| item.as_bool())
            .unwrap_or(true),
        aliases: string_list(value.get("aliases")),
        description: text("description"),
        tags: string_list(value.get("tags")),
        context_window_tokens: number("context_window_tokens"),
        max_output_tokens: number("max_output_tokens"),
        temperature: value.get("temperature").and_then(|item| item.as_f64()),
        native_vision: value.get("native_vision").and_then(|item| item.as_bool()),
        capabilities: omnicrawl_llm::ModelCapabilities::from_mapping(
            value.get("capabilities").unwrap_or(&Value::Null),
        ),
        provider_options: value
            .get("provider_options")
            .map(json_to_table)
            .unwrap_or_default(),
        sort_order: number("sort_order"),
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|item| item.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|item| item.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn reasoning_effort_matches_python() {
    let data = fixture();
    for case in data["reasoning_effort"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        match (
            llm_mod::normalize_reasoning_effort(value),
            case.get("expected"),
        ) {
            (Ok(effort), Some(expected)) => {
                assert_eq!(effort, expected.as_str().unwrap(), "输入：{value}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "输入：{value}"
                );
            }
            (Ok(effort), None) => panic!("输入 {value} 应报错，实际得到 {effort}"),
        }
    }
}

#[test]
fn llm_normalize_matches_python() {
    let data = fixture();
    for case in data["llm_normalize"].as_array().unwrap() {
        let env = env_for(case, MANAGED_HOME);
        let config = apply_llm_input(LlmConfig::with_environment(&env), &case["input"]);
        let name = case["name"].as_str().unwrap();
        match (config.normalize(), case.get("expected")) {
            (Ok(config), Some(expected)) => {
                assert_eq!(llm_config_to_value(&config), *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
}

#[test]
fn thinking_enabled_matches_python() {
    let data = fixture();
    for case in data["thinking_enabled"].as_array().unwrap() {
        let mut config =
            LlmConfig::with_environment(&R::ConfigEnvironment::new(MANAGED_HOME, "win32"));
        config.reasoning_effort = case["reasoning_effort"].as_str().unwrap().to_string();
        config.thinking_type = case["thinking_type"].as_str().unwrap().to_string();
        assert_eq!(
            config.thinking_enabled(),
            case["expected"].as_bool().unwrap(),
            "用例：{} / {}",
            case["reasoning_effort"],
            case["thinking_type"]
        );
    }
}

#[test]
fn active_refs_match_python() {
    let data = fixture();
    for case in data["active_refs"].as_array().unwrap() {
        let input = &case["input"];
        let reference = ActiveModelRef {
            source: input["source"].as_str().unwrap_or_default().to_string(),
            key: input["key"].as_str().unwrap_or_default().to_string(),
            profile: input["profile"].as_str().unwrap_or_default().to_string(),
            model_id: input["model_id"].as_str().unwrap_or_default().to_string(),
            protocol: input["protocol"].as_str().unwrap_or_default().to_string(),
        };
        assert_eq!(
            toml_to_json(&TomlValue::Table(reference.to_table())),
            case["expected"],
            "用例：{}",
            case["name"]
        );
    }
}

#[test]
fn model_store_parse_matches_python() {
    let data = fixture();
    for case in data["model_store_parse"].as_array().unwrap() {
        let table = json_to_table(&case["data"]);
        let name = case["name"].as_str().unwrap();
        match (store::parse_model_store(&table, None), case.get("expected")) {
            (Ok(parsed), Some(expected)) => {
                let actual = json!({
                    "version": parsed.version,
                    "models": parsed.models.iter().map(record_to_value).collect::<Vec<_>>(),
                });
                assert_eq!(actual, *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
}

#[test]
fn model_store_save_matches_python() {
    let data = fixture();
    for case in data["model_store_save"].as_array().unwrap() {
        let parsed = store_from_value(&case["store"]);
        let text = omnicrawl_config::toml::dump_document(&store::store_payload(&parsed));
        assert_eq!(
            text,
            case["text"].as_str().unwrap(),
            "用例：{}",
            case["name"]
        );
    }
}

#[test]
fn resolve_alias_matches_python() {
    let data = fixture();
    let store = ModelStore {
        version: 1,
        models: vec![
            record_fixture("a", "A", "m", &["x"]),
            record_fixture("b", "B", "n", &["y", "x"]),
        ],
        path: None,
    };
    let single = ModelStore {
        version: 1,
        models: vec![record_fixture("a", "A", "m", &["x"])],
        path: None,
    };
    for case in data["resolve_alias"].as_array().unwrap() {
        let token = case["token"].as_str().unwrap();
        let target = if case["name"] == "多命中" {
            &store
        } else {
            &single
        };
        match (target.resolve_alias(token), case.get("expected")) {
            (Ok(record), Some(expected)) => {
                assert_eq!(
                    record.map(|item| item.key.clone()),
                    expected.as_str().map(|item| item.to_string()),
                    "用例：{}",
                    case["name"]
                );
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{}",
                    case["name"]
                );
            }
            (Ok(_), None) => panic!("用例 {} 应报错", case["name"]),
        }
    }
}

fn record_fixture(
    key: &str,
    display_name: &str,
    model_id: &str,
    aliases: &[&str],
) -> CustomModelRecord {
    record_from_value(&json!({
        "key": key,
        "display_name": display_name,
        "profile": "p",
        "model_id": model_id,
        "protocol": "openai_chat_completions",
        "enabled": true,
        "aliases": aliases,
        "description": "",
        "tags": [],
        "context_window_tokens": 0,
        "max_output_tokens": 0,
        "sort_order": 0,
    }))
}

#[test]
fn llm_load_matches_python() {
    let data = fixture();
    let root = temp_root("llm-load");
    for (index, case) in data["llm_load"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        let env = env_for(case, &case_dir.to_string_lossy());
        let name = case["name"].as_str().unwrap();
        match (llm_mod::load_llm_config(&env), case.get("expected")) {
            (Ok(config), Some(expected)) => {
                assert_eq!(llm_config_to_value(&config), *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn llm_save_matches_python() {
    let data = fixture();
    let root = temp_root("llm-save");
    for (index, case) in data["llm_save"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        let target = dir.join("config.toml");
        write_config(&dir, "config.toml", &case["config"]);
        let env = env_for(case, &case_dir.to_string_lossy());
        let name = case["name"].as_str().unwrap();
        let result = match case["action"].as_str().unwrap() {
            "save_reasoning_effort" => {
                llm_mod::save_reasoning_effort(&env, case["payload"].as_str().unwrap(), None)
            }
            _ => {
                let payload = &case["payload"];
                let reference = ActiveModelRef {
                    source: payload["source"].as_str().unwrap_or_default().to_string(),
                    key: payload["key"].as_str().unwrap_or_default().to_string(),
                    profile: payload["profile"].as_str().unwrap_or_default().to_string(),
                    model_id: payload["model_id"].as_str().unwrap_or_default().to_string(),
                    protocol: payload["protocol"].as_str().unwrap_or_default().to_string(),
                };
                llm_mod::save_active_model_ref(&env, &reference, None)
            }
        };
        match (result, case.get("text")) {
            (Ok(_), Some(expected)) => {
                assert_eq!(
                    read_text(&target),
                    expected.as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn vision_load_matches_python() {
    let data = fixture();
    let root = temp_root("vision-load");
    for (index, case) in data["vision_load"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        let env = env_for(case, &case_dir.to_string_lossy());
        let name = case["name"].as_str().unwrap();
        match (
            vision::load_vision_configuration(&env, None),
            case.get("expected"),
        ) {
            (Ok(configuration), Some(expected)) => {
                let actual = json!({
                    "enabled": configuration.enabled,
                    "models": configuration.models.iter().map(ref_to_value).collect::<Vec<_>>(),
                });
                assert_eq!(actual, *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn native_vision_parse_matches_python() {
    let data = fixture();
    for case in data["native_vision_parse"].as_array().unwrap() {
        let raw = case["value"].clone();
        let value = if raw.is_null() {
            None
        } else {
            Some(json_to_toml(&raw))
        };
        let expected = case["expected"].as_bool();
        assert_eq!(vision::parse_native_vision(value.as_ref()), expected);
    }
}

#[test]
fn native_vision_resolve_matches_python() {
    let data = fixture();
    let root = temp_root("vision-resolve");
    for (index, case) in data["native_vision_resolve"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        std::fs::write(
            dir.join("config.toml"),
            omnicrawl_config::toml::dump_document(&json_to_table(&json!({
                "llm": {"profiles": {
                    "p": {"provider": "openai", "native_vision": true},
                    "q": {"provider": "openai", "native_vision": false},
                }}
            }))),
        )
        .expect("写配置");
        std::fs::write(
            dir.join("models.toml"),
            omnicrawl_config::toml::dump_document(&json_to_table(&json!({
                "version": 1,
                "models": {
                    "a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions", "native_vision": true},
                    "b": {"profile": "p", "model_id": "n", "protocol": "openai_chat_completions", "native_vision": false},
                }
            }))),
        )
        .expect("写模型目录");
        let env = R::ConfigEnvironment::new(case_dir.to_string_lossy().to_string(), "win32");
        let setting = vision::resolve_native_vision(
            &env,
            case["catalog_key"].as_str().unwrap(),
            case["profile_id"].as_str().unwrap(),
            None,
            None,
        )
        .expect("解析原生视觉");
        let actual = json!({
            "value": setting.value,
            "scope": setting.scope,
            "label": setting.label,
            "scope_text": setting.scope_text(),
        });
        assert_eq!(actual, case["expected"], "用例：{}", case["name"]);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn native_vision_save_matches_python() {
    let data = fixture();
    let root = temp_root("vision-save");
    for (index, case) in data["native_vision_save"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        std::fs::write(
            dir.join("config.toml"),
            omnicrawl_config::toml::dump_document(&json_to_table(&json!({
                "llm": {"profiles": {"p": {"provider": "openai", "native_vision": true}}}
            }))),
        )
        .expect("写配置");
        std::fs::write(
            dir.join("models.toml"),
            omnicrawl_config::toml::dump_document(&json_to_table(&json!({
                "version": 1,
                "models": {"a": {"profile": "p", "model_id": "m", "protocol": "openai_chat_completions"}}
            }))),
        )
        .expect("写模型目录");
        let env = R::ConfigEnvironment::new(case_dir.to_string_lossy().to_string(), "win32");
        let name = case["name"].as_str().unwrap();
        let value = case["value"].as_bool();
        let result = vision::save_native_vision(
            &env,
            value,
            case["scope"].as_str().unwrap(),
            case["catalog_key"].as_str().unwrap(),
            case["profile_id"].as_str().unwrap(),
            None,
            None,
        );
        match (result, case.get("config_text")) {
            (Ok(_), Some(expected)) => {
                assert_eq!(
                    read_text(&dir.join("config.toml")),
                    expected.as_str().unwrap(),
                    "用例：{name}（config.toml）"
                );
                assert_eq!(
                    read_text(&dir.join("models.toml")),
                    case["models_text"].as_str().unwrap(),
                    "用例：{name}（models.toml）"
                );
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn multi_load_matches_python() {
    let data = fixture();
    let root = temp_root("multi-load");
    for (index, case) in data["multi_load"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        if case["models"]
            .as_object()
            .map(|item| !item.is_empty())
            .unwrap_or(false)
        {
            write_config(&dir, "models.toml", &case["models"]);
        }
        let env = env_for(case, &case_dir.to_string_lossy());
        let name = case["name"].as_str().unwrap();
        match (llm_mod::load_llm_config(&env), case.get("expected")) {
            (Ok(config), Some(expected)) => {
                assert_eq!(llm_config_to_value(&config), *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn selection_matches_python() {
    let data = fixture();
    let root = temp_root("selection");
    for (index, case) in data["selection"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        write_config(&dir, "models.toml", &case["models"]);
        let env = env_for(case, &case_dir.to_string_lossy());
        let base = LlmConfig {
            api_key: "k".to_string(),
            base_url: "https://api".to_string(),
            model: "m".to_string(),
            provider: "openai".to_string(),
            protocol: "openai_chat_completions".to_string(),
            profile_id: "ch1".to_string(),
            model_source: "custom".to_string(),
            ..LlmConfig::with_environment(&env)
        };
        let name = case["name"].as_str().unwrap();
        let token = case["token"].as_str().unwrap();
        match (
            multi::apply_model_selection(&env, &base, token),
            case.get("expected"),
        ) {
            (Ok(config), Some(expected)) => {
                assert_eq!(llm_config_to_value(&config), *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profiles_match_python() {
    let data = fixture();
    for case in data["profiles"].as_array().unwrap() {
        let section = json_to_table(&case["section"]);
        let profiles = multi::parse_profiles(&section);
        let actual: Value = Value::Object(
            profiles
                .iter()
                .map(|(key, profile)| (key.clone(), profile_to_value(profile)))
                .collect(),
        );
        assert_eq!(actual, case["expected"], "用例：{}", case["name"]);
    }
}

#[test]
fn config_to_profile_matches_python() {
    let data = fixture();
    for case in data["config_to_profile"].as_array().unwrap() {
        let env = R::ConfigEnvironment::new(MANAGED_HOME, "win32");
        let config = apply_llm_input(LlmConfig::with_environment(&env), &case["config"])
            .normalize()
            .expect("视图应合法");
        let (profile, descriptor) = multi::llm_config_to_profile_and_descriptor(&config);
        assert_eq!(profile_to_value(&profile), case["profile"], "profile");
        let expected = &case["descriptor"];
        let actual = json!({
            "model_id": descriptor.identity.model_id,
            "profile_id": descriptor.identity.profile_id,
            "provider": descriptor.identity.provider.as_str(),
            "protocol": descriptor.identity.protocol.as_str(),
            "catalog_key": descriptor.identity.catalog_key,
            "display_name": descriptor.display_name,
            "source": descriptor.source,
            "context_window_tokens": descriptor.context_window_tokens,
            "max_output_tokens": descriptor.max_output_tokens,
            "temperature": descriptor.temperature,
            "capabilities": Value::Object(descriptor.capabilities.to_map()),
        });
        assert_eq!(actual, *expected, "descriptor");
    }
}
