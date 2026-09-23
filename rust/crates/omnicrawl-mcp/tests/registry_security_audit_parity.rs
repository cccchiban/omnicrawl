//! 能力注册表、安全校验与审计的对照测试。

mod common;

use std::sync::Arc;

use omnicrawl_mcp::audit::{AuditRecord, McpAuditLogger};
use omnicrawl_mcp::registry::{McpCapabilityRegistry, McpPromptMeta, McpResourceMeta, McpToolMeta};
use omnicrawl_mcp::security::{redact_sensitive_values, validate_tool_arguments};
use serde_json::{json, Map, Value};

#[test]
fn registry_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data, "registry");
    assert!(!cases.is_empty());
    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let mut registry = McpCapabilityRegistry::new();
        for step in common::field(&case, "steps").as_array().expect("步骤表") {
            match common::field(step, "kind").as_str().unwrap_or_default() {
                "tool" => registry.add_tool(McpToolMeta {
                    logical_name: text(step, "logical_name"),
                    server_name: text(step, "server_name"),
                    tool_name: text(step, "tool_name"),
                    description: text(step, "description"),
                    input_schema: step
                        .get("input_schema")
                        .and_then(|value| value.as_object())
                        .cloned()
                        .unwrap_or_default(),
                    requires_confirmation: true,
                    risk_level: "restricted".to_string(),
                }),
                "resource" => registry.add_resource(McpResourceMeta {
                    logical_uri: text(step, "logical_uri"),
                    server_name: text(step, "server_name"),
                    uri: text(step, "uri"),
                    name: text(step, "name"),
                    description: step
                        .get("description")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    mime_type: step
                        .get("mime_type")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }),
                _ => registry.add_prompt(McpPromptMeta {
                    logical_name: text(step, "logical_name"),
                    server_name: text(step, "server_name"),
                    prompt_name: text(step, "prompt_name"),
                    description: step
                        .get("description")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: step
                        .get("arguments")
                        .and_then(|value| value.as_array())
                        .cloned()
                        .unwrap_or_default(),
                }),
            }
        }

        let tools: Vec<Value> = registry
            .tools()
            .map(|meta| {
                json!({
                    "logical_name": meta.logical_name,
                    "server_name": meta.server_name,
                    "tool_name": meta.tool_name,
                    "description": meta.description,
                    "requires_confirmation": meta.requires_confirmation,
                    "risk_level": meta.risk_level,
                    "argument_schema": meta.argument_schema(),
                })
            })
            .collect();
        assert_eq!(
            Value::Array(tools),
            *common::field(&case, "tools"),
            "用例 {name} 的工具表不一致"
        );

        let resources: Vec<Value> = registry
            .resources()
            .map(|meta| {
                json!({
                    "logical_uri": meta.logical_uri,
                    "server_name": meta.server_name,
                    "uri": meta.uri,
                    "name": meta.name,
                    "description": meta.description,
                    "mime_type": meta.mime_type,
                })
            })
            .collect();
        assert_eq!(
            Value::Array(resources),
            *common::field(&case, "resources"),
            "用例 {name} 的资源表不一致"
        );

        let prompts: Vec<Value> = registry
            .prompts()
            .map(|meta| {
                json!({
                    "logical_name": meta.logical_name,
                    "server_name": meta.server_name,
                    "prompt_name": meta.prompt_name,
                    "description": meta.description,
                    "arguments": meta.arguments,
                })
            })
            .collect();
        assert_eq!(
            Value::Array(prompts),
            *common::field(&case, "prompts"),
            "用例 {name} 的 Prompt 表不一致"
        );

        let diagnostics: Vec<Value> = registry
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
    }
}

#[test]
fn argument_validation_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data["security"], "arguments");
    assert!(cases.len() >= 15);
    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let arguments = common::field(&case, "arguments")
            .as_object()
            .cloned()
            .unwrap_or_default();
        let schema = case
            .get("schema")
            .and_then(|value| value.as_object())
            .cloned();
        match validate_tool_arguments(&arguments, schema.as_ref()) {
            Ok(()) => assert_eq!(
                json!(true),
                *common::field(&case, "ok"),
                "用例 {name} 本应通过校验"
            ),
            Err(message) => assert_eq!(
                json!(message),
                *common::field(&case, "error"),
                "用例 {name} 的错误文案不一致"
            ),
        }
    }
}

