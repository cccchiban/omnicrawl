//! 飞书开放接口客户端：租户令牌、消息创建/更新、消息资源下载与长连接端点。
//!
//! 语义基准是 Python 侧用的 `lark-oapi` SDK 调用（`_send_raw` / `_patch_card` /
//! `_download_message_resource` / `_send_text`）与 `lark_oapi.ws.Client` 的端点请求。
//! SDK 自带的令牌缓存、重试与 `lark.ws.Client` 的握手/心跳在这里按同名语义实现：
//! 令牌按 `expire` 提前 60 秒续期，HTTP 失败只记录日志并让调用方兜底。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::text::split_text;
use crate::http::{with_query, HttpTransport, UreqTransport};
use crate::json;

/// 飞书开放平台默认域名。
pub const DEFAULT_BASE_URL: &str = "https://open.feishu.cn";

/// 长连接端点路径（与 SDK 的 `GEN_ENDPOINT_URI` 一致）。
pub const WS_ENDPOINT_PATH: &str = "/callback/ws/endpoint";

/// SDK 使用的 User-Agent 前缀；飞书按它识别客户端。
pub const USER_AGENT: &str = "omnicrawl-connectors/0.0.1";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// 令牌提前续期的余量。
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// 飞书接口调用失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuApiError {
    /// 传输层失败。
    Transport(String),
    /// HTTP 层成功但响应体不是 JSON。
    NonJson { status: u16 },
    /// 飞书返回 `code != 0`。
    Api { code: i64, message: String },
    /// 响应缺少必需字段。
    Missing(String),
}

impl std::fmt::Display for FeishuApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(detail) => write!(formatter, "飞书接口请求失败：{detail}"),
            Self::NonJson { status } => write!(formatter, "飞书接口返回非 JSON（HTTP {status}）"),
            Self::Api { code, message } => write!(formatter, "飞书接口错误 {code}：{message}"),
            Self::Missing(field) => write!(formatter, "飞书接口响应缺少 {field}"),
        }
    }
}

impl std::error::Error for FeishuApiError {}

/// 长连接端点返回的客户端配置（字段名与 SDK 的 PascalCase 一致）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientConfig {
    pub reconnect_count: Option<i64>,
    pub reconnect_interval: Option<i64>,
    pub reconnect_nonce: Option<i64>,
    pub ping_interval: Option<i64>,
    /// 端点 URL 里的 `service_id`。
    pub service_id: String,
}

impl ClientConfig {
    /// 心跳间隔秒数，缺省与 SDK 一致（120 秒）。
    pub fn ping_interval_seconds(&self) -> u64 {
        self.ping_interval
            .filter(|value| *value > 0)
            .map(|value| value as u64)
            .unwrap_or(120)
    }
}

/// 长连接入口：URL（自带 `device_id`/`service_id` 查询参数）与客户端配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsEndpoint {
    pub url: String,
    pub config: ClientConfig,
}

/// 飞书开放接口客户端。
pub struct FeishuApi {
    app_id: String,
    app_secret: String,
    base_url: String,
    transport: std::sync::Arc<dyn HttpTransport>,
    token: Mutex<Option<(String, Instant)>>,
}

impl FeishuApi {
    pub fn new(
        app_id: &str,
        app_secret: &str,
        transport: std::sync::Arc<dyn HttpTransport>,
    ) -> FeishuApi {
        FeishuApi {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            transport,
            token: Mutex::new(None),
        }
    }

    /// 生产形态：ureq 传输。
    pub fn with_ureq(app_id: &str, app_secret: &str) -> FeishuApi {
        FeishuApi::new(
            app_id,
            app_secret,
            std::sync::Arc::new(UreqTransport::new()),
        )
    }

