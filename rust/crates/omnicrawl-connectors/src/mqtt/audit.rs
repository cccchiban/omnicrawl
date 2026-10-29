//! 审计日志：每次决策一行 JSON，既是事后追溯，也是下一次唤醒的「最近动作」来源。
//!
//! 触发原因、现场快照、模型输出、下发指令、回执结果都在一行里，坏文件只影响回放，
//! 不影响主循环——写失败只记诊断，绝不让审计把控制回路拖停。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// 追加式的 JSONL 审计文件。
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// 打开（必要时建目录）：路径为空时由调用方给出默认位置。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建审计目录失败（{}）：{error}", parent.display()))?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一行：紧凑 JSON + 换行，失败返回原因由调用方决定是否只记诊断。
    pub fn append(&self, entry: &Value) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| format!("打开审计文件失败（{}）：{error}", self.path.display()))?;
        let line = serde_json::to_string(entry).map_err(|error| error.to_string())?;
        writeln!(file, "{line}")
            .map_err(|error| format!("写审计文件失败（{}）：{error}", self.path.display()))
    }

    /// 读最近若干条（可按棚过滤）：只回放尾部，不整文件解析。
    pub fn recent(&self, zone: Option<&str>, limit: usize) -> Vec<Value> {
        if limit == 0 {
            return Vec::new();
        }
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut entries: Vec<Value> = Vec::new();
        for line in text.lines().rev() {
            if entries.len() >= limit {
                break;
            }
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(zone) = zone {
                if value.get("zone").and_then(Value::as_str) != Some(zone) {
                    continue;
                }
            }
            entries.push(value);
        }
        entries.reverse();
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn append_then_read_back_in_order() {
        let dir = std::env::temp_dir().join(format!("oc-mqtt-audit-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let log = AuditLog::open(&path).expect("open");
        for index in 0..3 {
            log.append(&json!({"zone": "3", "seq": index})).expect("append");
        }
        log.append(&json!({"zone": "1", "seq": 9})).expect("append");

        let all = log.recent(None, 2);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1]["seq"], 9);
        assert_eq!(all[0]["seq"], 2);

        let zone_three = log.recent(Some("3"), 10);
        assert_eq!(zone_three.len(), 3);
        assert_eq!(zone_three[0]["seq"], 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let path = std::env::temp_dir().join("oc-mqtt-audit-absent/none.jsonl");
        let log = AuditLog::open(&path).expect("open");
        assert!(log.recent(None, 5).is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
