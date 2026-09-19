//! 飞书入站持久队列与跨重启去重（对齐 Python `omnicrawl/connectors/feishu_inbox.py`）。
//!
//! 为本地单 Agent 连接器提供「先持久化、后处理」的入站语义：
//!
//! - `enqueue`：事件先落盘（pending 日志）再登记内存；进程在任务处理中途退出时，
//!   重启后 `recover` 回放尚未确认的事件，避免消息在重启窗口丢失。
//! - 去重键默认用飞书 `message_id`（24 小时窗口，跨进程/重启生效）；`dedupe_key`
//!   可另外传重投递指纹。
//! - 文件损坏或不可写时降级为纯内存模式：进程内仍去重，只是不再跨重启持久化；
//!   所有写失败只记录状态，绝不阻断消息接收。
//!
//! 存储布局（每个目录一个队列实例）：
//!
//! ```text
//! <root>/
//!   pending.jsonl   # 已接收但尚未确认完成的事件（追加写）
//!   done.jsonl      # 已完成的去重键（追加写 + 启动时紧凑化）
//!   state.json      # 元数据（下一序号）
//! ```
//!
//! 与 Python 的差异：日志由调用方决定怎么记（内核连接器没有 Python 的 logging 设施）；
//! 文件句柄按次打开而不是常驻，语义等价（都是追加写 + flush，写失败即降级）。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::json;

/// 去重/待办记录保留窗口：24 小时，避免重连窗口内的重投递被当作新消息。
pub const DEFAULT_DEDUP_TTL_SECONDS: f64 = 24.0 * 60.0 * 60.0;
/// 单文件最大记录数；超过后启动紧凑化，避免日志无限增长。
pub const DEFAULT_MAX_RECORDS: usize = 10_000;
/// 每次紧凑化后保留窗口内的最新键。
pub const DEFAULT_COMPACT_KEEP: usize = 2_000;

/// pending/done 行版本；未来变更格式时据此迁移或丢弃。
const RECORD_VERSION: u64 = 1;

/// 入队后等待确认完成的一条事件。
#[derive(Debug, Clone, PartialEq)]
pub struct InboxRecord {
    pub seq: u64,
    pub event_id: String,
    pub dedupe_key: String,
    pub payload: Map<String, Value>,
    pub created_at: f64,
    pub version: u64,
}

struct InboxState {
    pending: Vec<InboxRecord>,
    done: HashMap<String, f64>,
    seq: u64,
    memory_only: bool,
}

/// 线程安全的持久入站队列。
pub struct FeishuInbox {
    root: Option<PathBuf>,
    dedup_ttl_seconds: f64,
    max_records: usize,
    compact_keep: usize,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
    state: Mutex<InboxState>,
}

fn system_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

impl FeishuInbox {
    /// 用系统时钟建队列；`root` 为 `None` 时是纯内存模式（不落盘）。
    pub fn open(
        root: Option<PathBuf>,
        dedup_ttl_seconds: f64,
        max_records: usize,
        compact_keep: usize,
    ) -> Self {
        Self::with_clock(
            root,
            dedup_ttl_seconds,
            max_records,
            compact_keep,
            Box::new(system_now),
        )
    }

    /// 出厂参数的队列：24 小时窗口、1 万条上限、紧凑化保留 2000 条。
    pub fn with_root(root: Option<PathBuf>) -> Self {
        Self::open(
            root,
            DEFAULT_DEDUP_TTL_SECONDS,
            DEFAULT_MAX_RECORDS,
            DEFAULT_COMPACT_KEEP,
        )
    }

    /// 可注入时钟的构造（对照与测试用）。
    pub fn with_clock(
        root: Option<PathBuf>,
        dedup_ttl_seconds: f64,
        max_records: usize,
        compact_keep: usize,
        now: Box<dyn Fn() -> f64 + Send + Sync>,
    ) -> Self {
        let mut memory_only = root.is_none();
        let root = match root {
            None => None,
            Some(directory) => {
                // 打开失败（只读目录/权限）→ 降级内存。
                let writable = fs::create_dir_all(&directory).is_ok()
                    && probe_writable(&directory.join("pending.jsonl"))
                    && probe_writable(&directory.join("done.jsonl"));
                if writable {
                    Some(directory)
                } else {
                    memory_only = true;
                    Some(directory)
                }
            }
        };

        let inbox = Self {
            root,
            dedup_ttl_seconds,
            max_records,
            compact_keep,
            now,
            state: Mutex::new(InboxState {
                pending: Vec::new(),
                done: HashMap::new(),
                seq: 0,
                memory_only,
            }),
        };
        if !memory_only {
            let mut state = inbox.state.lock().expect("队列锁");
            inbox.load_state(&mut state);
        }
        inbox
    }

