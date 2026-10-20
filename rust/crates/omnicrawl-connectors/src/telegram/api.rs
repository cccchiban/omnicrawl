//! Telegram Bot API 传输层：长轮询、发消息、编辑消息与文件下载。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `_api` / `_get_updates` /
//! `_send_message` / `_create_stream_message` / `_edit_stream_message` / `_download_file_bytes`。
//! HTTP 细节走 [`crate::http::HttpTransport`]：真实实现是 ureq（阻塞、无异步运行时），
//! 测试用记录型实现替换。

use std::fmt;
use std::sync::{Arc, Mutex};
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

/// 编辑去重缓存上限：条目落地后不再被编辑，超限整体清空即可。
const EDIT_CACHE_LIMIT: usize = 1024;

/// Telegram 对「内容未改变」的编辑返回的错误片段，按成功处理。
const NOT_MODIFIED: &str = "message is not modified";

/// `setMyCommands` 单次可提交的命令条数上限（Telegram 侧硬限制）。
pub const MAX_MENU_COMMANDS: usize = 100;
/// 菜单描述的最大字符数（Telegram 侧硬限制）。
const MAX_MENU_DESCRIPTION_CHARS: usize = 256;

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
    /// 每条消息最后一次成功落地的内容：跳过「未改变」的编辑（Telegram 对它会回 400）。
    last_edits: Mutex<std::collections::HashMap<i64, String>>,
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
            last_edits: Mutex::new(std::collections::HashMap::new()),
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

    /// 创建一条消息，返回 `message_id`；失败返回 None，不阻断任务。
    fn create_stream_message(&self, chat_id: i64, text: &str) -> Option<i64> {
        match self.call("sendMessage", &[], &chat_form(chat_id, text, None)) {
            Ok(result) => result.get("message_id").and_then(Value::as_i64),
            Err(error) => {
                eprintln!("[telegram] 创建流式消息失败（chat={chat_id}）：{error}");
                None
            }
        }
    }

    /// 编辑消息并回报成功与否；内容与上次落地相同或为空时直接视为成功。
    ///
    /// Telegram 对「内容未改变」的编辑回 400（`message is not modified`），时间线会把
    /// 同一条目反复交付（例如思考面板与计划清单），因此这里按最后一次成功内容去重。
    fn edit_if_changed(&self, chat_id: i64, message_id: i64, text: &str) -> bool {
        if text.is_empty() {
            return true;
        }
        {
            let mut cache = self.last_edits.lock().expect("编辑缓存锁中毒");
            if cache.get(&message_id).map(String::as_str) == Some(text) {
                return true;
            }
            // 缓存只用于「同内容不重发」，条目落地后不会再被编辑，超限直接清空即可。
            if cache.len() >= EDIT_CACHE_LIMIT {
                cache.clear();
            }
        }
        match self.call(
            "editMessageText",
            &[],
            &chat_form(chat_id, text, Some(message_id)),
        ) {
            Ok(_) => {
                self.last_edits
                    .lock()
                    .expect("编辑缓存锁中毒")
                    .insert(message_id, text.to_string());
                true
            }
            // 「内容未改变」不是失败：消息已经是目标内容，更新缓存即可。
            Err(TelegramApiError::Api { description, .. })
                if description.contains(NOT_MODIFIED) =>
            {
                self.last_edits
                    .lock()
                    .expect("编辑缓存锁中毒")
                    .insert(message_id, text.to_string());
                true
            }
            Err(error) => {
                eprintln!("[telegram] 编辑消息失败（chat={chat_id}, msg={message_id}）：{error}");
                false
            }
        }
    }

    /// 用 `setMyCommands` 覆盖 bot 的默认命令菜单；`commands` 先经 [`normalize_menu_commands`] 过滤。
    ///
    /// 菜单只是客户端侧的可视提示，服务端没有它一切照常工作，因此调用方拿到错误只需记日志。
    pub fn set_my_commands(&self, commands: &[MenuCommand]) -> Result<usize, TelegramApiError> {
        let normalized = normalize_menu_commands(commands);
        let payload = Value::Array(
            normalized
                .iter()
                .map(|command| {
                    serde_json::json!({
                        "command": command.command,
                        "description": command.description,
                    })
                })
                .collect(),
        );
        self.call(
            "setMyCommands",
            &[],
            &[("commands".to_string(), payload.to_string())],
        )?;
        Ok(normalized.len())
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

/// bot 命令菜单的一项：裸命令名（不带 `/`）与一句说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuCommand {
    pub command: String,
    pub description: String,
}

