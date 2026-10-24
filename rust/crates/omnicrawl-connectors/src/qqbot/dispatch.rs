//! QQ 事件解析与路由：把网关事件变成「谁、在哪里、说了什么」。
//!
//! 三个入口场景共用一套判定：单聊 `C2C_MESSAGE_CREATE`（`author.user_openid`）、
//! 群聊 `GROUP_AT_MESSAGE_CREATE`（`author.member_openid` + `group_openid`）、
//! 频道 `AT_MESSAGE_CREATE`（`author.id` + `channel_id`）。白名单按事件里出现的
//! openid / 用户 id 判定——不同 AppID 下同一用户的 openid 不同，因此白名单是「按机器人」的。
//!
//! 斜杠命令的分类与参数解析直接复用 Telegram 那份（`telegram::dispatch`）：
//! 两者都是「命令词 + 文本」的同一套语义，没必要各写一份。

use serde_json::Value;

pub use crate::telegram::dispatch::{
    classify, command_name, parse_thinking_command, workspace_argument, Command, ThinkingCommand,
};

use super::api::Target;
use super::config::{AccessPolicy, AccessScope};

/// 单聊消息事件。
pub const EVENT_C2C_MESSAGE: &str = "C2C_MESSAGE_CREATE";
/// 群内 @机器人 事件。
pub const EVENT_GROUP_AT_MESSAGE: &str = "GROUP_AT_MESSAGE_CREATE";
/// 群内全量消息事件：群开启「接收所有消息」后，每条消息（含 @机器人）都由它推送。
pub const EVENT_GROUP_MESSAGE: &str = "GROUP_MESSAGE_CREATE";
/// 频道内 @机器人 事件。
pub const EVENT_GUILD_AT_MESSAGE: &str = "AT_MESSAGE_CREATE";

/// 一条入站消息的落点与身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// 被动回复的目标（单聊用户 / 群）。
    pub target: Target,
    /// 发送者 openid（白名单与审批归属都看它）。
    pub sender: String,
    /// 发送者昵称：只用于日志辨认，平台可能不给。
    pub sender_name: String,
    /// 平台消息 ID：被动回复必须回传它。
    pub message_id: String,
    pub content: String,
    /// 附件 URL 列表（`(url, 建议文件名)`）。
    pub attachments: Vec<(String, String)>,
    /// 事件名，用于日志与区分群聊/单聊的回复窗口。
    pub event: String,
}

impl Inbound {
    /// 群聊会话键：同一群的多个话题共享一个会话（QQ 群没有话题概念）。
    pub fn conversation_key(&self) -> String {
        match &self.target {
            Target::User(openid) => format!("c2c:{openid}"),
            Target::Group(group) => format!("group:{group}"),
            Target::Channel(channel) => format!("channel:{channel}"),
        }
    }
}

/// 路由结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventRoute {
    /// 非消息事件（加好友、被拉群等）：只记日志。
    NotAMessage {
        event: String,
        /// 事件里能读到的会话键（`group:<群 openid>`），读不到时为空串。
        conversation: String,
    },
    /// 群全量消息里没有 @机器人：不响应，仅日志。
    NotAddressed {
        event: String,
        /// 会话键（`group:<群 openid>`），便于确认发生在哪个群。
        conversation: String,
        /// 诊断：正文与 mentions 的摘要，用于核对 @ 的真实写法。
        detail: String,
    },
    /// 缺字段：静默忽略。
    Malformed { event: String },
    /// 非白名单用户：不回复，仅日志。
    Unauthorized {
        sender: String,
        /// 昵称（可能为空），便于从日志认出是谁在申请。
        sender_name: String,
        /// 会话键（`group:<群 openid>` 等），便于认出发生在哪个群。
        conversation: String,
        /// 判定说明：命中的是哪张白名单。
        hint: &'static str,
        event: String,
    },
    /// 一条可处理的消息。
    Message(Inbound),
}

/// 机器人自身身份：识别群内 @ 需要用到。
#[derive(Debug, Clone, Copy, Default)]
pub struct BotIdentity<'a> {
    /// 机器人 openid（`READY.user.id` 或 `/users/@me` 的 `id`）。
    pub openid: &'a str,
    /// 机器人昵称：平台在全量群消息里把 @ 渲染成 `@昵称` 文本。
    pub name: &'a str,
}

