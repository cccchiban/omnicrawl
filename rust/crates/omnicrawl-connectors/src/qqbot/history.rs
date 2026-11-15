//! 群聊消息记录：给「纯 @ 机器人」提供最近上下文。
//!
//! 平台不为群聊提供任何历史消息接口，因此上下文只能由连接器自己攒：群开启「接收所有消息」
//! 后每条群消息都会推过来（[`super::dispatch::EVENT_GROUP_MESSAGE`]），本模块把它们按群
//! 分开存进各自的滚动窗口。
//!
//! 三条硬约束：
//!
//! - **按群隔离**：一个群的文件只装这个群的消息，任何跨群读取都不会发生（会话键 `group:<openid>`
//!   直接成为文件名的一部分）。
//! - **72 小时窗口**：写入时与读取时都按时间裁剪，过期消息自动消失，不需要额外的清理任务——
//!   进程停止期间攒下的过期数据会在下一次读写时一并丢掉。
//! - **只有文本**：图片、文件、语音等多媒体只留一个标记（[`MediaNote`]），既不下载也不存
//!   URL。附件 URL 是签名链接，存下来很快失效，留着只会让 Agent 看到打不开的地址。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

/// 记录保留时长：72 小时。
pub const RETENTION_SECONDS: i64 = 72 * 3600;

/// 单群记录条数上限：72 小时窗口内的兜底，防止热闹的群把文件撑大。
pub const MAX_RECORDS_PER_GROUP: usize = 500;

/// 注入任务文本的默认条数：最近十条。
pub const DEFAULT_CONTEXT_LIMIT: usize = 10;

/// 单条记录正文的最大字符数：长文只留开头，避免把整段刷屏带进上下文。
pub const MAX_RECORD_CHARS: usize = 600;

/// 记录根目录名（相对 `.agent_tmp`）：与附件分类目录同级但独立。
pub const RECORD_DIRNAME: &str = "qqbot_history";

/// 一条群消息记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMessage {
    /// Unix 秒。
    pub at: i64,
    /// 发送者 openid。
    pub sender: String,
    /// 发送者昵称；平台可能不给。
    pub sender_name: String,
    /// 正文（已剥掉 @机器人 占位符）；只发多媒体时为空。
    pub content: String,
    /// 多媒体标记：只记「有图/有文件」，不记内容。
    pub media: Vec<MediaNote>,
    /// 图片的下载地址（`(url, 建议文件名)`）：**只用于纯 @ 时补图**，不进任何展示文本，
    /// 也不写进 `line()`。签名链接会过期，补图失败就当没有这张图。
    pub photos: Vec<(String, String)>,
}

impl GroupMessage {
    /// 展示成一行：`[12:34] 昵称：正文 [图片]`。
    pub fn line(&self) -> String {
        let who = if self.sender_name.trim().is_empty() {
            self.sender.clone()
        } else {
            format!("{}（{}）", self.sender_name.trim(), self.sender)
        };
        let mut text = String::new();
        let body = self.content.trim();
        if !body.is_empty() {
            text.push_str(body);
        }
        for note in &self.media {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(note.label());
        }
        format!("{} {who}：{text}", clock_of(self.at))
    }

    fn to_json(&self) -> Value {
        json!({
            "at": self.at,
            "sender": self.sender,
            "sender_name": self.sender_name,
            "content": self.content,
            "media": self.media.iter().map(|note| note.as_str()).collect::<Vec<_>>(),
            "photos": self
                .photos
                .iter()
                .map(|(url, name)| json!({"url": url, "name": name}))
                .collect::<Vec<_>>(),
        })
    }

    fn from_json(value: &Value) -> Option<GroupMessage> {
        Some(GroupMessage {
            at: value.get("at").and_then(Value::as_i64)?,
            sender: string_of(value.get("sender")),
            sender_name: string_of(value.get("sender_name")),
            content: string_of(value.get("content")),
            media: value
                .get("media")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().and_then(MediaNote::parse))
                        .collect()
                })
                .unwrap_or_default(),
            photos: value
                .get("photos")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            (
                                string_of(item.get("url")),
                                string_of(item.get("name")),
                            )
                        })
                        .filter(|(url, _name)| !url.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// 多媒体标记：只留种类，不留内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaNote {
    Image,
    Video,
    File,
    Voice,
}

