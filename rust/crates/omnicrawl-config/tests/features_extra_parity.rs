//! features 其余模块（advisor / agent_workspace / desensitization / image_gen /
//! tool_output_compression / tts）的对照测试。
//!
//! 数据集是冻结的对照契约。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::features::advisor;
use omnicrawl_config::features::agent_workspace;
use omnicrawl_config::features::desensitization;
use omnicrawl_config::features::image_gen;
use omnicrawl_config::features::tool_output_compression as compression;
use omnicrawl_config::features::tts;
use omnicrawl_config::value::json_object_to_table;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/config_features_extra_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn temp_root(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("oc-features-extra-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn env_for(case_dir: &Path) -> R::ConfigEnvironment {
    R::ConfigEnvironment::new(case_dir.to_string_lossy().to_string(), "win32")
}

fn write_config(dir: &Path, data: &Value) {
    let table = json_object_to_table(data);
    std::fs::write(
        dir.join("config.toml"),
        omnicrawl_config::toml::dump_document(&table),
    )
    .expect("写配置");
}

/// 读取组：逐例比对返回值或错误文案。
fn check_load<F>(group: &str, load: F)
where
    F: Fn(&R::ConfigEnvironment) -> Result<Value, String>,
{
    let data = fixture();
    let cases = data[group].as_array().unwrap();
    let root = temp_root(group);
    for (index, case) in cases.iter().enumerate() {
        let case_dir = root.join(format!("case-{index}"));
        let user_dir = case_dir.join(R::USER_CONFIG_DIRNAME);
        std::fs::create_dir_all(&user_dir).expect("建立用户配置目录");
        write_config(&user_dir, &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        match (load(&env), case.get("result"), case.get("error")) {
            (Ok(value), Some(expected), _) => assert_eq!(&value, expected, "{group} / {name}"),
            (Err(message), _, Some(expected)) => {
                assert_eq!(message, expected.as_str().unwrap(), "{group} / {name}")
            }
            (Ok(value), None, Some(_)) => panic!("{group} / {name} 应报错，实际得到 {value}"),
            (Err(message), Some(_), None) => panic!("{group} / {name} 不应报错：{message}"),
            _ => panic!("{group} / {name} 数据集缺少期望值"),
        }
    }
}

/// 写回组：从同一初始文件出发，比对写回文本。
fn check_save<F>(group: &str, save: F)
where
    F: Fn(&R::ConfigEnvironment),
{
    let initial = json!({"llm": {"model": "keep-me"}, "tts": {"voice": "old"}});
    let data = fixture();
    let cases = data[group].as_array().unwrap();
    let root = temp_root(group);
    for (index, case) in cases.iter().enumerate() {
        let case_dir = root.join(format!("case-{index}"));
        let user_dir = case_dir.join(R::USER_CONFIG_DIRNAME);
        std::fs::create_dir_all(&user_dir).expect("建立用户配置目录");
        write_config(&user_dir, &initial);
        let env = env_for(&case_dir);
        save(&env);
        let text = std::fs::read_to_string(user_dir.join("config.toml")).expect("读回写结果");
        assert_eq!(text, case["text"].as_str().unwrap(), "{group} 写回文本");
    }
}

#[test]
fn advisor_load_matches_python() {
    check_load("advisor_load", |env| {
        advisor::load_advisor_config(env, None)
            .map(|config| {
                json!({
                    "enabled": config.enabled,
                    "model_key": config.model_key,
                    "effort": config.effort,
                    "disabled_for_models": config.disabled_for_models,
                    "active": config.active(),
                    "display_effort": config.display_effort(),
                })
            })
            .map_err(|error| error.message().to_string())
    });
}

#[test]
fn advisor_save_matches_python() {
    check_save("advisor_save", |env| {
        let config = advisor::AdvisorConfig {
            enabled: true,
            model_key: "gpt-5".to_string(),
            effort: "max".to_string(),
            disabled_for_models: vec!["x".to_string()],
        };
        advisor::save_advisor_config(env, &config, None).expect("写回 advisor");
    });
}

#[test]
fn advisor_clear_matches_python() {
    check_save("advisor_clear", |env| {
        advisor::clear_advisor_config(env, None).expect("清除 advisor");
    });
}

#[test]
fn tts_load_matches_python() {
    check_load("tts_load", |env| {
        tts::load_tts_configuration(env, None)
            .map(|config| {
                json!({
                    "enabled": config.enabled,
                    "model_dir": config.model_dir,
                    "voice": config.voice,
                    "auto_play": config.auto_play,
                    "thread_count": config.thread_count,
                    "device": config.device,
                    "streaming": config.streaming,
                    "output_dir": config.output_dir,
                })
            })
            .map_err(|error| error.message().to_string())
    });
}

#[test]
fn tts_save_matches_python() {
    check_save("tts_save", |env| {
        let config = tts::TtsConfiguration {
            enabled: true,
            voice: "Junhao".to_string(),
            thread_count: 4,
            ..tts::TtsConfiguration::default()
        };
        tts::save_tts_configuration(env, &config, None).expect("写回 tts");
    });
}

#[test]
fn agent_workspace_load_matches_python() {
    check_load("agent_workspace_load", |env| {
        agent_workspace::load_agent_workspace_config(env, None)
            .map(|config| {
                json!({
                    "enabled": config.enabled,
                    "mode": config.mode,
                    "base_branch": config.base_branch,
                    "base_ref": config.base_ref,
                    "detached": config.detached,
                    "apply_on_exit": config.apply_on_exit,
                    "cleanup_on_exit": config.cleanup_on_exit,
                    "sync_uncommitted": config.sync_uncommitted,
                    "copy_dirs": config.copy_dirs,
                    "env_scripts": config.env_scripts,
                })
            })
            .map_err(|error| error.message().to_string())
    });
}

#[test]
fn agent_workspace_save_matches_python() {
    check_save("agent_workspace_save", |env| {
        let config = agent_workspace::AgentWorkspaceConfig {
            mode: "local".to_string(),
            copy_dirs: vec![".env".to_string()],
            ..agent_workspace::AgentWorkspaceConfig::default()
        };
        agent_workspace::save_agent_workspace_config(env, &config, None)
            .expect("写回隔离工作区配置");
    });
}

#[test]
fn image_gen_load_matches_python() {
    check_load("image_gen_load", |env| {
        image_gen::load_image_gen_configuration(env, None)
            .map(|config| {
                json!({
                    "enabled": config.enabled,
                    "base_url": config.base_url,
                    "api_key": config.api_key,
                    "api_key_env": config.api_key_env,
                    "model": config.model,
                    "size": config.size,
                    "quality": config.quality,
                    "output_format": config.output_format,
                    "n": config.n,
                    "timeout_seconds": config.timeout_seconds,
                })
            })
            .map_err(|error| error.message().to_string())
    });
}

#[test]
fn image_gen_save_matches_python() {
    check_save("image_gen_save", |env| {
        let config = image_gen::ImageGenConfiguration {
            enabled: true,
            size: "1024x1024".to_string(),
            n: 2,
            ..image_gen::ImageGenConfiguration::default()
        };
        image_gen::save_image_gen_configuration(env, &config, None).expect("写回图像生成配置");
    });
}

#[test]
fn tool_output_compression_load_matches_python() {
    check_load("tool_output_compression_load", |env| {
        compression::load_tool_output_compression_config(env, None)
            .map(|config| {
                json!({
                    "enabled": config.enabled,
                    "model_key": config.model_key,
                    "thinking_enabled": config.thinking_enabled,
                    "reasoning_effort": config.reasoning_effort,
                    "min_chars": config.min_chars,
                    "max_input_chars": config.max_input_chars,
                    "max_output_chars": config.max_output_chars,
                    "timeout_seconds": config.timeout_seconds,
                    "active": config.active(),
                })
            })
            .map_err(|error| error.message().to_string())
    });
}

#[test]
fn tool_output_compression_save_matches_python() {
    check_save("tool_output_compression_save", |env| {
        let config = compression::ToolOutputCompressionConfig {
            enabled: true,
            model_key: "qwen2.5-3b-instruct".to_string(),
            ..compression::ToolOutputCompressionConfig::default()
        };
        compression::save_tool_output_compression_config(env, &config, None).expect("写回压缩配置");
    });
}

#[test]
fn tool_output_compression_clear_matches_python() {
    check_save("tool_output_compression_clear", |env| {
        compression::clear_tool_output_compression_config(env, None).expect("清除压缩配置");
    });
}

#[test]
fn desensitization_load_matches_python() {
    check_load("desensitization_load", |env| {
        desensitization::load_desensitization_config(env, None)
            .map(|config| {
                json!({
                    "enabled": config.enabled,
                    "fail_closed": config.fail_closed,
                    "strict_restore": config.strict_restore,
                    "extra_sensitive_keys": config.extra_sensitive_keys,
                    "exempt_keys": config.exempt_keys,
                    "entropy_enabled": config.entropy_enabled,
                    "entropy_min_length": config.entropy_min_length,
                    "entropy_min_bits": config.entropy_min_bits,
                    "entropy_pure_letters": config.entropy_pure_letters,
                    "entropy_pure_digits": config.entropy_pure_digits,
                    "detect_pem_private_key": config.detect_pem_private_key,
                    "detect_db_connection_string": config.detect_db_connection_string,
                    "detect_email": config.detect_email,
                    "detect_bank_card": config.detect_bank_card,
                    "detect_internal_ip": config.detect_internal_ip,
                    "detect_external_ip": config.detect_external_ip,
                    "detect_url": config.detect_url,
                    "detect_mac_address": config.detect_mac_address,
                    "detect_license_plate": config.detect_license_plate,
                    "gitleaks_enabled": config.gitleaks_enabled,
                    "gitleaks_config_path": config.gitleaks_config_path,
                    "ner_enabled": config.ner_enabled,
                    "ner_model_path": config.ner_model_path,
                    "ner_device": config.ner_device,
                    "ner_entity_types": config.ner_entity_types,
                    "ner_min_entity_chars": config.ner_min_entity_chars,
                    "ner_cache_size": config.ner_cache_size,
                })
            })
            .map_err(|error| error.message().to_string())
    });
}

#[test]
fn desensitization_save_matches_python() {
    check_save("desensitization_save", |env| {
        let config = desensitization::DesensitizationConfig {
            enabled: true,
            extra_sensitive_keys: vec!["secret".to_string()],
            ner_entity_types: vec!["PER".to_string(), "LOC".to_string()],
            ..desensitization::DesensitizationConfig::default()
        };
        desensitization::save_desensitization_config(env, &config, None).expect("写回脱敏配置");
    });
}
