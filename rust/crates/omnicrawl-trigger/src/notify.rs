//! 终态投递：把结论发到消息平台。
//!
//! 模型没有「发消息」工具——投递由服务在终态之后做，因此不存在「模型忘了发」。凭据一律复用
//! 已有连接器配置（`[telegram]` / `[feishu]`），触发接口不新开一份凭据。
//!
//! Telegram 与飞书能主动投递，QQ 这条通道暂未接入：
//!
//! * Telegram `sendMessage` 只要 `chat_id` 就能主动发；
//! * 飞书 `im/v1/messages` 用 `receive_id` 主动发；
//! * QQ 机器人也支持主动消息（不带 `msg_id`），但需要机器人具备主动消息能力
//!   （否则 `40034105`），并受 Bot 维度 30/60 qpm、单关系 20 qpm、每会话每天 1000 条
//!   的限制，用户还可以在客户端关闭接收。触发接口暂未对接这条通道，`type = "qqbot"`
//!   在投递时直接拒绝并说明原因。
//!
//! Telegram 的 `chat_id` 必须落在 `[telegram] allowed_user_ids` 内：本机任何程序都能调这个
//! 无鉴权接口，不能让它变成「给任意人发任意消息」的跳板。

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_connectors::feishu::{ConfigSource, FeishuApi, load_feishu_config};
use omnicrawl_connectors::telegram::{
    MAX_MESSAGE_LEN, TelegramApi, load_telegram_config, split_message,
};
use serde_json::json;

use crate::records::{MAX_RESULT_CHARS, NotifySpec, TriggerState};

/// 投递正文里附在标题后的分隔。
const SEPARATOR: &str = "\n\n---\n\n";

/// 按 `notify` 投递终态正文。
///
/// `Ok(())` 表示已交给平台；`Err(reason)` 给可读原因（写进 `notify.error`，**不改**触发状态）。
pub fn deliver(
    env: &ConfigEnvironment,
    spec: &NotifySpec,
    title: &str,
    state: TriggerState,
    result: &str,
    error: &str,
) -> Result<(), String> {
    let text = compose_text(spec, title, state, result, error);
    match spec.channel.as_str() {
        "telegram" => deliver_telegram(env, spec, &text),
        "feishu" => deliver_feishu(env, spec, &text),
        "qqbot" => Err(
            "QQ 机器人暂未接入主动投递：主动消息需要机器人具备相应权限，\
             且受每分钟条数限制。请改用 telegram 或 feishu 通道。"
                .to_string(),
        ),
        // `none` 在 `NotifySpec::matches` 里已经返回假，走不到这里；未知通道同样按不支持处理。
        other => Err(format!("不支持的投递通道：{other}")),
    }
}

/// 投递正文：标题行 + 状态行 + 正文（按 `max_chars` 截断）。
pub fn compose_text(
    spec: &NotifySpec,
    title: &str,
    state: TriggerState,
    result: &str,
    error: &str,
) -> String {
    let body = match state {
        TriggerState::Completed => result.trim(),
        _ => {
            let reason = error.trim();
            if reason.is_empty() {
                "触发未完成。"
            } else {
                reason
            }
        }
    };
    let mut text = String::new();
    if !title.trim().is_empty() {
        text.push_str(title.trim());
        text.push_str(SEPARATOR);
    }
    if state != TriggerState::Completed {
        text.push_str(&format!("状态：{}\n", state.as_str()));
    }
    text.push_str(body);
    truncate_for_notify(&text, spec.max_chars)
}

/// 按上限截断：先按投递上限，再按结果上限兜一层（防止构造出巨型消息）。
fn truncate_for_notify(text: &str, limit: usize) -> String {
    let limit = limit.clamp(1, MAX_RESULT_CHARS);
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n\n[已截断：完整内容见触发记录]")
}

