//! `config/features/subagents.py` 的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_config_subagents_fixture.py` 重新生成
//! `fixtures/config_subagents_parity.json` 后，本套件重放设置面板校验与读盘用例。
//! 环境变量由 `ConfigEnvironment` 注入，用例之间的取值互不干扰。

use std::collections::BTreeMap;
use std::path::PathBuf;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::subagents::{
    load_subagent_config, validate_subagent_advanced_setting, AdvancedValue, SubAgentConfig,
    SUBAGENT_ADVANCED_SETTING_KEYS,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/config_subagents_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn toml_value(value: &Value) -> toml::Value {
    match value {
        Value::Bool(inner) => toml::Value::Boolean(*inner),
        Value::Number(number) if number.is_i64() => {
            toml::Value::Integer(number.as_i64().expect("整数"))
        }
        Value::Number(number) => toml::Value::Float(number.as_f64().expect("浮点")),
        Value::String(inner) => toml::Value::String(inner.clone()),
        other => panic!("用例值不受支持：{other}"),
    }
}

fn config_view(config: &SubAgentConfig) -> Value {
    let overrides: BTreeMap<String, String> = config.model_overrides.clone();
    json!({
        "enabled": config.enabled,
        "max_depth": config.max_depth,
        "max_concurrency": config.max_concurrency,
        "max_tasks_per_batch": config.max_tasks_per_batch,
        "default_timeout_seconds": config.default_timeout_seconds,
        "model_request_concurrency": config.model_request_concurrency,
        "allow_background": config.allow_background,
        "allow_fork": config.allow_fork,
        "allow_shared_workspace_writes": config.allow_shared_workspace_writes,
        "allow_worktree": config.allow_worktree,
        "allow_standard_agent": config.allow_standard_agent,
        "enable_verify_agent": config.enable_verify_agent,
        "verify_command_timeout_seconds": config.verify_command_timeout_seconds,
        "task_retention_minutes": config.task_retention_minutes,
        "result_summary_chars": config.result_summary_chars,
        "model_overrides": overrides,
    })
}

fn case_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "omnicrawl-config-subagents-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("建临时目录");
    path
}

fn environment(env: &Value) -> ConfigEnvironment {
    let mut environment = ConfigEnvironment::new(std::env::temp_dir(), "windows");
    for (name, value) in env.as_object().expect("env 对象") {
        environment = environment.with_env_value(name, value.as_str().expect("env 值"));
    }
    environment
}

#[test]
fn advanced_setting_matches_python() {
    let data = fixture();
    let keys: Vec<String> = data["advanced_keys"]
        .as_array()
        .expect("advanced_keys")
        .iter()
        .map(|item| item.as_str().expect("键").to_string())
        .collect();
    assert_eq!(keys, SUBAGENT_ADVANCED_SETTING_KEYS.to_vec());

    for case in data["advanced"].as_array().expect("advanced") {
        let label = case["label"].as_str().expect("label");
        let name = case["name"].as_str().expect("name");
        let value = toml_value(&case["value"]);
        let result = validate_subagent_advanced_setting(name, &value);
        if case["ok"].as_bool().expect("ok") {
            let expected = &case["result"];
            match (
                result.expect("应成功"),
                case["kind"].as_str().expect("kind"),
            ) {
                (AdvancedValue::Int(actual), "int") => {
                    assert_eq!(Value::from(actual), *expected, "{label}")
                }
                (AdvancedValue::Number(actual), "number") => {
                    assert_eq!(
                        serde_json::Number::from_f64(actual).map(Value::Number),
                        Some(expected.clone()),
                        "{label}"
                    )
                }
                (other, kind) => panic!("{label}：类型不符，{other:?} vs {kind}"),
            }
        } else {
            let error = result.expect_err("应失败");
            assert_eq!(error.message(), case["error"].as_str().unwrap(), "{label}");
        }
    }
}

#[test]
fn load_config_matches_python() {
    let data = fixture();
    let dir = case_dir("load");
    for (index, case) in data["load"].as_array().expect("load").iter().enumerate() {
        let label = case["label"].as_str().expect("label");
        let path = dir.join(format!("case-{index}.toml"));
        std::fs::write(&path, case["toml"].as_str().expect("toml")).expect("写 TOML");
        let environment = environment(&case["env"]);
        let result = load_subagent_config(&environment, Some(&path));
        if case["ok"].as_bool().expect("ok") {
            let config = result.unwrap_or_else(|error| panic!("{label}：{}", error.message()));
            assert_eq!(config_view(&config), case["config"], "{label}");
        } else {
            let error = result
                .err()
                .unwrap_or_else(|| panic!("{label}：Rust 侧不应成功"));
            assert_eq!(error.message(), case["error"].as_str().unwrap(), "{label}");
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}
