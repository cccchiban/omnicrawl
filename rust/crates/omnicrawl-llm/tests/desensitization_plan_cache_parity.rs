//! 消息脱敏「屏蔽计划缓存」的跨语言 parity：期望值来自 Python
//! `omnicrawl/llm/desensitization/plan_cache.py`。
//!
//! 覆盖四组：文本指纹（SHA-256）、计划构建器（完整性判定与空阶段裁剪）、有界 LRU 缓存
//! （命中 / 淘汰 / 失效 / 清空 / 停用与计数口径），以及默认容量与阶段标签表。
//!
//! 生成器 `rust/tools/gen_desensitization_plan_cache_fixture.py` 按文件路径加载该模块
//! （Python 侧 `engine.py` / `middleware.py` 当前带着未合并的冲突标记，包导入会直接失败），
//! 详见生成器 docstring。改了任一侧实现都要重跑生成脚本再跑本测试。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omnicrawl_llm::desensitization::{
    stage_counter_field, text_key, MaskPlan, MaskPlanBuilder, MaskPlanCache, PlanSpan,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, STAGE_COUNTER_FIELDS,
};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/desensitization_plan_cache_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// 指纹按十六进制串比对：摘要本身是两侧共享的观测口径。
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn text_keys_match_python() {
    let fixture = fixture();
    let cases = fixture["text_keys"].as_array().expect("text_keys");
    assert!(!cases.is_empty());
    for case in cases {
        let text = case["text"].as_str().expect("text");
        assert_eq!(
            hex(&text_key(text)),
            case["digest"].as_str().expect("digest"),
            "文本指纹（{text:?}）"
        );
    }
}

#[test]
fn defaults_and_stage_counter_fields_match_python() {
    let fixture = fixture();
    let defaults = &fixture["defaults"];
    assert_eq!(
        DEFAULT_MAX_ENTRIES,
        defaults["max_entries"].as_u64().unwrap() as usize
    );
    assert_eq!(
        DEFAULT_MAX_BYTES,
        defaults["max_bytes"].as_u64().unwrap() as usize
    );

    let expected = defaults["stage_counter_fields"].as_object().expect("表");
    assert_eq!(STAGE_COUNTER_FIELDS.len(), expected.len());
    for (label, field) in expected {
        assert_eq!(
            stage_counter_field(label),
            Some(field.as_str().expect("计数口径")),
            "阶段标签 {label} 的计数口径"
        );
    }
    assert_eq!(stage_counter_field("无标签"), None);
}

/// 重放构建器的操作序列，产出与生成器同形的 JSON。
fn replay_builder(case: &Value) -> Value {
    let mut builder = MaskPlanBuilder::new();
    for op in case["ops"].as_array().expect("ops") {
        let name = op[0].as_str().expect("op 名");
        match name {
            "begin_stage" => builder.begin_stage(op[1].as_str().expect("阶段标签")),
            "record" => builder.record(
                op[1].as_u64().expect("start") as usize,
                op[2].as_u64().expect("end") as usize,
                op[3].as_u64().expect("seq"),
            ),
            "note_registration" => builder.note_registration(),
            other => panic!("未知构建器操作：{other}"),
        }
    }
    let plan = builder.build();
    match plan {
        None => Value::Null,
        Some(plan) => plan_json(&plan),
    }
}

fn plan_json(plan: &MaskPlan) -> Value {
    let stages: Vec<Value> = plan
        .stages
        .iter()
        .map(|stage| {
            Value::Array(
                stage
                    .iter()
                    .map(|span| {
                        json!({
                            "start": span.start,
                            "end": span.end,
                            "seq": span.seq,
                            "counter": span.counter,
                        })
                    })
                    .collect(),
            )
        })
        .collect();
    json!({ "stages": stages, "span_count": plan.span_count() })
}

#[test]
fn builder_cases_match_python() {
    let fixture = fixture();
    for case in fixture["builder_cases"].as_array().expect("builder_cases") {
        assert_eq!(
            replay_builder(case),
            case["plan"],
            "构建器（{}）",
            case["note"].as_str().unwrap_or("")
        );
    }
}

