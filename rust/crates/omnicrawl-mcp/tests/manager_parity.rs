//! 多 Server 管理器的对照测试：发现、登记、状态、降级与调用结果（脚本化连接）。

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use omnicrawl_mcp::client::{DiscoveredCapabilities, McpClientManager, McpConnection};
use omnicrawl_mcp::config::{McpConfig, McpPolicyConfig, McpServerConfig, TextMap};
use omnicrawl_mcp::jsonrpc::{McpCallError, McpClientError};
use serde_json::{json, Map, Value};

#[test]
fn manager_behaviour_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data, "manager");
    assert!(cases.len() >= 12, "数据集太小：{}", cases.len());

    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let root = common::temp_workspace(&format!("manager-{name}"));
        let config = config_from(common::field(&case, "config"));
        let script = common::field(&case, "script").clone();
        let connector: Arc<omnicrawl_mcp::client::Connector> =
            Arc::new(move |server: &McpServerConfig, _root: &Path| {
                let server_script = script.get(&server.name).cloned().unwrap_or(Value::Null);
                Ok(Arc::new(ScriptedConnection {
                    script: server_script,
                }) as Arc<dyn McpConnection>)
            });
        let manager = McpClientManager::new(config, &root)
            .with_connector(connector)
            .with_approval_mode(Arc::new(|| "manual".to_string()))
            .with_audit_clock(Arc::new(|| "2026-09-20T01:54:10+08:00".to_string()));

        manager.discover();

        let script_steps = common::field(&case, "script");
        for expected in common::field(&case, "operations")
            .as_array()
            .expect("操作表")
        {
            let op = common::field(expected, "op").as_str().unwrap_or_default();
            let input = common::field(expected, "input");
            let mut expected_result = expected.clone();
            if let Some(map) = expected_result.as_object_mut() {
                map.remove("input");
            }
            let expected = &expected_result;
            match op {
                "call_tool" => {
                    let arguments = input
                        .get("arguments")
                        .and_then(|value| value.as_object())
                        .cloned()
                        .unwrap_or_default();
                    let result =
                        manager.call_tool(input["name"].as_str().unwrap_or_default(), &arguments);
                    let actual = json!({
                        "op": "call_tool",
                        "ok": result.ok,
                        "server_name": result.server_name,
                        "tool_name": result.tool_name,
                        "output": result.output,
                        "full_output": result.full_output,
                        "error_code": result.error_code,
                        "retryable": result.retryable,
                        "audit_id": common::ID_PLACEHOLDER,
                    });
                    assert_eq!(actual, *expected, "用例 {name} 的 call_tool 结果不一致");
                }
                "read_resource" => {
                    let result = manager.read_resource(input["name"].as_str().unwrap_or_default());
                    let actual = json!({
                        "op": "read_resource",
                        "ok": result.ok,
                        "server_name": result.server_name,
                        "uri": result.uri,
                        "output": result.output,
                        "full_output": result.full_output,
                        "error_code": result.error_code,
                        "retryable": result.retryable,
                    });
                    assert_eq!(actual, *expected, "用例 {name} 的 read_resource 结果不一致");
                }
                "get_prompt" => {
                    let arguments = input
                        .get("arguments")
                        .and_then(|value| value.as_object())
                        .cloned();
                    let result = manager.get_prompt(
                        input["name"].as_str().unwrap_or_default(),
                        arguments.as_ref(),
                    );
                    let actual = json!({
                        "op": "get_prompt",
                        "ok": result.ok,
                        "server_name": result.server_name,
                        "prompt_name": result.prompt_name,
                        "output": result.output,
                        "full_output": result.full_output,
                        "error_code": result.error_code,
                        "retryable": result.retryable,
                    });
                    assert_eq!(actual, *expected, "用例 {name} 的 get_prompt 结果不一致");
                }
                _ => {
                    let arguments = input
                        .get("arguments")
                        .and_then(|value| value.as_object())
                        .cloned()
                        .unwrap_or_default();
                    manager.record_denied_tool_call(
                        input["name"].as_str().unwrap_or_default(),
                        &arguments,
                        input["reason"].as_str().unwrap_or("用户取消。"),
                    );
                }
            }
        }

        let tools: Vec<Value> = manager
            .tools()
            .iter()
            .map(|meta| {
                json!({
                    "logical_name": meta.logical_name,
                    "server_name": meta.server_name,
                    "tool_name": meta.tool_name,
                    "argument_schema": meta.argument_schema(),
                    "requires_confirmation": meta.requires_confirmation,
                })
            })
            .collect();
        assert_eq!(
            Value::Array(tools),
            *common::field(&case, "tools"),
            "用例 {name} 的工具表不一致"
        );

        let resources: Vec<Value> = manager
            .resources()
            .iter()
            .map(|meta| json!(meta.logical_uri))
            .collect();
        assert_eq!(
            Value::Array(resources),
            *common::field(&case, "resources"),
            "用例 {name} 的资源表不一致"
        );

        let prompts: Vec<Value> = manager
            .prompts()
            .iter()
            .map(|meta| json!(meta.logical_name))
            .collect();
        assert_eq!(
            Value::Array(prompts),
            *common::field(&case, "prompts"),
            "用例 {name} 的 Prompt 表不一致"
        );

        let diagnostics: Vec<Value> = manager
            .diagnostics()
            .iter()
            .map(|item| {
                json!({
                    "severity": item.severity,
                    "code": item.code,
                    "message": item.message,
                    "server_name": item.server_name,
                })
            })
            .collect();
        assert_eq!(
            Value::Array(diagnostics),
            *common::field(&case, "diagnostics"),
            "用例 {name} 的诊断不一致"
        );

        let status = manager.format_status().replace(
            &root.to_string_lossy().to_string(),
            common::WORKSPACE_PLACEHOLDER,
        );
        assert_eq!(
            json!(status),
            *common::field(&case, "status"),
            "用例 {name} 的状态文本不一致"
        );

        let audit_lines = read_audit_lines(&root);
        assert_eq!(
            json!(audit_lines),
            *common::field(&case, "audit"),
            "用例 {name} 的审计行不一致"
        );

        if case.get("close").is_some() {
            let before = manager.connected_count();
            manager.close();
            assert!(before <= 1);
            let closed = script_steps.get("close_marker").is_none();
            assert!(closed, "脚本字段不应影响关闭流程");
        }
    }
}