impl MenuCommand {
    pub fn new(command: impl Into<String>, description: impl Into<String>) -> MenuCommand {
        MenuCommand {
            command: command.into(),
            description: description.into(),
        }
    }
}

/// 把宿主给的命令表收敛成 Telegram 能接受的形状。
///
/// 规则来自 Bot API：名字只能含小写字母、数字与下划线（`a-z0-9_`，1–32 字符且以字母开头），
/// 描述 1–256 字符，同名只保留首次出现，总条数不超过 [`MAX_MENU_COMMANDS`]。不满足的条目
/// 直接丢弃——命令本身在聊天里照样可用，菜单只是可视提示。
pub fn normalize_menu_commands(commands: &[MenuCommand]) -> Vec<MenuCommand> {
    let mut seen: Vec<String> = Vec::new();
    let mut normalized: Vec<MenuCommand> = Vec::new();
    for command in commands {
        if normalized.len() >= MAX_MENU_COMMANDS {
            break;
        }
        let name = menu_command_name(&command.command);
        if name.is_empty() || seen.contains(&name) {
            continue;
        }
        let description = command.description.trim();
        if description.is_empty() {
            continue;
        }
        seen.push(name.clone());
        normalized.push(MenuCommand {
            command: name,
            description: description.chars().take(MAX_MENU_DESCRIPTION_CHARS).collect(),
        });
    }
    normalized
}

/// 规范化单个命令名：去掉前导斜杠、字母转小写后校验字符集。
///
/// Telegram 只接受 `a-z`、`0-9` 与下划线，且必须以字母开头。注册表里的
/// `/memory:clean`、`/approval:review` 这类带分隔符的名字无法表示——硬改成下划线会让
/// 菜单插入一个并不存在的命令，因此返回空串由调用方跳过（命令本身在聊天里照常可用）。
fn menu_command_name(raw: &str) -> String {
    let name = raw
        .trim()
        .trim_start_matches('/')
        .chars()
        .map(|character| character.to_ascii_lowercase())
        .collect::<String>();
    let mut characters = name.chars();
    let valid = matches!(characters.next(), Some(first) if first.is_ascii_alphabetic())
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && name.len() <= 32;
    if !valid {
        return String::new();
    }
    name
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

impl super::timeline::MessagePort for TelegramApi {
    fn create(&self, chat_id: i64, text: &str) -> Option<i64> {
        let message_id = self.create_stream_message(chat_id, text)?;
        self.last_edits
            .lock()
            .expect("编辑缓存锁中毒")
            .insert(message_id, text.to_string());
        Some(message_id)
    }

    fn edit(&self, chat_id: i64, message_id: i64, text: &str) -> bool {
        self.edit_if_changed(chat_id, message_id, text)
    }

    fn send_text(&self, chat_id: i64, text: &str) {
        self.send_message(chat_id, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(commands: &[MenuCommand]) -> Vec<String> {
        normalize_menu_commands(commands)
            .into_iter()
            .map(|command| command.command)
            .collect()
    }

    #[test]
    fn menu_names_are_normalized_and_deduplicated() {
        let commands = vec![
            MenuCommand::new("/START", "开始"),
            MenuCommand::new("status", "状态"),
            MenuCommand::new("/Status", "重复项被丢弃"),
            MenuCommand::new("/memory:clean", "带冒号的名字无法表示"),
            MenuCommand::new("/1st", "数字开头不合法"),
            MenuCommand::new("/too_long_name_that_exceeds_thirty_two_chars", "过长"),
            MenuCommand::new("plan", "  "),
            MenuCommand::new("workspace", "切换工作区"),
        ];
        assert_eq!(names(&commands), vec!["start", "status", "workspace"]);
    }

    #[test]
    fn menu_descriptions_are_trimmed_and_capped() {
        let long = "描".repeat(MAX_MENU_DESCRIPTION_CHARS + 10);
        let normalized = normalize_menu_commands(&[MenuCommand::new("start", &long)]);
        assert_eq!(normalized.len(), 1);
        assert_eq!(
            normalized[0].description.chars().count(),
            MAX_MENU_DESCRIPTION_CHARS
        );
    }

    #[test]
    fn menu_list_is_capped_at_telegram_limit() {
        let commands: Vec<MenuCommand> = (0..MAX_MENU_COMMANDS + 5)
            .map(|index| MenuCommand::new(format!("cmd{index}"), "说明"))
            .collect();
        assert_eq!(
            normalize_menu_commands(&commands).len(),
            MAX_MENU_COMMANDS
        );
    }
}
