//! QQ 机器人接入配置解析。
//!
//! 凭据只读 `config.toml` 的 `[qqbot]` 段（段对象由宿主配置层解析后传入，TOML 读取不属于
//! 本 crate）。`app_id` / `app_secret` 可在 QQ 开放平台管理端复制，也可以用
//! `omnicrawl-host connector qqbot --bind` 扫码直接写入。

use std::collections::BTreeSet;

use serde_json::{json, Value};

/// 工具确认超时默认秒数。
pub const DEFAULT_CONFIRM_TIMEOUT_SECONDS: f64 = 300.0;

/// 正式环境 API 基座。
pub const DEFAULT_BASE_URL: &str = "https://api.sgroup.qq.com";

/// 沙箱环境 API 基座（不受频率限制，适合调试）。
pub const SANDBOX_BASE_URL: &str = "https://sandbox.api.sgroup.qq.com";

/// AccessToken 端点。
pub const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// `GROUP_AND_C2C_EVENT`：单聊消息与群内 @机器人。
const INTENT_GROUP_AND_C2C: i64 = 1 << 25;
/// `PUBLIC_GUILD_MESSAGES`：频道内 @机器人（公域）。
const INTENT_PUBLIC_GUILD_MESSAGES: i64 = 1 << 30;

/// 默认订阅：单聊 + 群聊 + 频道公域消息。
pub const DEFAULT_INTENTS: i64 = INTENT_GROUP_AND_C2C | INTENT_PUBLIC_GUILD_MESSAGES;

/// 解析后的 QQ 机器人接入配置（不保留任何应隐藏的凭证副本以外的东西）。
#[derive(Debug, Clone, PartialEq)]
pub struct QqBotConfig {
    pub app_id: String,
    pub app_secret: String,
    /// 允许操作机器人的用户 openid；空集或包含 `*` 时为公开访问模式。
    pub allowed_user_ids: BTreeSet<String>,
    /// 允许在群聊里操作机器人的群 openid；空集时不按群判定（沿用用户白名单）。
    pub allowed_group_ids: BTreeSet<String>,
    pub confirm_timeout_seconds: f64,
    /// 沙箱环境。
    pub sandbox: bool,
    /// 事件订阅位掩码。
    pub intents: i64,
}

/// 一次消息的来源，决定按哪张白名单判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessScope<'a> {
    /// 单聊与频道：按发起者 id 判定。
    User,
    /// 群聊：配了群白名单就按群判定，否则沿用发起者 id。
    Group { group_id: &'a str },
}

/// 访问白名单（用户 + 群），按来源分别判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPolicy {
    users: BTreeSet<String>,
    groups: BTreeSet<String>,
}

impl AccessPolicy {
    /// 从配置取两张白名单。
    pub fn new(users: &BTreeSet<String>, groups: &BTreeSet<String>) -> AccessPolicy {
        AccessPolicy {
            users: users.clone(),
            groups: groups.clone(),
        }
    }

    /// 空用户白名单或包含 `*`：任何人可操作（启动时告警）。
    pub fn public_users(&self) -> bool {
        self.users.is_empty() || self.users.contains("*")
    }

    /// 是否配了群白名单：配了才把群聊的判定依据换成群 id。
    pub fn scoped_to_groups(&self) -> bool {
        !self.groups.is_empty()
    }

    /// 是否授权这次消息。
    pub fn allows(&self, scope: AccessScope<'_>, sender: &str) -> bool {
        if self.public_users() {
            return true;
        }
        match scope {
            AccessScope::Group { group_id } => {
                if self.scoped_to_groups() {
                    !group_id.is_empty() && self.groups.contains(group_id)
                } else {
                    self.allows_sender(sender)
                }
            }
            AccessScope::User => self.allows_sender(sender),
        }
    }

    /// 单聊与频道共用：按发起者 id 查用户白名单。
    pub fn allows_sender(&self, sender: &str) -> bool {
        self.public_users() || (!sender.is_empty() && self.users.contains(sender))
    }