impl MediaNote {
    /// 展示标签。
    pub fn label(self) -> &'static str {
        match self {
            MediaNote::Image => "[图片]",
            MediaNote::Video => "[视频]",
            MediaNote::File => "[文件]",
            MediaNote::Voice => "[语音]",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            MediaNote::Image => "image",
            MediaNote::Video => "video",
            MediaNote::File => "file",
            MediaNote::Voice => "voice",
        }
    }

    fn parse(text: &str) -> Option<MediaNote> {
        match text {
            "image" => Some(MediaNote::Image),
            "video" => Some(MediaNote::Video),
            "file" => Some(MediaNote::File),
            "voice" => Some(MediaNote::Voice),
            _ => None,
        }
    }

    /// 按附件扩展名判种类：判不出的当文件。
    ///
    /// 平台不给 MIME，只能看建议文件名；`file_type` 字段不随消息事件下发。
    pub fn of_name(name: &str) -> MediaNote {
        let suffix = Path::new(name)
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match suffix.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => MediaNote::Image,
            "mp4" | "mov" | "mkv" | "webm" | "avi" => MediaNote::Video,
            "silk" | "amr" | "mp3" | "wav" | "flac" | "ogg" => MediaNote::Voice,
            _ => MediaNote::File,
        }
    }
}

/// 一个群的滚动记录：内存里以「群 openid → 消息」保存，写盘时一群一个文件。
pub struct GroupHistory {
    root: PathBuf,
    retention: i64,
    limit: usize,
    groups: Mutex<BTreeMap<String, Vec<GroupMessage>>>,
}

impl GroupHistory {
    /// 按 `.agent_tmp/qqbot_history` 打开：目录不存在时第一次写入再建。
    pub fn open(temp_root: impl AsRef<Path>) -> GroupHistory {
        GroupHistory::with_limits(temp_root, RETENTION_SECONDS, MAX_RECORDS_PER_GROUP)
    }

    /// 自定义窗口与条数上限（测试与配置化用）。
    pub fn with_limits(temp_root: impl AsRef<Path>, retention: i64, limit: usize) -> GroupHistory {
        GroupHistory {
            root: temp_root.as_ref().join(RECORD_DIRNAME),
            retention: retention.max(1),
            limit: limit.max(1),
            groups: Mutex::new(BTreeMap::new()),
        }
    }

    /// 追加一条记录：先按窗口裁掉过期的，再截到条数上限。
    pub fn push(&self, group: &str, message: GroupMessage, now: i64) {
        if group.trim().is_empty() {
            return;
        }
        let mut groups = self.groups.lock().unwrap_or_else(|error| error.into_inner());
        let records = groups.entry(group.to_string()).or_default();
        records.push(message);
        let cutoff = now - self.retention;
        records.retain(|record| record.at >= cutoff);
        if records.len() > self.limit {
            let drop_count = records.len() - self.limit;
            records.drain(0..drop_count);
        }
        let snapshot = records.clone();
        drop(groups);
        // 落盘失败只影响重启后的上下文，不影响本轮对话：记日志即可。
        if let Err(error) = self.save(group, &snapshot) {
            eprintln!("[qqbot] 群消息记录落盘失败（{group}）：{error}");
        }
    }

    /// 取某群最近 `limit` 条记录（按时间正序，最旧的在前）。
    ///
    /// 读取时同样裁剪一次：进程停着的时候过期记录不会被清，读的时候丢掉即可。
    pub fn recent(&self, group: &str, limit: usize, now: i64) -> Vec<GroupMessage> {
        if group.trim().is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut groups = self.groups.lock().unwrap_or_else(|error| error.into_inner());
        if !groups.contains_key(group) {
            // 首次访问这个群：把上次进程留下的记录读进来。
            if let Ok(records) = self.load(group) {
                groups.insert(group.to_string(), records);
            }
        }
        let records = groups.entry(group.to_string()).or_default();
        let cutoff = now - self.retention;
        records.retain(|record| record.at >= cutoff);
        let start = records.len().saturating_sub(limit);
        records[start..].to_vec()
    }

    /// 内存里有哪些群有记录（总结器据此决定要不要跑）。
    pub fn groups_tracked(&self) -> Vec<String> {
        let groups = self.groups.lock().unwrap_or_else(|error| error.into_inner());
        groups
            .iter()
            .filter(|(_group, records)| !records.is_empty())
            .map(|(group, _records)| group.clone())
            .collect()
    }

    fn path_of(&self, group: &str) -> PathBuf {
        self.root.join(format!("{}.json", file_key(group)))
    }

    fn save(&self, group: &str, records: &[GroupMessage]) -> Result<(), String> {
        fs::create_dir_all(&self.root)
            .map_err(|error| format!("创建记录目录失败：{error}"))?;
        let payload = json!({
            "group": group,
            "records": records.iter().map(GroupMessage::to_json).collect::<Vec<_>>(),
        });
        let text = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
        let path = self.path_of(group);
        // 先写临时文件再改名：进程被杀在半路时不会留下半截 JSON。
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, text).map_err(|error| format!("写入记录失败：{error}"))?;
        fs::rename(&temp, &path).map_err(|error| format!("替换记录失败：{error}"))
    }

    fn load(&self, group: &str) -> Result<Vec<GroupMessage>, String> {
        let path = self.path_of(group);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("读取记录失败：{error}")),
        };
        let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        Ok(value
            .get("records")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(GroupMessage::from_json).collect())
            .unwrap_or_default())
    }
}

