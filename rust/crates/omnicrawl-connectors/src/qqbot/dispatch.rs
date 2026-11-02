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

/// 被引用的消息（`message_type=103` 时由 `msg_elements[]` 携带，可嵌套）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quoted {
    /// 被引用消息的发送者昵称：平台可能不给。
    pub author: String,
    /// 被引用消息的正文；只发了一张图时为空。
    pub content: String,
    /// 被引用消息里的附件（`(url, 建议文件名)`）。
    pub attachments: Vec<(String, String)>,
}

/// 一条 @ 提及的对象。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mention {
    /// 平台标识（openid / 频道成员 id）；正文里手打 `@昵称` 时为空。
    pub id: String,
    /// 被 @ 者的昵称；平台可能不给。
    pub name: String,
    /// 平台标记的「这是个机器人」：群里别的 bot 也是 true，不代表就是本机器人。
    pub bot: bool,
}

impl Mention {
    /// 展示用标签：昵称 + 括号内的机器人标记与标识。
    pub fn label(&self) -> String {
        let name = self.name.trim();
        let mut tags: Vec<&str> = Vec::new();
        if self.bot {
            tags.push("机器人");
        }
        if !self.id.trim().is_empty() {
            tags.push(self.id.trim());
        }
        match (name.is_empty(), tags.is_empty()) {
            (false, false) => format!("{name}（{}）", tags.join("，")),
            (false, true) => name.to_string(),
            (true, false) => tags.join("，"),
            (true, true) => "未知成员".to_string(),
        }
    }
}

/// 一条消息里 @ 到的对象：机器人自己有没有被 @、另外还 @ 了谁。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MentionScan {
    /// 机器人自己被 @ 了。
    pub to_bot: bool,
    /// 命中「@ 自己」的 `mentions[]` 条目标识：群消息里是群成员 openid，正文的
    /// `<@…>` 占位符用的就是它，剥正文时按它删（机器人 QQ 号在那里出现不了）。
    pub bot_ids: Vec<String>,
    /// @ 到的其他成员（已排除机器人自己，按出现顺序去重）。
    pub others: Vec<Mention>,
}

/// 一条入站消息的落点与身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// 被动回复的目标（单聊用户 / 群）。
    pub target: Target,
    /// 发送者 openid（白名单与审批归属都看它）。
    pub sender: String,
    /// 发送者昵称：只用于日志辨认，平台可能不给。
    pub sender_name: String,
    /// 这条消息同时 @ 到的其他成员（不含机器人自己）。
    pub others: Vec<Mention>,
    /// 平台消息 ID：被动回复必须回传它。
    pub message_id: String,
    pub content: String,
    /// 附件 URL 列表（`(url, 建议文件名)`）。
    pub attachments: Vec<(String, String)>,
    /// 本消息的引用索引（`message_scene.ext` 的 `msg_idx`）：最终回答据此挂上引用。
    pub ref_idx: String,
    /// 用户引用了哪条消息：被引用内容由平台装进 `msg_elements[]`。
    pub quoted: Vec<Quoted>,
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