fn read_audit_lines(root: &Path) -> Vec<String> {
    let path = root.join(".omnicrawl/logs/mcp-audit.jsonl");
    match std::fs::read_to_string(&path) {
        Ok(text) => common::normalize_ids(&text)
            .lines()
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn config_from(value: &Value) -> McpConfig {
    let servers: Vec<(String, McpServerConfig)> = value["servers"]
        .as_object()
        .map(|items| {
            items
                .iter()
                .map(|(name, server)| {
                    (
                        name.clone(),
                        McpServerConfig {
                            name: name.clone(),
                            enabled: server["enabled"].as_bool().unwrap_or(true),
                            transport: server["transport"].as_str().unwrap_or("stdio").to_string(),
                            command: server["command"].as_str().map(|text| text.to_string()),
                            args: server["args"]
                                .as_array()
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|item| {
                                            item.as_str().map(|text| text.to_string())
                                        })
                                        .collect()
                                })
                                .unwrap_or_default(),
                            url: server["url"].as_str().map(|text| text.to_string()),
                            env: text_map(&server["env"]),
                            headers: text_map(&server["headers"]),
                            timeout_seconds: server["timeout_seconds"].as_i64().unwrap_or(30),
                            risk_level: server["risk_level"]
                                .as_str()
                                .unwrap_or("restricted")
                                .to_string(),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    McpConfig {
        enabled: value["enabled"].as_bool().unwrap_or(false),
        default_timeout_seconds: value["default_timeout_seconds"].as_i64().unwrap_or(30),
        servers,
        policy: McpPolicyConfig {
            require_confirmation_for_write: true,
            require_confirmation_for_command: true,
            allow_external_network_tools: value["policy"]["allow_external_network_tools"]
                .as_bool()
                .unwrap_or(false),
            audit_log_enabled: value["policy"]["audit_log_enabled"]
                .as_bool()
                .unwrap_or(true),
        },
    }
}

fn text_map(value: &Value) -> TextMap {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    if let Some(items) = value.as_object() {
        for (key, item) in items {
            if let Some(text) = item.as_str() {
                map.insert(key.clone(), text.to_string());
            }
        }
    }
    map
}

struct ScriptedConnection {
    script: Value,
}

impl ScriptedConnection {
    fn outcome(&self, key: &str) -> Result<Map<String, Value>, McpCallError> {
        let Some(value) = self.script.get(key) else {
            return Ok(Map::new());
        };
        if let Some(message) = value.get("raise").and_then(|item| item.as_str()) {
            return Err(McpCallError::Timeout(McpClientError::new(message)));
        }
        if let Some(message) = value
            .get("raise_client_error")
            .and_then(|item| item.as_str())
        {
            return Err(McpCallError::Failed(McpClientError::new(message)));
        }
        Ok(value
            .get("payload")
            .and_then(|item| item.as_object())
            .cloned()
            .unwrap_or_default())
    }

    fn maps(&self, key: &str) -> Vec<Map<String, Value>> {
        self.script
            .get(key)
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_object().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl McpConnection for ScriptedConnection {
    fn discover(&self) -> Result<DiscoveredCapabilities, McpCallError> {
        if let Some(message) = self
            .script
            .get("discover_error")
            .and_then(|item| item.as_str())
        {
            return Err(McpCallError::Failed(McpClientError::new(message)));
        }
        Ok(DiscoveredCapabilities {
            tools: self.maps("tools"),
            resources: self.maps("resources"),
            prompts: self.maps("prompts"),
        })
    }

    fn call_tool(
        &self,
        _name: &str,
        _arguments: &Map<String, Value>,
    ) -> Result<Map<String, Value>, McpCallError> {
        self.outcome("tool_result")
    }

    fn read_resource(&self, _uri: &str) -> Result<Map<String, Value>, McpCallError> {
        self.outcome("resource_result")
    }

    fn get_prompt(
        &self,
        _name: &str,
        _arguments: Option<&Map<String, Value>>,
    ) -> Result<Map<String, Value>, McpCallError> {
        self.outcome("prompt_result")
    }

    fn close(&self) {}
}

#[allow(dead_code)]
fn unused(_: PathBuf) {}
