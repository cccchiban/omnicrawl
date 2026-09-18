//! 飞书接入配置解析。
//!
//! 语义基准是 Python `omnicrawl/connectors/fsapp.py` 的 `load_feishu_config` /
//! `_mask_secret` / `check_config`：**环境变量优先**，其次 `[feishu]` 段，再其次根级别
//! 的 `fs_app_id` / `fs_app_secret` / `fs_allowed_users`（兼容 GenericAgent 配置习惯）。
//! 配置对象由宿主配置层解析后传入，TOML 读取不属于本 crate。

use std::collections::BTreeSet;

use serde_json::{json, Value};

/// 数据来源：环境变量查询 + 已解析的配置对象。
pub struct ConfigSource<'a> {
    pub environment: &'a dyn Fn(&str) -> Option<String>,
    pub data: &'a Value,
}

/// 解析后的飞书连接器配置（不保留任何应隐藏的凭证副本）。
#[derive(Debug, Clone, PartialEq)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    pub allowed_user_ids: BTreeSet<String>,
    pub confirmation_timeout_seconds: f64,
}

impl FeishuConfig {
    /// 空白白名单或包含 `*` 时为公开访问模式（启动时告警）。
    pub fn public_access(&self) -> bool {
        self.allowed_user_ids.is_empty() || self.allowed_user_ids.contains("*")
    }

    /// 是否授权该 open_id 操作。
    pub fn allows(&self, open_id: &str) -> bool {
        self.public_access() || (!open_id.is_empty() && self.allowed_user_ids.contains(open_id))
    }
}

/// 按环境变量优先、`[feishu]` 次之的规则加载配置。
pub fn load_feishu_config(source: ConfigSource<'_>) -> Result<FeishuConfig, String> {
    let environment = source.environment;
    let data = source.data;
    let section = match data.get("feishu") {
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err("config.toml 的 [feishu] 必须是对象。".to_string()),
    };

    let app_id = first_nonempty(&[
        environment("FEISHU_APP_ID"),
        lookup(&section, "app_id"),
        lookup(&section, "fs_app_id"),
        lookup(data, "fs_app_id"),
    ])
    .trim()
    .to_string();
    let app_secret = first_nonempty(&[
        environment("FEISHU_APP_SECRET"),
        lookup(&section, "app_secret"),
        lookup(&section, "fs_app_secret"),
        lookup(data, "fs_app_secret"),
    ])
    .trim()
    .to_string();

    let raw_allowed = first_nonempty_value(&[
        environment("FEISHU_ALLOWED_USER_IDS").map(Value::String),
        config_value(&section, "allowed_user_ids"),
        config_value(&section, "allowed_users"),
        config_value(&section, "fs_allowed_users"),
        config_value(data, "fs_allowed_users"),
    ]);
    let allowed_user_ids = coerce_string_set(raw_allowed.as_ref());

    let raw_timeout = first_nonempty_value(&[
        environment("FEISHU_CONFIRM_TIMEOUT").map(Value::String),
        config_value(&section, "confirmation_timeout_seconds"),
        config_value(&section, "confirm_timeout_seconds"),
        Some(json!(300)),
    ]);
    let confirmation_timeout_seconds = parse_seconds(raw_timeout.as_ref())?;

    Ok(FeishuConfig {
        app_id,
        app_secret,
        allowed_user_ids,
        confirmation_timeout_seconds,
    })
}

/// Secret 掩码：短于 8 位全星号，否则保留前 4 后 4。
pub fn mask_secret(value: &str) -> String {
    let length = value.chars().count();
    if length <= 8 {
        return "*".repeat(length);
    }
    let head: String = value.chars().take(4).collect();
    let tail: String = value.chars().skip(length - 4).collect();
    format!("{head}{}{tail}", "*".repeat(length - 8))
}

/// 不泄露 Secret 的配置诊断信息（Agent 探测由宿主的诊断入口补充）。
pub fn check_config(config: &FeishuConfig) -> Value {
    json!({
        "app_id": config.app_id,
        "app_secret": mask_secret(&config.app_secret),
        "app_secret_present": !config.app_secret.is_empty(),
        "allowed_users": config.allowed_user_ids.iter().cloned().collect::<Vec<String>>(),
        "public_access": config.public_access(),
        "confirmation_timeout_seconds": config.confirmation_timeout_seconds,
        "ready": !config.app_id.is_empty() && !config.app_secret.is_empty(),
    })
}

