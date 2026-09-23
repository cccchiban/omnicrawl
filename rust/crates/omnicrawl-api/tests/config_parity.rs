//! 本地 API 配置面（`api/models.py::APIConfig` 与 `api/app.py::load_api_config`）的对照测试。
//!
//! 期望值来自 Python 真实现（生成器 `rust/tools/gen_api_config_fixture.py`）：
//! 逐条比对归一化字段，或比对异常文案字符串。

use omnicrawl_api::config::{api_config_from_section, ApiConfig, DEFAULT_HOST};
use omnicrawl_config::core::runtime::{get_section, ConfigEnvironment};
use omnicrawl_config::toml::{Table, Value as TomlValue};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/api_config_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

/// 与生成器里的固定 home 对应；平台取 win32 以对齐 `legacy_user_config_dirs` 分支。
fn environment(case: &Value) -> ConfigEnvironment {
    let mut env = ConfigEnvironment::new("C:\\oc-api-parity\\home", "win32");
    if let Some(map) = case["env"].as_object() {
        for (name, value) in map {
            if let Some(text) = value.as_str() {
                env = env.with_env_value(name, text);
            }
        }
    }
    env
}

fn toml_value(value: &Value) -> TomlValue {
    match value {
        Value::Null => TomlValue::String(String::new()),
        Value::Bool(flag) => TomlValue::Boolean(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => TomlValue::Integer(integer),
            None => TomlValue::Float(number.as_f64().unwrap_or_default()),
        },
        Value::String(text) => TomlValue::String(text.clone()),
        Value::Array(items) => TomlValue::Array(items.iter().map(toml_value).collect()),
        Value::Object(map) => TomlValue::Table(
            map.iter()
                .map(|(key, item)| (key.clone(), toml_value(item)))
                .collect::<Table>(),
        ),
    }
}

/// 生成器把用例的 `api` 段整段存下来，这里还原成一份只含 `api` 键的配置文档。
fn config_data(case: &Value) -> Table {
    let mut table = Table::new();
    table.insert("api".to_string(), toml_value(&case["section"]));
    table
}

fn outcome_of(config: Result<ApiConfig, omnicrawl_api::ApiConfigError>) -> Value {
    match config {
        Ok(config) => {
            let mut value = serde_json::Map::new();
            value.insert("bearer_token".into(), config.bearer_token.into());
            value.insert("host".into(), config.host.into());
            value.insert("port".into(), config.port.into());
            value.insert(
                "allowed_origins".into(),
                Value::Array(
                    config
                        .allowed_origins
                        .into_iter()
                        .map(Value::from)
                        .collect(),
                ),
            );
            value.insert(
                "confirmation_timeout_seconds".into(),
                config.confirmation_timeout_seconds.into(),
            );
            value.insert("workers".into(), config.workers.into());
            serde_json::json!({ "ok": Value::Object(value) })
        }
        Err(error) => serde_json::json!({ "error": error.message() }),
    }
}

fn int_field(case: &Value, key: &str, fallback: i64) -> i64 {
    case[key].as_i64().unwrap_or(fallback)
}

fn float_field(case: &Value, key: &str, fallback: f64) -> f64 {
    case[key].as_f64().unwrap_or(fallback)
}

fn origins_field(case: &Value) -> Vec<String> {
    case["allowed_origins"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn construct_cases_match_python() {
    let fixture = fixture();
    let cases = fixture["constructs"].as_array().expect("构造用例");
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap_or_default();
        let input = &case["input"];
        let token = input["bearer_token"].as_str().unwrap_or_default();
        let host = input["host"].as_str().unwrap_or(DEFAULT_HOST);
        let port = match input["port"].as_str() {
            // 生成器里的 `port="8765"` 走 Python 的 `int("8765")`。
            Some(text) => text.parse::<i64>().expect("用例里的数字字符串"),
            None => int_field(input, "port", 8765),
        };
        let config = ApiConfig::new(
            token,
            host,
            port,
            origins_field(input),
            float_field(input, "confirmation_timeout_seconds", 300.0),
            int_field(input, "workers", 1),
        );
        assert_eq!(outcome_of(config), case["outcome"], "构造用例不符：{name}");
    }
}

#[test]
fn load_cases_match_python() {
    let fixture = fixture();
    let cases = fixture["loads"].as_array().expect("装载用例");
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap_or_default();
        let environment = environment(case);
        // 走真实现的 `get_section`：段不是对象时由它给出类型错误文案。
        let outcome = match get_section(&config_data(case), "api") {
            Err(error) => serde_json::json!({ "error": error.to_string() }),
            Ok(section) => outcome_of(api_config_from_section(&section, &environment)),
        };
        assert_eq!(outcome, case["outcome"], "装载用例不符：{name}");
    }
}
