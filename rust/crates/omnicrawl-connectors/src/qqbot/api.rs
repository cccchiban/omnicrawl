//! QQ 开放平台接口客户端：AccessToken、网关地址、消息下发与媒体下载。
//!
//! 协议见 `https://bot.q.qq.com/wiki/develop/api-v2/`：鉴权走 `appId + clientSecret`
//! 换 AccessToken（7200 秒，提前 5 分钟续期），业务请求带 `Authorization: QQBot <token>`。
//! 同一个发送接口承担两种形态，区别只在带不带 `msg_id`：
//!
//! - **被动回复**：带触发消息的 `msg_id`（单聊/群聊还要 `msg_seq`），有条数与窗口上限
//!   （单聊 60 分钟内 5 条、群聊 5 分钟内 5 条）；
//! - **主动消息**：不带 `msg_id`，没有时效概念，但需要机器人具备主动消息能力，并受频率
//!   限制（Bot 维度 30 qpm 起、单关系 20 qpm、每个会话每天 1000 条）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::config::TOKEN_URL;
use super::upload;
use crate::http::{HttpTransport, UreqTransport};
use crate::json;

/// User-Agent：QQ 网关按它识别客户端。
pub const USER_AGENT: &str = "omnicrawl-connectors/0.0.1";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// 令牌提前续期的余量。
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// 被动回复窗口内的回复条数上限：群聊与单聊都是 5 条（见 [`Target::max_replies`]）。
pub const MAX_REPLIES_PER_GROUP_MESSAGE: usize = 5;
/// 单聊的回复条数上限：与群聊同为 5 条，只是窗口更长（60 分钟）。
pub const MAX_REPLIES_PER_C2C_MESSAGE: usize = 5;

/// 被动回复已不可用的错误码：`msg_id` 过期（40034005）与次数/时间超限（40034128）。
///
/// 端口层把平台错误压成字符串，这里按错误码匹配，用于降级成主动消息。
pub const PASSIVE_UNAVAILABLE_CODES: [&str; 2] = ["40034005", "40034128"];

/// 消息类型：0 纯文本、2 Markdown、7 富媒体（`msg_type` 决定哪个内容字段生效）。
pub const MSG_TYPE_TEXT: i64 = 0;
pub const MSG_TYPE_MARKDOWN: i64 = 2;
pub const MSG_TYPE_MEDIA: i64 = 7;

/// 富媒体类型：1 图片、2 视频、3 语音、4 文件。
pub const FILE_TYPE_IMAGE: i64 = 1;
pub const FILE_TYPE_VIDEO: i64 = 2;
pub const FILE_TYPE_VOICE: i64 = 3;
pub const FILE_TYPE_FILE: i64 = 4;

/// 富媒体二进制直传上限（超过要走分片上传，本连接器不支持）。
pub const MAX_DIRECT_UPLOAD_BYTES: usize = 5 * 1024 * 1024;

/// 消息投递目标：单聊按用户 openid，群聊按群 openid，频道按子频道 id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    User(String),
    Group(String),
    Channel(String),
}

impl Target {
    /// 消息发送路径。
    pub fn messages_path(&self) -> String {
        match self {
            Target::User(openid) => format!("/v2/users/{openid}/messages"),
            Target::Group(group) => format!("/v2/groups/{group}/messages"),
            Target::Channel(channel) => format!("/channels/{channel}/messages"),
        }
    }

    /// 媒体上传路径（上传到哪个场景，就只能发回哪个场景）。
    pub fn files_path(&self) -> String {
        match self {
            Target::User(openid) => format!("/v2/users/{openid}/files"),
            Target::Group(group) => format!("/v2/groups/{group}/files"),
            Target::Channel(channel) => format!("/channels/{channel}/files"),
        }
    }

    /// 分片上传预上传路径（只有单聊与群聊有；频道走另一套接口，本连接器不发频道富媒体）。
    pub fn upload_prepare_path(&self) -> Option<String> {
        match self {
            Target::User(openid) => Some(format!("/v2/users/{openid}/upload_prepare")),
            Target::Group(group) => Some(format!("/v2/groups/{group}/upload_prepare")),
            Target::Channel(_) => None,
        }
    }

    /// 分片上传的单片完成通知路径（与预上传同理，只有单聊与群聊）。
    pub fn upload_part_finish_path(&self) -> Option<String> {
        match self {
            Target::User(openid) => Some(format!("/v2/users/{openid}/upload_part_finish")),
            Target::Group(group) => Some(format!("/v2/groups/{group}/upload_part_finish")),
            Target::Channel(_) => None,
        }
    }