    /// 启动告警用的描述：None 表示无需告警。
    pub fn startup_warning(&self) -> Option<&'static str> {
        if self.public_users() {
            Some("allowed_user_ids 为空，当前为公开访问模式：任何能给机器人发消息的用户都可操作。")
        } else if self.scoped_to_groups() {
            Some("已启用 allowed_group_ids：群聊按群 openid 放行，单聊与频道仍按 allowed_user_ids 判定。")
        } else {
            None
        }
    }

    /// 未授权日志里的判定说明。
    pub fn denial_hint(&self, scope: AccessScope<'_>) -> &'static str {
        match scope {
            AccessScope::Group { .. } if self.scoped_to_groups() => "不在 allowed_group_ids",
            AccessScope::Group { .. } => "不在 allowed_user_ids（未配置 allowed_group_ids）",
            AccessScope::User => "不在 allowed_user_ids",
        }
    }
}

impl QqBotConfig {
    /// 按来源判定的访问策略。
    pub fn access(&self) -> AccessPolicy {
        AccessPolicy::new(&self.allowed_user_ids, &self.allowed_group_ids)
    }

    /// 空白名单或包含 `*` 时为公开访问模式（启动时告警）。
    pub fn public_access(&self) -> bool {
        self.access().public_users()
    }

    /// 是否授权该 openid 操作（单聊口径）。
    pub fn allows(&self, openid: &str) -> bool {
        self.access().allows_sender(openid)
    }

    /// API 基座：沙箱与正式环境只差域名。
    pub fn base_url(&self) -> &'static str {
        if self.sandbox {
            SANDBOX_BASE_URL
        } else {
            DEFAULT_BASE_URL
        }
    }
}

/// 读取 `[qqbot]` 段；缺项回落默认值，类型不对返回中文错误。
pub fn load_qqbot_config(section: Option<&Value>) -> Result<QqBotConfig, String> {
    let section = match section {
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err("config.toml 的 [qqbot] 必须是对象。".to_string()),
    };

    let app_id = text_of(section.get("app_id"));
    let app_secret = text_of(section.get("app_secret"));
    let allowed_user_ids = coerce_string_set(section.get("allowed_user_ids"));
    let allowed_group_ids = match section.get("allowed_group_ids") {
        // 空表等价于空数组：`[qqbot.allowed_group_ids]` 这种写法常由配置工具生成。
        Some(Value::Object(map)) if map.is_empty() => BTreeSet::new(),
        Some(Value::Object(_)) => {
            return Err(
                "qqbot.allowed_group_ids 必须是群 openid 字符串数组（如 [\"A1B2\"]），不能写成表。"
                    .to_string(),
            )
        }
        other => coerce_string_set(other),
    };
    let confirm_timeout_seconds = match section.get("confirmation_timeout_seconds") {
        None | Some(Value::Null) => DEFAULT_CONFIRM_TIMEOUT_SECONDS,
        Some(raw) => parse_seconds(raw)?,
    };
    let sandbox = section
        .get("sandbox")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let intents = match section.get("intents") {
        None | Some(Value::Null) => DEFAULT_INTENTS,
        Some(raw) => parse_intents(raw)?,
    };

    Ok(QqBotConfig {
        app_id,
        app_secret,
        allowed_user_ids,
        allowed_group_ids,
        confirm_timeout_seconds,
        sandbox,
        intents,
    })
}

/// 启动前校验：AppID 与 AppSecret 必填。
pub fn validate_config(config: &QqBotConfig) -> Result<(), String> {
    if config.app_id.trim().is_empty() {
        return Err("缺少 QQ 机器人 AppID（config.toml 的 [qqbot] app_id）。".to_string());
    }
    if config.app_secret.trim().is_empty() {
        return Err("缺少 QQ 机器人 AppSecret（config.toml 的 [qqbot] app_secret）。".to_string());
    }
    Ok(())
}

/// 超时下限：与其它连接器一致，至少 1 秒。
pub fn effective_confirm_timeout(seconds: f64) -> f64 {
    if seconds > 1.0 {
        seconds
    } else {
        1.0
    }
}

