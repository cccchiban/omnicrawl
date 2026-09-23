//! 多 Server 管理器：生命周期、能力发现、状态与诊断、工具调用与审计
//! （对应 `omnicrawl/mcp/client.py` 的 `MCPClientManager`）。
//!
//! 单个 Server 失败只记录诊断并降级，不影响其他 Server 或内置工具。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Map, Value};

use crate::audit::{AuditRecord, McpAuditLogger};
use crate::config::{
    McpConfig, McpServerConfig, MCP_RISK_EXTERNAL, MCP_TRANSPORT_STDIO,
    MCP_TRANSPORT_STREAMABLE_HTTP,
};
use crate::http::StreamableHttpMcpConnection;
use crate::ids;
use crate::jsonrpc::{
    stringify_prompt_payload, stringify_resource_payload, stringify_tool_result_payload,
    McpCallError, McpClientError,
};
use crate::registry::{
    namespace_capability_name, McpCapabilityRegistry, McpPromptMeta, McpResourceMeta, McpToolMeta,
};
use crate::security::{mcp_tool_requires_confirmation, validate_tool_arguments};
use crate::stdio::StdioMcpConnection;

/// 各 Server 的能力发现彼此独立（进程启动 / TLS 握手 + 若干次往返），并发执行
/// 可把首次可用时间的上界从「各 Server 之和」降到「最慢的那一个」。Server 数量
/// 通常是个位数，8 条线程足够覆盖，也避免异常配置一次拉起大量子进程。
pub const MAX_DISCOVER_WORKERS: usize = 8;

const KNOWN_TRANSPORTS: [&str; 2] = [MCP_TRANSPORT_STDIO, MCP_TRANSPORT_STREAMABLE_HTTP];

/// 一个 Server 发现到的原始能力清单（尚未命名空间化，交给管理器登记）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiscoveredCapabilities {
    pub tools: Vec<Map<String, Value>>,
    pub resources: Vec<Map<String, Value>>,
    pub prompts: Vec<Map<String, Value>>,
}

/// 与一个 MCP Server 的连接（stdio 与 Streamable HTTP 各一个实现）。
pub trait McpConnection: Send + Sync {
    fn discover(&self) -> Result<DiscoveredCapabilities, McpCallError>;

