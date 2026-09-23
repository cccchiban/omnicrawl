//! MCP Streamable HTTP 传输（对应 `omnicrawl/mcp/client.py` 的 `_StreamableHTTPMCPConnection`）。
//!
//! 每个 JSON-RPC 请求通过 HTTP POST 发到同一端点。服务端可以返回 `application/json`
//! 或 `text/event-stream`；初始化响应里的 `Mcp-Session-Id` 会附加到后续请求。
//! 与 Python 的 httpx 客户端不同，这里复用 `ureq` 的连接池且不做后台保活重连，
//! 每个 Server 连接各持一个 Agent。

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::client::{DiscoveredCapabilities, McpConnection};
use crate::config::McpServerConfig;
use crate::jsonrpc::{
    capability_declared, list_capability_pages, parse_sse_json_payloads, unwrap_json_rpc_response,
    McpCallError, McpClientError,
};

/// Streamable HTTP 的协议版本（与 Python 客户端一致）。
pub const MCP_STREAMABLE_HTTP_PROTOCOL_VERSION: &str = "2025-03-26";
const CLIENT_NAME: &str = "ai-voice-agent";
const CLIENT_VERSION: &str = "0.1";
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// HTTP 传输层失败：超时与其余失败分开，好让管理器映射到不同错误码。
enum HttpFailure {
    Timeout(String),
    Failed(String),
}

struct HttpState {
    session_id: Option<String>,
    initialized: bool,
    next_request_id: u64,
}

pub struct StreamableHttpMcpConnection {
    server: McpServerConfig,
    agent: ureq::Agent,
    state: Mutex<HttpState>,
}

impl StreamableHttpMcpConnection {
    pub fn new(server: &McpServerConfig) -> Result<Self, McpClientError> {
        if server.url.as_deref().unwrap_or("").trim().is_empty() {
            return Err(McpClientError::new("streamable_http MCP Server 缺少 url。"));
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self {
            server: server.clone(),
            agent,
            state: Mutex::new(HttpState {
                session_id: None,
                initialized: false,
                next_request_id: 1,
            }),
        })
    }