fn deliver_telegram(env: &ConfigEnvironment, spec: &NotifySpec, text: &str) -> Result<(), String> {
    let data = crate::config::config_data(env);
    let config = load_telegram_config(data.get("telegram"))?;
    if config.bot_token.trim().is_empty() {
        return Err("未配置 [telegram] bot_token，无法投递。".to_string());
    }
    let chat_id: i64 = spec
        .target
        .trim()
        .parse()
        .map_err(|_| format!("telegram 的 chat_id 必须是整数：{}", spec.target))?;
    // 白名单是唯一准入控制：无鉴权的本机接口不能成为「给任意人发消息」的跳板。
    if !config.allowed_user_ids.contains(&chat_id) {
        return Err(format!(
            "telegram chat_id {chat_id} 不在 [telegram] allowed_user_ids 内，已拒绝投递。"
        ));
    }
    let api = TelegramApi::with_ureq(&config.bot_token);
    // 不用 `send_message`：它把平台错误吞成日志，而投递失败必须回到 `notify.error`。
    for part in split_message(text, MAX_MESSAGE_LEN) {
        let form = vec![
            ("chat_id".to_string(), chat_id.to_string()),
            ("text".to_string(), part),
        ];
        api.call("sendMessage", &[], &form)
            .map_err(|error| format!("telegram 投递失败：{error:?}"))?;
    }
    Ok(())
}

fn deliver_feishu(env: &ConfigEnvironment, spec: &NotifySpec, text: &str) -> Result<(), String> {
    let data = crate::config::config_data(env);
    let config = load_feishu_config(ConfigSource { data: &data })?;
    if config.app_id.trim().is_empty() || config.app_secret.trim().is_empty() {
        return Err("未配置 [feishu] app_id / app_secret，无法投递。".to_string());
    }
    if !config.allows(&spec.target) {
        return Err(format!(
            "飞书 receive_id {} 不在 [feishu] allowed_user_ids 内，已拒绝投递。",
            spec.target
        ));
    }
    // open_id 以 `ou_` 开头，chat_id 以 `oc_` 开头：据此选 receive_id_type，避免把群 ID
    // 当用户 ID 发出去（平台会回 400 而不是投递成功）。
    let receive_id_type = if spec.target.starts_with("oc_") {
        "chat_id"
    } else {
        "open_id"
    };
    let api = FeishuApi::with_ureq(&config.app_id, &config.app_secret);
    let content = json!({"text": text}).to_string();
    match api.send_message(&spec.target, &content, "text", receive_id_type) {
        Some(_) => Ok(()),
        None => Err("飞书投递失败：平台未返回 message_id（详见服务日志）。".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(max_chars: usize) -> NotifySpec {
        NotifySpec {
            channel: "telegram".to_string(),
            target: "1".to_string(),
            on: "completed".to_string(),
            max_chars,
        }
    }

    #[test]
    fn completed_text_is_title_then_result() {
        let text = compose_text(
            &spec(4000),
            "每日新闻",
            TriggerState::Completed,
            "今天有三条重要新闻。",
            "",
        );
        assert!(text.starts_with("每日新闻"));
        assert!(text.ends_with("今天有三条重要新闻。"));
        assert!(!text.contains("状态："));
    }

    #[test]
    fn failed_text_carries_state_and_reason() {
        let text = compose_text(
            &spec(4000),
            "土壤告警",
            TriggerState::Failed,
            "",
            "回合超时（600 秒）已取消。",
        );
        assert!(text.contains("状态：failed"));
        assert!(text.contains("回合超时"));
    }

    #[test]
    fn empty_reason_does_not_produce_empty_message() {
        let text = compose_text(&spec(4000), "标题", TriggerState::Failed, "", "   ");
        assert!(text.contains("触发未完成。"));
    }

    #[test]
    fn long_body_is_truncated_with_a_marker() {
        let body = "字".repeat(5000);
        let text = compose_text(&spec(200), "标题", TriggerState::Completed, &body, "");
        assert!(text.chars().count() <= 200 + 32);
        assert!(text.ends_with("[已截断：完整内容见触发记录]"));
    }

    #[test]
    fn qqbot_is_rejected_until_active_delivery_is_supported() {
        let env = ConfigEnvironment::new("C:/oc-trigger-home", "win32");
        let spec = NotifySpec {
            channel: "qqbot".to_string(),
            target: "openid".to_string(),
            on: "completed".to_string(),
            max_chars: 4000,
        };
        let error = deliver(&env, &spec, "标题", TriggerState::Completed, "正文", "")
            .expect_err("QQ 主动投递暂未接入");
        assert!(error.contains("主动投递"), "{error}");
    }
}
