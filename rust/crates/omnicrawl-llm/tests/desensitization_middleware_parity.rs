//! middleware 编排件与 oneshot 的跨语言 parity：期望值来自 Python 真实现。
//!
//! 覆盖逐消息屏蔽（文本块 / 工具调用参数 / 工具结果 / 思考内容 / system 与图片块的豁免）、
//! 消息文本收集、工具参数里「已分配但无法还原」的序号扫描，以及一次性脱敏器的
//! 屏蔽 → 还原 → 注销全链路（含严格还原的错误文案）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omnicrawl_llm::desensitization::{
    assigned_but_unresolved, build_enabled_rules, iter_message_texts, mask_message,
    DesensitizationStats, MaskContext, OneShotMasker, OneshotOptions, PatternRule,
    SensitiveMatcher, SequenceRegistry,
};
use omnicrawl_protocol::ConversationMessage;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/desensitization_middleware_parity.json");

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

/// 规则集合跟着 fixture 走：Python 侧按 `[desensitization]` 的开关裁剪，内核侧收同一组类别。
fn enabled_rules(fixture: &Value) -> Vec<PatternRule> {
    let categories: Vec<&str> = fixture["enabled_categories"]
        .as_array()
        .expect("enabled_categories")
        .iter()
        .map(|item| item.as_str().expect("enabled_categories"))
        .collect();
    build_enabled_rules(&categories)
}

#[test]
fn message_masking_matches_python() {
    let fixture = fixture();
    let extras = string_list(&fixture, "extra_keys");
    let exempts = string_list(&fixture, "exempt_keys");
    let matcher = SensitiveMatcher::new(&extras, &exempts);
    let rules = enabled_rules(&fixture);
    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));
    let (mut cycle, _) = registry.begin_cycle("middleware 对照");
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
        gitleaks_rules: &[],
    };

    for case in fixture["messages"].as_array().expect("messages") {
        let message: ConversationMessage =
            serde_json::from_value(case["message"].clone()).expect("消息反序列化");
        let masked = mask_message(&message, &mut ctx);
        assert_eq!(
            json!(serde_json::to_value(&masked).expect("序列化")),
            case["masked"],
            "屏蔽结果（{}）",
            case["note"].as_str().unwrap_or("")
        );
        let texts: Vec<String> = iter_message_texts(&message);
        assert_eq!(
            json!(texts),
            case["texts"],
            "消息文本收集（{}）",
            case["note"].as_str().unwrap_or("")
        );
    }
}

#[test]
fn unreadable_placeholder_scan_matches_python() {
    let fixture = fixture();
    let extras = string_list(&fixture, "extra_keys");
    let exempts = string_list(&fixture, "exempt_keys");
    let matcher = SensitiveMatcher::new(&extras, &exempts);
    let rules = enabled_rules(&fixture);
    let leak = &fixture["leak"];
    let secret_message: ConversationMessage =
        serde_json::from_value(leak["secret_message"].clone()).expect("消息反序列化");

    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));
    let (mut cycle, _) = registry.begin_cycle("middleware 对照");
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
        gitleaks_rules: &[],
    };
    let masked = mask_message(&secret_message, &mut ctx);
    assert_eq!(
        json!(serde_json::to_value(&masked).expect("序列化")),
        leak["secret_masked"],
        "登记用的屏蔽结果"
    );
    let leaked = assigned_but_unresolved(&registry, &leak["value"]);
    assert_eq!(json!(leaked), leak["leaked"], "已分配但无法还原的序号");
}

#[test]
fn oneshot_matches_python() {
    let fixture = fixture();
    let extras = string_list(&fixture, "extra_keys");
    let exempts = string_list(&fixture, "exempt_keys");
    for scenario in fixture["oneshot"].as_array().expect("oneshot") {
        let strict = scenario["strict"].as_bool().expect("strict");
        let options = OneshotOptions {
            strict_restore: strict,
            ..OneshotOptions::default()
        };
        let registry = SequenceRegistry::with_sequence_source(source(counter(0)));
        let mut masker = OneShotMasker::with_registry(
            options,
            enabled_rules(&fixture),
            &extras,
            &exempts,
            registry,
        );
        for step in scenario["steps"].as_array().expect("steps") {
            let text = step["text"].as_str().expect("text");
            if step.get("masked").is_some() {
                let masked = masker.mask(text);
                assert_eq!(json!(masked), step["masked"], "屏蔽结果（{text:?}）");
                match masker.restore(&masked) {
                    Ok(restored) => {
                        assert_eq!(json!(restored), step["restored"], "还原结果（{text:?}）")
                    }
                    Err(error) => assert_eq!(
                        json!(error.message()),
                        step["error"],
                        "严格还原错误（{text:?}）"
                    ),
                }
            } else {
                match masker.restore(text) {
                    Ok(restored) => assert_eq!(
                        json!(restored),
                        step["restored"],
                        "未知序号还原（{text:?}）"
                    ),
                    Err(error) => assert_eq!(
                        json!(error.message()),
                        step["error"],
                        "未知序号严格错误（{text:?}）"
                    ),
                }
            }
        }
        assert_eq!(
            json!({
                "values_masked": masker.stats().values_masked,
                "skipped_values": masker.stats().skipped_values,
                "rules_masked": masker.stats().rules_masked,
                "entropy_masked": masker.stats().entropy_masked,
                "sequence_reuses": masker.stats().sequence_reuses,
            }),
            scenario["stats"],
            "一次性脱敏器的审计计数"
        );
        masker.close();
        let after = &scenario["after_close"];
        let text = after["text"].as_str().expect("text");
        match masker.restore(text) {
            Ok(restored) => assert_eq!(json!(restored), after["restored"], "注销后还原"),
            Err(error) => assert_eq!(json!(error.message()), after["error"], "注销后严格错误"),
        }
    }
}