    fn call_tool(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Result<Map<String, Value>, McpCallError>;

    fn read_resource(&self, uri: &str) -> Result<Map<String, Value>, McpCallError>;

    fn get_prompt(
        &self,
        name: &str,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Map<String, Value>, McpCallError>;

    fn close(&self);
}

/// 连接工厂：按 Server 配置造一条连接（测试可注入脚本化连接）。
pub type Connector =
    dyn Fn(&McpServerConfig, &Path) -> Result<Arc<dyn McpConnection>, McpClientError> + Send + Sync;

/// MCP Tool 调用返回给 Agent 的结构化结果。
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolCallResult {
    pub ok: bool,
    pub server_name: String,
    pub tool_name: String,
    pub output: String,
    pub full_output: String,
    pub error_code: Option<String>,
    pub retryable: bool,
    pub duration_ms: i64,
    pub audit_id: String,
}

/// MCP Resource 读取结果。
#[derive(Debug, Clone, PartialEq)]
pub struct McpResourceReadResult {
    pub ok: bool,
    pub server_name: String,
    pub uri: String,
    pub output: String,
    pub full_output: String,
    pub error_code: Option<String>,
    pub retryable: bool,
    pub duration_ms: i64,
}

/// MCP Prompt 获取结果。
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptReadResult {
    pub ok: bool,
    pub server_name: String,
    pub prompt_name: String,
    pub output: String,
    pub full_output: String,
    pub error_code: Option<String>,
    pub retryable: bool,
    pub duration_ms: i64,
}

/// 单个 Server 的发现结果，供并发发现后串行登记。
struct ServerDiscoveryResult {
    server: McpServerConfig,
    status: &'static str,
    error_message: String,
    connection: Option<Arc<dyn McpConnection>>,
    capabilities: Option<DiscoveredCapabilities>,
}

/// 管理器内部状态：连接表、状态表与连续失败计数（可被多个执行线程同时写）。
#[derive(Default)]
struct ManagerState {
    closed: bool,
    connections: Vec<(String, Arc<dyn McpConnection>)>,
    server_status: Vec<(String, String)>,
    failure_counts: BTreeMap<String, i64>,
}

impl ManagerState {
    fn set_status(&mut self, server_name: &str, status: &str) {
        match self
            .server_status
            .iter_mut()
            .find(|(name, _)| name == server_name)
        {
            Some(entry) => entry.1 = status.to_string(),
            None => self
                .server_status
                .push((server_name.to_string(), status.to_string())),
        }
    }

    fn status_of(&self, server_name: &str) -> String {
        self.server_status
            .iter()
            .find(|(name, _)| name == server_name)
            .map(|(_, status)| status.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn connection(&self, server_name: &str) -> Option<Arc<dyn McpConnection>> {
        self.connections
            .iter()
            .find(|(name, _)| name == server_name)
            .map(|(_, connection)| Arc::clone(connection))
    }

    fn put_connection(&mut self, name: &str, connection: Arc<dyn McpConnection>) {
        match self.connections.iter_mut().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = connection,
            None => self.connections.push((name.to_string(), connection)),
        }
    }
}

/// 管理多个 MCP Server 的生命周期、能力注册和工具调用。
pub struct McpClientManager {
    config: McpConfig,
    workspace_root: PathBuf,
    registry: Mutex<McpCapabilityRegistry>,
    state: Mutex<ManagerState>,
    discovered: AtomicBool,
    /// 发现期互斥：后台预热与首个回合的惰性发现可能同时触发，幂等锁保证只跑一次。
    discover_lock: Mutex<()>,
    session_id: String,
    approval_mode: Arc<dyn Fn() -> String + Send + Sync>,
    audit: McpAuditLogger,
    connector: Arc<Connector>,
}

impl McpClientManager {
    pub fn new(config: McpConfig, workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        let state = ManagerState {
            server_status: config
                .servers
                .iter()
                .map(|(name, _)| (name.clone(), "disabled".to_string()))
                .collect(),
            failure_counts: config
                .servers
                .iter()
                .map(|(name, _)| (name.clone(), 0))
                .collect(),
            ..ManagerState::default()
        };
        let audit = McpAuditLogger::new(&workspace_root, config.policy.audit_log_enabled);
        Self {
            config,
            workspace_root,
            registry: Mutex::new(McpCapabilityRegistry::new()),
            state: Mutex::new(state),
            discovered: AtomicBool::new(false),
            discover_lock: Mutex::new(()),
            session_id: ids::session_id(),
            approval_mode: Arc::new(String::new),
            audit,
            connector: Arc::new(default_connector),
        }
    }

    /// 注入审批模式取值（审计记录用）；默认空串，与 Python 的默认 getter 一致。
    pub fn with_approval_mode(
        mut self,
        approval_mode: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        self.approval_mode = approval_mode;
        self
    }

    /// 注入连接工厂（对照测试用脚本化连接替换真实传输）。
    pub fn with_connector(mut self, connector: Arc<Connector>) -> Self {
        self.connector = connector;
        self
    }

    /// 注入审计时间源（对照测试要固定时刻）。
    pub fn with_audit_clock(mut self, clock: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        self.audit = self.audit.clone().with_clock(clock);
        self
    }

    pub fn config(&self) -> &McpConfig {
        &self.config
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// 是否已经执行过能力发现，用于宿主惰性加载 MCP 能力。
    pub fn discovered(&self) -> bool {
        self.discovered.load(Ordering::SeqCst)
    }

    pub fn registry(&self) -> std::sync::MutexGuard<'_, McpCapabilityRegistry> {
        self.registry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// 已注册能力的快照（宿主构建工具表用）。
    pub fn tools(&self) -> Vec<McpToolMeta> {
        self.registry().tools().cloned().collect()
    }

    pub fn resources(&self) -> Vec<McpResourceMeta> {
        self.registry().resources().cloned().collect()
    }

    pub fn prompts(&self) -> Vec<McpPromptMeta> {
        self.registry().prompts().cloned().collect()
    }

    pub fn diagnostics(&self) -> Vec<crate::registry::McpDiagnostic> {
        self.registry().diagnostics().to_vec()
    }

    /// 已连接的 Server 数（HUD 展示用）。
    pub fn connected_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .server_status
            .iter()
            .filter(|(_, status)| status == "connected")
            .count()
    }

    /// 连接启用的 Server 并发现 Tool、Resource、Prompt。
    ///
    /// 单个 Server 失败只记录诊断并降级，不影响其他 Server 或内置工具。
    /// 发现标记在结束时才置位：发现期间再次调用会被锁阻塞并等待完成，
    /// 保证调用方拿到的工具表是完整的。关闭流程已开始后不再登记新 Server。
    pub fn discover(&self) {
        let _guard = self
            .discover_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.discovered.load(Ordering::SeqCst) {
            // 已完成（含全部失败降级）：无需重复执行
            return;
        }
        if self.is_closed() {
            // 关闭流程已开始：不再启动任何 Server
            return;
        }
        self.discover_inner();
        // 无论成功/失败/禁用，发现流程结束即标记完成（后续不再重复执行）
        self.discovered.store(true, Ordering::SeqCst);
    }

    fn discover_inner(&self) {
        if !self.config.enabled {
            self.registry()
                .add_diagnostic("info", "MCP_DISABLED", "MCP 已关闭。", None);
            return;
        }

        let enabled_servers = self.config.enabled_servers();
        if enabled_servers.is_empty() {
            self.registry().add_diagnostic(
                "warning",
                "NO_ENABLED_SERVERS",
                "MCP 已启用，但没有启用的 Server。",
                None,
            );
            return;
        }

        for result in self.connect_all(&enabled_servers) {
            self.apply_discovery_result(result);
        }
    }

    /// 并发完成「连接 + 能力枚举」，并按配置顺序返回结果。
    ///
    /// 分片并发（每片至多 [`MAX_DISCOVER_WORKERS`] 条线程）而不是一次拉起全部线程：
    /// 结果仍按配置顺序登记，状态、诊断与工具表因此保持确定顺序。
    fn connect_all(&self, servers: &[&McpServerConfig]) -> Vec<ServerDiscoveryResult> {
        let mut results: Vec<Option<ServerDiscoveryResult>> =
            (0..servers.len()).map(|_| None).collect();
        for chunk in (0..servers.len())
            .collect::<Vec<usize>>()
            .chunks(MAX_DISCOVER_WORKERS)
        {
            std::thread::scope(|scope| {
                let handles: Vec<(
                    usize,
                    std::thread::ScopedJoinHandle<'_, ServerDiscoveryResult>,
                )> = chunk
                    .iter()
                    .map(|index| {
                        let server = servers[*index];
                        (*index, scope.spawn(move || self.connect_server(server)))
                    })
                    .collect();
                for (index, handle) in handles {
                    results[index] =
                        Some(handle.join().unwrap_or_else(|_| ServerDiscoveryResult {
                            server: servers[index].clone(),
                            status: "degraded",
                            error_message:
                                "MCP Server 连接或能力发现失败：能力发现线程异常退出。".to_string(),
                            connection: None,
                            capabilities: None,
                        }));
                }
            });
        }
        results.into_iter().flatten().collect()
    }

    /// 连接单个 Server 并发现能力（不修改管理器状态）。
    fn connect_server(&self, server: &McpServerConfig) -> ServerDiscoveryResult {
        if !KNOWN_TRANSPORTS.contains(&server.transport.as_str()) {
            return ServerDiscoveryResult {
                server: server.clone(),
                status: "unsupported",
                error_message: format!(
                    "暂不支持 {} 传输，已跳过 Server：{}",
                    server.transport, server.name
                ),
                connection: None,
                capabilities: None,
            };
        }

        let connection = match (self.connector)(server, &self.workspace_root) {
            Ok(connection) => connection,
            Err(error) => {
                return self.degraded(server, format!("MCP Server 连接或能力发现失败：{error}"))
            }
        };

        match connection.discover() {
            Ok(capabilities) => ServerDiscoveryResult {
                server: server.clone(),
                status: "connected",
                error_message: String::new(),
                connection: Some(connection),
                capabilities: Some(capabilities),
            },
            Err(error) => {
                connection.close();
                self.degraded(server, format!("MCP Server 连接或能力发现失败：{error}"))
            }
        }
    }

    fn degraded(&self, server: &McpServerConfig, message: String) -> ServerDiscoveryResult {
        ServerDiscoveryResult {
            server: server.clone(),
            status: "degraded",
            error_message: message,
            connection: None,
            capabilities: None,
        }
    }

    /// 登记单个 Server 的发现结果；关闭流程已开始时回收连接。
    fn apply_discovery_result(&self, result: ServerDiscoveryResult) {
        let server = result.server;
        if result.status == "unsupported" {
            self.set_server_status(&server.name, "unsupported");
            self.registry().add_diagnostic(
                "warning",
                "TRANSPORT_UNSUPPORTED",
                result.error_message,
                Some(&server.name),
            );
            return;
        }

        let Some(connection) = result.connection else {
            self.set_server_status(&server.name, "degraded");
            self.bump_failure_count(&server.name);
            self.registry().add_diagnostic(
                "error",
                "SERVER_UNAVAILABLE",
                result.error_message,
                Some(&server.name),
            );
            return;
        };

        let rejected = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                true
            } else {
                state.put_connection(&server.name, Arc::clone(&connection));
                false
            }
        };
        if rejected {
            // 关闭流程已开始：拒绝注册并回收本次连接
            connection.close();
            return;
        }

        self.set_server_status(&server.name, "connected");
        if let Some(capabilities) = result.capabilities {
            self.register_capabilities(&server, &capabilities);
        }
    }

    /// 把发现到的能力登记进注册表（含策略拦截与元数据清洗）。
    fn register_capabilities(
        &self,
        server: &McpServerConfig,
        capabilities: &DiscoveredCapabilities,
    ) {
        if server.risk_level == MCP_RISK_EXTERNAL
            && !self.config.policy.allow_external_network_tools
        {
            self.registry().add_diagnostic(
                "warning",
                "EXTERNAL_TOOLS_BLOCKED",
                format!("外部 MCP Server 默认不暴露能力，已跳过：{}", server.name),
                Some(&server.name),
            );
            return;
        }

        let mut registry = self.registry();
        for raw_tool in &capabilities.tools {
            let name = match raw_tool.get("name") {
                Some(Value::String(text)) if !text.trim().is_empty() => text.clone(),
                _ => {
                    registry.add_diagnostic(
                        "warning",
                        "CAPABILITY_INVALID",
                        "MCP Tool 缺少有效 name，已跳过。",
                        Some(&server.name),
                    );
                    continue;
                }
            };
            let input_schema = match raw_tool
                .get("inputSchema")
                .or_else(|| raw_tool.get("input_schema"))
            {
                Some(Value::Object(map)) => map.clone(),
                _ => Map::new(),
            };
            let description = match raw_tool.get("description") {
                Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
                _ => format!("来自 MCP Server {} 的工具 {name}。", server.name),
            };
            let mut meta = McpToolMeta {
                logical_name: namespace_capability_name(&server.name, &name),
                server_name: server.name.clone(),
                tool_name: name,
                description,
                input_schema,
                requires_confirmation: true,
                risk_level: server.risk_level.clone(),
            };
            meta.requires_confirmation = mcp_tool_requires_confirmation(&meta, &self.config.policy);
            registry.add_tool(meta);
        }

        for raw_resource in &capabilities.resources {
            let uri = match raw_resource.get("uri") {
                Some(Value::String(text)) if !text.trim().is_empty() => text.clone(),
                _ => continue,
            };
            let name = match raw_resource.get("name") {
                Some(Value::String(text)) if !text.is_empty() => text.clone(),
                _ => uri.clone(),
            };
            let description = match raw_resource.get("description") {
                Some(Value::String(text)) => text.clone(),
                _ => String::new(),
            };
            let mime_type = match raw_resource
                .get("mimeType")
                .or_else(|| raw_resource.get("mime_type"))
            {
                Some(Value::String(text)) => text.clone(),
                _ => String::new(),
            };
            registry.add_resource(McpResourceMeta {
                logical_uri: format!("{}:{}", server.name, uri),
                server_name: server.name.clone(),
                uri,
                name,
                description,
                mime_type,
            });
        }

        for raw_prompt in &capabilities.prompts {
            let name = match raw_prompt.get("name") {
                Some(Value::String(text)) if !text.trim().is_empty() => text.clone(),
                _ => continue,
            };
            let description = match raw_prompt.get("description") {
                Some(Value::String(text)) => text.clone(),
                _ => String::new(),
            };
            let arguments = match raw_prompt.get("arguments") {
                Some(Value::Array(items)) => items.clone(),
                _ => Vec::new(),
            };
            registry.add_prompt(McpPromptMeta {
                logical_name: namespace_capability_name(&server.name, &name),
                server_name: server.name.clone(),
                prompt_name: name,
                description,
                arguments,
            });
        }
    }

    /// 关闭所有由 Host 启动的 MCP 连接。
    ///
    /// 与发现流程协同：关闭标记置位后清空连接表；晚于关闭完成的注册会被
    /// `discover()` 拒绝并回收，不会向已关闭的管理器泄漏子进程。
    pub fn close(&self) {
        let connections = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            std::mem::take(&mut state.connections)
        };
        for (_, connection) in connections {
            connection.close();
        }
    }

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed
    }

    pub fn set_server_status(&self, server_name: &str, status: &str) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_status(server_name, status);
    }

    fn bump_failure_count(&self, server_name: &str) -> i64 {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let count = state.failure_counts.get(server_name).copied().unwrap_or(0) + 1;
        state.failure_counts.insert(server_name.to_string(), count);
        count
    }

    fn reset_failure_count(&self, server_name: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.failure_counts.insert(server_name.to_string(), 0);
    }

    /// 调用已注册的 MCP Tool，并把协议结果归一化为文本输出。
    pub fn call_tool(
        &self,
        logical_name: &str,
        arguments: &Map<String, Value>,
    ) -> McpToolCallResult {
        let started = Instant::now();
        let audit_id = ids::audit_id();
        let Some(meta) = self.registry().tool(logical_name).cloned() else {
            let result = McpToolCallResult {
                ok: false,
                server_name: String::new(),
                tool_name: logical_name.to_string(),
                output: format!("未知 MCP Tool：{logical_name}"),
                full_output: String::new(),
                error_code: Some("TOOL_NOT_FOUND".to_string()),
                retryable: false,
                duration_ms: 0,
                audit_id,
            };
            self.audit_tool_result(&result, arguments, "not_found");
            return result;
        };

        if let Err(message) = validate_tool_arguments(arguments, Some(&meta.input_schema)) {
            let result = McpToolCallResult {
                ok: false,
                server_name: meta.server_name.clone(),
                tool_name: meta.tool_name.clone(),
                output: message,
                full_output: String::new(),
                error_code: Some("SCHEMA_INVALID".to_string()),
                retryable: false,
                duration_ms: 0,
                audit_id,
            };
            self.audit_tool_result(&result, arguments, "schema_invalid");
            return result;
        }

        let connection = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .connection(&meta.server_name);
        let Some(connection) = connection else {
            self.record_tool_failure(&meta.server_name);
            let result = McpToolCallResult {
                ok: false,
                server_name: meta.server_name.clone(),
                tool_name: meta.tool_name.clone(),
                output: format!("MCP Server 不可用：{}", meta.server_name),
                full_output: String::new(),
                error_code: Some("SERVER_UNAVAILABLE".to_string()),
                retryable: true,
                duration_ms: 0,
                audit_id,
            };
            self.audit_tool_result(&result, arguments, "server_unavailable");
            return result;
        };

        let (ok, output, error_code, retryable) =
            match connection.call_tool(&meta.tool_name, arguments) {
                Ok(payload) => {
                    let output = stringify_tool_result_payload(&payload);
                    let ok = !matches!(payload.get("isError"), Some(Value::Bool(true)));
                    (
                        ok,
                        output,
                        if ok {
                            None
                        } else {
                            Some("TOOL_FAILED".to_string())
                        },
                        false,
                    )
                }
                Err(error) => {
                    let (code, retryable) = error.code();
                    (
                        false,
                        error.message().to_string(),
                        Some(code.to_string()),
                        retryable,
                    )
                }
            };

        let duration_ms = started.elapsed().as_millis() as i64;
        if ok {
            self.reset_failure_count(&meta.server_name);
        } else {
            self.record_tool_failure(&meta.server_name);
        }

        let result = McpToolCallResult {
            ok,
            server_name: meta.server_name.clone(),
            tool_name: meta.tool_name.clone(),
            output: output.clone(),
            full_output: output,
            error_code,
            retryable,
            duration_ms,
            audit_id,
        };
        self.audit_tool_result(&result, arguments, "approved");
        result
    }

    /// 记录 Host 审批拒绝的 MCP Tool 调用。
    pub fn record_denied_tool_call(
        &self,
        logical_name: &str,
        arguments: &Map<String, Value>,
        reason: &str,
    ) {
        let meta = self.registry().tool(logical_name).cloned();
        let result = McpToolCallResult {
            ok: false,
            server_name: meta
                .as_ref()
                .map(|meta| meta.server_name.clone())
                .unwrap_or_default(),
            tool_name: meta
                .as_ref()
                .map(|meta| meta.tool_name.clone())
                .unwrap_or_else(|| logical_name.to_string()),
            output: reason.to_string(),
            full_output: String::new(),
            error_code: Some("APPROVAL_DENIED".to_string()),
            retryable: false,
            duration_ms: 0,
            audit_id: ids::audit_id(),
        };
        self.audit_tool_result(&result, arguments, "denied");
    }

    /// 按注册表中的逻辑 URI 读取 MCP Resource。
    pub fn read_resource(&self, logical_uri: &str) -> McpResourceReadResult {
        let started = Instant::now();
        let Some(meta) = self.registry().resource(logical_uri).cloned() else {
            return McpResourceReadResult {
                ok: false,
                server_name: String::new(),
                uri: logical_uri.to_string(),
                output: format!("未知 MCP Resource：{logical_uri}"),
                full_output: String::new(),
                error_code: Some("RESOURCE_NOT_FOUND".to_string()),
                retryable: false,
                duration_ms: 0,
            };
        };

        let connection = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .connection(&meta.server_name);
        let Some(connection) = connection else {
            return McpResourceReadResult {
                ok: false,
                server_name: meta.server_name.clone(),
                uri: meta.uri.clone(),
                output: format!("MCP Server 不可用：{}", meta.server_name),
                full_output: String::new(),
                error_code: Some("SERVER_UNAVAILABLE".to_string()),
                retryable: true,
                duration_ms: 0,
            };
        };

        let (ok, output, error_code, retryable) = match connection.read_resource(&meta.uri) {
            Ok(payload) => (true, stringify_resource_payload(&payload), None, false),
            Err(error) => {
                let (code, retryable) = error.code();
                (
                    false,
                    error.message().to_string(),
                    Some(code.to_string()),
                    retryable,
                )
            }
        };

        McpResourceReadResult {
            ok,
            server_name: meta.server_name,
            uri: meta.uri,
            output: output.clone(),
            full_output: output,
            error_code,
            retryable,
            duration_ms: started.elapsed().as_millis() as i64,
        }
    }

    /// 按注册表中的逻辑名称获取 MCP Prompt 内容。
    pub fn get_prompt(
        &self,
        logical_name: &str,
        arguments: Option<&Map<String, Value>>,
    ) -> McpPromptReadResult {
        let started = Instant::now();
        let Some(meta) = self.registry().prompt(logical_name).cloned() else {
            return McpPromptReadResult {
                ok: false,
                server_name: String::new(),
                prompt_name: logical_name.to_string(),
                output: format!("未知 MCP Prompt：{logical_name}"),
                full_output: String::new(),
                error_code: Some("PROMPT_NOT_FOUND".to_string()),
                retryable: false,
                duration_ms: 0,
            };
        };

        let connection = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .connection(&meta.server_name);
        let Some(connection) = connection else {
            return McpPromptReadResult {
                ok: false,
                server_name: meta.server_name.clone(),
                prompt_name: meta.prompt_name.clone(),
                output: format!("MCP Server 不可用：{}", meta.server_name),
                full_output: String::new(),
                error_code: Some("SERVER_UNAVAILABLE".to_string()),
                retryable: true,
                duration_ms: 0,
            };
        };

        let (ok, output, error_code, retryable) =
            match connection.get_prompt(&meta.prompt_name, arguments) {
                Ok(payload) => (true, stringify_prompt_payload(&payload), None, false),
                Err(error) => {
                    let (code, retryable) = error.code();
                    (
                        false,
                        error.message().to_string(),
                        Some(code.to_string()),
                        retryable,
                    )
                }
            };

        McpPromptReadResult {
            ok,
            server_name: meta.server_name,
            prompt_name: meta.prompt_name,
            output: output.clone(),
            full_output: output,
            error_code,
            retryable,
            duration_ms: started.elapsed().as_millis() as i64,
        }
    }

    /// 返回适合终端展示的 MCP 状态摘要。
    pub fn format_status(&self) -> String {
        if !self.config.enabled {
            return "MCP 已关闭。可在 config.toml 的 mcp.enabled=true 开启。".to_string();
        }

        if !self.discovered() {
            self.discover();
        }

        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let connected = state
            .server_status
            .iter()
            .filter(|(_, status)| status == "connected")
            .count();
        let enabled_total = self.config.enabled_servers().len();
        let registry = self.registry();
        let mut lines = vec![
            format!("MCP 已启用：{connected}/{enabled_total} 个 Server 已连接。"),
            format!(
                "已注册 Tool {} 个，Resource {} 个，Prompt {} 个。",
                registry.tool_count(),
                registry.resource_count(),
                registry.prompt_count()
            ),
        ];

        if !self.config.servers.is_empty() {
            lines.push("Server：".to_string());
            let mut servers: Vec<&(String, McpServerConfig)> = self.config.servers.iter().collect();
            servers.sort_by(|left, right| left.0.cmp(&right.0));
            for (name, server) in servers {
                let status = state.status_of(name);
                let enabled_label = if server.enabled { "启用" } else { "禁用" };
                let failures = state.failure_counts.get(name).copied().unwrap_or(0);
                let suffix = if failures > 0 {
                    format!(", 连续失败 {failures} 次")
                } else {
                    String::new()
                };
                lines.push(format!(
                    "  - {name}: {enabled_label}, {}, {status}{suffix}",
                    server.transport
                ));
            }
        }

        if registry.tool_count() > 0 {
            lines.push("Tool：".to_string());
            let mut tools: Vec<&McpToolMeta> = registry.tools().collect();
            tools.sort_by(|left, right| left.logical_name.cmp(&right.logical_name));
            for meta in tools {
                let confirm_label = if meta.requires_confirmation {
                    "需确认"
                } else {
                    "自动"
                };
                lines.push(format!("  - {} ({confirm_label})", meta.logical_name));
            }
        }

        if registry.resource_count() > 0 {
            lines.push("Resource：".to_string());
            let mut resources: Vec<&McpResourceMeta> = registry.resources().collect();
            resources.sort_by(|left, right| left.logical_uri.cmp(&right.logical_uri));
            for meta in resources {
                lines.push(format!("  - {} -> {}", meta.logical_uri, meta.uri));
            }
        }

        if registry.prompt_count() > 0 {
            lines.push("Prompt：".to_string());
            let mut prompts: Vec<&McpPromptMeta> = registry.prompts().collect();
            prompts.sort_by(|left, right| left.logical_name.cmp(&right.logical_name));
            for meta in prompts {
                let label = if meta.description.is_empty() {
                    &meta.prompt_name
                } else {
                    &meta.description
                };
                lines.push(format!("  - {}: {label}", meta.logical_name));
            }
        }

        let diagnostics = registry.diagnostics();
        if !diagnostics.is_empty() {
            lines.push("诊断：".to_string());
            let start = diagnostics.len().saturating_sub(10);
            for diagnostic in &diagnostics[start..] {
                let prefix = match &diagnostic.server_name {
                    Some(name) => format!("{name}: "),
                    None => String::new(),
                };
                lines.push(format!(
                    "  - [{}] {prefix}{}",
                    diagnostic.severity, diagnostic.message
                ));
            }
        }

        lines.join("\n")
    }

    fn record_tool_failure(&self, server_name: &str) {
        let count = self.bump_failure_count(server_name);
        if count >= 3 {
            self.set_server_status(server_name, "degraded");
            self.registry().add_diagnostic(
                "warning",
                "SERVER_DEGRADED",
                format!("MCP Server 连续失败 {count} 次，已标记为 degraded：{server_name}"),
                Some(server_name),
            );
        }
    }

    fn audit_tool_result(
        &self,
        result: &McpToolCallResult,
        arguments: &Map<String, Value>,
        approval_result: &str,
    ) {
        self.audit.record_tool_call(AuditRecord {
            session_id: &self.session_id,
            audit_id: &result.audit_id,
            server_name: &result.server_name,
            tool_name: &result.tool_name,
            arguments,
            approval_mode: &(self.approval_mode)(),
            approval_result,
            duration_ms: result.duration_ms,
            ok: result.ok,
            error_code: result.error_code.as_deref(),
            output: if result.full_output.is_empty() {
                &result.output
            } else {
                &result.full_output
            },
        });
    }
}