    /// 频道场景不使用 `msg_seq`（官方只对单聊与群聊要求递增序号）。
    pub fn uses_seq(&self) -> bool {
        !matches!(self, Target::Channel(_))
    }

    /// 同一条入站消息最多能回几条：群聊与单聊都是 5；频道没有条数限制，按同口径保守处理。
    pub fn max_replies(&self) -> usize {
        match self {
            Target::User(_) => MAX_REPLIES_PER_C2C_MESSAGE,
            Target::Group(_) | Target::Channel(_) => MAX_REPLIES_PER_GROUP_MESSAGE,
        }
    }
}

/// QQ 接口调用失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QqApiError {
    /// 传输层失败。
    Transport(String),
    /// HTTP 层成功但响应体不是 JSON。
    NonJson { status: u16 },
    /// 平台返回 `code != 0` / 非 2xx。
    Api {
        status: u16,
        code: Option<i64>,
        message: String,
    },
    /// 响应缺少必需字段。
    Missing(String),
}

impl QqApiError {
    /// 可重试：限流与服务端错误值得退避重试，鉴权与参数错误不值得。
    pub fn retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::NonJson { .. } => true,
            Self::Api { status, code, .. } => {
                *status == 429 || *status >= 500 || matches!(code, Some(20028) | Some(11244))
            }
            Self::Missing(_) => false,
        }
    }

    /// 鉴权失败：令牌作废，需要丢弃缓存后重试一次。
    pub fn unauthorized(&self) -> bool {
        match self {
            Self::Api { status, code, .. } => {
                *status == 401 || matches!(code, Some(11241) | Some(11242) | Some(11243))
            }
            _ => false,
        }
    }

    /// 内容审核拦截：不应重试，等待平台审核结果。
    pub fn under_audit(&self) -> bool {
        matches!(
            self,
            Self::Api {
                code: Some(304023) | Some(304024),
                ..
            }
        )
    }
}

impl std::fmt::Display for QqApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(detail) => write!(formatter, "QQ 接口请求失败：{detail}"),
            Self::NonJson { status } => write!(formatter, "QQ 接口返回非 JSON（HTTP {status}）"),
            Self::Api {
                status,
                code,
                message,
            } => {
                let code = code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string());
                write!(formatter, "QQ 接口错误 {code}（HTTP {status}）：{message}")
            }
            Self::Missing(field) => write!(formatter, "QQ 接口响应缺少 {field}"),
        }
    }
}

impl std::error::Error for QqApiError {}

/// QQ 开放平台客户端：一次调用一个接口。
pub struct QqApi {
    app_id: String,
    app_secret: String,
    base_url: String,
    token_url: String,
    transport: Arc<dyn HttpTransport>,
    token: Mutex<Option<(String, Instant)>>,
}

impl QqApi {
    pub fn new(
        app_id: &str,
        app_secret: &str,
        base_url: &str,
        transport: Arc<dyn HttpTransport>,
    ) -> QqApi {
        QqApi {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token_url: TOKEN_URL.to_string(),
            transport,
            token: Mutex::new(None),
        }
    }

    /// 生产形态：ureq 传输。
    pub fn with_ureq(config: &super::config::QqBotConfig) -> QqApi {
        QqApi::new(
            &config.app_id,
            &config.app_secret,
            config.base_url(),
            Arc::new(UreqTransport::new()),
        )
    }

    /// 测试用：覆盖令牌端点。
    pub fn with_token_url(mut self, url: &str) -> QqApi {
        self.token_url = url.to_string();
        self
    }

    /// 丢弃缓存的令牌（鉴权失败后重试用）。
    pub fn invalidate_token(&self) {
        if let Ok(mut cached) = self.token.lock() {
            *cached = None;
        }
    }