/// 同上，但提供机器人身份：全量群消息要据此剥掉 `@机器人`，并区分 @ 的是谁。
pub fn parse_message_with_bot(event: &str, data: &Value, bot: BotIdentity<'_>) -> Option<Inbound> {
    let message_id = string_of(data.get("id")).unwrap_or_default();
    let raw_content = data
        .get("content")
        .map(|value| value.as_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let mentions = scan_mentions(data, bot);
    let attachments = attachments_of(data);
    let ref_idx = scene_index(data, "msg_idx");
    let quoted = quoted_of(data);
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
                others: mentions.others,
                message_id,
                // 单聊没有 @ 语义：正文原样保留，只去掉首尾空白。
                content: raw_content.trim().to_string(),
                attachments,
                ref_idx,
                quoted,
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
            // @ 事件那边平台已去掉机器人前缀；全量模式自带的 `@机器人` 就地剥掉，
            // 只删提及本身，前后文都保留（`请看 @机器人 这张图` 不能被截成「这张图」）。
            let content = strip_bot_mention(&raw_content, bot, &mentions.bot_ids);
            Some(Inbound {
                target: Target::Group(group),
                sender: member,
                sender_name: sender_name_of(data),
                others: mentions.others,
                message_id,
                content,
                attachments,
                ref_idx,
                quoted,
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
            // 频道事件本身就是「@ 了机器人」：正文开头的占位符一律算机器人自己。
            let content = strip_mention(&strip_bot_mention(&raw_content, bot, &mentions.bot_ids));
            Some(Inbound {
                target: Target::Channel(channel),
                sender: author,
                sender_name: sender_name_of(data),
                others: mentions.others,
                message_id,
                content,
                attachments,
                ref_idx,
                quoted,
                event: event.to_string(),
            })
        }
        _ => None,
    }
}

/// `message_scene.ext` 里的 `key=value`：单聊与群聊用它带 `msg_idx` / `ref_msg_idx`。
pub fn scene_index(data: &Value, key: &str) -> String {
    let prefix = format!("{key}=");
    let list = match data
        .get("message_scene")
        .and_then(|scene| scene.get("ext"))
        .and_then(Value::as_array)
    {
        Some(list) => list,
        None => return String::new(),
    };
    for item in list {
        let text = item.as_str().unwrap_or_default().trim();
        if let Some(value) = text.strip_prefix(&prefix) {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// 被引用/转发的内容：`message_type=103` 时平台把它装进 `msg_elements[]`。
///
/// 每个元素自带 `content`、`attachments` 与 `author`，并且可以嵌套（引用一条引用消息），
/// 因此按深度展开，遇到没有正文也没有附件的元素就当没有引用。
pub fn quoted_of(data: &Value) -> Vec<Quoted> {
    let mut items = Vec::new();
    collect_quoted(data, &mut items, 0);
    items
}

fn collect_quoted(node: &Value, out: &mut Vec<Quoted>, depth: usize) {
    if depth > 3 {
        return;
    }
    let Some(list) = node.get("msg_elements").and_then(Value::as_array) else {
        return;
    };
    for item in list {
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let attachments = attachments_of(item);
        if !content.is_empty() || !attachments.is_empty() {
            out.push(Quoted {
                author: sender_name_of(item),
                content,
                attachments,
            });
        }
        collect_quoted(item, out, depth + 1);
    }
}

/// 被引用内容的文字摘要：附件按「[图片]/[文件]」占位，供任务文本使用。
pub fn quoted_text(quoted: &Quoted) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !quoted.content.is_empty() {
        parts.push(quoted.content.clone());
    }
    for (url, name) in &quoted.attachments {
        let label = attachment_label(name, url);
        parts.push(format!("[{label}]"));
    }
    let body = parts.join(" ");
    if quoted.author.is_empty() {
        body
    } else {
        format!("{}：{body}", quoted.author)
    }
}

/// 「同时 @ 了谁」的一行说明：没有其他成员时返回空串。
pub fn mention_others_text(others: &[Mention]) -> String {
    if others.is_empty() {
        return String::new();
    }
    let labels: Vec<String> = others.iter().map(Mention::label).collect();
    format!("同时 @ 了：{}", labels.join("、"))
}

/// 附件种类标签：图片 / 语音 / 视频 / 文件。
fn attachment_label(name: &str, url: &str) -> &'static str {
    let lower = name.to_lowercase();
    let url = url.to_lowercase();
    let is = |extensions: &[&str]| {
        extensions
            .iter()
            .any(|extension| lower.ends_with(extension) || url.contains(extension))
    };
    if is(&[".png", ".jpg", ".jpeg", ".gif", "image/"]) {
        "图片"
    } else if is(&[".mp4", "video/"]) {
        "视频"
    } else if is(&[".silk", ".mp3", ".wav", "voice"]) {
        "语音"
    } else {
        "文件"
    }
}

/// 扫一条消息里 @ 到的对象：机器人自己被 @ 了没有、另外还 @ 了谁。
///
/// 实测（2026-10-24 真实报文）：群里手打的 `@昵称` 是普通文本，`mentions` 为空数组，也没有
/// 文档所说的 `<@openid>` 占位符；只有用 QQ 的「@ 提及」功能选中成员才会有结构化条目。
/// 因此三条来源都要看：
/// - `mentions[]`：条目标了 `bot: true`、昵称与机器人相同、且正文里带 `@` 才算 @ 了自己；其余
///   条目算 @ 了别人（带 id 与昵称）。
/// - `<@openid>` / `<@!openid>` 占位符：与机器人 openid 相同即算 @ 机器人。
/// - 正文里的 `@昵称`：昵称后紧跟分隔符才算命中机器人，避免 `@小囫囵的猫` 误判；其余
///   `@名字` 记为 @ 了别人（只有昵称，没有 id）。
///
/// `mentions[]` 里的标识**不能**用来认自己：群消息的 `id` / `member_openid` 是群成员 openid，
/// 而机器人身份取自 `READY.user.id`（QQ 号），两套编号不同源，比对永远不成立。`bot: true`
/// 也只说明**被 @ 的对象本身**是机器人（官方 `User.bot`），群里别的 bot 同样是 true——所以三个
/// 条件必须同时成立：是机器人 + 昵称是我 + 正文里确实带 `@`。
///
/// 命中条目的标识记进 `bot_ids`：群消息正文的 `<@…>` 用的就是它，剥正文时按它删。
pub fn scan_mentions(data: &Value, bot: BotIdentity<'_>) -> MentionScan {
    let mut scan = MentionScan::default();
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_at = content.contains('@');
    if let Some(list) = data.get("mentions").and_then(Value::as_array) {
        for item in list {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let member = item
                .get("member_openid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let name = first_nonempty([
                item.get("username").and_then(Value::as_str),
                item.get("nickname").and_then(Value::as_str),
                item.get("member_username").and_then(Value::as_str),
            ]);
            let bot_flag = item.get("bot").and_then(Value::as_bool).unwrap_or(false);
            let target = if id.is_empty() {
                member.as_str()
            } else {
                id.as_str()
            };
            let same_name = !bot.name.is_empty() && name == bot.name;
            if bot_flag && same_name {
                if !target.is_empty() {
                    scan.bot_ids.push(target.to_string());
                }
                if has_at {
                    scan.to_bot = true;
                }
                continue;
            }
            push_other(
                &mut scan.others,
                Mention {
                    id: target.to_string(),
                    name,
                    bot: bot_flag,
                },
            );
        }
    }

    if !bot.openid.is_empty()
        && (content.contains(&format!("<@{}>", bot.openid))
            || content.contains(&format!("<@!{}>", bot.openid)))
    {
        scan.to_bot = true;
    }
    for name in text_mentions(content) {
        if !bot.name.is_empty() {
            if name == bot.name {
                scan.to_bot = true;
                continue;
            }
            // `@昵称的猫`：以机器人昵称开头但后面还接着字，既不是 @ 机器人（没有分隔符），
            // 也说不清 @ 的是谁——没有成员名册可核对，当作闲聊里的文字，不报成「@ 了别人」。
            if name.starts_with(bot.name) {
                continue;
            }
        }
        push_other(
            &mut scan.others,
            Mention {
                id: String::new(),
                name,
                bot: false,
            },
        );
    }
    scan
}

/// 全量模式的群消息是否 @ 了机器人。
fn addressed_to_bot(data: &Value, bot: BotIdentity<'_>) -> bool {
    scan_mentions(data, bot).to_bot
}

/// 按 id 或昵称去重后追加一条「@ 了别人」。
///
/// 同一个人可能同时以三种形式出现：结构化条目（id + 昵称）、正文里手打的 `@昵称` 与手打的
/// `@openid`。因此 id 与昵称交叉比对，任一命中就当同一个人。
fn push_other(others: &mut Vec<Mention>, mention: Mention) {
    if mention.id.is_empty() && mention.name.is_empty() {
        return;
    }
    let duplicate = others.iter().any(|known| {
        (!mention.id.is_empty() && (known.id == mention.id || known.name == mention.id))
            || (!mention.name.is_empty() && (known.name == mention.name || known.id == mention.name))
    });
    if !duplicate {
        others.push(mention);
    }
}

/// 正文里 `@名字` 形式的提及：按空白与标点切出名字，`<@…>` 占位符不算。
fn text_mentions(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    let chars: Vec<char> = content.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        // `<@openid>` / `<@!openid>` 是结构化占位符，不是手打的 @昵称。
        if chars[index] == '<' && chars.get(index + 1) == Some(&'@') {
            let mut next = chars.len();
            for (offset, char) in chars[index..].iter().enumerate() {
                if *char == '>' {
                    next = index + offset + 1;
                    break;
                }
            }
            index = next;
            continue;
        }
        if chars[index] != '@' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < chars.len() && !is_mention_terminator(chars[end]) {
            end += 1;
        }
        if end > start {
            names.push(chars[start..end].iter().collect::<String>());
        }
        index = end.max(index + 1);
    }
    names
}

