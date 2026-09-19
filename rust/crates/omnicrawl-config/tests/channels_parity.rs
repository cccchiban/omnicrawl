//! 模型渠道配置（channels）的对照测试。
//!
//! 期望值来自 Python 真实现（生成器 `rust/tools/gen_config_channels_fixture.py`）。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::models::channels as C;
use omnicrawl_config::value::json_object_to_table;
use serde_json::{json, Value as Json};

const FIXTURE: &str = include_str!("fixtures/config_channels_parity.json");

fn fixture() -> Json {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-channels-{}-{}", std::process::id(), tag));
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

fn channel_from_json(value: &Json) -> C::ChannelConfig {
    C::ChannelConfig {
        key: value["key"].as_str().unwrap().to_string(),
        name: value["name"].as_str().unwrap().to_string(),
        profile_id: value["profile_id"].as_str().unwrap().to_string(),
        provider: value["provider"].as_str().unwrap().to_string(),
        protocol: value["protocol"].as_str().unwrap().to_string(),
        base_url: value["base_url"].as_str().unwrap().to_string(),
        api_key: value["api_key"].as_str().unwrap().to_string(),
        model_id: value["model_id"].as_str().unwrap().to_string(),
        enabled: value["enabled"].as_bool().unwrap(),
        api_key_env: value["api_key_env"].as_str().unwrap().to_string(),
        user_agent: value["user_agent"].as_str().unwrap().to_string(),
    }
}

fn channel_to_json(channel: &C::ChannelConfig) -> Json {
    json!({
        "key": channel.key,
        "name": channel.name,
        "profile_id": channel.profile_id,
        "provider": channel.provider,
        "protocol": channel.protocol,
        "base_url": channel.base_url,
        "api_key": channel.api_key,
        "model_id": channel.model_id,
        "enabled": channel.enabled,
        "api_key_env": channel.api_key_env,
        "user_agent": channel.user_agent,
    })
}

#[test]
fn channel_defaults_match_python() {
    let data = fixture();
    for case in data["channel_defaults"].as_array().unwrap() {
        let provider = case["provider"].as_str().unwrap();
        let key = case["key"].as_str();
        match (C::default_channel(provider, key), case.get("error")) {
            (Ok(channel), None) => {
                assert_eq!(channel_to_json(&channel), case["result"], "{provider}")
            }
            (Err(error), Some(expected)) => {
                assert_eq!(error.message(), expected.as_str().unwrap(), "{provider}")
            }
            (Ok(channel), Some(_)) => panic!("{provider} 应报错，实际得到 {:?}", channel.key),
            (Err(error), None) => panic!("{provider} 不应报错：{}", error.message()),
        }
    }
}

#[test]
fn channel_unique_key_matches_python() {
    let data = fixture();
    for case in data["channel_unique_key"].as_array().unwrap() {
        let existing: Vec<String> = case["existing"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        let name = case["name"].as_str().unwrap();
        assert_eq!(
            C::unique_channel_key(name, &existing),
            case["expected"].as_str().unwrap(),
            "{name}"
        );
    }
}

#[test]
fn channel_labels_match_python() {
    let data = fixture();
    for case in data["channel_labels"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        let label = match case["kind"].as_str().unwrap() {
            "provider" => C::provider_label(value),
            _ => C::protocol_label(value),
        };
        assert_eq!(label, case["label"].as_str().unwrap(), "{value}");
    }
}

#[test]
fn channel_load_matches_python() {
    let data = fixture();
    let root = temp_root("load");
    for (index, case) in data["channel_load"].as_array().unwrap().iter().enumerate() {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let name = case["name"].as_str().unwrap();
        if case["config"]
            .as_object()
            .map(|m| !m.is_empty())
            .unwrap_or(false)
        {
            write_config(&user_dir, "config.toml", &case["config"]);
        }
        if !case["models"].is_null() {
            write_config(&user_dir, "models.toml", &case["models"]);
        }
        let env = env_for(&case_dir);
        match (
            C::load_channel_configuration(&env, None, None),
            case.get("error"),
        ) {
            (Ok(configuration), None) => {
                let payload = json!({
                    "channels": configuration
                        .channels
                        .iter()
                        .map(channel_to_json)
                        .collect::<Vec<Json>>(),
                    "default_key": configuration.default_key,
                });
                assert_eq!(payload, case["result"], "{name}");
            }
            (Err(error), Some(expected)) => {
                assert_eq!(error.message(), expected.as_str().unwrap(), "{name}")
            }
            (Ok(configuration), Some(_)) => {
                panic!(
                    "{name} 应报错，实际得到 {} 个渠道",
                    configuration.channels.len()
                )
            }
            (Err(error), None) => panic!("{name} 不应报错：{}", error.message()),
        }
    }
}

#[test]
fn channel_save_matches_python() {
    let data = fixture();
    let root = temp_root("save");
    for (index, case) in data["channel_save"].as_array().unwrap().iter().enumerate() {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let name = case["name"].as_str().unwrap();
        let config_path = user_dir.join("config.toml");
        let models_path = user_dir.join("models.toml");
        write_config(
            &user_dir,
            "config.toml",
            &json!({
                "llm": {
                    "profiles": {
                        "stale-main": {"provider": "openai", "api_key": "sk-stale"},
                        "openai-main": {"provider": "openai", "api_key": "old"},
                    },
                    "active_model": {"source": "custom", "key": "stale"},
                }
            }),
        );
        write_config(
            &user_dir,
            "models.toml",
            &json!({
                "version": 1,
                "models": {
                    "stale": {"profile": "stale-main", "model_id": "stale-model"},
                    "openai-main": {
                        "profile": "openai-main",
                        "model_id": "old-model",
                        "api_key": "leak",
                    },
                }
            }),
        );
        let channels: Vec<C::ChannelConfig> = case["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(channel_from_json)
            .collect();
        let configuration = C::ChannelConfiguration {
            channels,
            default_key: case["default_key"].as_str().unwrap().to_string(),
        };
        let env = env_for(&case_dir);
        match (
            C::save_channel_configuration(&env, &configuration, None, None),
            case.get("error"),
        ) {
            (Ok(_), None) => {
                assert_eq!(
                    read_text(&config_path),
                    case["config_text"].as_str().unwrap(),
                    "{name} config.toml"
                );
                assert_eq!(
                    read_text(&models_path),
                    case["models_text"].as_str().unwrap(),
                    "{name} models.toml"
                );
            }
            (Err(error), Some(expected)) => {
                assert_eq!(error.message(), expected.as_str().unwrap(), "{name}")
            }
            (Ok(_), Some(_)) => panic!("{name} 应报错，实际写盘成功"),
            (Err(error), None) => panic!("{name} 不应报错：{}", error.message()),
        }
    }
}

#[test]
fn channel_validate_matches_python() {
    let data = fixture();
    let root = temp_root("validate");
    for (index, case) in data["channel_validate"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let name = case["name"].as_str().unwrap();
        let channels: Vec<C::ChannelConfig> = case["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(channel_from_json)
            .collect();
        let configuration = C::ChannelConfiguration {
            channels,
            default_key: case["default_key"].as_str().unwrap().to_string(),
        };
        let env = env_for(&case_dir);
        match C::save_channel_configuration(&env, &configuration, None, None) {
            Ok(_) => assert!(
                case.get("error").is_none(),
                "{name} 应报错，实际写盘成功（{}）",
                user_dir.display()
            ),
            Err(error) => assert_eq!(error.message(), case["error"].as_str().unwrap(), "{name}"),
        }
    }
}

#[test]
fn channel_credentials_match_python() {
    let data = fixture();
    for case in data["channel_credentials"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mut env = R::ConfigEnvironment::new("C:\\oc-channels\\home", "win32");
        for (key, value) in case["env"].as_object().unwrap() {
            env = env.with_env_value(key, value.as_str().unwrap());
        }
        let channels: Vec<C::ChannelConfig> = case["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(channel_from_json)
            .collect();
        let configuration = C::ChannelConfiguration {
            channels,
            default_key: case["default_key"].as_str().unwrap().to_string(),
        };
        let missing: Vec<Json> = C::missing_enabled_credentials(&env, &configuration)
            .into_iter()
            .map(Json::String)
            .collect();
        assert_eq!(Json::Array(missing), case["missing"], "{name} missing");
        assert_eq!(
            C::has_usable_channel(&env, &configuration),
            case["usable"].as_bool().unwrap(),
            "{name} usable"
        );
    }
}