    /// 自建域名（私有化部署）时覆盖。
    pub fn with_base_url(mut self, base_url: &str) -> FeishuApi {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    /// 租户令牌：命中缓存直接返回，过期前 60 秒续期。
    pub fn tenant_token(&self) -> Result<String, FeishuApiError> {
        {
            let cached = self.token.lock().expect("令牌锁中毒");
            if let Some((token, expires_at)) = cached.as_ref() {
                if Instant::now() + TOKEN_REFRESH_MARGIN < *expires_at {
                    return Ok(token.clone());
                }
            }
        }
        let body = json::dumps(&json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        }));
        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.base_url
        );
        let reply = self
            .transport
            .post_json(&url, &body, TOKEN_TIMEOUT)
            .map_err(FeishuApiError::Transport)?;
        let payload = parse_payload(&reply)?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            return Err(FeishuApiError::Api {
                code,
                message: payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
                    .to_string(),
            });
        }
        let token = payload
            .get("tenant_access_token")
            .or_else(|| payload.get("app_access_token"))
            .and_then(Value::as_str)
            .ok_or_else(|| FeishuApiError::Missing("tenant_access_token".to_string()))?;
        let expire = payload
            .get("expire")
            .and_then(Value::as_i64)
            .unwrap_or(3600);
        let expires_at = Instant::now() + Duration::from_secs(expire.max(1) as u64);
        let mut cached = self.token.lock().expect("令牌锁中毒");
        *cached = Some((token.to_string(), expires_at));
        Ok(token.to_string())
    }

    /// 创建消息，返回 message_id；失败只记录日志并返回 None（与 SDK 调用点一致）。
    pub fn send_message(
        &self,
        receive_id: &str,
        payload: &str,
        msg_type: &str,
        receive_id_type: &str,
    ) -> Option<String> {
        if receive_id.is_empty() {
            return None;
        }
        let token = match self.tenant_token() {
            Ok(token) => token,
            Err(error) => {
                eprintln!("[feishu] 获取令牌失败：{error}");
                return None;
            }
        };
        let url = with_query(
            &format!("{}/open-apis/im/v1/messages", self.base_url),
            &[("receive_id_type".to_string(), receive_id_type.to_string())],
        );
        let body = json::dumps(&json!({
            "receive_id": receive_id,
            "msg_type": msg_type,
            "content": payload,
        }));
        match self.authorized("POST", &url, &body, Some(&token), REQUEST_TIMEOUT) {
            Ok(payload) => payload
                .get("data")
                .and_then(|data| data.get("message_id"))
                .and_then(Value::as_str)
                .map(|value| value.to_string()),
            Err(error) => {
                eprintln!("[feishu] 发送消息失败（receive_id={receive_id}）：{error}");
                None
            }
        }
    }

    /// 更新卡片内容。
    pub fn patch_card(&self, message_id: &str, card_payload: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let token = match self.tenant_token() {
            Ok(token) => token,
            Err(error) => {
                eprintln!("[feishu] 获取令牌失败：{error}");
                return false;
            }
        };
        let url = format!("{}/open-apis/im/v1/messages/{message_id}", self.base_url);
        let body = json::dumps(&json!({"content": card_payload}));
        match self.authorized("PATCH", &url, &body, Some(&token), REQUEST_TIMEOUT) {
            Ok(_payload) => true,
            Err(error) => {
                eprintln!("[feishu] 更新卡片失败（message_id={message_id}）：{error}");
                false
            }
        }
    }

    /// 下载消息资源；返回 (内容, 文件名)。
    ///
    /// 与 SDK 的差别：不解析 `Content-Disposition`，缺文件名时回落成 `file_key`，
    /// 由调用方按资源类型补扩展名（`resource_file_name`）。
    pub fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<(Vec<u8>, String), FeishuApiError> {
        // 语音与文件都按 file 类型读取二进制内容（与 SDK 用法一致）。
        let resource_type = if resource_type == "audio" {
            "file"
        } else {
            resource_type
        };
        let token = self.tenant_token()?;
        let url = with_query(
            &format!(
                "{}/open-apis/im/v1/messages/{message_id}/resources/{file_key}",
                self.base_url
            ),
            &[("type".to_string(), resource_type.to_string())],
        );
        let headers = [("Authorization".to_string(), format!("Bearer {token}"))];
        let reply = self
            .transport
            .request("GET", &url, &headers, None, DOWNLOAD_TIMEOUT)
            .map_err(FeishuApiError::Transport)?;
        if !(200..300).contains(&reply.status) {
            return Err(FeishuApiError::Api {
                code: reply.status as i64,
                message: "资源下载失败".to_string(),
            });
        }
        Ok((reply.body, file_key.to_string()))
    }

    /// 请求长连接端点，拿到连接 URL 与客户端配置。
    pub fn ws_endpoint(&self) -> Result<WsEndpoint, FeishuApiError> {
        let url = format!("{}{WS_ENDPOINT_PATH}", self.base_url);
        let body = json::dumps(&json!({
            "AppID": self.app_id,
            "AppSecret": self.app_secret,
        }));
        let headers = [
            ("locale".to_string(), "zh".to_string()),
            ("User-Agent".to_string(), USER_AGENT.to_string()),
        ];
        let reply = self
            .transport
            .request(
                "POST",
                &url,
                &headers,
                Some(body.as_bytes()),
                REQUEST_TIMEOUT,
            )
            .map_err(FeishuApiError::Transport)?;
        let payload = parse_payload(&reply)?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            return Err(FeishuApiError::Api {
                code,
                message: payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("system busy")
                    .to_string(),
            });
        }
        let data = payload
            .get("data")
            .ok_or_else(|| FeishuApiError::Missing("data".to_string()))?;
        let url = data
            .get("URL")
            .and_then(Value::as_str)
            .ok_or_else(|| FeishuApiError::Missing("URL".to_string()))?
            .to_string();
        let config_block = data.get("ClientConfig").cloned().unwrap_or(Value::Null);
        let config = ClientConfig {
            reconnect_count: config_block.get("ReconnectCount").and_then(Value::as_i64),
            reconnect_interval: config_block
                .get("ReconnectInterval")
                .and_then(Value::as_i64),
            reconnect_nonce: config_block.get("ReconnectNonce").and_then(Value::as_i64),
            ping_interval: config_block.get("PingInterval").and_then(Value::as_i64),
            service_id: query_value(&url, "service_id").unwrap_or_default(),
        };
        Ok(WsEndpoint { url, config })
    }

    fn authorized(
        &self,
        method: &str,
        url: &str,
        body: &str,
        token: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, FeishuApiError> {
        let mut headers = vec![(
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )];
        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        let reply = self
            .transport
            .request(method, url, &headers, Some(body.as_bytes()), timeout)
            .map_err(FeishuApiError::Transport)?;
        let payload = parse_payload(&reply)?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            return Err(FeishuApiError::Api {
                code,
                message: payload
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
                    .to_string(),
            });
        }
        Ok(payload)
    }
}