/// 解析一帧事件推送。
pub fn route_event(event: &str, data: &Value, access: &AccessPolicy) -> EventRoute {
    route_event_with_bot(event, data, access, BotIdentity::default())
}

/// 同上，但提供机器人自身身份：`GROUP_MESSAGE_CREATE` 靠它判断有没有 @ 到机器人。
pub fn route_event_with_bot(
    event: &str,
    data: &Value,
    access: &AccessPolicy,
    bot: BotIdentity<'_>,
) -> EventRoute {
    let event_name = event.to_string();
    if !matches!(
        event,
        EVENT_C2C_MESSAGE | EVENT_GROUP_AT_MESSAGE | EVENT_GROUP_MESSAGE | EVENT_GUILD_AT_MESSAGE
    ) {
        return EventRoute::NotAMessage {
            event: event_name,
            conversation: conversation_of(data),
        };
    }
    // 全量模式会把群里每句话都推过来，只有 @ 到机器人的才当指令。
    if event == EVENT_GROUP_MESSAGE && !addressed_to_bot(data, bot) {
        return EventRoute::NotAddressed {
            event: event_name,
            conversation: conversation_of(data),
            detail: mention_digest(data, bot),
        };
    }
    let Some(inbound) = parse_message_with_bot(event, data, bot) else {
        return EventRoute::Malformed { event: event_name };
    };
    if !allowed_message(access, &inbound) {
        let conversation = inbound.conversation_key();
        let hint = access.denial_hint(scope_of(&inbound));
        return EventRoute::Unauthorized {
            sender: inbound.sender,
            sender_name: inbound.sender_name,
            conversation,
            hint,
            event: event_name,
        };
    }
    EventRoute::Message(inbound)
}

/// 这条消息的判定来源：群聊看群，单聊与频道看发起者。
fn scope_of(inbound: &Inbound) -> AccessScope<'_> {
    match &inbound.target {
        Target::Group(group) => AccessScope::Group { group_id: group },
        Target::User(_) | Target::Channel(_) => AccessScope::User,
    }
}

fn allowed_message(access: &AccessPolicy, inbound: &Inbound) -> bool {
    access.allows(scope_of(inbound), &inbound.sender)
}

/// 按事件类型取字段；缺必需字段返回 None。
pub fn parse_message(event: &str, data: &Value) -> Option<Inbound> {
    parse_message_with_bot(event, data, BotIdentity::default())
}

