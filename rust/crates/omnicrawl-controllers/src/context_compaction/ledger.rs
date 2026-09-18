//! `omnicrawl/agent/context_compaction/ledger.py`：测量事件的 Token 账本序列化。

use serde_json::{Map, Value};

use super::policy::{ContextBudgetSnapshot, TokenUsageSample};

/// 测量载荷的 Schema 版本。
pub const MEASUREMENT_SCHEMA_VERSION: i64 = 1;

/// 把预算快照转为不含会话正文的可持久化诊断载荷。
#[derive(Debug, Clone, Default)]
pub struct UsageLedger;

impl UsageLedger {
    pub fn measurement_payload(
        &self,
        snapshot: &ContextBudgetSnapshot,
        usage: &TokenUsageSample,
    ) -> Value {
        let mut map = Map::new();
        map.insert(
            "schema_version".to_string(),
            Value::from(MEASUREMENT_SCHEMA_VERSION),
        );
        map.insert("mode".to_string(), Value::from("measurement_only"));
        if let Value::Object(snapshot) = snapshot.to_dict() {
            for (key, value) in snapshot {
                map.insert(key, value);
            }
        }
        map.insert("usage".to_string(), usage.to_dict());
        Value::Object(map)
    }
}
