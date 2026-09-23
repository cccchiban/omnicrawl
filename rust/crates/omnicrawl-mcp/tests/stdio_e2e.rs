//! stdio 传输端到端自测：管理器真的拉起 `omnicrawl-mcp-server` 子进程，
//! 两侧都跑本仓库的 Rust 实现（握手、分帧、stderr 排空、超时回收都在真进程上验证）。

mod common;

use omnicrawl_mcp::client::McpClientManager;
use omnicrawl_mcp::config::{McpConfig, McpPolicyConfig, McpServerConfig, MCP_TRANSPORT_STDIO};
use serde_json::json;

fn server_config(name: &str, workspace: &std::path::Path) -> McpServerConfig {
    let mut server = McpServerConfig::new(name, MCP_TRANSPORT_STDIO);
    server.command = Some(env!("CARGO_BIN_EXE_omnicrawl-mcp-server").to_string());
    server.env.insert(
        "MCP_WORKSPACE_ROOT".to_string(),
        workspace.to_string_lossy().to_string(),
    );
    server.timeout_seconds = 15;
    server
}

fn config(server: McpServerConfig) -> McpConfig {
    McpConfig {
        enabled: true,
        default_timeout_seconds: 15,
        servers: vec![(server.name.clone(), server)],
        policy: McpPolicyConfig::default(),
    }
}

#[test]
fn discovery_and_reads_run_through_a_real_subprocess() {
    let root = common::temp_workspace("stdio-e2e");
    common::prepare_workspace(&root);
    let manager = McpClientManager::new(config(server_config("local", &root)), &root);
    manager.discover();

    assert_eq!(manager.connected_count(), 1, "{}", manager.format_status());
    assert!(manager.tools().is_empty(), "本地 Server 不暴露工具");
    assert_eq!(manager.prompts().len(), 4);

    let resources: Vec<String> = manager
        .resources()
        .iter()
        .map(|meta| meta.logical_uri.clone())
        .collect();
    assert!(
        resources.contains(&"local:project://README.md".to_string()),
        "{resources:?}"
    );
    assert!(
        resources.contains(&"local:omnicrawl://docs/MCP_USAGE.md".to_string()),
        "{resources:?}"
    );

    let readme = manager.read_resource("local:project://README.md");
    assert!(readme.ok, "{readme:?}");
    assert_eq!(readme.output, "Resource project://README.md:\n# 项目\n");

    let health = manager.read_resource("local:server://local_project/health");
    assert!(health.ok, "{health:?}");
    assert!(health.output.contains("ok"), "{health:?}");

    // 受保护文件在 `resources/list` 阶段就被过滤，因此这里按「未注册」拒绝。
    assert!(!resources.contains(&"local:project://config.toml".to_string()));
    let protected = manager.read_resource("local:project://config.toml");
    assert_eq!(protected.error_code.as_deref(), Some("RESOURCE_NOT_FOUND"));

    let prompt = manager.get_prompt("local.debug_triage", Some(&prompt_arguments()));
    assert!(prompt.ok, "{prompt:?}");
    assert_eq!(
        prompt.output,
        "user: 请进行排障分析。\n错误现象：崩了\n期望行为：\n要求：给出最可能原因、验证步骤和最小修复路径。"
    );

    // 资源与 Prompt 读取不进审计；工具调用（含被拒绝的调用）才写审计行。
    manager.record_denied_tool_call("local.unknown", &serde_json::Map::new(), "用户取消。");
    let audit = root.join(".omnicrawl").join("logs").join("mcp-audit.jsonl");
    let text = std::fs::read_to_string(&audit)
        .unwrap_or_else(|error| panic!("审计日志应当写入 {}：{error}", audit.display()));
    assert!(text.contains("APPROVAL_DENIED"), "{text}");

    manager.close();
    let after_close = manager.read_resource("local:project://README.md");
    assert_eq!(
        after_close.error_code.as_deref(),
        Some("SERVER_UNAVAILABLE")
    );
}

#[test]
fn unreachable_command_degrades_without_blocking_other_servers() {
    let root = common::temp_workspace("stdio-e2e-degraded");
    common::prepare_workspace(&root);
    let mut broken = McpServerConfig::new("broken", MCP_TRANSPORT_STDIO);
    broken.command = Some("omnicrawl-definitely-missing-command".to_string());
    let mut working = server_config("local", &root);
    working.timeout_seconds = 15;
    let manager = McpClientManager::new(
        McpConfig {
            enabled: true,
            default_timeout_seconds: 15,
            servers: vec![
                (broken.name.clone(), broken),
                (working.name.clone(), working),
            ],
            policy: McpPolicyConfig::default(),
        },
        &root,
    );
    manager.discover();

    assert_eq!(manager.connected_count(), 1);
    let diagnostics = manager.diagnostics();
    let codes: Vec<&str> = diagnostics.iter().map(|item| item.code.as_str()).collect();
    assert!(codes.contains(&"SERVER_UNAVAILABLE"), "{codes:?}");
    let status = manager.format_status();
    assert!(status.contains("broken: 启用, stdio, degraded"), "{status}");
    assert!(status.contains("local: 启用, stdio, connected"), "{status}");
    manager.close();
}

#[test]
fn discovery_is_idempotent_and_lazy() {
    let root = common::temp_workspace("stdio-e2e-idempotent");
    common::prepare_workspace(&root);
    let manager = McpClientManager::new(config(server_config("local", &root)), &root);
    assert!(!manager.discovered());
    manager.discover();
    let first = manager.resources().len();
    manager.discover();
    assert_eq!(manager.resources().len(), first, "重复发现不该重复登记");
    assert!(manager.discovered());
    manager.close();
}

fn prompt_arguments() -> serde_json::Map<String, serde_json::Value> {
    let mut arguments = serde_json::Map::new();
    arguments.insert("error".to_string(), json!("崩了"));
    arguments
}
