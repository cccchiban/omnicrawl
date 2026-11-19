//! 入站消息的文本装配：命令建议、引用上文、任务正文与来源行。
//!
//! 全是纯函数：只读 `Inbound` 与消息记录，不碰运行态。放在一起是因为它们共同决定
//! 「这条入站消息最终以什么文本交给 Agent」，改动理由一致。

use crate::qqbot::dispatch::Inbound;
use crate::qqbot::history::GroupMessage;
use crate::qqbot::api::Target;
use crate::qqbot::history::MAX_RECORD_CHARS;

pub(super) fn suggest_command(text: &str, menu: &[(String, String)]) -> Option<String> {
    let token = text
        .trim()
        .strip_prefix('/')?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("");
    if token.is_empty() {
        return None;
    }
    let typed: Vec<char> = token.to_lowercase().chars().collect();
    // 短命令一个字符之差就可能撞上另一条命令（`/new` 与 `/rew`），因此按长度放宽阈值。
    let limit = match typed.len() {
        0..=4 => return None,
        5..=8 => 1,
        _ => 2,
    };
    menu.iter()
        .map(|(name, _)| (edit_distance(&typed, &name.to_lowercase()), name))
        .filter(|(distance, _)| *distance > 0 && *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, name)| name.to_string())
}

/// 标准莱文斯坦距离（按字符计）。
pub(super) fn edit_distance(left: &[char], right: &str) -> usize {
    let target: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=target.len()).collect();
    for (i, left_char) in left.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, right_char) in target.iter().enumerate() {
            let cost = if left_char == right_char { 0 } else { 1 };
            current.push(
                (previous[j] + cost)
                    .min(previous[j + 1] + 1)
                    .min(current[j] + 1),
            );
        }
        previous = current;
    }
    previous[target.len()]
}

/// 从提问选项里解析用户答案：序号、选项正文与自由文本都接受。
pub(super) fn match_answer(options: &[String], text: &str) -> String {
    let trimmed = text.trim();
    if let Ok(index) = trimmed.parse::<usize>() {
        if index >= 1 && index <= options.len() {
            return options[index - 1].clone();
        }
    }
    trimmed.to_string()
}

/// 被引用消息的附件（展平所有嵌套层级）。
pub(super) fn quoted_attachments(inbound: &Inbound) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = Vec::new();
    for quoted in &inbound.quoted {
        for (url, name) in &quoted.attachments {
            if items.iter().any(|(known, _)| known == url) {
                continue;
            }
            items.push((url.clone(), name.clone()));
        }
    }
    items
}

/// 任务文本里的引用上下文：被引用内容的原文，附件换成已落盘的本地路径。
pub(super) fn quoted_context(inbound: &Inbound, saved: &[String]) -> Option<String> {
    if inbound.quoted.is_empty() {
        return None;
    }
    let mut lines = vec!["用户引用的消息：".to_string()];
    let mut file_index = 0_usize;
    for quoted in &inbound.quoted {
        let mut body = quoted.content.clone();
        for _ in &quoted.attachments {
            let path = saved
                .get(file_index)
                .map(|path| path.as_str())
                .unwrap_or("（下载失败）");
            file_index += 1;
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(&format!("[文件:{path}]"));
        }
        let who = if quoted.author.is_empty() {
            "引用".to_string()
        } else {
            quoted.author.clone()
        };
        lines.push(format!("- {who}：{body}"));
    }
    Some(lines.join("\n"))
}

/// 交给 Agent 的完整任务文本：谁在哪里发来、同时 @ 了谁、引用了什么，最后才是这句话。
///
/// 连接器只发用户原话时，模型看不出这句话是在 QQ 群里被 @ 出来的、也不知道发言人是谁，
/// 于是会拿「用户」当本地终端里的人。这里把来源写成结构化的一行，让模型自己判断语气。
/// `context` 非空时插入群聊历史段（在来源行之后、用户消息之前）。
pub(super) fn task_text_with_context(inbound: &Inbound, context: Option<String>) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(source_line(inbound));
    let others = crate::qqbot::dispatch::mention_others_text(&inbound.others);
    if !others.is_empty() {
        parts.push(others);
    }
    if let Some(context) = context {
        parts.push(context);
    }
    if !inbound.quoted.is_empty() {
        let mut lines = vec!["用户引用的消息：".to_string()];
        for quoted in &inbound.quoted {
            lines.push(format!("- {}", crate::qqbot::dispatch::quoted_text(quoted)));
        }
        parts.push(lines.join("\n"));
    }
    if !inbound.content.trim().is_empty() {
        parts.push(format!("用户消息：{}", inbound.content));
    }
    parts.join("\n\n")
}

/// 群聊历史段：`最近的群聊消息（新→旧）`。
///
/// 只在群聊里出现，且**不含本次提问本身**——它在记录里也有一份，重复一遍会让 Agent 以为
/// 有人说了两次。
pub(super) fn group_context_of(records: &[GroupMessage], inbound: &Inbound) -> Option<String> {
    let lines: Vec<String> = records
        .iter()
        .filter(|record| record.sender != inbound.sender || record.content != inbound.content)
        .map(GroupMessage::line)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "这个群最近的消息（共 {} 条，供你理解上下文；不含多媒体内容）：\n{}",
        lines.len(),
        lines.join("\n")
    ))
}

/// 记录里的正文上限：超长只留开头。
pub(super) fn clamp_content(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_RECORD_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX_RECORD_CHARS).collect();
    format!("{head}…")
}

/// 来源行：`来自 QQ 群 <群 openid> 的 <昵称>（<openid>）` 等。
pub(super) fn source_line(inbound: &Inbound) -> String {
    let who = if inbound.sender_name.trim().is_empty() {
        inbound.sender.clone()
    } else {
        format!("{}（{}）", inbound.sender_name.trim(), inbound.sender)
    };
    match &inbound.target {
        Target::User(_) => format!("来自 QQ 单聊的 {who}"),
        Target::Group(group) => format!("来自 QQ 群 {group} 的 {who}"),
        Target::Channel(channel) => format!("来自 QQ 频道 {channel} 的 {who}"),
    }
}
