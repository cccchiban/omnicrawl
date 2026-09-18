//! Telegram 更新解析与命令分发。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `_handle_update` /
//! `_is_allowed` / `_dispatch` / `_handle_thinking_command` / `_handle_workspace_command`：
//! 该模块只做**判定**，网络与 Agent 调用由调用方执行。

use std::collections::HashSet;

use serde_json::Value;

use super::files::{extract_telegram_file, TelegramFile};

/// 文本消息的处置分类；其余斜杠命令交给宿主注册表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start,
    Status,
    Session,
    Reset,
    Cancel,
    Approve,
    Reject,
    Thinking,
    Workspace,
    /// 其他 `/` 开头的命令：转发宿主命令注册表。
    Harness,
    /// 普通文本：作为任务交给 Agent。
    Task,
}

/// 一条更新的路由结果。
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateRoute {
    /// 缺 chat_id 或 user_id：静默忽略。
    MissingIdentifiers,
    /// 非白名单用户：不回复，仅日志。
    Unauthorized { chat_id: i64, user_id: i64 },
    /// 文本消息。
    Text {
        chat_id: i64,
        user_id: i64,
        text: String,
    },
    /// 文件消息：下载、分类落盘后交给 Agent。
    File {
        chat_id: i64,
        user_id: i64,
        file: TelegramFile,
        caption: String,
    },
    /// 既无文本也无文件（贴纸等）：静默忽略。
    Unsupported,
}

/// `/thinking` 的三种结果：查询当前状态、开启、关闭、用法提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingCommand {
    Query,
    Enable,
    Disable,
    Usage,
}

/// 命令可能带 bot 用户名后缀（`/start@MyBot`），先取命令词再匹配。
pub fn command_name(text: &str) -> String {
    let first = text.split(' ').next().unwrap_or("");
    first.split('@').next().unwrap_or("").to_string()
}

/// 分发分类：基础命令由连接器直接处理，其余 `/` 命令与普通文本分别落到宿主与任务。
pub fn classify(text: &str) -> Command {
    match command_name(text).as_str() {
        "/start" => Command::Start,
        "/status" => Command::Status,
        "/session" => Command::Session,
        "/reset" => Command::Reset,
        "/cancel" => Command::Cancel,
        "/approve" => Command::Approve,
        "/reject" => Command::Reject,
        "/thinking" => Command::Thinking,
        "/workspace" => Command::Workspace,
        _ if text.starts_with('/') => Command::Harness,
        _ => Command::Task,
    }
}

/// 解析 `/thinking` 的参数（`parts[1]` 判定，与 Python 的 `text.split()` 一致）。
pub fn parse_thinking_command(text: &str) -> ThinkingCommand {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() == 1 {
        return ThinkingCommand::Query;
    }
    match parts[1].to_lowercase().as_str() {
        "on" | "1" | "true" | "yes" | "开" | "开启" => ThinkingCommand::Enable,
        "off" | "0" | "false" | "no" | "关" | "关闭" => ThinkingCommand::Disable,
        _ => ThinkingCommand::Usage,
    }
}

/// 解析 `/workspace` 的路径参数；无参数或参数为空时返回 None（表示只查看）。
pub fn workspace_argument(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    parts.next()?;
    let argument = parts.next().unwrap_or("").trim();
    if argument.is_empty() {
        return None;
    }
    Some(argument.to_string())
}

/// 解析一条 `getUpdates` 更新：标识 → 白名单 → 文本 / 文件。
pub fn route_update(update: &Value, allowed: &HashSet<i64>) -> UpdateRoute {
    let message = update.get("message").unwrap_or(&Value::Null);
    let chat_id = message
        .get("chat")
        .and_then(|chat| chat.get("id"))
        .and_then(as_identifier);
    let user_id = message
        .get("from")
        .and_then(|from| from.get("id"))
        .and_then(as_identifier);
    let (Some(chat_id), Some(user_id)) = (chat_id, user_id) else {
        return UpdateRoute::MissingIdentifiers;
    };
    if !allowed.contains(&user_id) {
        return UpdateRoute::Unauthorized { chat_id, user_id };
    }
    let text = message
        .get("text")
        .map(value_as_text)
        .unwrap_or_default()
        .trim()
        .to_string();
    if !text.is_empty() {
        return UpdateRoute::Text {
            chat_id,
            user_id,
            text,
        };
    }
    match extract_telegram_file(message) {
        Some(file) => UpdateRoute::File {
            chat_id,
            user_id,
            file,
            caption: message
                .get("caption")
                .map(value_as_text)
                .unwrap_or_default()
                .trim()
                .to_string(),
        },
        None => UpdateRoute::Unsupported,
    }
}

/// `int(...)` 语义的标识解析：JSON 数字与数字字符串都接受。
pub fn as_identifier(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

/// 是否授权：非数字标识一律拒绝。
pub fn is_allowed(identifier: &Value, allowed: &HashSet<i64>) -> bool {
    as_identifier(identifier)
        .map(|value| allowed.contains(&value))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn allowed() -> HashSet<i64> {
        HashSet::from([7_i64])
    }

    #[test]
    fn bot_suffix_is_stripped() {
        assert_eq!(classify("/status@MyBot now"), Command::Status);
        assert_eq!(classify("/plan"), Command::Harness);
        assert_eq!(classify("普通任务"), Command::Task);
    }

    #[test]
    fn unauthorized_user_is_rejected_without_reply() {
        let update = json!({"message": {"chat": {"id": 1}, "from": {"id": 9}, "text": "hi"}});
        assert_eq!(
            route_update(&update, &allowed()),
            UpdateRoute::Unauthorized {
                chat_id: 1,
                user_id: 9
            }
        );
    }

    #[test]
    fn file_message_carries_caption() {
        let update = json!({"message": {
            "chat": {"id": 1},
            "from": {"id": 7},
            "caption": "  提取文字  ",
            "document": {"file_id": "f1", "file_name": "a.png"},
        }});
        match route_update(&update, &allowed()) {
            UpdateRoute::File { file, caption, .. } => {
                assert_eq!(file.file_id, "f1");
                assert_eq!(caption, "提取文字");
            }
            other => panic!("应为文件路由，实际 {other:?}"),
        }
    }

    #[test]
    fn missing_chat_or_user_is_ignored() {
        let update = json!({"message": {"from": {"id": 7}, "text": "hi"}});
        assert_eq!(
            route_update(&update, &allowed()),
            UpdateRoute::MissingIdentifiers
        );
    }

    #[test]
    fn thinking_and_workspace_arguments() {
        assert_eq!(parse_thinking_command("/thinking"), ThinkingCommand::Query);
        assert_eq!(
            parse_thinking_command("/thinking  ON "),
            ThinkingCommand::Enable
        );
        assert_eq!(
            parse_thinking_command("/thinking 关"),
            ThinkingCommand::Disable
        );
        assert_eq!(
            parse_thinking_command("/thinking 呀"),
            ThinkingCommand::Usage
        );
        assert_eq!(workspace_argument("/workspace"), None);
        assert_eq!(workspace_argument("/workspace   "), None);
        assert_eq!(
            workspace_argument("/workspace  D:/demo 双 空格 "),
            Some("D:/demo 双 空格".to_string())
        );
    }
}
