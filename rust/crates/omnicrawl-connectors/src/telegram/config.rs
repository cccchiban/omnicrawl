//! Telegram 接入配置解析。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `load_telegram_config`：
//! 凭据只读 `config.toml` 的 `[telegram]` 段（段对象由宿主配置层解析后传入，
//! TOML 读取不属于本 crate）。

use serde_json::Value;

/// 工具确认超时默认秒数。
pub const DEFAULT_CONFIRM_TIMEOUT_SECONDS: f64 = 300.0;

/// 一次 `getUpdates` 长轮询的等待秒数（Telegram 上限 50）。
pub const POLLING_TIMEOUT_SECONDS: u64 = 25;

/// 解析后的 Telegram 接入配置。
#[derive(Debug, Clone, PartialEq)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_user_ids: Vec<i64>,
    pub confirm_timeout_seconds: f64,
}

/// 读取配置：只读配置段，缺项回落默认值；解析失败返回中文错误。
pub fn load_telegram_config(section: Option<&Value>) -> Result<TelegramConfig, String> {
    let bot_token = value_as_text(section_or(
        section,
        "bot_token",
        Value::String(String::new()),
    ))
    .trim()
    .to_string();

    let raw_allowed = section_or(
        section,
        "allowed_user_ids",
        Value::String(String::new()),
    );
    let allowed_user_ids = parse_allowed_user_ids(&raw_allowed)?;

    let raw_timeout = section_or(
        section,
        "confirmation_timeout_seconds",
        Value::String(DEFAULT_CONFIRM_TIMEOUT_SECONDS.to_string()),
    );
    let confirm_timeout_seconds = parse_seconds(&raw_timeout)?;

    Ok(TelegramConfig {
        bot_token,
        allowed_user_ids,
        confirm_timeout_seconds,
    })
}

/// 构造 Bot 前的校验：Token 与白名单必填，超时下限 1 秒。
pub fn validate_config(config: &TelegramConfig) -> Result<(), String> {
    if config.bot_token.trim().is_empty() {
        return Err("缺少 Telegram Bot Token。".to_string());
    }
    if config.allowed_user_ids.is_empty() {
        return Err("allowed_user_ids 不能为空：至少需要一个授权用户 ID。".to_string());
    }
    Ok(())
}

/// 超时下限：与 Python `max(1.0, float(...))` 一致。
pub fn effective_confirm_timeout(seconds: f64) -> f64 {
    if seconds > 1.0 {
        seconds
    } else {
        1.0
    }
}

fn section_or(section: Option<&Value>, config_key: &str, default: Value) -> Value {
    if let Some(found) = section.and_then(|value| value.get(config_key)) {
        return found.clone();
    }
    default
}

/// TOML 数组与逗号分隔字符串两种写法都接受（全角逗号也算分隔符）。
fn parse_allowed_user_ids(raw: &Value) -> Result<Vec<i64>, String> {
    let mut allowed = Vec::new();
    match raw {
        Value::Array(items) => {
            for item in items {
                match identifier_of(item) {
                    Some(value) => allowed.push(value),
                    None => {
                        return Err(format!(
                            "Telegram 允许用户 ID 必须是整数：{}",
                            display(item)
                        ))
                    }
                }
            }
        }
        other => {
            let text = value_as_text(other.clone()).replace('，', ",");
            for item in text.split(',') {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }
                match item.parse::<i64>() {
                    Ok(value) => allowed.push(value),
                    Err(_) => {
                        return Err(format!("Telegram 允许用户 ID 必须是整数：{item}"));
                    }
                }
            }
        }
    }
    Ok(allowed)
}

fn parse_seconds(raw: &Value) -> Result<f64, String> {
    let parsed = match raw {
        Value::Number(number) => number.as_f64(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed.ok_or_else(|| {
        "telegram.confirmation_timeout_seconds 必须是数字。".to_string()
    })
}

fn identifier_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn value_as_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => display(&other),
    }
}

fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn section_supplies_everything() {
        let section = json!({
            "bot_token": "  from-config  ",
            "allowed_user_ids": [1, 2],
            "confirmation_timeout_seconds": 12,
        });
        let config = load_telegram_config(Some(&section)).expect("配置可解析");
        assert_eq!(config.bot_token, "from-config");
        assert_eq!(config.allowed_user_ids, vec![1, 2]);
        assert_eq!(config.confirm_timeout_seconds, 12.0);
    }

    #[test]
    fn comma_separated_ids_support_full_width_comma() {
        let section = json!({"allowed_user_ids": "1，2, 3 ,"});
        let config = load_telegram_config(Some(&section)).expect("配置可解析");
        assert_eq!(config.allowed_user_ids, vec![1, 2, 3]);
    }

    #[test]
    fn invalid_user_id_reports_item() {
        let section = json!({"allowed_user_ids": "1,abc"});
        let error = load_telegram_config(Some(&section)).expect_err("应报错");
        assert_eq!(error, "Telegram 允许用户 ID 必须是整数：abc");
    }

    #[test]
    fn invalid_timeout_reports_hint() {
        let section = json!({"confirmation_timeout_seconds": "soon"});
        let error = load_telegram_config(Some(&section)).expect_err("应报错");
        assert!(error.contains("必须是数字"), "{error}");
    }

    #[test]
    fn defaults_apply_when_absent() {
        let config = load_telegram_config(None).expect("配置可解析");
        assert_eq!(config.bot_token, "");
        assert!(config.allowed_user_ids.is_empty());
        assert_eq!(config.confirm_timeout_seconds, 300.0);
        assert_eq!(
            validate_config(&config).expect_err("缺 token"),
            "缺少 Telegram Bot Token。"
        );
    }

    #[test]
    fn timeout_floor_is_one_second() {
        assert_eq!(effective_confirm_timeout(0.2), 1.0);
        assert_eq!(effective_confirm_timeout(300.0), 300.0);
    }
}
