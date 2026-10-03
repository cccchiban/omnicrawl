//! MCP 配置对照：`config.toml` 的 `[mcp]` 段与校验错误文案（环境变量通道已移除）。

mod common;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_mcp::config::{load_mcp_config, McpConfig, McpServerConfig};
use serde_json::{json, Map, Value};

#[test]
fn config_matches_python() {
    let data = common::fixture();
    let root = common::temp_workspace("config-parity");
    let config_path = root.join("config.toml");
    let cases = common::cases(&data, "config");
    assert!(cases.len() >= 20, "数据集太小：{}", cases.len());

    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let mut env = ConfigEnvironment::new(&root, "linux");
        if let Some(values) = case.get("env").and_then(|value| value.as_object()) {
            for (key, value) in values {
                env = env.with_env_value(key, value.as_str().unwrap_or_default());
            }
        }
        std::fs::write(
            &config_path,
            common::field(&case, "toml").as_str().unwrap_or_default(),
        )
        .expect("写临时配置");

        match load_mcp_config(&env, Some(&config_path)) {
            Ok(config) => assert_eq!(
                config_to_json(&config),
                *common::field(&case, "config"),
                "用例 {name} 的取值不一致"
            ),
            Err(error) => assert_eq!(
                json!(error.to_string()),
                *common::field(&case, "error"),
                "用例 {name} 的错误文案不一致"
            ),
        }
    }
}

fn config_to_json(config: &McpConfig) -> Value {
    let mut servers = Map::new();
    for (name, server) in &config.servers {
        servers.insert(name.clone(), server_to_json(server));
    }
    json!({
        "enabled": config.enabled,
        "default_timeout_seconds": config.default_timeout_seconds,
        "servers": servers,
        "policy": {
            "require_confirmation_for_write": config.policy.require_confirmation_for_write,
            "require_confirmation_for_command": config.policy.require_confirmation_for_command,
            "allow_external_network_tools": config.policy.allow_external_network_tools,
            "audit_log_enabled": config.policy.audit_log_enabled,
        },
    })
}

fn server_to_json(server: &McpServerConfig) -> Value {
    json!({
        "name": server.name,
        "enabled": server.enabled,
        "transport": server.transport,
        "command": server.command,
        "args": server.args,
        "url": server.url,
        "env": text_map_to_json(&server.env),
        "headers": text_map_to_json(&server.headers),
        "timeout_seconds": server.timeout_seconds,
        "risk_level": server.risk_level,
    })
}

fn text_map_to_json(map: &omnicrawl_mcp::config::TextMap) -> Value {
    let mut result = Map::new();
    for (key, value) in map {
        result.insert(key.clone(), json!(value));
    }
    Value::Object(result)
}