/// 提及名字的终止字符：空白、全角空格、`<`、`@`、`>` 与中英文标点。
fn is_mention_terminator(char: char) -> bool {
    char.is_whitespace()
        || char == '\u{3000}'
        || char == '<'
        || char == '>'
        || char == '@'
        || matches!(
            char,
            ',' | '.' | '!' | '?' | ':' | ';' | '，' | '。' | '！' | '？' | '：' | '；' | '、'
        )
}

/// 去掉正文里对机器人自己的 @（文本与占位符），其余成员与上下文原样保留。
///
/// 只删提及本身、不截断后文：`请看 @机器人 这张图` 要变成 `请看 这张图`，而不是「这张图」。
///
/// 占位符里的标识要用 `scan.bot_ids`（群消息的 `<@…>` 是群成员 openid），不能只用机器人 QQ 号。
fn strip_bot_mention(content: &str, bot: BotIdentity<'_>, scan_ids: &[String]) -> String {
    let mut rest = content.to_string();
    let mut placeholders: Vec<String> = Vec::new();
    if !bot.openid.is_empty() {
        placeholders.push(format!("<@{}>", bot.openid));
        placeholders.push(format!("<@!{}>", bot.openid));
    }
    for id in scan_ids {
        if !id.is_empty() {
            placeholders.push(format!("<@{id}>"));
            placeholders.push(format!("<@!{id}>"));
        }
    }
    for token in placeholders {
        if rest.contains(&token) {
            rest = rest.replace(&token, " ");
        }
    }
    if !bot.name.is_empty() {
        let needle = format!("@{}", bot.name);
        let mut out = String::with_capacity(rest.len());
        let mut cursor = 0_usize;
        while let Some(found) = rest[cursor..].find(&needle) {
            let start = cursor + found;
            let end = start + needle.len();
            let after = rest[end..].chars().next();
            let boundary = match after {
                None => true,
                Some(char) => char.is_whitespace() || char == '\u{3000}' || is_mention_terminator(char),
            };
            if boundary {
                out.push_str(&rest[cursor..start]);
                // 前后都是空白时补一个空格，避免把两段文字黏在一起。
                if !out.ends_with(char::is_whitespace) {
                    out.push(' ');
                }
                cursor = end;
            } else {
                out.push_str(&rest[cursor..end]);
                cursor = end;
            }
        }
        out.push_str(&rest[cursor..]);
        rest = out;
    }
    collapse_spaces(rest.trim())
}

