//! Telegram Bot API 传输层：长轮询、发消息、编辑消息与文件下载。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `_api` / `_get_updates` /
//! `_send_message` / `_create_stream_message` / `_edit_stream_message` / `_download_file_bytes`。
//! HTTP 细节走 [`crate::http::HttpTransport`]：真实实现是 ureq（阻塞、无异步运行时），
//! 测试用记录型实现替换。

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::config::POLLING_TIMEOUT_SECONDS;
use super::format::split_message;
use crate::http::HttpTransport;

pub use crate::http::UreqTransport;

/// Telegram Bot API 地址模板：`{token}` 由实例持有。
pub const API_BASE_TEMPLATE: &str = "https://api.telegram.org/bot{token}";
/// 文件下载地址模板（与 `getFile` 返回的 `file_path` 拼接）。
pub const FILE_DOWNLOAD_TEMPLATE: &str = "https://api.telegram.org/file/bot{token}/{path}";

/// API 调用失败：网络、非 JSON 响应与 Telegram 的业务错误三类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelegramApiError {
    /// 网络请求失败（Python 侧是 `requests.RequestException`）。
    Network(String),
    /// HTTP 层成功但响应体不是 JSON。
    NonJson { status: u16 },
    /// Telegram 返回 `ok != true`，`code` 是它给出的 `error_code`。
    Api {
        code: Option<i64>,
        description: String,
        retryable: bool,
    },
    /// 文件下载失败。
    Download(String),
}

impl TelegramApiError {
    /// 可重试：认证类（401/400/404）之外都值得重试。
    pub fn retryable(&self) -> bool {
        match self {
            Self::Api { retryable, .. } => *retryable,
            _ => true,
        }
    }
}

impl fmt::Display for TelegramApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(detail) => write!(formatter, "Telegram API 网络请求失败：{detail}"),
            Self::NonJson { status } => {
                write!(formatter, "Telegram API 返回非 JSON（HTTP {status}）")
            }
            Self::Api {
                code, description, ..
            } => {
                let code = code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "None".to_string());
                write!(formatter, "Telegram API 错误 {code}：{description}")
            }
            Self::Download(detail) => write!(formatter, "下载文件失败：{detail}"),
        }
    }
}

impl std::error::Error for TelegramApiError {}

/// Telegram Bot API 客户端：一次调用一个方法，`offset` 由调用方持有。
pub struct TelegramApi {
    token: String,
    base: String,
    file_base: String,
    transport: Arc<dyn HttpTransport>,
    request_timeout: Duration,
}

impl TelegramApi {
    /// 用给定传输构造；`token` 由调用方保证非空。
    pub fn new(token: &str, transport: Arc<dyn HttpTransport>) -> TelegramApi {
        let token = token.trim().to_string();
        let base = API_BASE_TEMPLATE.replace("{token}", &token);
        let file_base = FILE_DOWNLOAD_TEMPLATE
            .replace("{token}", &token)
            .replace("{path}", "");
        TelegramApi {
            token,
            base,
            file_base,
            transport,
            request_timeout: Duration::from_secs(30),
        }
    }

    /// 生产形态：ureq 传输。
    pub fn with_ureq(token: &str) -> TelegramApi {
        TelegramApi::new(token, Arc::new(UreqTransport::new()))
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// 调用一个方法，返回 `result`；`ok != true` 一律转成 [`TelegramApiError::Api`]。
    pub fn call(
        &self,
        method: &str,
        query: &[(String, String)],
        form: &[(String, String)],
    ) -> Result<Value, TelegramApiError> {
        let url = format!("{}/{method}", self.base);
        let reply = self
            .transport
            .post_form(&url, query, form, self.request_timeout)
            .map_err(TelegramApiError::Network)?;
        let payload: Value =
            serde_json::from_slice(&reply.body).map_err(|_| TelegramApiError::NonJson {
                status: reply.status,
            })?;
        if payload.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = payload.get("error_code").and_then(Value::as_i64);
            let description = payload
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string();
            let retryable = !matches!(code, Some(400 | 401 | 404));
            return Err(TelegramApiError::Api {
                code,
                description,
                retryable,
            });
        }
        Ok(payload.get("result").cloned().unwrap_or(Value::Null))
    }