impl super::timeline::MessagePort for FeishuApi {
    fn send_raw(
        &self,
        receive_id: &str,
        payload: &str,
        msg_type: &str,
        receive_id_type: &str,
    ) -> Option<String> {
        self.send_message(receive_id, payload, msg_type, receive_id_type)
    }

    fn patch_card(&self, message_id: &str, payload: &str) -> bool {
        FeishuApi::patch_card(self, message_id, payload)
    }

    /// 普通文本：脱敏后按上限分段，每段用 `{"text": ...}` 负载发送。
    fn send_text(&self, receive_id: &str, text: &str, receive_id_type: &str) -> bool {
        let redacted = omnicrawl_session::redact_sensitive_text(text);
        let parts = split_text(&redacted);
        if parts.is_empty() {
            return false;
        }
        let mut sent = true;
        for part in parts {
            let payload = json::dumps(&json!({"text": part}));
            sent = self
                .send_message(receive_id, &payload, "text", receive_id_type)
                .is_some()
                && sent;
        }
        sent
    }
}

fn parse_payload(reply: &crate::http::HttpReply) -> Result<Value, FeishuApiError> {
    serde_json::from_slice(&reply.body).map_err(|_| FeishuApiError::NonJson {
        status: reply.status,
    })
}

/// 从 URL 查询串里取值（`service_id` 等）。
fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let name = parts.next()?;
        if name == key {
            return parts.next().map(|value| value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_id_is_read_from_endpoint_url() {
        let url = "wss://example.feishu.cn/connect?device_id=dev_1&service_id=1234&app_id=cli_x";
        assert_eq!(query_value(url, "service_id"), Some("1234".to_string()));
        assert_eq!(query_value(url, "missing"), None);
        assert_eq!(
            query_value("wss://example.feishu.cn/connect", "service_id"),
            None
        );
    }

    #[test]
    fn ping_interval_falls_back_to_sdk_default() {
        let mut config = ClientConfig::default();
        assert_eq!(config.ping_interval_seconds(), 120);
        config.ping_interval = Some(30);
        assert_eq!(config.ping_interval_seconds(), 30);
        config.ping_interval = Some(0);
        assert_eq!(config.ping_interval_seconds(), 120);
    }

    #[test]
    fn error_messages_are_readable() {
        assert_eq!(
            FeishuApiError::Api {
                code: 99991663,
                message: "app not found".to_string()
            }
            .to_string(),
            "飞书接口错误 99991663：app not found"
        );
        assert_eq!(
            FeishuApiError::Missing("URL".to_string()).to_string(),
            "飞书接口响应缺少 URL"
        );
    }
}