    /// 启动时恢复：读取 state、pending 与 done，剔除过期/损坏行。
    fn load_state(&self, state: &mut InboxState) {
        let Some(root) = self.root.clone() else {
            return;
        };

        state.seq = fs::read_to_string(root.join("state.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.get("seq").and_then(scalar_int))
            .unwrap_or(0);

        let now = self.now_value();
        // 恢复 pending：损坏行跳过；仅保留窗口内记录，避免陈旧任务在长时间停机后突然执行。
        let mut recovered: Vec<InboxRecord> = Vec::new();
        for row in iter_json_lines(&root.join("pending.jsonl")) {
            let Some(record) = parse_record(&row) else {
                continue;
            };
            if now - record.created_at > self.dedup_ttl_seconds {
                continue;
            }
            recovered.push(record);
        }
        // 同一事件可能出现多条 pending，只保留每个 dedupe_key 最新一条。
        let mut by_key: HashMap<String, InboxRecord> = HashMap::new();
        for record in recovered {
            by_key.insert(record.dedupe_key.clone(), record);
        }
        state.pending = by_key.into_values().collect();
        state.pending.sort_by_key(|record| record.seq);
        if let Some(last) = state.pending.last() {
            state.seq = state.seq.max(last.seq);
        }

        // 恢复 done 去重表。
        let mut done: HashMap<String, f64> = HashMap::new();
        for row in iter_json_lines(&root.join("done.jsonl")) {
            let Ok(value) = serde_json::from_str::<Value>(&row) else {
                continue;
            };
            let key = value
                .get("key")
                .map(scalar_text)
                .unwrap_or_default()
                .trim()
                .to_string();
            let timestamp = value.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
            if !key.is_empty() && now - timestamp <= self.dedup_ttl_seconds {
                let entry = done.entry(key).or_insert(0.0);
                *entry = entry.max(timestamp);
            }
        }
        state.done = done;

        if state.done.len() >= self.max_records {
            self.compact(state);
        }
    }

    /// 先落盘再登记内存；返回是否应处理（`false` = 重复/空键）。
    pub fn enqueue(
        &self,
        event_id: &str,
        dedupe_key: Option<&str>,
        payload: Option<Map<String, Value>>,
    ) -> bool {
        let event_id = event_id.trim().to_string();
        if event_id.is_empty() {
            return false;
        }
        let key = dedupe_key
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| event_id.clone());
        let payload = payload.unwrap_or_default();
        let now = self.now_value();

        let mut state = self.state.lock().expect("队列锁");
        if seen_recent(&state.done, &key, now, self.dedup_ttl_seconds) {
            return false;
        }
        let seq = state.seq + 1;
        state.seq = seq;
        let record = InboxRecord {
            seq,
            event_id,
            dedupe_key: key.clone(),
            payload,
            created_at: now,
            version: RECORD_VERSION,
        };
        state.pending.push(record.clone());
        state.done.insert(key.clone(), now);
        if !state.memory_only {
            if let Some(root) = self.root.clone() {
                if !append_record(&root, &record) || !append_key(&root, &key, now) {
                    state.memory_only = true;
                }
            }
        }
        true
    }

    /// 任务处理完成（成功或失败都算终结），从待办移除。
    pub fn confirm(&self, dedupe_key: &str) {
        let key = dedupe_key.trim();
        if key.is_empty() {
            return;
        }
        let mut state = self.state.lock().expect("队列锁");
        let before = state.pending.len();
        state.pending.retain(|record| record.dedupe_key != key);
        // done 键已在 enqueue 时登记，无需重复追加；只是从待办移除。
        if !state.memory_only && state.pending.len() != before {
            if let Some(root) = self.root.clone() {
                if !rewrite_pending(&root, &state.pending) {
                    state.memory_only = true;
                }
            }
        }
    }

    /// 返回尚未确认的待办事件（按入队顺序）；调用方负责重新分发。
    pub fn recover(&self) -> Vec<InboxRecord> {
        self.state.lock().expect("队列锁").pending.clone()
    }

    pub fn is_duplicate(&self, dedupe_key: &str) -> bool {
        let key = dedupe_key.trim();
        if key.is_empty() {
            return false;
        }
        let state = self.state.lock().expect("队列锁");
        seen_recent(&state.done, key, self.now_value(), self.dedup_ttl_seconds)
    }

    pub fn pending_count(&self) -> usize {
        self.state.lock().expect("队列锁").pending.len()
    }

    pub fn memory_only(&self) -> bool {
        self.state.lock().expect("队列锁").memory_only
    }

    /// 当前内存里的去重键数量（紧凑化与恢复用）。
    pub fn done_count(&self) -> usize {
        self.state.lock().expect("队列锁").done.len()
    }

    /// 把 done 表按最新 ts 保留 `compact_keep` 条，重写 `done.jsonl`。
    fn compact(&self, state: &mut InboxState) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if state.memory_only {
            return;
        }
        let mut entries: Vec<(String, f64)> = state
            .done
            .iter()
            .map(|(key, timestamp)| (key.clone(), *timestamp))
            .collect();
        entries.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries.truncate(self.compact_keep);

        let mut text = String::new();
        for (key, timestamp) in &entries {
            text.push_str(&json::dumps(&json!({"key": key, "ts": timestamp})));
            text.push('\n');
        }
        state.done = entries.into_iter().collect();
        // 紧凑化失败只记状态，不降级（与 Python 一致）。
        let _ = write_atomic(&root.join("done.jsonl"), &text);
    }