#[test]
fn redaction_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data["security"], "redaction");
    assert!(!cases.is_empty());
    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        assert_eq!(
            redact_sensitive_values(common::field(&case, "value")),
            *common::field(&case, "redacted"),
            "用例 {name} 的脱敏结果不一致"
        );
    }
}

#[test]
fn audit_lines_match_python() {
    let data = common::fixture();
    let cases = common::cases(&data, "audit");
    let root = common::temp_workspace("audit-parity");
    let clock: Arc<dyn Fn() -> String + Send + Sync> =
        Arc::new(|| "2026-09-20T01:54:10+08:00".to_string());

    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        match name {
            "escape_path" => {
                let logger = McpAuditLogger::with_relative_path(&root, true, "../escape.jsonl");
                let tail: Vec<String> = logger
                    .path()
                    .components()
                    .rev()
                    .take(3)
                    .map(|item| item.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<String>>()
                    .into_iter()
                    .rev()
                    .collect();
                assert_eq!(
                    json!(tail.join("/")),
                    *common::field(&case, "tail"),
                    "越界日志路径应当回落到默认位置"
                );
            }
            "disabled" => {
                let logger = McpAuditLogger::new(&root, false);
                logger.record_tool_call(sample_record());
                assert_eq!(
                    json!(logger
                        .path()
                        .metadata()
                        .map(|meta| meta.len() != 0)
                        .unwrap_or(false)),
                    *common::field(&case, "wrote_file"),
                    "关闭审计时不该写文件"
                );
            }
            _ => {
                let path = root.join(format!("{name}.jsonl"));
                let logger =
                    McpAuditLogger::with_relative_path(&root, true, &format!("{name}.jsonl"))
                        .with_clock(Arc::clone(&clock));
                logger.record_tool_call(audit_record(name));
                let line = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("用例 {name} 未写出审计行：{error}"));
                let line = common::normalize_ids(line.trim_end());
                assert_eq!(
                    json!(line),
                    *common::field(&case, "line"),
                    "用例 {name} 的审计行不一致"
                );
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn audit_record(name: &str) -> AuditRecord<'static> {
    match name {
        "approved" => AuditRecord {
            session_id: "session-0123456789ab",
            audit_id: "mcp-0123456789ab",
            server_name: "files",
            tool_name: "read",
            arguments: APPROVED_ARGUMENTS.get_or_init(approved_arguments),
            approval_mode: "manual",
            approval_result: "approved",
            duration_ms: 12,
            ok: true,
            error_code: None,
            output: "  done  ",
        },
        other => AuditRecord {
            session_id: "session-0123456789ab",
            audit_id: "mcp-0123456789ab",
            server_name: "files",
            tool_name: "read",
            arguments: LONG_ARGUMENTS.get_or_init(Map::new),
            approval_mode: "manual",
            approval_result: if other == "long_output" {
                "denied"
            } else {
                "approved"
            },
            duration_ms: 12,
            ok: false,
            error_code: Some("TOOL_FAILED"),
            output: LONG_OUTPUT
                .get_or_init(|| format!("token=sk-0123456789abcdefghijklmn {}", "x".repeat(3000)))
                .as_str(),
        },
    }
}

fn sample_record() -> AuditRecord<'static> {
    static ARGUMENTS: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    AuditRecord {
        session_id: "s",
        audit_id: "mcp-0123456789ab",
        server_name: "",
        tool_name: "read",
        arguments: ARGUMENTS.get_or_init(Map::new),
        approval_mode: "",
        approval_result: "not_found",
        duration_ms: 0,
        ok: false,
        error_code: Some("TOOL_NOT_FOUND"),
        output: "",
    }
}

fn approved_arguments() -> Map<String, Value> {
    let mut arguments = Map::new();
    arguments.insert("path".to_string(), json!("/tmp/a"));
    arguments.insert("api_key".to_string(), json!("sk-0123456789abcdefghijklmn"));
    arguments
}

static APPROVED_ARGUMENTS: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
static LONG_ARGUMENTS: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
static LONG_OUTPUT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn text(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .to_string()
}
