//! MCP 子系统（`omnicrawl/mcp/` 的 Rust 移植）。
//!
//! 按职责分为：配置（`config`）、能力注册表（`registry`）、安全与脱敏（`security`）、
//! 审计（`audit`）、线上协议的纯函数（`jsonrpc`）、两种传输（`stdio`、`http`）、
//! 多 Server 管理器（`client`）与本地 stdio Server（`server`）。
//!
//! 与 Python 的差别集中在三处，逐项见 `README.md`：
//! 1. 异常换成 `Result` 与错误枚举，文案逐字保留；
//! 2. 审计时间源与消息 ID 可注入（对照测试要固定取值）；
//! 3. 内置文档打进二进制（`bundled`），不再依赖安装目录里的 `docs/`。

pub mod audit;
pub mod bundled;
pub mod client;
pub mod config;
pub mod http;
pub mod ids;
pub mod jsonrpc;
pub mod registry;
pub mod security;
pub mod server;
pub mod stdio;

pub use client::{McpClientManager, McpPromptReadResult, McpResourceReadResult, McpToolCallResult};
pub use config::{load_mcp_config, McpConfig, McpConfigError, McpPolicyConfig, McpServerConfig};
pub use jsonrpc::{
    capability_declared, list_capability_pages, parse_content_length, parse_sse_json_payloads,
    stringify_prompt_payload, stringify_resource_payload, stringify_tool_result_payload,
    unwrap_json_rpc_response, FrameKind, McpCallError, McpClientError,
};
pub use registry::{
    namespace_capability_name, McpCapabilityRegistry, McpDiagnostic, McpPromptMeta,
    McpResourceMeta, McpToolMeta,
};
pub use security::{mcp_tool_requires_confirmation, validate_tool_arguments};
