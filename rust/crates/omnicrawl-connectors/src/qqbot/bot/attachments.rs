//! 入站附件的落盘与富媒体回发。
//!
//! QQ 的图片/文件在事件里只有下载 URL，必须先落到 Agent 工作区、再用 `[FILE:路径]`
//! 交给宿主的工具链。保存位置与分类规则与 `files` 子模块共用。

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_session::redact_sensitive_text;

use crate::agent::AgentDriver;

use crate::qqbot::dispatch::Inbound;
use super::reply::ReplyHandle;
use super::QqBot;
use crate::qqbot::files::{classify_file_name, relative_display_path, safe_name};
use super::query::{quoted_attachments, quoted_context};

impl<D: AgentDriver> QqBot<D> {
    /// 下载 → 分类存放 → 通知用户 → 启动任务；各步失败都回传脱敏错误。
    pub fn process_attachments(self: &Arc<Self>, inbound: Inbound, reply: Arc<ReplyHandle>) {
        let temp_root = self.driver.temp_root();
        let workspace_root = PathBuf::from(self.driver.status().workspace_root);
        let stamp = crate::qqbot::files::unix_seconds();
        let mut index = 0_usize;
        let mut save_one = |url: &str, name: &str| -> Result<String, String> {
            let position = stamp + index as i64;
            index += 1;
            let content = self
                .port
                .download(url)
                .map_err(|error| format!("❌ 附件下载失败：{}", redact_sensitive_text(&error)))?;
            let file_name = safe_name(name, url, position, "");
            let subdir = classify_file_name(&file_name);
            let destination =
                crate::qqbot::files::destination(&temp_root, subdir, &file_name).map_err(|error| {
                    format!(
                        "❌ 附件保存失败：{}",
                        redact_sensitive_text(&error.to_string())
                    )
                })?;
            fs::write(&destination, &content).map_err(|error| {
                format!(
                    "❌ 附件保存失败：{}",
                    redact_sensitive_text(&error.to_string())
                )
            })?;
            Ok(relative_display_path(&destination, &workspace_root))
        };

        // 本次消息带的附件与被引用消息里的附件分开记，任务文本里才好区分。
        let mut saved: Vec<String> = Vec::new();
        for (url, name) in inbound.attachments.iter() {
            match save_one(url, name) {
                Ok(path) => saved.push(path),
                Err(text) => {
                    eprintln!("[qqbot] 附件处理失败：{text}");
                    reply.send_notice(&text);
                    return;
                }
            }
        }
        let mut quoted_files: Vec<String> = Vec::new();
        for (url, name) in quoted_attachments(&inbound) {
            match save_one(&url, &name) {
                Ok(path) => quoted_files.push(path),
                Err(text) => {
                    eprintln!("[qqbot] 引用附件处理失败：{text}");
                    reply.send_notice(&text);
                    return;
                }
            }
        }

        let mut notices: Vec<String> = Vec::new();
        if !saved.is_empty() {
            notices.push(format!(
                "✅ 已收到 {} 个附件：\n{}",
                saved.len(),
                saved.join("\n")
            ));
        }
        if !quoted_files.is_empty() {
            notices.push(format!(
                "✅ 引用消息里的 {} 个附件：\n{}",
                quoted_files.len(),
                quoted_files.join("\n")
            ));
        }
        if notices.is_empty() {
            return;
        }
        reply.send_notice(&notices.join("\n\n"));
        if self.is_stopped() {
            eprintln!("[qqbot] 服务已停止，跳过附件任务启动。");
            return;
        }
        let mut task_text = String::new();
        if !saved.is_empty() {
            task_text.push_str(&format!("已收到附件：位于 {}", saved.join("、")));
        }
        if let Some(quoted) = quoted_context(&inbound, &quoted_files) {
            if !task_text.is_empty() {
                task_text.push_str("\n\n");
            }
            task_text.push_str(&quoted);
        }
        if !inbound.content.is_empty() {
            if !task_text.is_empty() {
                task_text.push_str("\n\n");
            }
            task_text.push_str(&format!("用户补充说明：{}", inbound.content));
        }
        // 来源与「同时 @ 了谁」由 `task_text_of` 统一补在最前面：附件任务的正文是上面那段
        // 拼出来的说明，来源信息不能因此丢失。
        self.start_task(
            Inbound {
                target: inbound.target.clone(),
                sender: inbound.sender.clone(),
                sender_name: inbound.sender_name.clone(),
                others: inbound.others.clone(),
                message_id: inbound.message_id.clone(),
                content: task_text,
                attachments: Vec::new(),
                ref_idx: inbound.ref_idx.clone(),
                quoted: Vec::new(),
                event: inbound.event.clone(),
            },
            reply,
        );
    }
}
