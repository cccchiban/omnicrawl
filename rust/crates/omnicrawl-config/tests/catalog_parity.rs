//! 模型目录（model_catalog）的对照测试。
//!
//! 数据集是冻结的对照契约。

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::models::llm::LlmConfig;
use omnicrawl_config::models::model_catalog as MC;
use omnicrawl_config::models::ProviderProfile;
use omnicrawl_config::value::json_object_to_table;
use omnicrawl_llm::{DiscoveryModel, DiscoveryResult, DiscoveryStatus, ModelCapabilities};
use omnicrawl_protocol::Protocol;
use serde_json::{json, Value as Json};

const FIXTURE: &str = include_str!("fixtures/config_catalog_parity.json");

fn fixture() -> Json {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-catalog-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn case_dirs(root: &Path, index: usize) -> (PathBuf, PathBuf) {
    let case_dir = root.join(format!("case-{index}"));
    let user_dir = case_dir.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&user_dir).expect("建立用户配置目录");
    (case_dir, user_dir)
}

fn env_for(case_dir: &Path) -> R::ConfigEnvironment {
    R::ConfigEnvironment::new(case_dir.to_string_lossy().to_string(), "win32")
}

fn write_config(dir: &Path, name: &str, data: &Json) {
    std::fs::write(
        dir.join(name),
        omnicrawl_config::toml::dump_document(&json_object_to_table(data)),
    )
    .expect("写配置");
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn option_from_json(value: &Json) -> MC::ModelOption {
    MC::ModelOption {
        id: value["id"].as_str().unwrap().to_string(),
        name: value["name"].as_str().unwrap().to_string(),
        provider: value["provider"].as_str().unwrap().to_string(),
    }
}

fn model_to_json(model: &MC::CatalogModel) -> Json {
    json!({
        "source": model.source,
        "key": model.key,
        "profile_id": model.profile_id,
        "provider": model.provider,
        "protocol": model.protocol,
        "model_id": model.model_id,
        "display_name": model.display_name,
        "context_window_tokens": model.context_window_tokens,
        "availability": model.availability,
        "matched_custom_key": model.matched_custom_key,
        "tags": model.tags,
        "aliases": model.aliases,
        "sort_order": model.sort_order,
    })
}

fn build_config(env: &R::ConfigEnvironment, base_url: &str, api_key: &str) -> LlmConfig {
    let mut config = LlmConfig::with_environment(env);
    config.api_key = api_key.to_string();
    config.base_url = base_url.to_string();
    config.model = "gpt-5".to_string();
    config.model_source = "custom".to_string();
    config.profile_id = "openai-main".to_string();
    config.normalize().expect("构造对照用 LLMConfig")
}

#[test]
fn catalog_detect_matches_python() {
    let data = fixture();
    let env = R::ConfigEnvironment::new("C:\\oc-catalog\\home", "win32");
    for case in data["catalog_detect"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let config = build_config(
            &env,
            case["base_url"].as_str().unwrap(),
            case["api_key"].as_str().unwrap(),
        );
        let captured: RefCell<Vec<MC::ModelListRequest>> = RefCell::new(Vec::new());
        let outcome = outcome_from_json(case["name"].as_str().unwrap());
        let fetch = |request: &MC::ModelListRequest| -> MC::ModelListOutcome {
            captured.borrow_mut().push(request.clone());
            outcome.clone()
        };
        match (
            MC::detect_model_options(&config, MC::MODEL_LIST_TIMEOUT_SECONDS, &fetch),
            case.get("error"),
        ) {
            (Ok(options), None) => {
                let payload: Vec<Json> = options
                    .iter()
                    .map(|option| {
                        json!({"id": option.id, "name": option.name, "provider": option.provider})
                    })
                    .collect();
                assert_eq!(Json::Array(payload), case["result"], "{name}");
            }
            (Err(error), Some(expected)) => {
                assert_eq!(error.message(), expected.as_str().unwrap(), "{name}")
            }
            (Ok(_), Some(_)) => panic!("{name} 应报错"),
            (Err(error), None) => panic!("{name} 不应报错：{}", error.message()),
        }
        let request = captured.borrow();
        if let Some(expected) = case.get("request").and_then(|item| item.as_object()) {
            if let Some(expected_url) = expected.get("url").and_then(|item| item.as_str()) {
                let actual = request.first().expect("已发出请求");
                assert_eq!(actual.endpoint, expected_url, "{name} endpoint");
                let header = |key: &str| -> Option<String> {
                    actual
                        .headers
                        .iter()
                        .find(|(name, _)| name == key)
                        .map(|(_, value)| value.clone())
                };
                assert_eq!(
                    header("Authorization"),
                    expected["authorization"].as_str().map(str::to_string),
                    "{name} Authorization"
                );
                assert_eq!(
                    header("Accept"),
                    expected["accept"].as_str().map(str::to_string),
                    "{name} Accept"
                );
                assert_eq!(
                    header("User-Agent"),
                    expected["user_agent"].as_str().map(str::to_string),
                    "{name} User-Agent"
                );
            }
        }
    }
}

/// 用生成器记下的响应描述重放数据集的响应分支。
fn outcome_from_json(name: &str) -> MC::ModelListOutcome {
    let data = fixture();
    let case = data["catalog_detect"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"].as_str() == Some(name))
        .expect("用例存在");
    let response = &case["response"];
    match response["kind"].as_str().unwrap() {
        "body" => {
            let body = if let Some(list) = response.get("bytes").and_then(|item| item.as_array()) {
                list.iter()
                    .map(|item| item.as_u64().unwrap() as u8)
                    .collect()
            } else {
                response["text"].as_str().unwrap().as_bytes().to_vec()
            };
            MC::ModelListOutcome::Body(body)
        }
        "http" => MC::ModelListOutcome::Failure(MC::ModelListFailure::Http {
            status: Some(response["status"].as_i64().unwrap()),
        }),
        "urlerror" => MC::ModelListOutcome::Failure(MC::ModelListFailure::Connect {
            message: response["message"].as_str().unwrap().to_string(),
        }),
        _ => MC::ModelListOutcome::Failure(MC::ModelListFailure::Io {
            message: response["message"].as_str().unwrap().to_string(),
        }),
    }
}

#[test]
fn catalog_misc_matches_python() {
    let data = fixture();
    let misc = &data["catalog_misc"];

    for case in misc["provider_detect"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(
            MC::detect_model_provider(input),
            case["expected"].as_str().unwrap(),
            "{input}"
        );
    }

    for case in misc["ensure_current"].as_array().unwrap() {
        let options: Vec<MC::ModelOption> = case["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(option_from_json)
            .collect();
        let current = case["current"].as_str().unwrap();
        let actual: Vec<Json> = MC::ensure_current_model_option(&options, current)
            .iter()
            .map(
                |option| json!({"id": option.id, "name": option.name, "provider": option.provider}),
            )
            .collect();
        assert_eq!(
            Json::Array(actual),
            case["expected"],
            "ensure_current {current}"
        );
    }

    for case in misc["format_options"].as_array().unwrap() {
        let options: Vec<MC::ModelOption> = case["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(option_from_json)
            .collect();
        let text = MC::format_model_options(
            &options,
            case["current"].as_str().unwrap(),
            case["limit"].as_u64().unwrap() as usize,
        );
        assert_eq!(text, case["expected"].as_str().unwrap(), "format_options");
    }

    for case in misc["env_override"].as_array().unwrap() {
        let mut env = R::ConfigEnvironment::new("C:\\oc-catalog\\home", "win32");
        for (key, value) in case["env"].as_object().unwrap() {
            env = env.with_env_value(key, value.as_str().unwrap());
        }
        assert_eq!(
            MC::model_env_override_active(&env),
            case["expected"].as_bool().unwrap()
        );
    }
}

#[test]
fn catalog_descriptor_matches_python() {
    let data = fixture();
    for case in data["catalog_descriptor"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let source = name;
        let item = MC::CatalogModel {
            source: source.to_string(),
            key: if source == "custom" {
                "gpt-5".to_string()
            } else {
                "openai-main/gpt-5".to_string()
            },
            profile_id: "openai-main".to_string(),
            provider: "openai".to_string(),
            protocol: if source == "custom" {
                "openai_responses".to_string()
            } else {
                "openai_chat_completions".to_string()
            },
            model_id: "gpt-5".to_string(),
            display_name: if source == "custom" {
                "GPT-5".to_string()
            } else {
                String::new()
            },
            capabilities: Default::default(),
            context_window_tokens: if source == "custom" { 200_000 } else { 0 },
            availability: "unknown".to_string(),
            matched_custom_key: String::new(),
            diagnostic: String::new(),
            tags: if source == "custom" {
                vec!["fast".to_string()]
            } else {
                Vec::new()
            },
            aliases: if source == "custom" {
                vec!["gpt5".to_string()]
            } else {
                Vec::new()
            },
            sort_order: if source == "custom" { 3 } else { 0 },
        };
        let descriptor = MC::catalog_model_to_descriptor(&item).expect("转换描述");
        let payload = json!({
            "profile_id": descriptor.identity.profile_id,
            "provider": descriptor.identity.provider.as_str(),
            "protocol": descriptor.identity.protocol.as_str(),
            "model_id": descriptor.identity.model_id,
            "catalog_key": descriptor.identity.catalog_key,
            "display_name": descriptor.display_name,
            "context_window_tokens": descriptor.context_window_tokens,
            "max_output_tokens": descriptor.max_output_tokens,
            "aliases": descriptor.aliases,
            "tags": descriptor.tags,
            "source": descriptor.source,
            "sort_order": descriptor.sort_order,
            "enabled": descriptor.enabled,
        });
        assert_eq!(payload, case["result"], "{name}");
    }
}

#[test]
fn catalog_save_model_matches_python() {
    let data = fixture();
    let root = temp_root("save-model");
    for (index, case) in data["catalog_save_model"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let name = case["name"].as_str().unwrap();
        write_config(&user_dir, "config.toml", &case["initial_config"].clone());
        write_config(&user_dir, "models.toml", &case["models"]);
        let env = env_for(&case_dir);
        let target = user_dir.join("config.toml");
        match (
            MC::save_llm_model(&env, case["model_id"].as_str().unwrap(), None),
            case.get("error"),
        ) {
            (Ok(_), None) => assert_eq!(
                read_text(&target),
                case["config_text"].as_str().unwrap(),
                "{name}"
            ),
            (Err(error), Some(expected)) => {
                assert_eq!(error.message(), expected.as_str().unwrap(), "{name}")
            }
            (Ok(_), Some(_)) => panic!("{name} 应报错，实际写回成功"),
            (Err(error), None) => panic!("{name} 不应报错：{}", error.message()),
        }
    }
}

#[test]
fn catalog_build_matches_python() {
    let data = fixture();
    let root = temp_root("build");
    for (index, case) in data["catalog_build"].as_array().unwrap().iter().enumerate() {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let name = case["name"].as_str().unwrap();
        write_config(&user_dir, "config.toml", &case["config"]);
        write_config(&user_dir, "models.toml", &case["models"]);
        let env = env_for(&case_dir);
        let include_detected = case["include_detected"].as_bool().unwrap();
        let failed = case["discovery_status"].as_str().unwrap() != "ok";
        let discovery = discovery_result(failed);
        let cache = MC::DiscoveryCache::new();
        let calls: RefCell<Vec<(String, f64)>> = RefCell::new(Vec::new());
        let ports = MC::CatalogPorts {
            discover: &|profile: &ProviderProfile, _protocol: Protocol, timeout: f64| {
                calls.borrow_mut().push((profile.id.clone(), timeout));
                discovery.clone()
            },
            cache: &cache,
        };

        let request = MC::CatalogRequest {
            include_detected,
            ..MC::CatalogRequest::default()
        };
        let first = MC::build_catalog(&env, &request, &ports).expect("构建目录");
        let first_calls = calls.borrow().len();
        calls.borrow_mut().clear();

        let refresh = case["second_pass"].as_bool().unwrap();
        let second_request = MC::CatalogRequest {
            include_detected,
            refresh,
            ..MC::CatalogRequest::default()
        };
        let second = MC::build_catalog(&env, &second_request, &ports).expect("二次构建");
        let second_calls = calls.borrow().len();

        let expected = &case["first"];
        assert_eq!(
            omnicrawl_config::value::toml_to_json_object(&first.current),
            expected["current"],
            "{name} current"
        );
        let custom: Vec<Json> = first.custom.iter().map(model_to_json).collect();
        assert_eq!(Json::Array(custom), expected["custom"], "{name} custom");
        let mut detected: Vec<Json> = first.detected.iter().map(model_to_json).collect();
        sort_models(&mut detected);
        assert_eq!(
            Json::Array(detected),
            expected["detected"],
            "{name} detected"
        );
        let mut diagnostics: Vec<Json> = first
            .diagnostics
            .iter()
            .map(omnicrawl_config::value::toml_to_json_object)
            .collect();
        diagnostics.sort_by_key(|item| item["profile"].as_str().unwrap_or("").to_string());
        assert_eq!(
            Json::Array(diagnostics),
            expected["diagnostics"],
            "{name} diagnostics"
        );
        assert_eq!(
            first_calls,
            case["first_calls"].as_u64().unwrap() as usize,
            "{name} 首次调用次数"
        );
        assert_eq!(
            second_calls,
            case["second_calls"].as_u64().unwrap() as usize,
            "{name} 二次调用次数"
        );
        let mut second_detected: Vec<Json> = second.detected.iter().map(model_to_json).collect();
        sort_models(&mut second_detected);
        assert_eq!(
            Json::Array(second_detected),
            case["second_detected"],
            "{name} 二次 detected"
        );
    }
}

fn sort_models(models: &mut [Json]) {
    models.sort_by_key(|item| {
        (
            item["profile_id"].as_str().unwrap_or("").to_string(),
            item["model_id"].as_str().unwrap_or("").to_string(),
        )
    });
}

fn discovery_result(failed: bool) -> DiscoveryResult {
    if failed {
        return DiscoveryResult {
            profile_id: "openai-main".to_string(),
            status: DiscoveryStatus::Unavailable,
            message: String::new(),
            models: Vec::new(),
        };
    }
    DiscoveryResult {
        profile_id: "openai-main".to_string(),
        status: DiscoveryStatus::Ok,
        message: String::new(),
        models: vec![
            DiscoveryModel {
                profile_id: "openai-main".to_string(),
                provider: "openai".to_string(),
                protocol: Protocol::OpenaiChatCompletions,
                model_id: "gpt-5".to_string(),
                display_name: "GPT-5".to_string(),
                capabilities: ModelCapabilities::default(),
                context_window_tokens: 0,
            },
            DiscoveryModel {
                profile_id: "openai-main".to_string(),
                provider: "openai".to_string(),
                protocol: Protocol::OpenaiChatCompletions,
                model_id: "gpt-4.1".to_string(),
                display_name: String::new(),
                capabilities: ModelCapabilities::default(),
                context_window_tokens: 0,
            },
        ],
    }
}