    /// 关闭队列并持久化元数据。内核不持有常驻句柄，这里只写 `state.json`。
    pub fn close(&self) {
        let state = self.state.lock().expect("队列锁");
        if state.memory_only {
            return;
        }
        if let Some(root) = &self.root {
            let text = json::dumps(&json!({"seq": state.seq}));
            let _ = fs::write(root.join("state.json"), text);
        }
    }

    fn now_value(&self) -> f64 {
        (self.now)()
    }
}

fn seen_recent(done: &HashMap<String, f64>, key: &str, now: f64, ttl: f64) -> bool {
    match done.get(key) {
        Some(timestamp) => now - timestamp <= ttl,
        None => false,
    }
}

fn probe_writable(path: &Path) -> bool {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .is_ok()
}

fn iter_json_lines(path: &Path) -> Vec<String> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn parse_record(row: &str) -> Option<InboxRecord> {
    let value: Value = serde_json::from_str(row).ok()?;
    let object = value.as_object()?;
    let seq = object.get("seq").and_then(scalar_int).unwrap_or(0);
    let event_id = object.get("event_id").map(scalar_text).unwrap_or_default();
    let dedupe_key = object
        .get("dedupe_key")
        .map(scalar_text)
        .unwrap_or_default();
    let payload = object.get("payload").and_then(Value::as_object).cloned()?;
    let created_at = object
        .get("created_at")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let version = object.get("version").and_then(scalar_int).unwrap_or(0);
    if dedupe_key.is_empty() || event_id.is_empty() || version != RECORD_VERSION {
        return None;
    }
    Some(InboxRecord {
        seq,
        event_id,
        dedupe_key,
        payload,
        created_at,
        version,
    })
}

fn record_line(record: &InboxRecord) -> String {
    // 追加行是紧凑写法（Python 的 separators=(",", ":")），与紧凑化的 done 行不同。
    let mut line = serde_json::to_string(&json!({
        "version": record.version,
        "seq": record.seq,
        "event_id": record.event_id,
        "dedupe_key": record.dedupe_key,
        "created_at": record.created_at,
        "payload": Value::Object(record.payload.clone()),
    }))
    .expect("入站记录可序列化");
    line.push('\n');
    line
}

fn append_record(root: &Path, record: &InboxRecord) -> bool {
    append_text(&root.join("pending.jsonl"), &record_line(record))
}

fn append_key(root: &Path, key: &str, timestamp: f64) -> bool {
    let mut line =
        serde_json::to_string(&json!({"key": key, "ts": timestamp})).expect("去重键可序列化");
    line.push('\n');
    append_text(&root.join("done.jsonl"), &line)
}

fn append_text(path: &Path, text: &str) -> bool {
    use std::io::Write;
    let opened = fs::OpenOptions::new().create(true).append(true).open(path);
    match opened {
        Ok(mut handle) => handle
            .write_all(text.as_bytes())
            .and_then(|_| handle.flush())
            .is_ok(),
        Err(_) => false,
    }
}

fn rewrite_pending(root: &Path, pending: &[InboxRecord]) -> bool {
    let mut text = String::new();
    for record in pending {
        text.push_str(&record_line(record));
    }
    write_atomic(&root.join("pending.jsonl"), &text)
}

fn write_atomic(path: &Path, text: &str) -> bool {
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));
    if fs::write(&temporary, text).is_err() {
        return false;
    }
    fs::rename(&temporary, path).is_ok()
}

/// Python 的 `int(value)` 子集：整数直接取，浮点截断，字符串按十进制解析。
fn scalar_int(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().map(|value| value.max(0.0) as u64)),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        Value::Bool(true) => Some(1),
        Value::Bool(false) => Some(0),
        _ => None,
    }
}