    /// 长轮询拉取增量更新：返回更新列表与推进后的 `offset`。
    ///
    /// 收到即推进 offset（`max(offset, update_id + 1)`），处理失败也不重发同一批消息。
    pub fn get_updates(&self, offset: i64) -> Result<(Vec<Value>, i64), TelegramApiError> {
        let query = vec![
            ("timeout".to_string(), POLLING_TIMEOUT_SECONDS.to_string()),
            ("offset".to_string(), offset.to_string()),
            ("allowed_updates".to_string(), "message".to_string()),
        ];
        let result = self.call("getUpdates", &query, &[])?;
        let mut next_offset = offset;
        let mut updates = Vec::new();
        if let Some(items) = result.as_array() {
            for update in items {
                let update_id = update.get("update_id").and_then(Value::as_i64).unwrap_or(0);
                next_offset = next_offset.max(update_id + 1);
                updates.push(update.clone());
            }
        }
        Ok((updates, next_offset))
    }

    /// 发送文本，按单条长度上限分段；单段失败只记日志，不中断主循环。
    pub fn send_message(&self, chat_id: i64, text: &str) {
        if text.is_empty() {
            return;
        }
        for part in split_message(text, super::format::MAX_MESSAGE_LEN) {
            if let Err(error) = self.call("sendMessage", &[], &chat_form(chat_id, &part, None)) {
                eprintln!("[telegram] 发送消息失败（chat={chat_id}）：{error}");
            }
        }
    }

    /// 创建一条流式消息，返回 `message_id`；失败返回 None，不阻断任务。
    pub fn create_stream_message(&self, chat_id: i64, text: &str) -> Option<i64> {
        match self.call("sendMessage", &[], &chat_form(chat_id, text, None)) {
            Ok(result) => result.get("message_id").and_then(Value::as_i64),
            Err(error) => {
                eprintln!("[telegram] 创建流式消息失败（chat={chat_id}）：{error}");
                None
            }
        }
    }

    /// 编辑流式消息；失败只记日志，不阻断后续输出。
    pub fn edit_stream_message(&self, chat_id: i64, message_id: i64, text: &str) {
        if let Err(error) = self.call(
            "editMessageText",
            &[],
            &chat_form(chat_id, text, Some(message_id)),
        ) {
            eprintln!("[telegram] 编辑流式消息失败（chat={chat_id}, msg={message_id}）：{error}");
        }
    }

    /// `getFile` 换取远程路径。
    pub fn get_file_path(&self, file_id: &str) -> Result<String, TelegramApiError> {
        let result = self.call(
            "getFile",
            &[],
            &[("file_id".to_string(), file_id.to_string())],
        )?;
        let path = result
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if path.is_empty() {
            return Err(TelegramApiError::Download(
                "getFile 未返回 file_path".to_string(),
            ));
        }
        Ok(path)
    }

    /// 从 Telegram CDN 下载文件内容。
    pub fn download_file(&self, remote_path: &str) -> Result<Vec<u8>, TelegramApiError> {
        let url = format!("{}{remote_path}", self.file_base);
        let reply = self
            .transport
            .get(&url, Duration::from_secs(120))
            .map_err(TelegramApiError::Download)?;
        if !(200..300).contains(&reply.status) {
            return Err(TelegramApiError::Download(format!("HTTP {}", reply.status)));
        }
        Ok(reply.body)
    }
}

fn chat_form(chat_id: i64, text: &str, message_id: Option<i64>) -> Vec<(String, String)> {
    let mut form = vec![
        ("chat_id".to_string(), chat_id.to_string()),
        ("text".to_string(), text.to_string()),
    ];
    if let Some(message_id) = message_id {
        form.push(("message_id".to_string(), message_id.to_string()));
    }
    form
}