/// 默认连接工厂：按传输类型构造真实连接。
fn default_connector(
    server: &McpServerConfig,
    workspace_root: &Path,
) -> Result<Arc<dyn McpConnection>, McpClientError> {
    match server.transport.as_str() {
        MCP_TRANSPORT_STDIO => Ok(Arc::new(StdioMcpConnection::new(server, workspace_root))),
        MCP_TRANSPORT_STREAMABLE_HTTP => Ok(Arc::new(StreamableHttpMcpConnection::new(server)?)),
        other => Err(McpClientError::new(format!(
            "暂不支持 {other} 传输，已跳过 Server：{}",
            server.name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpPolicyConfig;
    use serde_json::json;

    /// 脚本化连接：把「请求 → 响应」写成表，用于验证管理器的登记、状态与错误映射。
    struct ScriptedConnection {
        capabilities: DiscoveredCapabilities,
        tool_result: Option<Result<Map<String, Value>, McpCallError>>,
        closed: AtomicBool,
    }

    impl McpConnection for ScriptedConnection {
        fn discover(&self) -> Result<DiscoveredCapabilities, McpCallError> {
            Ok(self.capabilities.clone())
        }

        fn call_tool(
            &self,
            _name: &str,
            _arguments: &Map<String, Value>,
        ) -> Result<Map<String, Value>, McpCallError> {
            self.tool_result.clone().unwrap_or_else(|| Ok(Map::new()))
        }

        fn read_resource(&self, uri: &str) -> Result<Map<String, Value>, McpCallError> {
            Ok(json!({"contents": [{"uri": uri, "text": "正文"}]})
                .as_object()
                .cloned()
                .unwrap_or_default())
        }

        fn get_prompt(
            &self,
            name: &str,
            _arguments: Option<&Map<String, Value>>,
        ) -> Result<Map<String, Value>, McpCallError> {
            Ok(json!({"messages": [{"role": "user", "content": {"type": "text", "text": format!("{name} 模板")}}]})
                .as_object()
                .cloned()
                .unwrap_or_default())
        }

        fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    fn config_with(server: McpServerConfig) -> McpConfig {
        McpConfig {
            enabled: true,
            default_timeout_seconds: 30,
            servers: vec![(server.name.clone(), server)],
            policy: McpPolicyConfig::default(),
        }
    }

    fn stdio_server(name: &str) -> McpServerConfig {
        let mut server = McpServerConfig::new(name, MCP_TRANSPORT_STDIO);
        server.command = Some("echo".to_string());
        server
    }

    fn manager(
        config: McpConfig,
        connection: Arc<ScriptedConnection>,
    ) -> (McpClientManager, Arc<ScriptedConnection>) {
        let root = std::env::temp_dir().join("omnicrawl-mcp-manager");
        let _ = std::fs::create_dir_all(&root);
        let shared = Arc::clone(&connection);
        let connector: Arc<Connector> =
            Arc::new(move |_server, _root| Ok(Arc::clone(&shared) as Arc<dyn McpConnection>));
        (
            McpClientManager::new(config, &root).with_connector(connector),
            connection,
        )
    }

    fn connection(
        tool_result: Option<Result<Map<String, Value>, McpCallError>>,
    ) -> Arc<ScriptedConnection> {
        Arc::new(ScriptedConnection {
            capabilities: DiscoveredCapabilities {
                tools: vec![json!({"name": "read", "description": "读一个文件", "inputSchema": {"type": "object", "required": ["path"]}})
                    .as_object()
                    .cloned()
                    .unwrap_or_default()],
                resources: vec![json!({"uri": "file:///a", "name": "a", "mimeType": "text/plain"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default()],
                prompts: vec![json!({"name": "review", "description": "审查", "arguments": []})
                    .as_object()
                    .cloned()
                    .unwrap_or_default()],
            },
            tool_result,
            closed: AtomicBool::new(false),
        })
    }

    #[test]
    fn discovery_registers_namespaced_capabilities() {
        let (manager, _) = manager(config_with(stdio_server("files")), connection(None));
        manager.discover();
        assert!(manager.discovered());
        let tool = &manager.tools()[0];
        assert_eq!(tool.logical_name, "files.read");
        assert_eq!(tool.tool_name, "read");
        assert!(tool.requires_confirmation);
        assert_eq!(manager.resources()[0].logical_uri, "files:file:///a");
        assert_eq!(manager.prompts()[0].logical_name, "files.review");
        assert_eq!(manager.connected_count(), 1);
        assert!(manager
            .format_status()
            .starts_with("MCP 已启用：1/1 个 Server 已连接。"));
    }

    #[test]
    fn external_server_is_blocked_by_default() {
        let mut server = stdio_server("remote");
        server.risk_level = MCP_RISK_EXTERNAL.to_string();
        let (manager, _) = manager(config_with(server), connection(None));
        manager.discover();
        assert!(manager.tools().is_empty());
        assert_eq!(manager.diagnostics()[0].code, "EXTERNAL_TOOLS_BLOCKED");
    }

    #[test]
    fn disabled_config_reports_diagnostic_without_connecting() {
        let mut config = config_with(stdio_server("files"));
        config.enabled = false;
        let (manager, _) = manager(config, connection(None));
        manager.discover();
        assert_eq!(
            manager.format_status(),
            "MCP 已关闭。可在 config.toml 的 mcp.enabled=true 开启。"
        );
        assert_eq!(manager.diagnostics()[0].code, "MCP_DISABLED");
    }

    #[test]
    fn tool_failures_degrade_the_server_after_three_strikes() {
        let (manager, _) = manager(
            config_with(stdio_server("files")),
            connection(Some(Err(McpCallError::Timeout(McpClientError::new(
                "超时",
            ))))),
        );
        manager.discover();
        let mut arguments = Map::new();
        arguments.insert("path".to_string(), json!("/tmp/a"));
        for _ in 0..3 {
            let result = manager.call_tool("files.read", &arguments);
            assert!(!result.ok);
            assert_eq!(result.error_code.as_deref(), Some("TOOL_TIMEOUT"));
            assert!(result.retryable);
        }
        let status = manager.format_status();
        assert!(
            status.contains("files: 启用, stdio, degraded, 连续失败 3 次"),
            "{status}"
        );
        assert!(
            status.contains("MCP Server 连续失败 3 次，已标记为 degraded：files"),
            "{status}"
        );
    }

    #[test]
    fn schema_violation_is_reported_before_connecting() {
        let (manager, _) = manager(config_with(stdio_server("files")), connection(None));
        manager.discover();
        let result = manager.call_tool("files.read", &Map::new());
        assert_eq!(result.error_code.as_deref(), Some("SCHEMA_INVALID"));
        assert_eq!(result.output, "MCP Tool 参数缺少必填字段：path。");
    }

    #[test]
    fn unknown_tool_and_closed_manager_are_reported() {
        let (manager, _) = manager(config_with(stdio_server("files")), connection(None));
        manager.discover();
        let unknown = manager.call_tool("files.nope", &Map::new());
        assert_eq!(unknown.output, "未知 MCP Tool：files.nope");
        assert_eq!(unknown.error_code.as_deref(), Some("TOOL_NOT_FOUND"));

        manager.close();
        let mut arguments = Map::new();
        arguments.insert("path".to_string(), json!("/tmp/a"));
        let unavailable = manager.call_tool("files.read", &arguments);
        assert_eq!(
            unavailable.error_code.as_deref(),
            Some("SERVER_UNAVAILABLE")
        );
        assert!(unavailable.retryable);
    }

    #[test]
    fn resource_and_prompt_reads_are_textualized() {
        let (manager, _) = manager(config_with(stdio_server("files")), connection(None));
        manager.discover();
        let resource = manager.read_resource("files:file:///a");
        assert!(resource.ok);
        assert_eq!(resource.output, "Resource file:///a:\n正文");
        let prompt = manager.get_prompt("files.review", None);
        assert!(prompt.ok);
        assert_eq!(prompt.output, "user: review 模板");
        assert_eq!(
            manager.read_resource("files:missing").error_code.as_deref(),
            Some("RESOURCE_NOT_FOUND")
        );
    }
}
