//! 消息脱敏「序号注册表」的跨语言 parity：期望值来自 Python `registry.py`。
//!
//! 两侧都注入自增计数器让序号确定；周期号是进程级全局的，只比对「复用即同号、换请求即换号」
//! 这类性质；指纹用进程级随机盐，只比对同值/异值性质与长度。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omnicrawl_llm::desensitization::{
    collect_placeholder_numbers, find_placeholders, format_placeholder, has_placeholder_prefix,
    sequence_fingerprint, SequenceRegistry, SessionSequenceCache, StableSequenceIndex,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/desensitization_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn counter(start: u64) -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(start))
}

fn source(shared: Arc<AtomicU64>) -> impl Fn() -> u64 + Send + Sync + 'static {
    move || shared.fetch_add(1, Ordering::SeqCst) + 1
}

#[test]
fn placeholder_protocol_matches_python() {
    let fixture = fixture();
    let formatted: Vec<String> = [1_u64, 12, 999]
        .iter()
        .map(|seq| format_placeholder(*seq))
        .collect();
    assert_eq!(json!(formatted), fixture["formatted"], "规范占位符");

    for case in fixture["placeholders"].as_array().expect("placeholders") {
        let text = case["text"].as_str().expect("text");
        let found: Vec<Value> = find_placeholders(text)
            .into_iter()
            .map(|(start, end, seq)| json!({"text": &text[start..end], "seq": seq}))
            .collect();
        assert_eq!(json!(found), case["found"], "解析 {text:?}");
    }

    let prefixes: Vec<bool> = fixture["placeholders"]
        .as_array()
        .expect("placeholders")
        .iter()
        .map(|case| has_placeholder_prefix(case["text"].as_str().expect("text")))
        .collect();
    assert_eq!(json!(prefixes), fixture["prefixes"], "疑似前缀识别");

    let texts: Vec<&str> = fixture["placeholders"]
        .as_array()
        .expect("placeholders")
        .iter()
        .map(|case| case["text"].as_str().expect("text"))
        .collect();
    let mut collected: Vec<u64> = collect_placeholder_numbers(texts).into_iter().collect();
    collected.sort_unstable();
    assert_eq!(json!(collected), fixture["collected"], "收集已有序号");
}

#[test]
fn fingerprints_keep_same_value_same_slot() {
    let fixture = fixture();
    let same = sequence_fingerprint("同一个值");
    assert_eq!(
        json!(same == sequence_fingerprint("同一个值")),
        fixture["fingerprints"]["same"]
    );
    assert_eq!(
        json!(same != sequence_fingerprint("另一个值")),
        fixture["fingerprints"]["different"]
    );
    assert_eq!(json!(same.len()), fixture["fingerprints"]["length"]);
}

#[test]
fn stable_index_matches_python() {
    let fixture = fixture();
    let shared = counter(99);
    let index = StableSequenceIndex::with_sequence_source(source(shared));
    for step in fixture["index_steps"].as_array().expect("index_steps") {
        let value = step["value"].as_str().expect("value");
        let reserved: HashSet<u64> = step["reserved"]
            .as_array()
            .expect("reserved")
            .iter()
            .map(|item| item.as_u64().expect("序号"))
            .collect();
        let (seq, reused) = index.sequence_for(value, &reserved);
        assert_eq!(json!(seq), step["seq"], "序号 {value}");
        assert_eq!(json!(reused), step["reused"], "复用标记 {value}");
    }
    assert_eq!(
        json!(index.size()),
        fixture["index_state"]["size"],
        "已登记值数量"
    );
    assert_eq!(
        json!(index.assigned(102)),
        fixture["index_state"]["assigned_102"],
        "分配过 102"
    );
}

