//! 请求解析：入参校验与 `message` + `data` 的确定性注入格式。
//!
//! 校验只做「系统边界」该做的事——字段类型、必填、长度与取值域。注入格式是设计的一部分：
//! 调用方拿到的消息形状必须是可预期的，否则同一个脚本在不同版本上喂给模型的上下文会变样。

use chrono::{Local, TimeZone};
use serde_json::{Map, Value};

use crate::app::ApiError;
use crate::records::NotifySpec;

/// `message` 上限（字符）。
pub const MAX_MESSAGE_CHARS: usize = 20000;
/// `data` 序列化后的上限（字符）。
pub const MAX_DATA_CHARS: usize = 64000;
/// `idempotency_key` 上限（字符）。
pub const MAX_IDEMPOTENCY_KEY_CHARS: usize = 200;
/// `wait_seconds` 上限：同步等待不该把 HTTP 连接挂太久。
pub const MAX_WAIT_SECONDS: i64 = 300;
/// 缺省标题长度：取 `message` 前 N 个字符。
pub const TITLE_CHARS: usize = 40;
/// 投递正文上限的取值域。
pub const MIN_NOTIFY_CHARS: i64 = 200;
pub const MAX_NOTIFY_CHARS: i64 = 20000;
pub const DEFAULT_NOTIFY_CHARS: i64 = 4000;
pub use omnicrawl_config::features::trigger_api::{
    MAX_TRIGGER_TIMEOUT_SECONDS, MIN_TRIGGER_TIMEOUT_SECONDS,
};

/// 已校验的创建请求。
pub struct CreateRequest {
    pub title: String,
    pub message: String,
    pub data: Option<Value>,
    pub session_key: String,
    pub workspace: String,
    pub timeout_seconds: Option<i64>,
    pub wait_seconds: i64,
    pub idempotency_key: String,
    pub notify: Option<NotifySpec>,
    pub tags: Map<String, Value>,
}

/// 解析 `POST /v1/triggers` 的请求体。
pub fn parse_create(payload: &Value, session_mode: &str) -> Result<CreateRequest, ApiError> {
    let body = payload
        .as_object()
        .ok_or_else(|| invalid("请求体必须是 JSON 对象。"))?;

    let message = required_text(body, "message", MAX_MESSAGE_CHARS)?;

    let data = match body.get("data") {
        None | Some(Value::Null) => None,
        Some(value @ Value::Object(_)) => {
            let encoded = serde_json::to_string(value).unwrap_or_default();
            if encoded.chars().count() > MAX_DATA_CHARS {
                return Err(invalid(&format!(
                    "data 序列化后不能超过 {MAX_DATA_CHARS} 字符。"
                )));
            }
            Some(value.clone())
        }
        Some(_) => return Err(invalid("data 必须是 JSON 对象。")),
    };

    let title = match body.get("title") {
        None | Some(Value::Null) => default_title(&message),
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        Some(_) => return Err(invalid("title 必须是非空字符串。")),
    };

    let session_key = optional_text(body, "session_key", MAX_IDEMPOTENCY_KEY_CHARS)?;
    if !session_key.is_empty() && session_mode != "keyed" {
        return Err(invalid(
            "session_key 需要 [trigger_api] session_mode = \"keyed\" 才生效；\
             当前为 per_trigger，每次触发都会新开会话。",
        ));
    }

    let workspace = optional_text(body, "workspace", 4096)?;

    let timeout_seconds = match body.get("timeout_seconds") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let seconds =
                as_integer(value).ok_or_else(|| invalid("timeout_seconds 必须是整数秒。"))?;
            if !(MIN_TRIGGER_TIMEOUT_SECONDS..=MAX_TRIGGER_TIMEOUT_SECONDS).contains(&seconds) {
                return Err(invalid(&format!(
                    "timeout_seconds 必须在 {MIN_TRIGGER_TIMEOUT_SECONDS} 到 \
                     {MAX_TRIGGER_TIMEOUT_SECONDS} 之间。"
                )));
            }
            Some(seconds)
        }
    };

    let wait_seconds = match body.get("wait_seconds") {
        None | Some(Value::Null) => 0,
        Some(value) => {
            let seconds = as_integer(value).ok_or_else(|| invalid("wait_seconds 必须是整数。"))?;
            if !(0..=MAX_WAIT_SECONDS).contains(&seconds) {
                return Err(invalid(&format!(
                    "wait_seconds 必须在 0 到 {MAX_WAIT_SECONDS} 之间。"
                )));
            }
            seconds
        }
    };

    let idempotency_key = optional_text(body, "idempotency_key", MAX_IDEMPOTENCY_KEY_CHARS)?;

    let notify = match body.get("notify") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_notify(value)?),
    };

    let tags = match body.get("tags") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err(invalid("tags 必须是字符串键值对象。")),
    };

    Ok(CreateRequest {
        title,
        message,
        data,
        session_key,
        workspace,
        timeout_seconds,
        wait_seconds,
        idempotency_key,
        notify,
        tags,
    })
}