/// 不泄露 Secret 的配置诊断信息。
pub fn check_config(config: &QqBotConfig) -> Value {
    json!({
        "app_id": config.app_id,
        "app_secret": mask_secret(&config.app_secret),
        "app_secret_present": !config.app_secret.is_empty(),
        "allowed_users": config.allowed_user_ids.iter().cloned().collect::<Vec<String>>(),
        "allowed_groups": config.allowed_group_ids.iter().cloned().collect::<Vec<String>>(),
        "public_access": config.public_access(),
        "confirmation_timeout_seconds": config.confirm_timeout_seconds,
        "sandbox": config.sandbox,
        "intents": config.intents,
        "ready": !config.app_id.is_empty() && !config.app_secret.is_empty(),
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

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// 兼容 TOML 数组、逗号字符串（含全角逗号）与单个值。
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
                let text = match item {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    _ => String::new(),
                };
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    result.insert(trimmed.to_string());
                }
            }
        }
        Value::Null => {}
        other => {
            let text = other.to_string();
            let trimmed = text.trim().trim_matches('"');
            if !trimmed.is_empty() {
                result.insert(trimmed.to_string());
            }
        }
    }
    result
}

fn parse_seconds(raw: &Value) -> Result<f64, String> {
    let parsed = match raw {
        Value::Number(number) => number.as_f64(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed.ok_or_else(|| "qqbot.confirmation_timeout_seconds 必须是数字。".to_string())
}

/// Intents 接受整数与数字字符串；负数或零视为配置错误。
fn parse_intents(raw: &Value) -> Result<i64, String> {
    let parsed = match raw {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    match parsed {
        Some(value) if value > 0 => Ok(value),
        _ => Err("qqbot.intents 必须是正整数（事件订阅位掩码）。".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn section_supplies_everything() {
        let section = json!({
            "app_id": " 102012345 ",
            "app_secret": "secret-value",
            "allowed_user_ids": ["F6A8B9C0D1E2"],
            "confirmation_timeout_seconds": 12,
            "sandbox": true,
            "intents": 513,
        });
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        assert_eq!(config.app_id, "102012345");
        assert_eq!(config.app_secret, "secret-value");
        assert!(config.allows("F6A8B9C0D1E2"));
        assert!(!config.allows("other"));
        assert_eq!(config.confirm_timeout_seconds, 12.0);
        assert_eq!(config.base_url(), SANDBOX_BASE_URL);
        assert_eq!(config.intents, 513);
    }

    #[test]
    fn empty_whitelist_is_public_access() {
        let config = load_qqbot_config(None).expect("配置可解析");
        assert!(config.public_access());
        assert!(config.allows("anyone"));
        assert_eq!(config.base_url(), DEFAULT_BASE_URL);
        assert_eq!(config.intents, DEFAULT_INTENTS);
        assert_eq!(config.confirm_timeout_seconds, 300.0);
    }

    #[test]
    fn comma_separated_ids_support_full_width_comma() {
        let section = json!({"allowed_user_ids": "a，b, c ,"});
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        assert_eq!(
            config.allowed_user_ids,
            ["a", "b", "c"]
                .iter()
                .map(|item| item.to_string())
                .collect()
        );
    }

    #[test]
    fn wildcard_whitelist_is_public_access() {
        let section = json!({"allowed_user_ids": ["*"]});
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        assert!(config.public_access());
    }

    #[test]
    fn missing_credentials_are_reported() {
        let config = load_qqbot_config(None).expect("配置可解析");
        assert_eq!(
            validate_config(&config).expect_err("缺 appid"),
            "缺少 QQ 机器人 AppID（config.toml 的 [qqbot] app_id）。"
        );
        let section = json!({"app_id": "1"});
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        assert!(validate_config(&config)
            .expect_err("缺 secret")
            .contains("AppSecret"));
    }

    #[test]
    fn invalid_values_report_hints() {
        let section = json!({"confirmation_timeout_seconds": "soon"});
        assert!(load_qqbot_config(Some(&section))
            .expect_err("应报错")
            .contains("必须是数字"));
        let section = json!({"intents": -1});
        assert!(load_qqbot_config(Some(&section))
            .expect_err("应报错")
            .contains("必须是正整数"));
        let section = json!("not-an-object");
        assert!(load_qqbot_config(Some(&section))
            .expect_err("应报错")
            .contains("必须是对象"));
    }

    #[test]
    fn secret_is_masked_without_leaking_middle() {
        assert_eq!(mask_secret("short"), "*****");
        assert_eq!(mask_secret("abcdefghijkl"), "abcd****ijkl");
        assert!(
            !check_config(&load_qqbot_config(None).unwrap())["app_secret"]
                .as_str()
                .unwrap()
                .contains("secret")
        );
    }

    #[test]
    fn group_whitelist_scopes_the_group_chat_only() {
        let section = json!({
            "allowed_user_ids": ["F6A8"],
            "allowed_group_ids": ["A1B2", "C3D4"],
        });
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        let access = config.access();
        assert!(access.scoped_to_groups());
        assert!(!access.public_users());
        // 群聊按群 openid 放行，不再看成员 openid。
        assert!(access.allows(AccessScope::Group { group_id: "A1B2" }, "STRANGER"));
        assert!(!access.allows(AccessScope::Group { group_id: "OTHER" }, "F6A8"));
        // 单聊仍按用户白名单。
        assert!(access.allows(AccessScope::User, "F6A8"));
        assert!(!access.allows(AccessScope::User, "STRANGER"));
        assert_eq!(config.allowed_group_ids, set(&["A1B2", "C3D4"]));
    }

    #[test]
    fn empty_group_whitelist_keeps_sender_based_group_check() {
        let section = json!({"allowed_user_ids": ["F6A8"]});
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        let access = config.access();
        assert!(!access.scoped_to_groups());
        assert!(access.allows(AccessScope::Group { group_id: "ANY" }, "F6A8"));
        assert!(!access.allows(AccessScope::Group { group_id: "ANY" }, "STRANGER"));
    }

    #[test]
    fn public_users_still_opens_groups() {
        let section = json!({"allowed_user_ids": ["*"], "allowed_group_ids": ["A1B2"]});
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        let access = config.access();
        assert!(access.public_users());
        assert!(access.allows(AccessScope::Group { group_id: "OTHER" }, "anyone"));
        assert!(access.allows(AccessScope::User, "anyone"));
        assert!(access
            .startup_warning()
            .expect("应告警")
            .contains("公开访问"));
    }

    #[test]
    fn startup_warning_explains_active_policy() {
        let limited =
            load_qqbot_config(Some(&json!({"allowed_user_ids": ["F6A8"]}))).expect("配置可解析");
        assert!(limited.access().startup_warning().is_none());
        let scoped = load_qqbot_config(Some(&json!({
            "allowed_user_ids": ["F6A8"],
            "allowed_group_ids": ["A1B2"],
        })))
        .expect("配置可解析");
        assert!(scoped
            .access()
            .startup_warning()
            .expect("应提示群白名单")
            .contains("allowed_group_ids"));
        assert_eq!(
            scoped
                .access()
                .denial_hint(AccessScope::Group { group_id: "A1B2" }),
            "不在 allowed_group_ids"
        );
        assert_eq!(
            scoped.access().denial_hint(AccessScope::User),
            "不在 allowed_user_ids"
        );
        assert_eq!(
            limited
                .access()
                .denial_hint(AccessScope::Group { group_id: "A1B2" }),
            "不在 allowed_user_ids（未配置 allowed_group_ids）"
        );
    }

    #[test]
    fn check_config_exposes_group_whitelist() {
        let section = json!({
            "allowed_user_ids": ["F6A8"],
            "allowed_group_ids": ["A1B2"],
        });
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        let checked = check_config(&config);
        assert_eq!(checked["allowed_groups"], json!(["A1B2"]));
        assert_eq!(checked["public_access"], json!(false));
    }

    #[test]
    fn empty_table_counts_as_an_empty_group_whitelist() {
        // 配置工具把空数组落成 `[qqbot.allowed_group_ids]` 时不应解析成一个名为 {} 的群。
        let section = json!({"allowed_user_ids": ["F6A8"], "allowed_group_ids": {}});
        let config = load_qqbot_config(Some(&section)).expect("配置可解析");
        assert!(config.allowed_group_ids.is_empty());
        assert!(!config.access().scoped_to_groups());
        let section = json!({"allowed_group_ids": {"A1B2": true}});
        assert!(load_qqbot_config(Some(&section))
            .expect_err("表应报错")
            .contains("字符串数组"));
    }

    #[test]
    fn timeout_floor_is_one_second() {
        assert_eq!(effective_confirm_timeout(0.2), 1.0);
        assert_eq!(effective_confirm_timeout(300.0), 300.0);
    }
}
