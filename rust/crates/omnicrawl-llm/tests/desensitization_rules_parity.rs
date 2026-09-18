//! 消息脱敏「值类型规则层」的跨语言 parity：期望值来自 Python `omnicrawl/llm/desensitization/rules.py`。
//!
//! 覆盖全部 11 条内置规则（PEM 私钥 / 连接串 / 网址 / 邮箱 / 车牌 / 银行卡 / MAC / 内外网 IP）与整套
//! 规则语义：逐条规则的候选区间（含熵 / 豁免 / 校验 / 尾部收缩）、按优先级的整段扫描（重叠去重、
//! 按起点排序）、香农熵、Luhn 校验、邮箱豁免、内外网判定。数据集自带每条文本的期望命中，测试同时
//! 校验它，避免语料被写成空转。

use omnicrawl_llm::desensitization::rules::{
    is_example_domain, is_external_ip, is_internal_ip, is_luhn_valid, PatternRule, RuleMatch,
    TRAILING_TRIM_CHARS,
};
use omnicrawl_llm::desensitization::{builtin_rules, scan_pattern_rules, shannon_entropy_bits};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/desensitization_rules_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// Python 侧的区间是字符下标，内核侧是字节偏移：比对前统一折成字符下标。
fn char_index(text: &str, offset: usize) -> usize {
    text[..offset].chars().count()
}

fn hit_json(rule_id: &str, category: &str, text: &str, start: usize, end: usize) -> Value {
    json!({
        "rule": rule_id,
        "category": category,
        "start": char_index(text, start),
        "end": char_index(text, end),
        "value": &text[start..end],
    })
}

fn scan_json(matches: &[RuleMatch], text: &str) -> Vec<Value> {
    matches
        .iter()
        .map(|item| hit_json(item.rule_id, item.category, text, item.start, item.end))
        .collect()
}

fn candidates_json(rule: &PatternRule, text: &str) -> Vec<Value> {
    rule.find(text)
        .into_iter()
        .map(|(start, end)| hit_json(rule.rule_id, rule.category, text, start, end))
        .collect()
}

fn texts(fixture: &Value) -> &Vec<Value> {
    fixture["texts"].as_array().expect("texts")
}

#[test]
fn builtin_rules_match_python_declaration() {
    let fixture = fixture();
    let rules = builtin_rules();
    let ids: Vec<&str> = rules.iter().map(|rule| rule.rule_id).collect();
    assert_eq!(json!(ids), fixture["rule_ids"], "内置规则清单与顺序");
    assert_eq!(
        json!(rules.len()),
        fixture["rule_ids"].as_array().expect("rule_ids").len(),
        "规则条数"
    );
    assert_eq!(
        json!(TRAILING_TRIM_CHARS),
        fixture["trailing_trim_chars"],
        "尾部剪裁字符集"
    );
}

#[test]
fn categories_match_python_declaration() {
    let fixture = fixture();
    let ours: Vec<Value> = omnicrawl_llm::desensitization::rules::CATEGORY_CONFIG_FLAGS
        .iter()
        .map(|(category, flag)| json!([category, flag]))
        .collect();
    assert_eq!(json!(ours), fixture["categories"], "类别与配置字段映射");
}

#[test]
fn precedence_order_matches_python() {
    let fixture = fixture();
    let expected: Vec<Value> = fixture["texts"]
        .as_array()
        .expect("texts")
        .iter()
        .map(|text| text["expected"].clone())
        .collect();
    let actual: Vec<Value> = texts(&fixture)
        .iter()
        .map(|item| {
            let text = item["text"].as_str().expect("text");
            json!(scan_pattern_rules(text, builtin_rules())
                .iter()
                .map(|hit| hit.rule_id)
                .collect::<Vec<&str>>())
        })
        .collect();
    assert_eq!(
        actual, expected,
        "每条语料的命中规则序列（优先级与重叠去重）"
    );
}

#[test]
fn rule_candidates_match_python() {
    let fixture = fixture();
    for item in texts(&fixture) {
        let text = item["text"].as_str().expect("text");
        let expected = item["rule_matches"].as_array().expect("rule_matches");
        for rule in builtin_rules() {
            let ours = candidates_json(rule, text);
            let theirs: Vec<&Value> = expected
                .iter()
                .filter(|hit| hit["rule"].as_str() == Some(rule.rule_id))
                .collect();
            assert_eq!(
                json!(ours),
                json!(theirs),
                "{} 的候选区间（{text:?}）",
                rule.rule_id
            );
        }
    }
}

#[test]
fn scan_matches_python() {
    let fixture = fixture();
    for item in texts(&fixture) {
        let text = item["text"].as_str().expect("text");
        let ours = scan_json(&scan_pattern_rules(text, builtin_rules()), text);
        assert_eq!(json!(ours), item["scan"], "整段扫描（{text:?}）");
    }
}

#[test]
fn entropy_matches_python() {
    let fixture = fixture();
    for case in fixture["entropy"].as_array().expect("entropy") {
        let text = case["text"].as_str().expect("text");
        let expected = case["bits"].as_f64().expect("bits");
        let actual = shannon_entropy_bits(text);
        assert!(
            (actual - expected).abs() < 1e-12,
            "香农熵 {text:?}：期望 {expected}，实际 {actual}"
        );
    }
}

#[test]
fn validators_match_python() {
    let fixture = fixture();
    for case in fixture["luhn"].as_array().expect("luhn") {
        let value = case["value"].as_str().expect("value");
        assert_eq!(
            json!(is_luhn_valid(value)),
            case["valid"],
            "Luhn 校验（{value:?}）"
        );
    }
    for case in fixture["email_allowlist"].as_array().expect("allowlist") {
        let value = case["value"].as_str().expect("value");
        assert_eq!(
            json!(is_example_domain(value)),
            case["allowlisted"],
            "邮箱豁免（{value:?}）"
        );
    }
}

#[test]
fn ip_classification_matches_python() {
    let fixture = fixture();
    for case in fixture["ip_classification"].as_array().expect("ip") {
        let value = case["value"].as_str().expect("value");
        assert_eq!(
            json!(is_internal_ip(value)),
            case["internal"],
            "内网判定（{value:?}）"
        );
        assert_eq!(
            json!(is_external_ip(value)),
            case["external"],
            "外网判定（{value:?}）"
        );
    }
}