/// 解析 `notify`：通道、目标、触发条件与正文上限。
pub fn parse_notify(value: &Value) -> Result<NotifySpec, ApiError> {
    let body = value
        .as_object()
        .ok_or_else(|| invalid("notify 必须是 JSON 对象。"))?;
    let channel = body
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .trim()
        .to_lowercase();
    if !["telegram", "feishu", "qqbot", "none"].contains(&channel.as_str()) {
        return Err(invalid(
            "notify.type 只能是 telegram / feishu / qqbot / none。",
        ));
    }
    // 目标字段名按平台习惯取：telegram 用 chat_id，飞书用 receive_id，QQ 用 openid。
    let target = ["chat_id", "receive_id", "openid", "target"]
        .iter()
        .find_map(|key| body.get(*key))
        .map(scalar_to_text)
        .unwrap_or_default();
    if channel != "none" && target.trim().is_empty() {
        return Err(invalid(
            "notify 缺少投递目标：telegram 用 chat_id，飞书用 receive_id，QQ 用 openid。",
        ));
    }
    let on = body
        .get("on")
        .and_then(Value::as_str)
        .unwrap_or("completed")
        .trim()
        .to_lowercase();
    if !["completed", "failed", "always"].contains(&on.as_str()) {
        return Err(invalid("notify.on 只能是 completed / failed / always。"));
    }
    let max_chars = match body.get("max_chars") {
        None | Some(Value::Null) => DEFAULT_NOTIFY_CHARS,
        Some(value) => {
            let chars =
                as_integer(value).ok_or_else(|| invalid("notify.max_chars 必须是整数。"))?;
            if !(MIN_NOTIFY_CHARS..=MAX_NOTIFY_CHARS).contains(&chars) {
                return Err(invalid(&format!(
                    "notify.max_chars 必须在 {MIN_NOTIFY_CHARS} 到 {MAX_NOTIFY_CHARS} 之间。"
                )));
            }
            chars
        }
    };
    Ok(NotifySpec {
        channel,
        target: target.trim().to_string(),
        on,
        max_chars: max_chars as usize,
    })
}

/// 注入格式：`[外部触发] 标题` + 时间/来源 + `data` + 正文。
///
/// 确定性拼装，不做任何模型改写。空 `data` 时整段「数据：」省略；标题缺省时省略首行，
/// 这样「只发一句话」的调用方看到的就是那句话本身加上时间与来源。
pub fn build_prompt(title: &str, raw_message: &str, data: Option<&Value>, now: i64) -> String {
    let mut out = String::new();
    if !title.trim().is_empty() {
        out.push_str(&format!("[外部触发] {}\n", title.trim()));
    }
    out.push_str(&format!("时间：{}\n", format_local(now)));
    out.push_str("来源：omnicrawl 触发接口\n");
    if let Some(data) = data {
        let compact = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
        out.push_str(&format!("数据：{compact}\n"));
    }
    out.push('\n');
    out.push_str(raw_message.trim());
    out
}

/// 本机时区的 ISO-8601（带偏移），用于注入头部。
fn format_local(epoch_seconds: i64) -> String {
    match Local.timestamp_opt(epoch_seconds, 0).single() {
        Some(moment) => moment.format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        None => epoch_seconds.to_string(),
    }
}

/// 缺省标题：取正文前 40 个字符并压成单行。
fn default_title(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let taken: String = single_line.chars().take(TITLE_CHARS).collect();
    if taken.is_empty() {
        "外部触发".to_string()
    } else {
        taken
    }
}

fn required_text(body: &Map<String, Value>, key: &str, limit: usize) -> Result<String, ApiError> {
    let Some(value) = body.get(key) else {
        return Err(invalid(&format!("缺少必填字段 {key}。")));
    };
    let Some(text) = value.as_str() else {
        return Err(invalid(&format!("{key} 必须是字符串。")));
    };
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid(&format!("{key} 不能为空。")));
    }
    if text.chars().count() > limit {
        return Err(invalid(&format!("{key} 不能超过 {limit} 字符。")));
    }
    Ok(text.to_string())
}

fn optional_text(body: &Map<String, Value>, key: &str, limit: usize) -> Result<String, ApiError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.chars().count() > limit {
                return Err(invalid(&format!("{key} 不能超过 {limit} 字符。")));
            }
            Ok(text.to_string())
        }
        Some(_) => Err(invalid(&format!("{key} 必须是字符串。"))),
    }
}

/// 整数：`10`、`10.0` 与 `"10"` 都认——脚本传数字字符串是常态，不该为此报错。
fn as_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|float| float.is_finite())
                .map(|float| float as i64)
        }),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// 标量 → 文本：`chat_id` 允许整数或字符串两种写法。