#[test]
fn cycle_matches_python() {
    let fixture = fixture();
    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));
    let (mut cycle, reused) = registry.begin_cycle("请求 A");
    assert!(!reused, "首次开始周期不应是复用");

    for step in fixture["cycle_steps"].as_array().expect("cycle_steps") {
        let value = step["value"].as_str().expect("value");
        let (seq, first) = cycle.seq_for_value(value);
        assert_eq!(json!(seq), step["seq"], "序号 {value}");
        assert_eq!(json!(first), step["first"], "首次登记 {value}");
    }
    cycle.adopt("值三", 777);
    cycle.adopt("值三", 777);

    let expected = &fixture["cycle_close"];
    assert_eq!(
        json!(cycle.stable_reuses),
        expected["stable_reuses"],
        "跨周期复用计数"
    );
    assert_eq!(
        json!(cycle.lookup(777)),
        expected["lookup_known"],
        "还原命中"
    );
    assert_eq!(
        json!(cycle.lookup(999)),
        expected["lookup_unknown"],
        "未注册序号"
    );
    let pairs: Vec<Value> = cycle
        .pairs_from(1)
        .into_iter()
        .map(|(seq, value)| json!([seq, value]))
        .collect();
    assert_eq!(
        json!(pairs),
        expected["pairs_from_1"],
        "从第 2 条起的登记对"
    );

    cycle.close();
    let after = &fixture["cycle_after_close"];
    assert_eq!(json!(cycle.closed), after["closed"], "关闭标记");
    let pairs: Vec<Value> = cycle
        .pairs_from(0)
        .into_iter()
        .map(|(seq, value)| json!([seq, value]))
        .collect();
    assert_eq!(json!(pairs), after["pairs"], "关闭后共享映射仍在");
}

#[test]
fn session_cache_matches_python() {
    let fixture = fixture();
    let mut cache = SessionSequenceCache::new("会话一");
    cache.entries.push((1, "原文一".to_string()));
    cache.rebind("");
    assert_eq!(
        json!(cache
            .entries
            .iter()
            .map(|(seq, value)| json!([seq, value]))
            .collect::<Vec<Value>>()),
        fixture["cache"]["after_unknown"],
        "未知会话标识不算变更"
    );
    cache.rebind("会话二");
    assert_eq!(
        json!(cache
            .entries
            .iter()
            .map(|(seq, value)| json!([seq, value]))
            .collect::<Vec<Value>>()),
        fixture["cache"]["after_change"],
        "会话切换应丢弃原文"
    );
    cache.entries.push((2, "原文二".to_string()));
    cache.clear();
    assert_eq!(
        json!(cache
            .entries
            .iter()
            .map(|(seq, value)| json!([seq, value]))
            .collect::<Vec<Value>>()),
        fixture["cache"]["after_clear"],
        "会话结束应清空"
    );
}

#[test]
fn registry_lifecycle_matches_python() {
    let fixture = fixture();
    let expected = &fixture["registry"];
    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));

    let (first, first_reused) = registry.begin_cycle("同请求");
    registry.mark_masked(&first.cycle_id, "屏蔽副本");
    let (second, second_reused) = registry.begin_cycle("同请求");
    let (third, third_reused) = registry.begin_cycle("新请求");
    let open_after_switch = registry.open_cycle_count();
    registry.close_cycle(&third);
    let open_after_close = registry.open_cycle_count();
    registry.drop_all();
    let open_after_drop = registry.open_cycle_count();

    assert_eq!(
        json!(first.cycle_id == second.cycle_id),
        expected["cycle_ids_equal_on_reuse"],
        "重试复用应当是同一个周期"
    );
    assert_eq!(json!(first_reused), expected["first_reused"]);
    assert_eq!(json!(second_reused), expected["second_reused"]);
    assert_eq!(json!(third_reused), expected["third_reused"]);
    assert_eq!(
        json!(third.cycle_id != first.cycle_id),
        expected["third_cycle_distinct"]
    );
    assert_eq!(json!(open_after_switch), expected["open_after_switch"]);
    assert_eq!(json!(open_after_close), expected["open_after_close"]);
    assert_eq!(json!(open_after_drop), expected["open_after_drop"]);
    assert_eq!(
        json!(registry.stats.cycles_started),
        expected["stats"]["cycles_started"]
    );
    assert_eq!(
        json!(registry.stats.cycles_reused),
        expected["stats"]["cycles_reused"]
    );
}