/// 群 openid → 文件名：openid 含大小写字母与数字，直接可用；只挡路径分隔符与点。
fn file_key(group: &str) -> String {
    group
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '.' => '_',
            other => other,
        })
        .collect()
}

/// `[时:分]`（本地时区）。
fn clock_of(at: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(at, 0) {
        chrono::LocalResult::Single(time) => time.format("[%H:%M]").to_string(),
        _ => "[--:--]".to_string(),
    }
}

fn string_of(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(at: i64, sender: &str, content: &str) -> GroupMessage {
        GroupMessage {
            at,
            sender: sender.to_string(),
            sender_name: String::new(),
            content: content.to_string(),
            media: Vec::new(),
            photos: Vec::new(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "omnicrawl-qqbot-history-{tag}-{}-{}",
            std::process::id(),
            crate::qqbot::files::unix_seconds()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn groups_stay_separated() {
        let dir = temp_dir("separate");
        let history = GroupHistory::open(&dir);
        history.push("G-1", message(100, "A", "群一的话"), 200);
        history.push("G-2", message(101, "B", "群二的话"), 200);
        assert_eq!(history.recent("G-1", 10, 200).len(), 1);
        assert_eq!(history.recent("G-1", 10, 200)[0].content, "群一的话");
        assert_eq!(history.recent("G-2", 10, 200)[0].content, "群二的话");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_older_than_the_window_are_dropped() {
        let dir = temp_dir("window");
        let history = GroupHistory::open(&dir);
        let now = 100 * 3600;
        history.push("G-1", message(now - 73 * 3600, "A", "太旧了"), now);
        history.push("G-1", message(now - 71 * 3600, "A", "还在窗口内"), now);
        let recent = history.recent("G-1", 10, now);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].content, "还在窗口内");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_keeps_the_newest_entries_in_order() {
        let dir = temp_dir("recent");
        let history = GroupHistory::open(&dir);
        for index in 0..15 {
            history.push("G-1", message(1000 + index, "A", &format!("第{index}条")), 2000);
        }
        let recent = history.recent("G-1", 10, 2000);
        assert_eq!(recent.len(), 10);
        assert_eq!(recent[0].content, "第5条");
        assert_eq!(recent[9].content, "第14条");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_survive_a_reopen() {
        let dir = temp_dir("persist");
        {
            let history = GroupHistory::open(&dir);
            history.push("G-1", message(1000, "A", "重启前"), 1000);
        }
        let reopened = GroupHistory::open(&dir);
        let recent = reopened.recent("G-1", 10, 1000);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].content, "重启前");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_count_limit_drops_the_oldest() {
        let dir = temp_dir("limit");
        let history = GroupHistory::with_limits(&dir, RETENTION_SECONDS, 3);
        for index in 0..6 {
            history.push("G-1", message(1000 + index, "A", &format!("第{index}条")), 1000);
        }
        let recent = history.recent("G-1", 10, 1000);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].content, "第3条");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_is_only_a_marker() {
        let note = MediaNote::of_name("photo.PNG");
        assert_eq!(note, MediaNote::Image);
        assert_eq!(MediaNote::of_name("clip.mp4"), MediaNote::Video);
        assert_eq!(MediaNote::of_name("report.pdf"), MediaNote::File);
        let record = GroupMessage {
            at: 1000,
            sender: "A".to_string(),
            sender_name: "小明".to_string(),
            content: String::new(),
            media: vec![MediaNote::Image],
            photos: Vec::new(),
        };
        let line = record.line();
        assert!(line.contains("小明"));
        assert!(line.contains("[图片]"));
        // 记录里不该出现任何 URL。
        assert!(!line.contains("http"));
    }

    #[test]
    fn line_joins_text_and_markers() {
        let record = GroupMessage {
            at: 1000,
            sender: "A".to_string(),
            sender_name: String::new(),
            content: "看这个".to_string(),
            media: vec![MediaNote::Image, MediaNote::File],
            photos: Vec::new(),
        };
        let line = record.line();
        assert!(line.contains("看这个 [图片] [文件]"), "实际：{line}");
    }

    #[test]
    fn file_keys_cannot_escape_the_directory() {
        // `..` `/` `..` `/` `.` 共 6 个字符被替换成下划线，剩下 `etc/passwd` 的 `/` 也换掉。
        assert_eq!(file_key("../../etc/passwd"), "______etc_passwd");
        assert_eq!(file_key("ABC123"), "ABC123");
    }
}