fn scalar_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

fn invalid(message: &str) -> ApiError {
    ApiError::bad_request("INVALID_REQUEST", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> Result<CreateRequest, ApiError> {
        parse_create(&value, "per_trigger")
    }

    #[test]
    fn minimal_request_uses_message_as_title() {
        let request = parse(json!({"message": "收集今日新闻"})).expect("应当通过");
        assert_eq!(request.title, "收集今日新闻");
        assert_eq!(request.wait_seconds, 0);
        assert!(request.data.is_none());
        assert!(request.notify.is_none());
    }

    #[test]
    fn long_message_title_is_truncated_to_forty_chars() {
        let message = "新".repeat(100);
        let request = parse(json!({"message": message})).expect("应当通过");
        assert_eq!(request.title.chars().count(), TITLE_CHARS);
    }

    #[test]
    fn rejects_missing_or_blank_message() {
        assert_eq!(
            parse(json!({})).expect_err("缺字段").code,
            "INVALID_REQUEST"
        );
        assert_eq!(
            parse(json!({"message": "   "}))
                .expect_err("空白正文")
                .message,
            "message 不能为空。"
        );
        assert_eq!(
            parse(json!({"message": 42})).expect_err("类型不符").message,
            "message 必须是字符串。"
        );
    }

    #[test]
    fn rejects_non_object_data_and_oversized_payload() {
        assert!(parse(json!({"message": "x", "data": [1, 2]})).is_err());
        let huge = json!({"message": "x", "data": {"blob": "a".repeat(MAX_DATA_CHARS)}});
        assert!(parse(huge).is_err());
    }

    #[test]
    fn wait_seconds_and_timeout_are_range_checked() {
        assert!(parse(json!({"message": "x", "wait_seconds": MAX_WAIT_SECONDS + 1})).is_err());
        assert!(parse(json!({"message": "x", "wait_seconds": -1})).is_err());
        assert!(parse(json!({"message": "x", "timeout_seconds": 10})).is_err());
        assert!(parse(json!({"message": "x", "timeout_seconds": 600})).is_ok());
        // 数字字符串照常生效。
        assert_eq!(
            parse(json!({"message": "x", "wait_seconds": "30"}))
                .expect("应当通过")
                .wait_seconds,
            30
        );
    }

    #[test]
    fn session_key_requires_keyed_mode() {
        let error = parse(json!({"message": "x", "session_key": "daily"})).expect_err("应当拒绝");
        assert!(error.message.contains("session_mode"));
        let request = parse_create(&json!({"message": "x", "session_key": "daily"}), "keyed")
            .expect("keyed 模式应当通过");
        assert_eq!(request.session_key, "daily");
    }

    #[test]
    fn notify_accepts_integer_chat_id_and_rejects_unknown_channel() {
        let request = parse(json!({
            "message": "x",
            "notify": {"type": "telegram", "chat_id": 123456789}
        }))
        .expect("应当通过");
        let notify = request.notify.expect("有投递");
        assert_eq!(notify.channel, "telegram");
        assert_eq!(notify.target, "123456789");
        assert_eq!(notify.on, "completed");
        assert_eq!(notify.max_chars, DEFAULT_NOTIFY_CHARS as usize);

        assert!(parse(json!({"message": "x", "notify": {"type": "sms"}})).is_err());
        // 非 none 时目标必填。
        assert!(parse(json!({"message": "x", "notify": {"type": "telegram"}})).is_err());
    }

    #[test]
    fn notify_none_skips_target_requirement() {
        let request = parse(json!({"message": "x", "notify": {"type": "none"}})).expect("应当通过");
        let notify = request.notify.expect("有投递配置");
        assert_eq!(notify.channel, "none");
        assert!(!notify.matches(crate::records::TriggerState::Completed));
    }

    #[test]
    fn prompt_contains_header_data_and_body() {
        let data = json!({"moisture": 18.2, "zone": "阳台"});
        let prompt = build_prompt("土壤告警", "给出浇水建议", Some(&data), 1_790_000_000);
        assert!(prompt.starts_with("[外部触发] 土壤告警\n"));
        assert!(prompt.contains("来源：omnicrawl 触发接口"));
        assert!(prompt.contains(r#"数据：{"moisture":18.2,"zone":"阳台"}"#));
        assert!(prompt.ends_with("给出浇水建议"));
    }

    #[test]
    fn prompt_omits_data_section_when_absent() {
        let prompt = build_prompt("标题", "正文", None, 1_790_000_000);
        assert!(!prompt.contains("数据："));
        assert!(prompt.contains("\n\n正文"));
    }

    #[test]
    fn blank_title_omits_header_line() {
        let prompt = build_prompt("", "正文", None, 1_790_000_000);
        assert!(!prompt.contains("[外部触发]"));
        assert!(prompt.starts_with("时间："));
    }
}