fn lookup(section: &Value, key: &str) -> Option<String> {
    section.get(key).and_then(value_as_string)
}

fn config_value(section: &Value, key: &str) -> Option<Value> {
    section.get(key).cloned()
}

/// `_first_nonempty` 的等价实现：字符串取去空白后的非空值，其他类型非 null 即取。
pub fn first_nonempty(values: &[Option<String>]) -> String {
    for value in values.iter().flatten() {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

fn first_nonempty_value(values: &[Option<Value>]) -> Option<Value> {
    for value in values.iter().flatten() {
        match value {
            Value::String(text) => {
                if !text.trim().is_empty() {
                    return Some(value.clone());
                }
            }
            Value::Null => {}
            other => return Some(other.clone()),
        }
    }
    None
}

/// 兼容 TOML 数组、逗号字符串与单个值，并去掉每项的空白。
pub fn coerce_string_set(value: Option<&Value>) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let Some(value) = value else { return result };
    match value {
        Value::String(text) => {
            for item in text.replace('，', ",").split(',') {
                let item = item.trim();
                if !item.is_empty() {
                    result.insert(item.to_string());
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                let text = value_as_text(item);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    result.insert(trimmed.to_string());
                }
            }
        }
        Value::Null => {}
        other => {
            let text = value_as_text(other);
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                result.insert(trimmed.to_string());
            }
        }
    }
    result
}

fn parse_seconds(value: Option<&Value>) -> Result<f64, String> {
    let parsed = value.and_then(|raw| match raw {
        Value::Number(number) => number.as_f64(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    });
    let seconds = parsed.ok_or_else(|| {
        "FEISHU_CONFIRM_TIMEOUT / feishu.confirmation_timeout_seconds 必须是数字。".to_string()
    })?;
    Ok(if seconds > 1.0 { seconds } else { 1.0 })
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(value_as_text(other)),
    }
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn environment_wins_and_aliases_are_accepted() {
        let data = json!({
            "feishu": {"app_id": "cli_section", "allowed_users": ["ou_a"]},
            "fs_app_secret": "root-secret",
        });
        let env = environment(&[("FEISHU_APP_ID", " cli_env ")]);
        let config = load_feishu_config(ConfigSource {
            environment: &env,
            data: &data,
        })
        .expect("配置可解析");
        assert_eq!(config.app_id, "cli_env");
        assert_eq!(config.app_secret, "root-secret");
        assert!(config.allowed_user_ids.contains("ou_a"));
        assert_eq!(config.confirmation_timeout_seconds, 300.0);
    }

    #[test]
    fn empty_whitelist_is_public_access() {
        let data = json!({"feishu": {"app_id": "a", "app_secret": "b"}});
        let env = environment(&[]);
        let config = load_feishu_config(ConfigSource {
            environment: &env,
            data: &data,
        })
        .expect("配置可解析");
        assert!(config.public_access());
        assert!(config.allows("ou_any"));
    }

    #[test]
    fn star_whitelist_is_public_access_and_others_are_checked() {
        let data = json!({"feishu": {"allowed_user_ids": ["ou_x"]}});
        let env = environment(&[]);
        let config = load_feishu_config(ConfigSource {
            environment: &env,
            data: &data,
        })
        .expect("配置可解析");
        assert!(config.allows("ou_x"));
        assert!(!config.allows("ou_y"));
        assert!(!config.allows(""));
    }

    #[test]
    fn section_must_be_object() {
        let data = json!({"feishu": "cli_x"});
        let env = environment(&[]);
        let error = load_feishu_config(ConfigSource {
            environment: &env,
            data: &data,
        })
        .expect_err("应报错");
        assert_eq!(error, "config.toml 的 [feishu] 必须是对象。");
    }

    #[test]
    fn mask_secret_hides_middle() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("12345678"), "********");
        assert_eq!(mask_secret("123456789"), "1234*6789");
    }
}