/// 多个连续空格压成一个（剥掉提及后常留下两个空格）；换行原样保留，避免改坏多行消息。
fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_space = false;
    for char in text.chars() {
        let horizontal = matches!(char, ' ' | '\t' | '\u{3000}');
        if horizontal {
            if !previous_space {
                out.push(char);
            }
            previous_space = true;
        } else {
            out.push(char);
            previous_space = false;
        }
    }
    out
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
            let name = item
                .get("username")
                .and_then(Value::as_str)
                .or_else(|| item.get("nickname").and_then(Value::as_str))
                .unwrap_or("");
            mentions.push(format!(
                "id={id}/member={member}/name={name}/bot={bot_flag}"
            ));
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
    let author = data.get("author");
    first_nonempty([
        author.and_then(|item| item.get("username")).and_then(Value::as_str),
        author.and_then(|item| item.get("nickname")).and_then(Value::as_str),
    ])
}

/// 首个非空文本：提及条目在不同事件里用的昵称字段名不一致。
fn first_nonempty<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> String {
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|text| !text.is_empty())
        .unwrap_or_default()
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
        // 兼容平台可能给出的结构化写法：<@openid> 占位符，或 mentions 里 bot + 昵称 + 正文带 @。
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
        // 群消息里条目的 id 是群成员 openid，与机器人 QQ 号不同源：靠 bot + 昵称 + 正文里的
        // `<@该条目id>` 认自己。
        let by_mentions = json!({
            "id": "m-12",
            "content": "<@GROUP-MEMBER-ID> 你好",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "mentions": [
                {"id": "GROUP-MEMBER-ID", "member_openid": "GROUP-MEMBER-ID", "username": "小囫囵", "bot": true},
            ],
        });
        match route_event_with_bot(
            EVENT_GROUP_MESSAGE,
            &by_mentions,
            &allowed(&["M-9"]),
            bot("小囫囵", "6859546824373965879"),
        ) {
            EventRoute::Message(inbound) => assert_eq!(inbound.content, "你好"),
            other => panic!("应为消息：{other:?}"),
        }
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

    /// 引用消息的真实报文形状（`message_type=103`）：被引用内容在 `msg_elements[]` 里。
    fn quoted_message() -> Value {
        json!({
            "id": "m-30",
            "content": "@奶酪 你怎么这么多稀奇古怪的东西",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9", "username": "塔斯汀"},
            "message_type": 103,
            "msg_elements": [{
                "msg_idx": "REFIDX_quote==",
                "message_type": 0,
                "author": {"member_openid": "M-1", "username": "奶酪"},
                "content": "这是我新买的酒",
                "attachments": [{
                    "content_type": "image/jpeg",
                    "filename": "photo.jpg",
                    "url": "https://multimedia.nt.qq.com.cn/download/photo.jpg",
                }],
            }],
            "message_scene": {
                "source": "default",
                "ext": ["ref_msg_idx=REFIDX_quote==", "msg_idx=REFIDX_self=="],
            },
        })
    }

    #[test]
    fn quoted_message_carries_the_referenced_content_and_attachments() {
        let data = quoted_message();
        match route_event(EVENT_GROUP_AT_MESSAGE, &data, &allowed(&["M-9"])) {
            EventRoute::Message(inbound) => {
                assert_eq!(inbound.ref_idx, "REFIDX_self==");
                assert_eq!(inbound.quoted.len(), 1);
                assert_eq!(inbound.quoted[0].author, "奶酪");
                assert_eq!(inbound.quoted[0].content, "这是我新买的酒");
                assert_eq!(
                    inbound.quoted[0].attachments,
                    vec![(
                        "https://multimedia.nt.qq.com.cn/download/photo.jpg".to_string(),
                        "photo.jpg".to_string()
                    )]
                );
                assert_eq!(quoted_text(&inbound.quoted[0]), "奶酪：这是我新买的酒 [图片]");
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn quoted_message_with_only_an_image_keeps_a_placeholder() {
        let data = json!({
            "id": "m-31",
            "content": "",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "message_type": 103,
            "msg_elements": [{
                "author": {"member_openid": "M-1", "username": "奶酪"},
                "attachments": [{
                    "content_type": "image/png",
                    "filename": "a.png",
                    "url": "https://multimedia.nt.qq.com.cn/download/a.png",
                }],
            }],
        });
        let inbound = parse_message(EVENT_GROUP_AT_MESSAGE, &data).expect("可解析");
        assert_eq!(inbound.quoted.len(), 1);
        assert_eq!(quoted_text(&inbound.quoted[0]), "奶酪：[图片]");
    }

    #[test]
    fn nested_quotes_are_flattened() {
        let data = json!({
            "id": "m-32",
            "content": "接着上面说",
            "author": {"user_openid": "F6A8"},
            "message_type": 103,
            "msg_elements": [{
                "author": {"user_openid": "F6A9", "username": "小明"},
                "content": "第二层引用",
                "msg_elements": [{
                    "author": {"user_openid": "F6A7", "username": "小红"},
                    "content": "第一层原文",
                }],
            }],
        });
        let inbound = parse_message(EVENT_C2C_MESSAGE, &data).expect("可解析");
        let bodies: Vec<&str> = inbound
            .quoted
            .iter()
            .map(|item| item.content.as_str())
            .collect();
        assert_eq!(bodies, vec!["第二层引用", "第一层原文"]);
    }

    #[test]
    fn plain_message_has_no_quote_and_no_index() {
        let data = json!({
            "id": "m-33",
            "content": "看看目录",
            "author": {"user_openid": "F6A8"},
        });
        let inbound = parse_message(EVENT_C2C_MESSAGE, &data).expect("可解析");
        assert!(inbound.quoted.is_empty());
        assert!(inbound.ref_idx.is_empty());
    }

    #[test]
    fn structured_mention_tells_apart_the_bot_from_other_members() {
        // 结构化提及：机器人自己按「bot + 昵称」认出来，另一个成员算「@ 了别人」。
        let data = json!({
            "id": "m-50",
            "content": "<@BOT-ID> 帮 @M-1 看看这个目录",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "mentions": [
                {"id": "BOT-ID", "member_openid": "BOT-ID", "username": "小囫囵", "bot": true},
                {"id": "M-1", "member_openid": "M-1", "username": "奶酪"},
            ],
        });
        match route_event_with_bot(
            EVENT_GROUP_AT_MESSAGE,
            &data,
            &allowed(&["M-9"]),
            bot("小囫囵", "BOT-ID"),
        ) {
            EventRoute::Message(inbound) => {
                assert_eq!(inbound.others.len(), 1);
                assert_eq!(inbound.others[0].id, "M-1");
                assert_eq!(inbound.others[0].name, "奶酪");
                assert_eq!(inbound.others[0].label(), "奶酪（M-1）");
                // 机器人自己的占位符被剥掉，别人的 @ 保留在正文里。
                assert_eq!(inbound.content, "帮 @M-1 看看这个目录");
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn mentioning_only_another_member_is_not_addressing_the_bot() {
        // 全量模式：只 @ 了别的成员，不算 @ 机器人。
        let scan = scan_mentions(
            &json!({
                "content": "@奶酪 你看这个",
                "mentions": [{"id": "M-1", "member_openid": "M-1", "username": "奶酪"}],
            }),
            bot("小囫囵", "BOT-ID"),
        );
        assert!(!scan.to_bot);
        assert_eq!(scan.others.len(), 1);
        assert_eq!(scan.others[0].name, "奶酪");
    }

    #[test]
    fn hand_typed_mention_names_are_classified_by_nickname() {
        // 实测里的手打 @：mentions 为空数组，只能按昵称区分是 @ 机器人还是 @ 别人。
        let data = json!({
            "id": "m-51",
            "content": "@小囫囵 帮 @奶酪 查一下，@小囫囵的猫 也要看",
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
                // `@小囫囵的猫` 不是 @ 机器人（昵称后没有分隔符），也不算别人（名字带后缀）。
                assert_eq!(
                    inbound.others.iter().map(|item| item.name.clone()).collect::<Vec<_>>(),
                    vec!["奶酪".to_string()]
                );
                assert!(inbound.content.contains("帮 @奶酪 查一下"), "{}", inbound.content);
                assert!(!inbound.content.contains("@小囫囵 "), "{}", inbound.content);
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn stripping_the_bot_mention_keeps_the_surrounding_text() {
        // 中间位置的 @机器人 只删提及本身，前后文都留着。
        let data = json!({
            "id": "m-52",
            "content": "请看 @小囫囵 这张截图",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
        });
        let inbound = parse_message_with_bot(
            EVENT_GROUP_MESSAGE,
            &data,
            bot("小囫囵", "BOT-ID"),
        )
        .expect("可解析");
        assert_eq!(inbound.content, "请看 这张截图");
    }

    #[test]
    fn mention_others_text_lists_the_other_members() {
        assert_eq!(mention_others_text(&[]), "");
        let others = vec![
            Mention { id: "M-1".to_string(), name: "奶酪".to_string(), bot: false },
            Mention { id: String::new(), name: "小明".to_string(), bot: false },
            Mention { id: "B-1".to_string(), name: "别的bot".to_string(), bot: true },
        ];
        assert_eq!(
            mention_others_text(&others),
            "同时 @ 了：奶酪（M-1）、小明、别的bot（机器人，B-1）"
        );
    }

    #[test]
    fn mentioning_another_bot_is_not_addressing_the_bot() {
        // 群里还有别的机器人：`bot: true` 只说明对方是 bot，不是「@ 了我」。
        let data = json!({
            "id": "m-53",
            "content": "<@OTHER-BOT> /status",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "mentions": [
                {"id": "OTHER-BOT", "member_openid": "OTHER-BOT", "username": "别的bot", "bot": true},
            ],
        });
        match route_event_with_bot(
            EVENT_GROUP_MESSAGE,
            &data,
            &allowed(&["M-9"]),
            bot("小囫囵", "BOT-ID"),
        ) {
            EventRoute::NotAddressed { detail, .. } => {
                assert!(detail.contains("OTHER-BOT"), "诊断串应带被 @ 的 bot：{detail}");
            }
            other => panic!("@ 别的 bot 不应触发本机器人：{other:?}"),
        }
        let scan = scan_mentions(&data, bot("小囫囵", "BOT-ID"));
        assert!(!scan.to_bot);
        assert_eq!(scan.others.len(), 1);
        assert!(scan.others[0].bot);
        assert_eq!(scan.others[0].label(), "别的bot（机器人，OTHER-BOT）");
    }

    #[test]
    fn another_bot_without_ids_is_judged_by_nickname() {
        // 结构化条目两个 id 都缺：只能靠昵称，昵称对不上就不算 @ 我。
        let data = json!({
            "id": "m-54",
            "content": "@123456 查一下",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "mentions": [{"username": "123456", "bot": true}],
        });
        let scan = scan_mentions(&data, bot("小囫囵", "BOT-ID"));
        assert!(!scan.to_bot);
        assert_eq!(scan.others.len(), 1);
        assert_eq!(scan.others[0].name, "123456");
    }

    #[test]
    fn bot_flag_with_the_same_nickname_is_the_bot() {
        // 群内条目的 id 与机器人 QQ 号不同源，认自己只能靠 bot 标记 + 昵称；正文还要带 @。
        let scan = scan_mentions(
            &json!({
                "content": "@小囫囵 在吗",
                "mentions": [{"id": "GROUP-MEMBER-ID", "username": "小囫囵", "bot": true}],
            }),
            bot("小囫囵", ""),
        );
        assert!(scan.to_bot);
        assert!(scan.others.is_empty());
    }

    #[test]
    fn a_bot_entry_without_an_at_in_the_content_is_not_an_at() {
        // 只有条目元数据、正文里没有 @：不当成 @ 机器人，也不当成「@ 了别人」。
        let scan = scan_mentions(
            &json!({
                "content": "hi",
                "mentions": [{"id": "GROUP-MEMBER-ID", "username": "小囫囵", "bot": true}],
            }),
            bot("小囫囵", "6859546824373965879"),
        );
        assert!(!scan.to_bot);
        assert!(scan.others.is_empty());
        assert_eq!(scan.bot_ids, vec!["GROUP-MEMBER-ID".to_string()]);
    }

    #[test]
    fn a_human_named_like_the_bot_is_not_the_bot() {
        // 昵称撞车但对方是人（bot: false）：条目本身不足以认定 @ 了我。
        let scan = scan_mentions(
            &json!({
                "content": "小囫囵 你谁",
                "mentions": [{"id": "M-7", "username": "小囫囵"}],
            }),
            bot("小囫囵", "6859546824373965879"),
        );
        assert!(!scan.to_bot);
        assert_eq!(scan.others.len(), 1);
    }

    #[test]
    fn mentioning_the_bot_alongside_another_bot_addresses_the_bot() {
        // 同时 @ 本机器人与另一个 bot：仍然要响应，并把对方报成「同时 @ 了」。
        let data = json!({
            "id": "m-55",
            "content": "<@BOT-ID> 和 <@OTHER-BOT> 一起看看",
            "group_openid": "A1B2",
            "author": {"member_openid": "M-9"},
            "mentions": [
                {"id": "BOT-ID", "member_openid": "BOT-ID", "username": "小囫囵", "bot": true},
                {"id": "OTHER-BOT", "member_openid": "OTHER-BOT", "username": "别的bot", "bot": true},
            ],
        });
        match route_event_with_bot(
            EVENT_GROUP_MESSAGE,
            &data,
            &allowed(&["M-9"]),
            bot("小囫囵", "BOT-ID"),
        ) {
            EventRoute::Message(inbound) => {
                assert_eq!(inbound.others.len(), 1);
                assert!(inbound.others[0].bot);
                assert_eq!(inbound.others[0].name, "别的bot");
            }
            other => panic!("应为消息：{other:?}"),
        }
    }

    #[test]
    fn duplicate_mentions_are_collapsed() {
        // 同一个成员既有结构化条目又在正文里手打：只记一次。
        let scan = scan_mentions(
            &json!({
                "content": "@奶酪 看看",
                "mentions": [{"id": "M-1", "username": "奶酪"}],
            }),
            bot("小囫囵", "BOT-ID"),
        );
        assert_eq!(scan.others.len(), 1);
        assert_eq!(scan.others[0].id, "M-1");
    }
}
