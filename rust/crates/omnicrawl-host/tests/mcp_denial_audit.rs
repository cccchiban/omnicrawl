//! 审批拒绝的 MCP Tool 调用必须落一条拒绝审计（对映 Python `_approve_tool_call` 里的
//! `mcp_manager.record_denied_tool_call`）。
//!
//! 这里直接驱动 `host::record_mcp_denial`——它是 `TurnRunner`（API/内核宿主）与 TUI 审批
//! 面板共用的唯一落点，因此测它就覆盖了三条调用路径的接线：人工拒绝、审查模型拒绝、
//! TUI 面板拒绝。MCP 侧「怎么写这条审计」由 `omnicrawl-mcp` 的对照数据集负责，本用例只
//! 关心「该不该写」。
//!
//! 工具表用的是**注入式能力注册表**：往管理器的注册表里塞一条 Tool，就不必真起 MCP
//! Server；配置里没有任何 Server，`discover()` 因此只是记一条诊断。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use omnicrawl_host::host::record_mcp_denial;
use omnicrawl_host::tools::{RegistryOptions, ToolRegistry};
use omnicrawl_mcp::config::{McpConfig, McpPolicyConfig, MCP_RISK_RESTRICTED};
use omnicrawl_mcp::registry::McpToolMeta;
use omnicrawl_mcp::McpClientManager;
use serde_json::{json, Map, Value};

/// 注入的 Server / Tool 名（逻辑名与真实发现一致：`<server>.<tool>`）。
const SERVER: &str = "local";
const TOOL: &str = "echo";
const LOGICAL: &str = "local.echo";
/// 内置工具：不是 MCP 能力，拒绝它不该写 MCP 审计。
const BUILTIN: &str = "bash";

fn temp_workspace(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "oc-host-mcp-audit-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时工作区");
    path
}

/// 只带一条注入 Tool 的管理器：不连任何真实 Server，但审计与拒绝记录都是真实现。
fn manager_with_one_tool(workspace: &Path) -> Arc<McpClientManager> {
    let config = McpConfig {
        enabled: true,
        default_timeout_seconds: 30,
        servers: Vec::new(),
        policy: McpPolicyConfig {
            audit_log_enabled: true,
            ..McpPolicyConfig::default()
        },
    };
    let manager = McpClientManager::new(config, workspace)
        .with_approval_mode(Arc::new(|| "manual".to_string()));
    manager.registry().add_tool(McpToolMeta {
        logical_name: LOGICAL.to_string(),
        server_name: SERVER.to_string(),
        tool_name: TOOL.to_string(),
        description: "回显".to_string(),
        input_schema: Map::new(),
        requires_confirmation: true,
        risk_level: MCP_RISK_RESTRICTED.to_string(),
    });
    manager.discover();
    Arc::new(manager)
}

fn registry_with(workspace: &Path, mcp: Option<Arc<McpClientManager>>) -> ToolRegistry {
    let options = RegistryOptions {
        mcp,
        ..RegistryOptions::default()
    };
    ToolRegistry::new(workspace, &options, 5).expect("工具表应当建成")
}

fn audit_lines(workspace: &Path) -> Vec<Value> {
    let path = workspace.join(".omnicrawl/logs/mcp-audit.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .expect("读取审计日志")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

#[test]
fn denied_mcp_tool_call_is_audited_but_builtin_is_not() {
    let workspace = temp_workspace("denied");
    let manager = manager_with_one_tool(&workspace);
    let registry = registry_with(&workspace, Some(Arc::clone(&manager)));
    assert!(
        registry.is_mcp_tool(LOGICAL),
        "注入的 Tool 应当被工具表认作 MCP Tool"
    );
    assert!(!registry.is_mcp_tool(BUILTIN));

    let arguments = json!({"text": "hello"})
        .as_object()
        .cloned()
        .unwrap_or_default();
    let reason = "用户取消执行：local.echo。";

    // 非 MCP 工具：空操作，连审计文件都不该出现。
    record_mcp_denial(&registry, BUILTIN, &arguments, reason);
    assert!(
        audit_lines(&workspace).is_empty(),
        "内置工具的拒绝不该写 MCP 审计"
    );

    // MCP Tool：写一条 `approval_result = denied` 的记录，参数与文案都带上。
    record_mcp_denial(&registry, LOGICAL, &arguments, reason);
    let lines = audit_lines(&workspace);
    assert_eq!(lines.len(), 1, "审计应当只多一条：{lines:?}");
    let entry = &lines[0];
    assert_eq!(entry["approval_result"], json!("denied"));
    assert_eq!(entry["approval_mode"], json!("manual"));
    assert_eq!(entry["server_name"], json!(SERVER));
    assert_eq!(entry["tool_name"], json!(TOOL));
    assert_eq!(entry["ok"], json!(false));
    assert_eq!(entry["error_code"], json!("APPROVAL_DENIED"));
    assert_eq!(entry["arguments_redacted"]["text"], json!("hello"));
    assert!(
        entry["output_preview"]
            .as_str()
            .unwrap_or_default()
            .contains(reason),
        "审计里的原因应与拒绝文案一致：{entry:?}"
    );
}

/// 没有 MCP 运行期（未配置 / 已跳过）时是空操作：不能因为审计失败把回合带崩。
#[test]
fn without_mcp_runtime_denial_recording_is_a_no_op() {
    let workspace = temp_workspace("no-mcp");
    let registry = registry_with(&workspace, None);
    assert!(!registry.is_mcp_tool(LOGICAL));

    let arguments = Map::new();
    record_mcp_denial(&registry, LOGICAL, &arguments, "用户取消执行：local.echo。");
    assert!(audit_lines(&workspace).is_empty());
}

/// 审计关闭时也不写盘（`[mcp.policy].audit_log_enabled = false`）。
#[test]
fn audit_disabled_writes_nothing() {
    let workspace = temp_workspace("audit-off");
    let config = McpConfig {
        enabled: true,
        default_timeout_seconds: 30,
        servers: Vec::new(),
        policy: McpPolicyConfig {
            audit_log_enabled: false,
            ..McpPolicyConfig::default()
        },
    };
    let manager = Arc::new(McpClientManager::new(config, &workspace));
    manager.registry().add_tool(McpToolMeta {
        logical_name: LOGICAL.to_string(),
        server_name: SERVER.to_string(),
        tool_name: TOOL.to_string(),
        description: "回显".to_string(),
        input_schema: Map::new(),
        requires_confirmation: true,
        risk_level: MCP_RISK_RESTRICTED.to_string(),
    });
    let registry = registry_with(&workspace, Some(manager));

    record_mcp_denial(
        &registry,
        LOGICAL,
        &Map::new(),
        "用户取消执行：local.echo。",
    );
    assert!(audit_lines(&workspace).is_empty());
}
