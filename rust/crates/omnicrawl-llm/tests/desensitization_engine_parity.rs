//! 消息脱敏「匹配引擎」的跨语言 parity：期望值来自 Python `omnicrawl/llm/desensitization/engine.py`。
//!
//! 覆盖三层替换（结构层 → 值类型规则层 → 熵兜底）、已解析结构体的递归屏蔽，以及各层判定函数：
//! 键名归一与命中、跳过规则、形态白名单、候选判定、词形判定与扫描区间。期望值由生成器把同一批
//! 语料喂给 Python 真实现后记下（含逐步累积的审计计数）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omnicrawl_llm::desensitization::engine::is_word_shaped_letters;
use omnicrawl_llm::desensitization::{
    builtin_rules, find_entropy_spans, is_entropy_candidate, is_entropy_exempt,
    mask_structured_value, mask_text, normalize_key, should_skip_value, DesensitizationStats,
    MaskContext, SensitiveMatcher, SequenceRegistry,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/desensitization_engine_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn counter(start: u64) -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(start))
}

fn source(shared: Arc<AtomicU64>) -> impl Fn() -> u64 + Send + Sync + 'static {
    move || shared.fetch_add(1, Ordering::SeqCst) + 1
}

fn string_list(fixture: &Value, field: &str) -> Vec<String> {
    fixture[field]
        .as_array()
        .expect(field)
        .iter()
        .map(|item| item.as_str().expect(field).to_string())
        .collect()
}

fn stats_json(stats: &DesensitizationStats) -> Value {
    json!({
        "values_masked": stats.values_masked,
        "skipped_values": stats.skipped_values,
        "rules_masked": stats.rules_masked,
        "entropy_masked": stats.entropy_masked,
    })
}

/// 重放一组文本用例：与生成器一样，同一个上下文逐条累积审计计数。
fn replay_texts(fixture: &Value, field: &str, pure_letters: bool, pure_digits: bool) {
    let extras = string_list(fixture, "extra_keys");
    let exempts = string_list(fixture, "exempt_keys");
    let matcher = SensitiveMatcher::new(&extras, &exempts);
    let rules = builtin_rules();
    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));
    let (mut cycle, _) = registry.begin_cycle("引擎对照");
    let mut stats = DesensitizationStats::default();
    let mut ctx = MaskContext {
        matcher: &matcher,
        cycle: &mut cycle,
        stats: &mut stats,
        entropy_enabled: true,
        entropy_min_length: 20,
        entropy_min_bits: 3.5,
        entropy_pure_letters: pure_letters,
        entropy_pure_digits: pure_digits,
        pattern_rules: &rules,
    };

    for case in fixture[field].as_array().expect(field) {
        let text = case["text"].as_str().expect("text");
        let masked = mask_text(text, &mut ctx);
        assert_eq!(
            json!(masked),
            case["masked"],
            "{field} 的屏蔽结果（{} / {text:?}）",
            case["note"].as_str().unwrap_or("")
        );
        assert_eq!(
            stats_json(ctx.stats),
            case["stats"],
            "{field} 的审计计数（{text:?}）"
        );
    }
}

#[test]
fn mask_text_matches_python() {
    replay_texts(&fixture(), "texts", false, false);
}

#[test]
fn mask_text_with_single_class_switches_matches_python() {
    replay_texts(&fixture(), "pure_texts", true, true);
}

#[test]
fn mask_structured_value_matches_python() {
    let fixture = fixture();
    let extras = string_list(&fixture, "extra_keys");
    let exempts = string_list(&fixture, "exempt_keys");
    let matcher = SensitiveMatcher::new(&extras, &exempts);
    let rules = builtin_rules();
    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));
    let (mut cycle, _) = registry.begin_cycle("引擎对照");
    let mut stats = DesensitizationStats::default();
    let mut ctx = MaskContext {
        matcher: &matcher,
        cycle: &mut cycle,
        stats: &mut stats,
        entropy_enabled: true,
        entropy_min_length: 20,
        entropy_min_bits: 3.5,
        entropy_pure_letters: false,
        entropy_pure_digits: false,
        pattern_rules: &rules,
    };
    for case in fixture["structured"].as_array().expect("structured") {
        let value = &case["value"];
        let masked = mask_structured_value(value, &mut ctx);
        assert_eq!(json!(masked), case["masked"], "结构屏蔽（{value}）");
    }
}

