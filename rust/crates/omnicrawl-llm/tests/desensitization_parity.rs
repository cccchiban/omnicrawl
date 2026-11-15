//! 消息脱敏「序号注册表」的跨语言 parity：期望值来自 Python `registry.py`。
//!
//! 两侧都注入自增计数器让序号确定；周期号是进程级全局的，只比对「复用即同号、换请求即换号」
//! 这类性质；指纹用进程级随机盐，只比对同值/异值性质与长度。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omnicrawl_llm::desensitization::{
    collect_placeholder_numbers, find_placeholders, format_placeholder, has_placeholder_prefix,
    sequence_fingerprint, DesensitizationStats, SequenceRegistry, SessionSequenceCache,
    StableSequenceIndex, StreamRestorer,
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

/// 旧的纯数字占位符必须仍能解析。
///
/// 协议已改为单词组标签（`｛Desensitized:red_tiny_male_cat｝`），但历史消息、旧会话转录与
/// 早先保存的测试夹具里仍有 `｛Desensitized:7｝` 这种形式——解析器不认它们，那些序号就再也
/// 还原不回来。这条用例是兼容分支的守卫：删掉兼容逻辑会在这里红。
#[test]
fn legacy_numeric_placeholders_still_parse() {
    for (text, seq) in [
        ("\u{ff5b}Desensitized:7\u{ff5d}", 7_u64),
        ("前缀 \u{ff5b}Desensitized:123\u{ff5d} 后缀", 123),
        ("\u{ff5b}DESENSITIZED:9\u{ff5d}", 9),
        ("{Desensitized:42}", 42),
    ] {
        let found = find_placeholders(text);
        assert_eq!(found.len(), 1, "应解析出一个占位符：{text:?}");
        let (start, end, parsed) = found[0];
        assert_eq!(parsed, seq, "序号：{text:?}");
        assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
    }
}

/// 新形式与旧形式在同一段文本里共存时都能解析，且各自拿到正确序号。
#[test]
fn word_labels_and_legacy_numbers_coexist() {
    let worded = format_placeholder(1);
    let text = format!("{worded} 与 \u{ff5b}Desensitized:7\u{ff5d}");
    let found = find_placeholders(&text);
    assert_eq!(found.len(), 2, "两种形式各解析一个");
    assert_eq!(found[0].2, 1, "单词组标签解出 1");
    assert_eq!(found[1].2, 7, "旧数字解出 7");
}

/// 端到端：出站屏蔽产出单词组标签，模型原样复述后能还原成原文；
/// 旧数字形式同样能还原——这正是本次改动要保证的两件事。
#[test]
fn end_to_end_mask_and_restore_both_formats() {
    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));
    let (mut cycle, _) = registry.begin_cycle("端到端");
    let secret = "sk-live-8f3a2b1c";
    let (seq, _) = cycle.seq_for_value(secret);

    let label = format_placeholder(seq);
    assert!(
        label.contains("Desensitized:"),
        "占位符带固定标记：{label}"
    );
    // 生成端不应再产出纯数字形式
    let inside = label
        .trim_start_matches(['\u{ff5b}', '{'])
        .trim_end_matches(['\u{ff5d}', '}'])
        .trim_start_matches("Desensitized:")
        .trim();
    assert!(
        !inside.bytes().all(|b| b.is_ascii_digit()),
        "生成端已改为单词组，不再输出纯数字：{label}"
    );

    let mut stats = DesensitizationStats::default();

    // 模型原样复述新形式：还原成原文
    let mut restorer = StreamRestorer::new(&cycle, &mut stats, false);
    let restored = restorer.restore_string(&label).expect("还原新形式");
    assert_eq!(restored, secret, "新形式应还原为原文");
    drop(restorer);

    // 模型输出旧数字形式：同样还原（历史消息兼容）
    let legacy = format!("\u{ff5b}Desensitized:{seq}\u{ff5d}");
    let mut restorer = StreamRestorer::new(&cycle, &mut stats, false);
    let restored = restorer.restore_string(&legacy).expect("还原旧形式");
    assert_eq!(restored, secret, "旧数字形式也应还原为原文");
}