/// Python 的 `str(value)` 子集：字符串原样，其余按 JSON 写法。
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "omnicrawl-inbox-unit-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn enqueue_persists_and_recover_returns_pending() {
        let root = temp_root("cycle");
        let inbox = FeishuInbox::with_root(Some(root.clone()));
        assert!(inbox.enqueue("e1", None, None));
        assert!(!inbox.enqueue("e1", None, None), "同键第二次应被去重");
        assert!(!inbox.enqueue("  ", None, None), "空事件 id 不入队");
        assert_eq!(inbox.pending_count(), 1);
        assert!(inbox.is_duplicate("e1"));

        let recovered = inbox.recover();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].event_id, "e1");
        assert_eq!(recovered[0].seq, 1);
        inbox.close();

        let reopened = FeishuInbox::with_root(Some(root.clone()));
        assert_eq!(reopened.pending_count(), 1, "重启后应回放未确认事件");
        assert!(reopened.is_duplicate("e1"), "重启后去重键仍在窗口内");
        reopened.confirm("e1");
        assert_eq!(reopened.pending_count(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_only_mode_keeps_in_process_dedup() {
        let inbox = FeishuInbox::with_root(None);
        assert!(inbox.memory_only());
        assert!(inbox.enqueue("e1", None, None));
        assert!(!inbox.enqueue("e1", None, None));
        assert_eq!(inbox.pending_count(), 1);
    }

    #[test]
    fn expired_records_are_dropped_on_load() {
        let root = temp_root("expired");
        let inbox = FeishuInbox::with_clock(
            Some(root.clone()),
            100.0,
            DEFAULT_MAX_RECORDS,
            DEFAULT_COMPACT_KEEP,
            Box::new(|| 1_000.0),
        );
        assert!(inbox.enqueue("e1", Some("k1"), None));
        assert!(inbox.enqueue("e2", Some("k2"), None));
        inbox.close();

        let later = FeishuInbox::with_clock(
            Some(root.clone()),
            100.0,
            DEFAULT_MAX_RECORDS,
            DEFAULT_COMPACT_KEEP,
            Box::new(|| 1_250.0),
        );
        assert_eq!(later.pending_count(), 0, "超出窗口的待办不再回放");
        assert!(!later.is_duplicate("k1"), "超出窗口的去重键失效");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_keys_collapse_to_latest_on_load() {
        let root = temp_root("collapse");
        let inbox = FeishuInbox::with_clock(
            Some(root.clone()),
            1_000.0,
            DEFAULT_MAX_RECORDS,
            DEFAULT_COMPACT_KEEP,
            Box::new(|| 1_000.0),
        );
        assert!(inbox.enqueue("e1", Some("k1"), None));
        // 直接往 pending 追加一条同键记录，模拟重连期间的重复入队。
        append_record(
            &root,
            &InboxRecord {
                seq: 5,
                event_id: "e1b".to_string(),
                dedupe_key: "k1".to_string(),
                payload: Map::new(),
                created_at: 1_000.0,
                version: RECORD_VERSION,
            },
        );
        inbox.close();

        let reopened = FeishuInbox::with_clock(
            Some(root.clone()),
            1_000.0,
            DEFAULT_MAX_RECORDS,
            DEFAULT_COMPACT_KEEP,
            Box::new(|| 1_000.0),
        );
        let recovered = reopened.recover();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].seq, 5, "同键只保留最新一条");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compact_keeps_newest_keys() {
        let root = temp_root("compact");
        let clock = std::sync::Arc::new(Mutex::new(1_000.0_f64));
        let reader = std::sync::Arc::clone(&clock);
        let inbox = FeishuInbox::with_clock(
            Some(root.clone()),
            DEFAULT_DEDUP_TTL_SECONDS,
            3,
            2,
            Box::new(move || *reader.lock().expect("时钟锁")),
        );
        for index in 0..4 {
            *clock.lock().expect("时钟锁") = 1_000.0 + f64::from(index);
            assert!(inbox.enqueue(&format!("e{index}"), Some(&format!("k{index}")), None));
        }
        assert_eq!(inbox.done_count(), 4, "写入期不做紧凑化");
        inbox.close();

        // 紧凑化发生在启动加载时（与 Python 一致）。
        let reopened = FeishuInbox::with_clock(
            Some(root.clone()),
            DEFAULT_DEDUP_TTL_SECONDS,
            3,
            2,
            Box::new(move || *clock.lock().expect("时钟锁")),
        );
        assert_eq!(reopened.done_count(), 2, "紧凑化后只留最新 2 条");
        assert!(reopened.is_duplicate("k3"));
        assert!(!reopened.is_duplicate("k1"));
        let _ = fs::remove_dir_all(&root);
    }
}