    /// AccessToken：命中缓存直接返回，过期前 5 分钟续期。
    pub fn access_token(&self) -> Result<String, QqApiError> {
        {
            let cached = self.token.lock().expect("令牌锁中毒");
            if let Some((token, expires_at)) = cached.as_ref() {
                if Instant::now() + TOKEN_REFRESH_MARGIN < *expires_at {
                    return Ok(token.clone());
                }
            }
        }
        let body = json::dumps(&json!({
            "appId": self.app_id,
            "clientSecret": self.app_secret,
        }));
        let reply = self
            .transport
            .post_json(&self.token_url, &body, TOKEN_TIMEOUT)
            .map_err(QqApiError::Transport)?;
        let payload = parse_payload(&reply)?;
        let token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| QqApiError::Api {
                status: reply.status,
                code: payload.get("code").and_then(Value::as_i64),
                message: payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未返回 access_token")
                    .to_string(),
            })?
            .to_string();
        // `expires_in` 官方语义是数字，实际按字符串返回。
        let expires_in = match payload.get("expires_in") {
            Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
            Some(Value::Number(number)) => number.as_i64(),
            _ => None,
        }
        .unwrap_or(7200);
        let expires_at = Instant::now() + Duration::from_secs(expires_in.max(60) as u64);
        let mut cached = self.token.lock().expect("令牌锁中毒");
        *cached = Some((token.clone(), expires_at));
        Ok(token)
    }

    /// 带鉴权的请求；`body` 为 `None` 时发 GET。
    fn authorized(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, QqApiError> {
        let token = self.access_token()?;
        let reply = self.send_with_token(method, url, body, &token, timeout)?;
        if !(200..300).contains(&reply.status) {
            // 鉴权失败时令牌可能刚被重置：丢缓存重试一次（官方建议「重试一次即可」）。
            let error = api_error(&reply)?;
            if error.unauthorized() {
                self.invalidate_token();
                let token = self.access_token()?;
                let reply = self.send_with_token(method, url, body, &token, timeout)?;
                if (200..300).contains(&reply.status) {
                    return parse_payload(&reply);
                }
                return Err(api_error(&reply).unwrap_or(QqApiError::NonJson {
                    status: reply.status,
                }));
            }
            return Err(error);
        }
        parse_payload(&reply)
    }

    fn send_with_token(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
        token: &str,
        timeout: Duration,
    ) -> Result<crate::http::HttpReply, QqApiError> {
        let headers = [
            ("Authorization".to_string(), format!("QQBot {token}")),
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("User-Agent".to_string(), USER_AGENT.to_string()),
        ];
        self.transport
            .request(method, url, &headers, body.map(str::as_bytes), timeout)
            .map_err(QqApiError::Transport)
    }

    /// 网关地址（`wss://…`）。
    pub fn gateway_url(&self) -> Result<String, QqApiError> {
        let url = format!("{}/gateway", self.base_url);
        let payload = self.authorized("GET", &url, None, REQUEST_TIMEOUT)?;
        payload
            .get("url")
            .and_then(Value::as_str)
            .map(|value| value.to_string())
            .ok_or_else(|| QqApiError::Missing("url".to_string()))
    }

    /// 机器人自身信息（`/users/@me`），用于启动日志。
    pub fn me(&self) -> Result<Value, QqApiError> {
        let url = format!("{}/users/@me", self.base_url);
        self.authorized("GET", &url, None, REQUEST_TIMEOUT)
    }

    /// 被动回复一条文本：`msg_id` 是触发消息，`msg_seq` 是同一条消息的第几条回复。
    ///
    /// `reference` 非空时以引用形式展示（值是 `message_scene.ext` 的 `msg_idx`）。
    pub fn send_text(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, QqApiError> {
        let mut body = serde_json::Map::new();
        body.insert("content".to_string(), json!(content));
        body.insert("msg_type".to_string(), json!(MSG_TYPE_TEXT));
        self.post_message(target, body, msg_id, msg_seq, reference)
    }

    /// 被动回复一条 Markdown：`msg_type=2`，正文放在 `markdown.content`。
    ///
    /// 平台约束：填了 `markdown` 就不能再带 `content`，且 `markdown.content` 不允许为空。
    pub fn send_markdown(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, QqApiError> {
        let mut body = serde_json::Map::new();
        body.insert("msg_type".to_string(), json!(MSG_TYPE_MARKDOWN));
        body.insert("markdown".to_string(), json!({ "content": content }));
        self.post_message(target, body, msg_id, msg_seq, reference)
    }

    /// 被动回复一条富媒体：`msg_type=7`，`file_info` 来自 [`QqApi::upload_media`]。
    pub fn send_media(
        &self,
        target: &Target,
        file_info: &str,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, QqApiError> {
        let mut body = serde_json::Map::new();
        body.insert("msg_type".to_string(), json!(MSG_TYPE_MEDIA));
        body.insert("media".to_string(), json!({ "file_info": file_info }));
        self.post_message(target, body, msg_id, msg_seq, reference)
    }

    /// 统一下发：补 `msg_id` / `msg_seq` / `message_reference` 后 POST 到目标场景的消息路径。
    fn post_message(
        &self,
        target: &Target,
        mut body: serde_json::Map<String, Value>,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, QqApiError> {
        if !msg_id.is_empty() {
            body.insert("msg_id".to_string(), json!(msg_id));
            if target.uses_seq() {
                body.insert("msg_seq".to_string(), json!(msg_seq));
            }
        }
        if !reference.is_empty() {
            body.insert(
                "message_reference".to_string(),
                json!({ "message_id": reference }),
            );
        }
        let url = format!("{}{}", self.base_url, target.messages_path());
        let payload = self.authorized(
            "POST",
            &url,
            Some(&json::dumps(&Value::Object(body))),
            REQUEST_TIMEOUT,
        )?;
        Ok(payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// 上传富媒体（`srv_send_msg=false`），返回 `file_info` 供后续引用。
    ///
    /// 不超过 [`MAX_DIRECT_UPLOAD_BYTES`] 走二进制直传；更大的走分片上传——平台图片软限
    /// 20MB、视频 30MB、语音 20MB、其余文件 200MB，直传的 5MB 显然不够用。
    pub fn upload_media(
        &self,
        target: &Target,
        file_type: i64,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<String, QqApiError> {
        if bytes.len() <= MAX_DIRECT_UPLOAD_BYTES {
            return self.upload_media_direct(target, file_type, file_name, bytes);
        }
        self.upload_media_chunked(target, file_type, file_name, bytes)
    }

    /// 二进制直传（`file_data`，上限 5MB）。
    fn upload_media_direct(
        &self,
        target: &Target,
        file_type: i64,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<String, QqApiError> {
        if bytes.len() > MAX_DIRECT_UPLOAD_BYTES {
            return Err(QqApiError::Api {
                status: 200,
                code: Some(850031),
                message: format!(
                    "文件超过 {}MB 直传上限",
                    MAX_DIRECT_UPLOAD_BYTES / 1024 / 1024
                ),
            });
        }
        let body = json::dumps(&json!({
            "file_type": file_type,
            "file_name": file_name,
            "file_data": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                bytes,
            ),
            "srv_send_msg": false,
        }));
        let url = format!("{}{}", self.base_url, target.files_path());
        let payload = self.authorized("POST", &url, Some(&body), DOWNLOAD_TIMEOUT)?;
        payload
            .get("file_info")
            .and_then(Value::as_str)
            .map(|value| value.to_string())
            .ok_or_else(|| QqApiError::Missing("file_info".to_string()))
    }

    /// 分片上传：预上传 → 逐片 PUT 到预签名地址并通知完成 → 用 `upload_id` 合并。
    ///
    /// 预签名地址指向对象存储，**不能**带平台的 `Authorization` 头（签名会因此失效）。
    fn upload_media_chunked(
        &self,
        target: &Target,
        file_type: i64,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<String, QqApiError> {
        let (Some(prepare_path), Some(finish_path)) = (
            target.upload_prepare_path(),
            target.upload_part_finish_path(),
        ) else {
            return Err(QqApiError::Api {
                status: 200,
                code: Some(850031),
                message: "该场景不支持分片上传".to_string(),
            });
        };
        let prepare_url = format!("{}{prepare_path}", self.base_url);
        let prepare_body = json::dumps(&upload::prepare_body(file_type, file_name, bytes));
        let prepared =
            self.authorized("POST", &prepare_url, Some(&prepare_body), REQUEST_TIMEOUT)?;
        let upload_id = prepared
            .get("upload_id")
            .and_then(Value::as_str)
            .ok_or_else(|| QqApiError::Missing("upload_id".to_string()))?
            .to_string();
        let parts = prepared
            .get("parts")
            .and_then(Value::as_array)
            .ok_or_else(|| QqApiError::Missing("parts".to_string()))?;

        let finish_url = format!("{}{finish_path}", self.base_url);
        // 分片顺序与大小都以响应为准：按 index 升序逐片推进偏移，不假设 block_size 恒定。
        let mut ordered: Vec<&Value> = parts.iter().collect();
        ordered.sort_by_key(|part| part.get("index").and_then(Value::as_i64).unwrap_or(0));
        let mut offset = 0_usize;
        for part in ordered {
            let index = part.get("index").and_then(Value::as_i64).unwrap_or(0);
            let presigned = part
                .get("presigned_url")
                .and_then(Value::as_str)
                .ok_or_else(|| QqApiError::Missing("parts[].presigned_url".to_string()))?;
            let size = part
                .get("block_size")
                .and_then(Value::as_str)
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if size == 0 || offset >= bytes.len() {
                continue;
            }
            let end = (offset + size).min(bytes.len());
            let chunk = &bytes[offset..end];
            offset = end;
            self.put_presigned(presigned, chunk)?;
            let finish_body = json::dumps(&json!({
                "upload_id": upload_id,
                "part_index": index,
                "block_size": chunk.len().to_string(),
                "md5": upload::hex(&upload::md5(chunk)),
            }));
            self.authorized("POST", &finish_url, Some(&finish_body), REQUEST_TIMEOUT)?;
        }

        // 全部分片就绪后用 `upload_id` 调一次上传接口完成合并。
        let body = json::dumps(&json!({
            "file_type": file_type,
            "file_name": file_name,
            "upload_id": upload_id,
            "srv_send_msg": false,
        }));
        let url = format!("{}{}", self.base_url, target.files_path());
        let payload = self.authorized("POST", &url, Some(&body), DOWNLOAD_TIMEOUT)?;
        payload
            .get("file_info")
            .and_then(Value::as_str)
            .map(|value| value.to_string())
            .ok_or_else(|| QqApiError::Missing("file_info".to_string()))
    }

    /// 把一个分片 PUT 到预签名地址（对象存储，不带平台鉴权头）。
    fn put_presigned(&self, url: &str, bytes: &[u8]) -> Result<(), QqApiError> {
        let headers = [("User-Agent".to_string(), USER_AGENT.to_string())];
        let reply = self
            .transport
            .request("PUT", url, &headers, Some(bytes), DOWNLOAD_TIMEOUT)
            .map_err(QqApiError::Transport)?;
        if (200..300).contains(&reply.status) {
            return Ok(());
        }
        Err(QqApiError::Api {
            status: reply.status,
            code: None,
            message: "分片上传失败".to_string(),
        })
    }

    /// 主动下载平台给的附件 URL（`multimedia.nt.qq.com.cn`，无需鉴权）。
    pub fn download(&self, url: &str) -> Result<Vec<u8>, QqApiError> {
        let headers = [("User-Agent".to_string(), USER_AGENT.to_string())];
        let reply = self
            .transport
            .request("GET", url, &headers, None, DOWNLOAD_TIMEOUT)
            .map_err(QqApiError::Transport)?;
        if !(200..300).contains(&reply.status) {
            return Err(QqApiError::Api {
                status: reply.status,
                code: None,
                message: "附件下载失败".to_string(),
            });
        }
        Ok(reply.body)
    }
}

/// 解析响应体为 JSON 对象；非 2xx 时优先还原平台的错误结构。
fn parse_payload(reply: &crate::http::HttpReply) -> Result<Value, QqApiError> {
    if reply.body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice::<Value>(&reply.body).map_err(|_| QqApiError::NonJson {
        status: reply.status,
    })
}

fn api_error(reply: &crate::http::HttpReply) -> Result<QqApiError, QqApiError> {
    let payload = parse_payload(reply)?;
    Ok(QqApiError::Api {
        status: reply.status,
        code: payload.get("code").and_then(Value::as_i64),
        message: payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpReply;

    /// 记录请求并按 URL 回放的传输桩件。
    #[derive(Default)]
    struct MockHttp {
        requests: Mutex<Vec<(String, String, String)>>,
        token_calls: Mutex<usize>,
        /// 每次对预签名地址发送时携带的 `Authorization` 头（应为空）。
        presigned_authorizations: Mutex<Vec<String>>,
    }

    impl HttpTransport for MockHttp {
        fn request(
            &self,
            method: &str,
            url: &str,
            headers: &[(String, String)],
            body: Option<&[u8]>,
            _timeout: Duration,
        ) -> Result<HttpReply, String> {
            self.requests.lock().expect("锁中毒").push((
                method.to_string(),
                url.to_string(),
                body.map(|raw| String::from_utf8_lossy(raw).to_string())
                    .unwrap_or_default(),
            ));
            let authorization = headers
                .iter()
                .find(|(key, _value)| key == "Authorization")
                .map(|(_key, value)| value.clone())
                .unwrap_or_default();
            if url.starts_with("https://cos.example.com/") {
                self.presigned_authorizations
                    .lock()
                    .expect("锁中毒")
                    .push(authorization.clone());
            }
            let payload = if url.contains("getAppAccessToken") {
                let mut calls = self.token_calls.lock().expect("锁中毒");
                *calls += 1;
                json!({"access_token": format!("token-{calls}"), "expires_in": "7200"})
            } else if url.ends_with("/gateway") {
                if authorization.is_empty() {
                    return Ok(HttpReply {
                        status: 401,
                        body: b"{}".to_vec(),
                        headers: Vec::new(),
                    });
                }
                json!({"url": "wss://api.bot.qq.com/websocket/"})
            } else if url.ends_with("/upload_prepare") {
                // 两个分片：每片 5MB 的固定口径，末片由客户端按实际长度截断。
                json!({
                    "upload_id": "upload-1",
                    "block_size": (5 * 1024 * 1024).to_string(),
                    "parts": [
                        {"index": 0, "presigned_url": "https://cos.example.com/p0", "block_size": (5 * 1024 * 1024).to_string()},
                        {"index": 1, "presigned_url": "https://cos.example.com/p1", "block_size": (5 * 1024 * 1024).to_string()},
                    ],
                })
            } else if url.ends_with("/upload_part_finish") {
                json!({})
            } else if url.starts_with("https://cos.example.com/") {
                json!({})
            } else if url.ends_with("/files") {
                json!({"file_info": "FILE-INFO", "ttl": 300})
            } else {
                json!({"id": "msg-1", "timestamp": "1677000000"})
            };
            Ok(HttpReply {
                status: 200,
                body: payload.to_string().into_bytes(),
                headers: Vec::new(),
            })
        }
    }

    fn api() -> (QqApi, Arc<MockHttp>) {
        let transport = Arc::new(MockHttp::default());
        let api = QqApi::new(
            "102012345",
            "secret",
            "https://api.sgroup.qq.com",
            transport.clone(),
        )
        .with_token_url("https://bots.qq.com/app/getAppAccessToken");
        (api, transport)
    }

    #[test]
    fn access_token_is_cached_between_calls() {
        let (api, transport) = api();
        assert_eq!(api.access_token().expect("令牌"), "token-1");
        assert_eq!(api.access_token().expect("令牌"), "token-1");
        assert_eq!(*transport.token_calls.lock().unwrap(), 1);
    }

    #[test]
    fn token_payload_uses_the_camel_case_field_names() {
        let (api, transport) = api();
        api.access_token().expect("令牌");
        let body = transport.requests.lock().unwrap()[0].2.clone();
        assert!(body.contains("\"appId\""), "{body}");
        assert!(body.contains("\"clientSecret\""), "{body}");
    }

    #[test]
    fn gateway_uses_the_qqbot_authorization_scheme() {
        let (api, transport) = api();
        assert_eq!(
            api.gateway_url().expect("网关"),
            "wss://api.bot.qq.com/websocket/"
        );
        let requests = transport.requests.lock().unwrap();
        let gateway = requests
            .iter()
            .find(|(_method, url, _body)| url.ends_with("/gateway"))
            .expect("应有网关请求");
        assert_eq!(gateway.0, "GET");
    }

    #[test]
    fn passive_reply_carries_msg_id_and_seq() {
        let (api, transport) = api();
        api.send_text(&Target::User("F6A8".to_string()), "你好", "m-1", 2, "")
            .expect("发送");
        let requests = transport.requests.lock().unwrap();
        let send = requests
            .iter()
            .find(|(_method, url, _body)| url.contains("/v2/users/F6A8/messages"))
            .expect("应有发送请求");
        assert!(send.2.contains("\"msg_id\": \"m-1\""), "{}", send.2);
        assert!(send.2.contains("\"msg_seq\": 2"), "{}", send.2);
        assert!(send.2.contains("\"msg_type\": 0"), "{}", send.2);
        assert!(
            !send.2.contains("message_reference"),
            "不引用时不能带 message_reference：{}",
            send.2
        );
    }

    #[test]
    fn reference_reply_carries_message_reference() {
        let (api, transport) = api();
        api.send_text(
            &Target::Group("A1B2".to_string()),
            "看过了",
            "m-1",
            2,
            "REFIDX_zzz==",
        )
        .expect("发送");
        let requests = transport.requests.lock().unwrap();
        let send = requests
            .iter()
            .find(|(_method, url, _body)| url.contains("/v2/groups/A1B2/messages"))
            .expect("应有发送请求");
        let body: Value = serde_json::from_str(&send.2).expect("请求体是 JSON");
        assert_eq!(
            body["message_reference"]["message_id"],
            json!("REFIDX_zzz==")
        );
        assert_eq!(body["msg_id"], json!("m-1"));
    }

    #[test]
    fn group_targets_use_the_group_paths() {
        let target = Target::Group("A1B2".to_string());
        assert_eq!(target.messages_path(), "/v2/groups/A1B2/messages");
        assert_eq!(target.files_path(), "/v2/groups/A1B2/files");
    }

    #[test]
    fn markdown_reply_uses_msg_type_2_and_no_content_field() {
        let (api, transport) = api();
        api.send_markdown(&Target::Group("A1B2".to_string()), "# 标题", "m-2", 2, "")
            .expect("发送");
        let requests = transport.requests.lock().unwrap();
        let send = requests
            .iter()
            .find(|(_method, url, _body)| url.contains("/v2/groups/A1B2/messages"))
            .expect("应有发送请求");
        let body: Value = serde_json::from_str(&send.2).expect("请求体是 JSON");
        assert_eq!(body["msg_type"], json!(MSG_TYPE_MARKDOWN));
        assert_eq!(body["markdown"]["content"], json!("# 标题"));
        assert!(
            body.get("content").is_none(),
            "markdown 与 content 不能并存"
        );
        assert_eq!(body["msg_id"], json!("m-2"));
        assert_eq!(body["msg_seq"], json!(2));
    }

    #[test]
    fn rich_media_reply_uses_msg_type_7_and_file_info() {
        let (api, transport) = api();
        api.send_media(&Target::User("F6A8".to_string()), "FILE-INFO", "m-3", 3, "")
            .expect("发送");
        let requests = transport.requests.lock().unwrap();
        let send = requests
            .iter()
            .find(|(_method, url, _body)| url.contains("/v2/users/F6A8/messages"))
            .expect("应有发送请求");
        let body: Value = serde_json::from_str(&send.2).expect("请求体是 JSON");
        assert_eq!(body["msg_type"], json!(MSG_TYPE_MEDIA));
        assert_eq!(body["media"]["file_info"], json!("FILE-INFO"));
    }

    #[test]
    fn upload_sends_binary_with_the_matching_file_type() {
        let (api, transport) = api();
        api.upload_media(
            &Target::Group("A1B2".to_string()),
            FILE_TYPE_IMAGE,
            "a.png",
            b"PNG",
        )
        .expect("上传");
        let requests = transport.requests.lock().unwrap();
        let upload = requests
            .iter()
            .find(|(_method, url, _body)| url.contains("/v2/groups/A1B2/files"))
            .expect("应有上传请求");
        let body: Value = serde_json::from_str(&upload.2).expect("请求体是 JSON");
        assert_eq!(body["file_type"], json!(FILE_TYPE_IMAGE));
        assert_eq!(body["file_name"], json!("a.png"));
        assert_eq!(body["srv_send_msg"], json!(false));
        assert_eq!(body["file_data"], json!("UE5H"));
    }

    #[test]
    fn oversized_upload_switches_to_chunked_upload() {
        let (api, transport) = api();
        let big = vec![7_u8; MAX_DIRECT_UPLOAD_BYTES + 1];
        let info = api
            .upload_media(
                &Target::User("F6A8".to_string()),
                FILE_TYPE_FILE,
                "big.bin",
                &big,
            )
            .expect("超 5MB 应改走分片上传");
        assert_eq!(info, "FILE-INFO");

        let requests = transport.requests.lock().expect("锁中毒");
        let prepare = requests
            .iter()
            .find(|(_method, url, _body)| url.ends_with("/upload_prepare"))
            .expect("应先预上传");
        let body: Value = serde_json::from_str(&prepare.2).expect("请求体是 JSON");
        assert_eq!(body["file_type"], json!(FILE_TYPE_FILE));
        assert_eq!(body["file_name"], json!("big.bin"));
        assert_eq!(body["file_size"], json!(big.len().to_string()));
        assert_eq!(body["md5"].as_str().map(str::len), Some(32));
        assert_eq!(body["sha1"].as_str().map(str::len), Some(40));
        assert_eq!(body["md5_10m"].as_str().map(str::len), Some(32));

        // 两片各自 PUT 到预签名地址，且**不带**平台鉴权头。
        let puts: Vec<_> = requests
            .iter()
            .filter(|(method, url, _body)| method == "PUT" && url.starts_with("https://cos."))
            .collect();
        assert_eq!(puts.len(), 2, "两片都该 PUT：{requests:?}");
        assert_eq!(puts[0].1, "https://cos.example.com/p0");

        // 每片完成后都要通知，最后用 upload_id 合并。
        let finishes = requests
            .iter()
            .filter(|(_method, url, _body)| url.ends_with("/upload_part_finish"))
            .count();
        assert_eq!(finishes, 2, "每片都要通知完成：{requests:?}");
        let merge = requests
            .iter()
            .find(|(method, url, _body)| method == "POST" && url.ends_with("/files"))
            .expect("最后要合并");
        let body: Value = serde_json::from_str(&merge.2).expect("请求体是 JSON");
        assert_eq!(body["upload_id"], json!("upload-1"));
        assert_eq!(body["file_type"], json!(FILE_TYPE_FILE));
        assert!(
            body.get("file_data").is_none(),
            "分片上传不能再带 file_data"
        );
    }

    #[test]
    fn chunked_upload_sends_every_part_but_the_last_full() {
        let (api, transport) = api();
        let big = vec![3_u8; MAX_DIRECT_UPLOAD_BYTES + 11];
        api.upload_media(
            &Target::Group("A1B2".to_string()),
            FILE_TYPE_VIDEO,
            "big.mp4",
            &big,
        )
        .expect("分片上传");
        let requests = transport.requests.lock().expect("锁中毒");
        // 桩件把请求体按 UTF-8 有损记录，这里只核分片数量与顺序；
        // 「字节一一对应」由下面的完成通知 md5 用例钉住。
        let puts: Vec<_> = requests
            .iter()
            .filter(|(method, url, _body)| method == "PUT" && url.starts_with("https://cos."))
            .collect();
        assert_eq!(puts.len(), 2, "两片各 PUT 一次：{requests:?}");
        assert_eq!(puts[0].1, "https://cos.example.com/p0");
        assert_eq!(puts[1].1, "https://cos.example.com/p1");
        assert!(!puts[0].2.is_empty());
        // 分片完成通知里带的是该片自己的 MD5（首片是完整的 5MB 整块）。
        let finish = requests
            .iter()
            .find(|(_method, url, _body)| url.ends_with("/upload_part_finish"))
            .expect("应有完成通知");
        let body: Value = serde_json::from_str(&finish.2).expect("请求体是 JSON");
        assert_eq!(body["upload_id"], json!("upload-1"));
        assert_eq!(
            body["md5"],
            json!(crate::qqbot::upload::hex(&crate::qqbot::upload::md5(
                &big[..5 * 1024 * 1024]
            )))
        );
        assert_eq!(body["block_size"], json!((5 * 1024 * 1024).to_string()));
    }

    #[test]
    fn presigned_put_carries_no_platform_authorization() {
        let (api, transport) = api();
        let big = vec![0_u8; MAX_DIRECT_UPLOAD_BYTES + 1];
        api.upload_media(
            &Target::User("F6A8".to_string()),
            FILE_TYPE_FILE,
            "big.bin",
            &big,
        )
        .expect("分片上传");
        let recorded = transport.presigned_authorizations.lock().expect("锁中毒");
        assert_eq!(recorded.len(), 2, "两片都要走预签名地址：{recorded:?}");
        assert!(
            recorded.iter().all(String::is_empty),
            "预签名地址带 Authorization 会让签名失效：{recorded:?}"
        );
    }

    #[test]
    fn retryable_and_audit_classification() {
        let limited = QqApiError::Api {
            status: 429,
            code: Some(20028),
            message: "limit".to_string(),
        };
        assert!(limited.retryable());
        let audit = QqApiError::Api {
            status: 200,
            code: Some(304023),
            message: "audit".to_string(),
        };
        assert!(audit.under_audit());
        assert!(!audit.retryable());
        let unauthorized = QqApiError::Api {
            status: 401,
            code: None,
            message: "401".to_string(),
        };
        assert!(unauthorized.unauthorized());
    }
}