/// 同上，但提供机器人身份：全量群消息要据此剥掉 `@昵称` 前缀。
pub fn parse_message_with_bot(event: &str, data: &Value, bot: BotIdentity<'_>) -> Option<Inbound> {
    let message_id = string_of(data.get("id")).unwrap_or_default();
    let raw_content = data
        .get("content")
        .map(|value| value.as_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let content = raw_content.trim().to_string();
    let attachments = attachments_of(data);
    match event {
        EVENT_C2C_MESSAGE => {
            let openid = string_of(
                data.get("author")
                    .and_then(|author| author.get("user_openid")),
            )?;
            if openid.is_empty() {
                return None;
            }
            Some(Inbound {
                target: Target::User(openid.clone()),
                sender: openid,
                sender_name: sender_name_of(data),
                message_id,
                content,
                attachments,
                event: event.to_string(),
            })
        }
        EVENT_GROUP_AT_MESSAGE | EVENT_GROUP_MESSAGE => {
            let group = string_of(data.get("group_openid"))?;
            let member = string_of(
                data.get("author")
                    .and_then(|author| author.get("member_openid")),
            )
            .unwrap_or_default();
            if group.is_empty() || member.is_empty() {
                return None;
            }
            // 全量模式的正文自带 `@机器人`（文本或占位符）；@ 事件那边平台已去掉。
            let content = if event == EVENT_GROUP_MESSAGE {
                strip_bot_mention(&content, bot)
            } else {
                content
            };
            Some(Inbound {
                target: Target::Group(group),
                sender: member,
                sender_name: sender_name_of(data),
                message_id,
                content,
                attachments,
                event: event.to_string(),
            })
        }
        EVENT_GUILD_AT_MESSAGE => {
            // 频道消息没有单聊/群聊的加锁路径：用「回复窗口」同一套时序，落点按子频道走。
            let channel = string_of(data.get("channel_id"))?;
            let author = string_of(data.get("author").and_then(|author| author.get("id")))?;
            if channel.is_empty() || author.is_empty() {
                return None;
            }
            Some(Inbound {
                target: Target::Channel(channel),
                sender: author,
                sender_name: sender_name_of(data),
                message_id,
                content: strip_mention(&content),
                attachments,
                event: event.to_string(),
            })
        }
        _ => None,
    }
}

/// 全量模式的群消息是否 @ 了机器人。
///
/// 实测（2026-10-24 真实报文）：平台把 @ 渲染成 `@昵称` 普通文本，`mentions` 为空数组，
/// 也没有文档所说的 `<@openid>` 占位符。因此昵称匹配是主判据：
/// - 正文以 `@昵称` 开头（或包含 `@昵称` 后紧跟空白/结尾/标点）即算 @；
/// - 另外兼容 `mentions[]` 里指向机器人自身（或 `bot: true`）与 `<@openid>` 占位符，
///   以防平台在某些群/版本上改回结构化写法。
fn addressed_to_bot(data: &Value, bot: BotIdentity<'_>) -> bool {
    if let Some(list) = data.get("mentions").and_then(Value::as_array) {
        for item in list {
            let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let member = item
                .get("member_openid")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let same = !bot.openid.is_empty() && (id == bot.openid || member == bot.openid);
            if same || item.get("bot").and_then(Value::as_bool).unwrap_or(false) {
                return true;
            }
        }
    }
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !bot.openid.is_empty()
        && (content.contains(&format!("<@{}>", bot.openid))
            || content.contains(&format!("<@!{}>", bot.openid)))
    {
        return true;
    }
    if bot.name.is_empty() {
        return false;
    }
    mentions_name(content, bot.name)
}

/// 正文是否以 `@昵称` 形式 @ 了目标。要求昵称后面紧跟分隔符，避免 `@小囫囵的猫` 误判。
fn mentions_name(content: &str, name: &str) -> bool {
    let needle = format!("@{name}");
    let mut rest = content;
    while let Some(index) = rest.find(&needle) {
        let after = &rest[index + needle.len()..];
        let boundary = match after.chars().next() {
            None => true,
            Some(char) => {
                char.is_whitespace()
                    || char == '\u{3000}'
                    || matches!(char, ',' | '.' | '!' | '?' | ':' | ';' | '，' | '。' | '！' | '？' | '：' | '；' | '、')
            }
        };
        if boundary {
            return true;
        }
        rest = &after[..];
    }
    false
}

/// 去掉 @机器人 的文本/占位符前缀。
fn strip_bot_mention(content: &str, bot: BotIdentity<'_>) -> String {
    let mut rest = strip_mention(content);
    if !bot.openid.is_empty() {
        for prefix in [format!("<@{}>", bot.openid), format!("<@!{}>", bot.openid)] {
            if let Some(tail) = rest.strip_prefix(&prefix) {
                rest = tail.trim_start().to_string();
            }
        }
    }
    if !bot.name.is_empty() {
        let needle = format!("@{}", bot.name);
        let mut current = rest.clone();
        while let Some(index) = current.find(&needle) {
            let after = &current[index + needle.len()..];
            let boundary = match after.chars().next() {
                None => true,
                Some(char) => char.is_whitespace() || char == '\u{3000}',
            };
            if boundary {
                rest = after.trim_start().to_string();
                break;
            }
            current = after.to_string();
        }
    }
    rest.trim().to_string()
}

/// 未 @ 机器人的群消息诊断串：正文前缀 + mentions 各条目的 id/member_openid/bot。
fn mention_digest(data: &Value, bot: BotIdentity<'_>) -> String {
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let head: String = content.chars().take(40).collect();
    let mut mentions = Vec::new();
    if let Some(list) = data.get("mentions").and_then(Value::as_array) {
        for item in list {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let member = item
                .get("member_openid")
                .and_then(Value::as_str)
                .unwrap_or("");
            let bot_flag = item.get("bot").and_then(Value::as_bool).unwrap_or(false);
            mentions.push(format!("id={id}/member={member}/bot={bot_flag}"));
        }
    }
    format!(
        "正文『{head}』，mentions=[{}]，机器人 openid={}、昵称={}",
        mentions.join("; "),
        if bot.openid.is_empty() {
            "未知"
        } else {
            bot.openid
        },
        if bot.name.is_empty() { "未知" } else { bot.name }
    )
}

/// 去掉正文里的 `<@!id>` 形式的前置 @（频道消息正文自带）。
pub fn strip_mention(content: &str) -> String {
    let mut rest = content.trim_start();
    while rest.starts_with("<@") {
        match rest.find('>') {
            Some(index) => rest = rest[index + 1..].trim_start(),
            None => break,
        }
    }
    rest.trim().to_string()
}

/// 附件：`attachments[].url` 与 `filename`（图片可能没有文件名）。
pub fn attachments_of(data: &Value) -> Vec<(String, String)> {
    let mut items = Vec::new();
    if let Some(list) = data.get("attachments").and_then(Value::as_array) {
        for item in list {
            let url = match item.get("url").and_then(Value::as_str) {
                Some(url) if !url.trim().is_empty() => url.trim().to_string(),
                _ => continue,
            };
            let name = item
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            items.push((url, name));
        }
    }
    items
}

fn string_of(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

/// 发送者昵称（`author.username`）：平台可能不给，缺失时为空串。
fn sender_name_of(data: &Value) -> String {
    data.get("author")
        .and_then(|author| author.get("username"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 非消息事件里能拼出的会话键：优先群，其次频道，再次单聊；都没有时为空串。
fn conversation_of(data: &Value) -> String {
    if let Some(group) = string_of(data.get("group_openid")) {
        if !group.is_empty() {
            return format!("group:{group}");
        }
    }
    if let Some(channel) = string_of(data.get("channel_id")) {
        if !channel.is_empty() {
            return format!("channel:{channel}");
        }
    }
    if let Some(openid) = string_of(
        data.get("author")
            .and_then(|author| author.get("user_openid")),
    ) {
        if !openid.is_empty() {
            return format!("c2c:{openid}");
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use serde_json::json;

    fn allowed(items: &[&str]) -> AccessPolicy {
        AccessPolicy::new(&set_of(items), &BTreeSet::new())
    }

    fn groups(users: &[&str], group_ids: &[&str]) -> AccessPolicy {
        AccessPolicy::new(&set_of(users), &set_of(group_ids))
    }

    fn set_of(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn c2c_message_routes_to_the_user_openid() {
        let data = json!({
            "id": "m-1",
            "content": "  看看目录  ",
            "author": {"user_openid": "F6A8"},
            "attachments": [{"url": "https://x/a.png", "filename": "a.png"}],
        });
        match route_event(EVENT_C2C_MESSAGE, &data, &allowed(&["F6A8"])) {
            EventRoute::Message(inbound) => {
                assert_eq!(inbound.target, Target::User("F6A8".to_string()));
                assert_eq!(inbound.content, "看看目录");
                assert_eq!(inbound.message_id, "m-1");
                assert_eq!(inbound.conversation_key(), "c2c:F6A8");
                assert_eq!(
                    inbound.attachments,
                    vec![("https://x/a.png".to_string(), "a.png".to_string())]
                );
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn group_message_uses_member_openid_for_the_whitelist() {
        let data = json!({
            "id": "m-2",
            "content": "/status",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
        });
        match route_event(EVENT_GROUP_AT_MESSAGE, &data, &allowed(&["M-9"])) {
            EventRoute::Message(inbound) => {
                assert_eq!(inbound.target, Target::Group("A1B2".to_string()));
                assert_eq!(inbound.sender, "M-9");
                assert_eq!(inbound.conversation_key(), "group:A1B2");
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn unauthorized_sender_is_reported_without_a_reply() {
        let data = json!({
            "id": "m-3",
            "content": "hi",
            "author": {"user_openid": "STRANGER"},
        });
        assert_eq!(
            route_event(EVENT_C2C_MESSAGE, &data, &allowed(&["F6A8"])),
            EventRoute::Unauthorized {
                sender: "STRANGER".to_string(),
                sender_name: String::new(),
                conversation: "c2c:STRANGER".to_string(),
                hint: "不在 allowed_user_ids",
                event: EVENT_C2C_MESSAGE.to_string()
            }
        );
    }

    #[test]
    fn unauthorized_group_message_carries_nickname_and_group() {
        let data = json!({
            "id": "m-5",
            "content": "hi",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9", "username": " 小明 "},
        });
        assert_eq!(
            route_event(EVENT_GROUP_AT_MESSAGE, &data, &allowed(&["F6A8"])),
            EventRoute::Unauthorized {
                sender: "M-9".to_string(),
                sender_name: "小明".to_string(),
                conversation: "group:A1B2".to_string(),
                hint: "不在 allowed_user_ids（未配置 allowed_group_ids）",
                event: EVENT_GROUP_AT_MESSAGE.to_string()
            }
        );
    }

    #[test]
    fn group_whitelist_lets_members_in_while_strangers_stay_out() {
        let data = json!({
            "id": "m-6",
            "content": "跑测试",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9", "username": "小明"},
        });
        // 群在白名单里：成员本人不在用户白名单也放行。
        assert!(matches!(
            route_event(EVENT_GROUP_AT_MESSAGE, &data, &groups(&["F6A8"], &["A1B2"])),
            EventRoute::Message(_)
        ));
        // 群不在白名单里：即使成员本人在用户白名单也被拒。
        assert_eq!(
            route_event(EVENT_GROUP_AT_MESSAGE, &data, &groups(&["M-9"], &["OTHER"])),
            EventRoute::Unauthorized {
                sender: "M-9".to_string(),
                sender_name: "小明".to_string(),
                conversation: "group:A1B2".to_string(),
                hint: "不在 allowed_group_ids",
                event: EVENT_GROUP_AT_MESSAGE.to_string()
            }
        );
    }

    #[test]
    fn group_whitelist_never_widens_private_chat() {
        // 配了群白名单也不影响单聊：陌生人私聊仍被拒。
        let data = json!({"id": "m-7", "content": "hi", "author": {"user_openid": "STRANGER"}});
        assert_eq!(
            route_event(EVENT_C2C_MESSAGE, &data, &groups(&["F6A8"], &["A1B2"])),
            EventRoute::Unauthorized {
                sender: "STRANGER".to_string(),
                sender_name: String::new(),
                conversation: "c2c:STRANGER".to_string(),
                hint: "不在 allowed_user_ids",
                event: EVENT_C2C_MESSAGE.to_string()
            }
        );
    }

    #[test]
    fn empty_group_whitelist_keeps_judging_by_sender() {
        // 未配群白名单：群聊仍按成员 openid 判定，行为与旧版一致。
        let data = json!({
            "id": "m-8",
            "content": "hi",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
        });
        assert!(matches!(
            route_event(EVENT_GROUP_AT_MESSAGE, &data, &allowed(&["M-9"])),
            EventRoute::Message(_)
        ));
    }

    #[test]
    fn empty_whitelist_allows_everyone() {
        let data = json!({"id": "m", "content": "hi", "author": {"user_openid": "X"}});
        assert!(matches!(
            route_event(EVENT_C2C_MESSAGE, &data, &allowed(&[])),
            EventRoute::Message(_)
        ));
    }

    #[test]
    fn non_message_events_are_ignored() {
        assert_eq!(
            route_event("GROUP_ADD_ROBOT", &json!({}), &allowed(&["X"])),
            EventRoute::NotAMessage {
                event: "GROUP_ADD_ROBOT".to_string(),
                conversation: String::new(),
            }
        );
        assert_eq!(
            route_event(EVENT_C2C_MESSAGE, &json!({"id": "m"}), &allowed(&["X"])),
            EventRoute::Malformed {
                event: EVENT_C2C_MESSAGE.to_string()
            }
        );
    }

    #[test]
    fn non_message_group_events_carry_the_group_id() {
        // 非消息事件（加好友、被拉群等）也要能看出发生在哪个群。
        let data = json!({"group_openid": "A1B2C3", "op_member_openid": "M-9"});
        assert_eq!(
            route_event("GROUP_ADD_ROBOT", &data, &allowed(&["M-9"])),
            EventRoute::NotAMessage {
                event: "GROUP_ADD_ROBOT".to_string(),
                conversation: "group:A1B2C3".to_string(),
            }
        );
    }

    fn bot<'a>(name: &'a str, openid: &'a str) -> BotIdentity<'a> {
        BotIdentity { openid, name }
    }

    #[test]
    fn full_group_message_without_mention_is_not_addressed() {
        // 全量模式会推送群里每一条消息，没有 @ 机器人的不能被当成指令。
        let data = json!({
            "id": "m-10",
            "content": "大家早上好",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
        });
        match route_event_with_bot(EVENT_GROUP_MESSAGE, &data, &allowed(&["M-9"]), bot("小囫囵", "BOT-ID")) {
            EventRoute::NotAddressed {
                event,
                conversation,
                detail,
            } => {
                assert_eq!(event, EVENT_GROUP_MESSAGE);
                assert_eq!(conversation, "group:A1B2");
                assert!(detail.contains("大家早上好"), "诊断串应带正文：{detail}");
                assert!(detail.contains("小囫囵"), "诊断串应带昵称：{detail}");
            }
            other => panic!("应判为未 @：{other:?}"),
        }
    }

    #[test]
    fn real_world_nickname_mention_is_a_command() {
        // 实测报文：@ 被渲染成 `@小囫囵 `，mentions 是空数组，没有 <@openid> 占位符。
        let data = json!({
            "id": "m-14",
            "content": "@小囫囵 /status ",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "mentions": [],
        });
        match route_event_with_bot(
            EVENT_GROUP_MESSAGE,
            &data,
            &allowed(&["M-9"]),
            bot("小囫囵", "BOT-ID"),
        ) {
            EventRoute::Message(inbound) => {
                assert_eq!(inbound.content, "/status");
                assert_eq!(inbound.target, Target::Group("A1B2".to_string()));
                assert_eq!(inbound.sender, "M-9");
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn nickname_mention_needs_a_boundary() {
        // `@小囫囵的猫` 不是 @ 机器人，避免误触发。
        let data = json!({
            "id": "m-15",
            "content": "@小囫囵的猫好可爱",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
        });
        assert!(matches!(
            route_event_with_bot(
                EVENT_GROUP_MESSAGE,
                &data,
                &allowed(&["M-9"]),
                bot("小囫囵", "BOT-ID")
            ),
            EventRoute::NotAddressed { .. }
        ));
    }

    #[test]
    fn structured_mention_still_works() {
        // 兼容平台可能给出的结构化写法：<@openid> 占位符或 mentions 里的 bot 标记。
        let by_placeholder = json!({
            "id": "m-11",
            "content": "<@BOT-ID> 看看目录",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
        });
        match route_event_with_bot(
            EVENT_GROUP_MESSAGE,
            &by_placeholder,
            &allowed(&["M-9"]),
            bot("小囫囵", "BOT-ID"),
        ) {
            EventRoute::Message(inbound) => assert_eq!(inbound.content, "看看目录"),
            other => panic!("应为消息：{other:?}"),
        }
        let by_mentions = json!({
            "id": "m-12",
            "content": "你好",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "mentions": [{"id": "BOT-ID", "bot": true}],
        });
        assert!(matches!(
            route_event_with_bot(
                EVENT_GROUP_MESSAGE,
                &by_mentions,
                &allowed(&["M-9"]),
                bot("小囫囵", "BOT-ID")
            ),
            EventRoute::Message(_)
        ));
    }

    #[test]
    fn full_group_message_still_obeys_the_group_whitelist() {
        // @ 到机器人但群不在白名单：仍然是未授权，不回复。
        let data = json!({
            "id": "m-13",
            "content": "@小囫囵 hi",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
        });
        assert_eq!(
            route_event_with_bot(
                EVENT_GROUP_MESSAGE,
                &data,
                &groups(&["M-9"], &["OTHER"]),
                bot("小囫囵", "BOT-ID")
            ),
            EventRoute::Unauthorized {
                sender: "M-9".to_string(),
                sender_name: String::new(),
                conversation: "group:A1B2".to_string(),
                hint: "不在 allowed_group_ids",
                event: EVENT_GROUP_MESSAGE.to_string()
            }
        );
    }

    #[test]
    fn channel_mentions_are_stripped_from_the_content() {
        let data = json!({
            "id": "m-4",
            "content": "<@!6158788878435714165>  跑测试",
            "channel_id": "123456",
            "author": {"id": "u-1"},
        });
        match route_event(EVENT_GUILD_AT_MESSAGE, &data, &allowed(&["u-1"])) {
            EventRoute::Message(inbound) => {
                assert_eq!(inbound.content, "跑测试");
                assert_eq!(inbound.target, Target::Channel("123456".to_string()));
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn commands_share_the_telegram_classification() {
        assert_eq!(classify("/status"), Command::Status);
        assert_eq!(classify("看看目录"), Command::Task);
        assert_eq!(command_name("/status@Bot"), "/status");
        assert_eq!(
            parse_thinking_command("/thinking on"),
            ThinkingCommand::Enable
        );
        assert_eq!(
            workspace_argument("/workspace D:/demo"),
            Some("D:/demo".to_string())
        );
    }
}
