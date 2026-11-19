//! 群聊消息记录与「纯 @ 机器人」的上下文注入。
//!
//! QQ 群消息默认不下发给机器人，只有 @ 它时才收到一条。为了让「@ 一下问刚才在聊什么」
//! 有意义，这里把群消息按群记到本地，纯 @ 时取最近若干条拼成上下文。
//!
//! 与网关循环、任务队列无关，故独立成 `impl QqBot` 块。

use serde_json::Value;

use crate::agent::AgentDriver;

use crate::qqbot::dispatch::Inbound;
use crate::qqbot::history::{GroupMessage, MediaNote};
use super::QqBot;
use crate::qqbot::api::Target;
use super::query::{clamp_content, group_context_of, task_text_with_context};

impl<D: AgentDriver> QqBot<D> {
    pub(super) fn record_group_message(&self, data: &Value) {
        let bot_openid = self
            .bot_openid
            .lock()
            .map(|id| id.clone())
            .unwrap_or_default();
        let bot_name = self
            .bot_name
            .lock()
            .map(|name| name.clone())
            .unwrap_or_default();
        let Some(inbound) = crate::qqbot::dispatch::parse_message_with_bot(
            crate::qqbot::dispatch::EVENT_GROUP_MESSAGE,
            data,
            crate::qqbot::dispatch::BotIdentity {
                openid: &bot_openid,
                name: &bot_name,
            },
        ) else {
            return;
        };
        self.store_group_record(&inbound);
    }

    /// @ 机器人的群消息：同样入本群记录（此时正文已剥掉提及）。
    pub(super) fn record_inbound(&self, inbound: &Inbound) {
        if matches!(inbound.target, Target::Group(_)) {
            self.store_group_record(inbound);
        }
    }

    /// 落一条群记录：正文与标记入库，图片只留可下载的引用（供纯 @ 时补图）。
    pub(super) fn store_group_record(&self, inbound: &Inbound) {
        let Target::Group(group) = &inbound.target else {
            return;
        };
        let content = clamp_content(&inbound.content);
        let media: Vec<MediaNote> = inbound
            .attachments
            .iter()
            .map(|(_url, name)| MediaNote::of_name(name))
            .collect();
        if content.is_empty() && media.is_empty() {
            return;
        }
        // 只留图片的地址：视频/文件不补，签链接过期与体积都不值得。
        let photos: Vec<(String, String)> = inbound
            .attachments
            .iter()
            .filter(|(_url, name)| MediaNote::of_name(name) == MediaNote::Image)
            .cloned()
            .collect();
        let record = GroupMessage {
            at: crate::qqbot::files::unix_seconds(),
            sender: inbound.sender.clone(),
            sender_name: inbound.sender_name.clone(),
            content,
            media,
            photos,
        };
        self.history
            .push(group, record, crate::qqbot::files::unix_seconds());
    }

    /// 该群是否有可供纯 @ 使用的历史（排除本次提问自身的记录）。
    pub(super) fn has_group_context(&self, inbound: &Inbound) -> bool {
        let Target::Group(group) = &inbound.target else {
            return false;
        };
        let now = crate::qqbot::files::unix_seconds();
        let records = self.history.recent(group, self.context_size, now);
        group_context_of(&records, inbound).is_some()
    }

    /// 任务文本：群聊带上该群最近的消息（纯 @ 也有上下文），其余场景与原来一致。
    pub(super) fn task_text_for(&self, inbound: &Inbound) -> String {
        let context = match &inbound.target {
            Target::Group(group) => {
                let now = crate::qqbot::files::unix_seconds();
                let records = self.history.recent(group, self.context_size, now);
                group_context_of(&records, inbound)
            }
            Target::User(_) | Target::Channel(_) => None,
        };
        task_text_with_context(inbound, context)
    }

    /// 群聊历史里最近的图片（最多 `context_size` 条消息里的）。
    ///
    /// 只取图片：视频与文件既不补也补不动（体积、过期都不可控）。本次提问自己带的附件
    /// 不走这条路——那条链路会把文件落盘并单独告知路径。
    pub(super) fn history_photos(&self, inbound: &Inbound) -> Vec<(String, String)> {
        let Target::Group(group) = &inbound.target else {
            return Vec::new();
        };
        let now = crate::qqbot::files::unix_seconds();
        let records = self.history.recent(group, self.context_size, now);
        let mut photos: Vec<(String, String)> = Vec::new();
        for record in records.iter() {
            // 本次提问本身也在这批记录里：它自己的附件已经走过落盘链路。
            if record.sender == inbound.sender && record.content == inbound.content {
                continue;
            }
            for (url, name) in record.photos.iter() {
                if !url.trim().is_empty() {
                    photos.push((url.clone(), name.clone()));
                }
            }
        }
        photos
    }}