    pub fn server(&self) -> &McpServerConfig {
        &self.server
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HttpState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn discover_capabilities(&self) -> Result<DiscoveredCapabilities, McpCallError> {
        let init_result = self.request(
            "initialize",
            &json_object(&[
                (
                    "protocolVersion",
                    Value::String(MCP_STREAMABLE_HTTP_PROTOCOL_VERSION.to_string()),
                ),
                ("capabilities", Value::Object(Map::new())),
                (
                    "clientInfo",
                    serde_json::json!({"name": CLIENT_NAME, "version": CLIENT_VERSION}),
                ),
            ]),
            false,
        )?;
        self.lock().initialized = true;
        self.request("notifications/initialized", &Map::new(), true)?;
        let discover = |method: &str, result_key: &str, capability: &str| {
            if !capability_declared(&init_result, capability) {
                // 服务器未在 initialize 响应中声明该能力：跳过请求，少一次网络往返。
                return Vec::new();
            }
            list_capability_pages(
                |method, params| {
                    self.request(method, params, false)
                        .map_err(|error| McpClientError::new(error.message().to_string()))
                },
                method,
                result_key,
            )
        };
        Ok(DiscoveredCapabilities {
            tools: discover("tools/list", "tools", "tools"),
            resources: discover("resources/list", "resources", "resources"),
            prompts: discover("prompts/list", "prompts", "prompts"),
        })
    }

    /// 发送一次请求（或通知），返回拆包后的 `result`。
    ///
    /// 公开是为了让对照测试能直接驱动协议层（Python 侧的 `_request` 同样是内部入口）。
    pub fn request(
        &self,
        method: &str,
        params: &Map<String, Value>,
        notification: bool,
    ) -> Result<Map<String, Value>, McpCallError> {
        // 整个请求串行化：会话头与请求编号都要按序推进（与 Python 的同一把锁一致）。
        let mut state = self.lock();
        let mut message = Map::new();
        message.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        message.insert("method".to_string(), Value::String(method.to_string()));
        message.insert("params".to_string(), Value::Object(params.clone()));
        let request_id = if notification {
            None
        } else {
            let id = state.next_request_id;
            state.next_request_id += 1;
            message.insert("id".to_string(), Value::from(id));
            Some(id)
        };

        // 认证头等用户自定义 Header 只属于远程 HTTP 连接；协议头由 Client 维护，
        // 并覆盖同名（大小写不敏感）配置，避免配置破坏会话。
        let mut headers: Vec<(String, String)> = self
            .server
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let mut managed: Vec<(String, String)> = vec![
            (
                "Accept".to_string(),
                "application/json, text/event-stream".to_string(),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        if let Some(session_id) = state.session_id.clone() {
            managed.push((SESSION_HEADER.to_string(), session_id));
        }
        if state.initialized {
            managed.push((
                "MCP-Protocol-Version".to_string(),
                MCP_STREAMABLE_HTTP_PROTOCOL_VERSION.to_string(),
            ));
        }
        for (name, value) in managed {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
            headers.push((name, value));
        }

        let url = self.server.url.clone().unwrap_or_default();
        let response = match self.post(&url, &headers, &Value::Object(message.clone())) {
            Ok(response) => response,
            Err(HttpFailure::Timeout(message)) => {
                return Err(McpCallError::Timeout(McpClientError::new(message)))
            }
            Err(HttpFailure::Failed(message)) => {
                return Err(McpCallError::Failed(McpClientError::new(message)))
            }
        };

        if let Some(session_id) = response.session_id {
            state.session_id = Some(session_id);
        }
        if notification && response.status == 202 {
            return Ok(Map::new());
        }
        if response.status >= 400 {
            return Err(McpCallError::Failed(McpClientError::new(format!(
                "MCP Server HTTP 请求失败：HTTP {}",
                response.status
            ))));
        }
        if response.body.is_empty() {
            return Ok(Map::new());
        }

        if response.content_type.contains("text/event-stream") {
            let payloads = parse_sse_json_payloads(&response.body);
            if let Some(request_id) = request_id {
                for payload in &payloads {
                    if payload.get("id").and_then(|value| value.as_u64()) == Some(request_id) {
                        return unwrap_json_rpc_response(payload, method)
                            .map_err(McpCallError::from);
                    }
                }
            }
            if let Some(first) = payloads.first() {
                return unwrap_json_rpc_response(first, method).map_err(McpCallError::from);
            }
            return Err(McpCallError::Failed(McpClientError::new(
                "MCP Streamable HTTP 返回空 SSE 事件流。",
            )));
        }

        let payload: Value = match serde_json::from_str(&response.body) {
            Ok(value) => value,
            Err(_) => {
                return Err(McpCallError::Failed(McpClientError::new(
                    "MCP Streamable HTTP 返回的不是合法 JSON。",
                )))
            }
        };
        let Value::Object(payload) = payload else {
            return Err(McpCallError::Failed(McpClientError::new(
                "MCP Streamable HTTP 响应必须是 JSON 对象。",
            )));
        };
        unwrap_json_rpc_response(&payload, method).map_err(McpCallError::from)
    }

    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<HttpResponse, HttpFailure> {
        let timeout = Duration::from_secs(self.server.timeout_seconds.max(1) as u64);
        let mut builder = self
            .agent
            .post(url)
            .config()
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_recv_body(Some(timeout))
            .build();
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let payload = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
        match builder.send(payload.as_str()) {
            Ok(response) => {
                let status = response.status().as_u16();
                let session_id = response
                    .headers()
                    .get(SESSION_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(|value| value.to_string());
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_lowercase();
                let body = response.into_body().read_to_string().unwrap_or_default();
                Ok(HttpResponse {
                    status,
                    session_id,
                    content_type,
                    body,
                })
            }
            Err(ureq::Error::Timeout(_)) => Err(HttpFailure::Timeout(format!(
                "MCP 请求超过 {} 秒：{}",
                self.server.timeout_seconds, self.server.name
            ))),
            Err(error) => Err(HttpFailure::Failed(format!(
                "MCP Server HTTP 请求失败：{error}"
            ))),
        }
    }
}

struct HttpResponse {
    status: u16,
    session_id: Option<String>,
    content_type: String,
    body: String,
}

impl McpConnection for StreamableHttpMcpConnection {
    fn discover(&self) -> Result<DiscoveredCapabilities, McpCallError> {
        self.discover_capabilities()
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Result<Map<String, Value>, McpCallError> {
        self.request(
            "tools/call",
            &json_object(&[
                ("name", Value::String(name.to_string())),
                ("arguments", Value::Object(arguments.clone())),
            ]),
            false,
        )
    }

    fn read_resource(&self, uri: &str) -> Result<Map<String, Value>, McpCallError> {
        self.request(
            "resources/read",
            &json_object(&[("uri", Value::String(uri.to_string()))]),
            false,
        )
    }

    fn get_prompt(
        &self,
        name: &str,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Map<String, Value>, McpCallError> {
        self.request(
            "prompts/get",
            &json_object(&[
                ("name", Value::String(name.to_string())),
                (
                    "arguments",
                    Value::Object(arguments.cloned().unwrap_or_default()),
                ),
            ]),
            false,
        )
    }

    /// `ureq` 的 Agent 由结构体自身持有，没有显式的关闭方法；为保持契约留空实现。
    fn close(&self) {}
}

fn json_object(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MCP_TRANSPORT_STREAMABLE_HTTP;

    #[test]
    fn missing_url_is_reported_at_construction() {
        let server = McpServerConfig::new("remote", MCP_TRANSPORT_STREAMABLE_HTTP);
        let Err(error) = StreamableHttpMcpConnection::new(&server) else {
            panic!("缺少 url 的连接不该构造成功");
        };
        assert_eq!(error.message(), "streamable_http MCP Server 缺少 url。");
    }
}