/// 把紧凑写法还原成计划对象：阶段列表，阶段内是 `[start, end, seq, counter]`。
fn plan_from_spec(spec: &Value) -> MaskPlan {
    let stages = spec
        .as_array()
        .expect("阶段表")
        .iter()
        .map(|stage| {
            stage
                .as_array()
                .expect("阶段")
                .iter()
                .map(|span| {
                    let items = span.as_array().expect("区间");
                    PlanSpan {
                        start: items[0].as_u64().expect("start") as usize,
                        end: items[1].as_u64().expect("end") as usize,
                        seq: items[2].as_u64().expect("seq"),
                        counter: items[3].as_str().expect("counter").to_string(),
                    }
                })
                .collect()
        })
        .collect();
    MaskPlan { stages }
}

fn stats_json(stats: &omnicrawl_llm::desensitization::PlanCacheStats) -> Value {
    let mut map = Map::new();
    map.insert("entries".into(), json!(stats.entries));
    map.insert("size_bytes".into(), json!(stats.size_bytes));
    map.insert("lookups".into(), json!(stats.lookups));
    map.insert("hits".into(), json!(stats.hits));
    map.insert("misses".into(), json!(stats.misses));
    map.insert("invalid".into(), json!(stats.invalid));
    map.insert("evictions".into(), json!(stats.evictions));
    Value::Object(map)
}

#[test]
fn cache_cases_match_python() {
    let fixture = fixture();
    let cases = fixture["cache_cases"].as_array().expect("cache_cases");
    assert!(!cases.is_empty());
    for case in cases {
        let note = case["note"].as_str().unwrap_or("");
        let cache = MaskPlanCache::new(
            case["max_entries"].as_u64().expect("max_entries") as usize,
            case["max_bytes"].as_u64().expect("max_bytes") as usize,
        );
        let mut results: Vec<bool> = Vec::new();
        for op in case["ops"].as_array().expect("ops") {
            match op[0].as_str().expect("op 名") {
                "put" => cache.put(op[1].as_str().expect("text"), plan_from_spec(&op[2])),
                "get" => results.push(cache.get(op[1].as_str().expect("text")).is_some()),
                "note_invalid" => cache.note_invalid(),
                "clear" => cache.clear(),
                other => panic!("未知缓存操作：{other}"),
            }
        }
        assert_eq!(json!(results), case["results"], "缓存返回值（{note}）");
        assert_eq!(
            stats_json(&cache.stats()),
            case["stats"],
            "缓存计数（{note}）"
        );
    }
}

/// 缓存是运行期对象：容量为默认值时构造出的实例应当是启用的。
#[test]
fn default_cache_is_enabled_and_starts_empty() {
    let cache = MaskPlanCache::default();
    assert!(cache.enabled());
    let stats = cache.stats();
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.size_bytes, 0);
    // 空文本不进缓存，也不计 lookup（与 Python 的 `not text` 早退一致）。
    cache.put("", MaskPlan { stages: Vec::new() });
    assert!(cache.get("").is_none());
    assert_eq!(cache.stats().lookups, 0);
}

/// 计划缓存与规则扫描缓存共用一份「计数只增不减」的观测口径（此处钉住类型即可）。
#[test]
fn stats_counters_are_atomic_enough_for_concurrent_readers() {
    let cache = Arc::new(MaskPlanCache::default());
    let shared = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for index in 0..4u64 {
        let cache = Arc::clone(&cache);
        let shared = Arc::clone(&shared);
        handles.push(std::thread::spawn(move || {
            let text = format!("API_KEY={index}");
            let mut builder = MaskPlanBuilder::new();
            builder.begin_stage("rules");
            builder.record(0, 3, index + 1);
            builder.note_registration();
            let plan = builder.build().expect("计划完整");
            cache.put(&text, plan);
            if cache.get(&text).is_some() {
                shared.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("线程未 panic");
    }
    assert_eq!(shared.load(Ordering::SeqCst), 4);
    assert_eq!(cache.stats().entries, 4);
}