#[test]
fn key_normalization_and_matching_match_python() {
    let fixture = fixture();
    let extras = string_list(&fixture, "extra_keys");
    let exempts = string_list(&fixture, "exempt_keys");
    let matcher = SensitiveMatcher::new(&extras, &exempts);
    for case in fixture["normalize_key"].as_array().expect("normalize_key") {
        let key = case["key"].as_str().expect("key");
        assert_eq!(
            json!(normalize_key(key)),
            case["normalized"],
            "归一化（{key:?}）"
        );
    }
    for case in fixture["is_sensitive"].as_array().expect("is_sensitive") {
        let key = case["key"].as_str().expect("key");
        assert_eq!(
            json!(matcher.is_sensitive(key)),
            case["sensitive"],
            "键名命中（{key:?}）"
        );
    }
    for case in fixture["should_skip"].as_array().expect("should_skip") {
        let value = case["value"].as_str().expect("value");
        assert_eq!(
            json!(should_skip_value(value)),
            case["skip"],
            "跳过规则（{value:?}）"
        );
    }
}

#[test]
fn entropy_predicates_match_python() {
    let fixture = fixture();
    for case in fixture["entropy_exempt"]
        .as_array()
        .expect("entropy_exempt")
    {
        let token = case["token"].as_str().expect("token");
        assert_eq!(
            json!(is_entropy_exempt(token)),
            case["exempt"],
            "形态白名单（{token:?}）"
        );
    }
    for case in fixture["word_shaped"].as_array().expect("word_shaped") {
        let token = case["token"].as_str().expect("token");
        assert_eq!(
            json!(is_word_shaped_letters(token)),
            case["word_shaped"],
            "词形判定（{token:?}）"
        );
    }
    for case in fixture["entropy_candidate"].as_array().expect("candidate") {
        let token = case["token"].as_str().expect("token");
        let actual = is_entropy_candidate(
            token,
            case["min_length"].as_u64().expect("min_length") as usize,
            case["min_bits"].as_f64().expect("min_bits"),
            case["pure_letters"].as_bool().expect("pure_letters"),
            case["pure_digits"].as_bool().expect("pure_digits"),
        );
        assert_eq!(json!(actual), case["candidate"], "候选判定（{token:?}）");
    }
}

#[test]
fn entropy_spans_match_python() {
    let fixture = fixture();
    for case in fixture["spans"].as_array().expect("spans") {
        let text = case["text"].as_str().expect("text");
        let spans: Vec<Value> = find_entropy_spans(
            text,
            case["min_length"].as_u64().expect("min_length") as usize,
            case["min_bits"].as_f64().expect("min_bits"),
            false,
            false,
        )
        .into_iter()
        .map(|(start, end)| json!([text[..start].chars().count(), text[..end].chars().count()]))
        .collect();
        assert_eq!(json!(spans), case["spans"], "扫描区间（{text:?}）");
    }
}

/// 三条语料通道都非空，且每条文本用例都带期望结果（防止新增语料被静默跳过）。
#[test]
fn every_text_case_is_replayed() {
    let fixture = fixture();
    for field in ["texts", "pure_texts"] {
        let cases = fixture[field].as_array().expect(field);
        assert!(!cases.is_empty(), "{field} 不应为空");
        for case in cases {
            assert!(case.get("masked").is_some(), "{field} 的用例缺少期望结果");
            assert!(case.get("stats").is_some(), "{field} 的用例缺少审计计数");
        }
    }
    assert!(!fixture["structured"]
        .as_array()
        .expect("structured")
        .is_empty());
}
