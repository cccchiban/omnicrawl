//! gitleaks 规则接入的跨语言 parity：数据集是冻结的对照契约。
//!
//! 覆盖规则集（逐条字段）、模式归一化、扫描命中（区间换算成字节偏移）与自定义文件的
//! 覆盖 / 追加语义；另有快照字节一致性校验，防止两侧快照漂移。

use std::path::PathBuf;

use omnicrawl_llm::desensitization::{
    gitleaks_default_rules, load_gitleaks_rules, normalize_gitleaks_pattern, scan_gitleaks_rules,
    GITLEAKS_SNAPSHOT,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/gitleaks_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn sorted_owned(values: &[String]) -> Vec<String> {
    let mut owned: Vec<String> = values.to_vec();
    owned.sort();
    owned
}

/// 内嵌快照的 sha256（与 fixture 里的 Python 侧快照哈希对照，防漂移）。
fn snapshot_digest() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(GITLEAKS_SNAPSHOT.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[test]
fn snapshot_is_byte_identical_with_python() {
    let fixture = fixture();
    assert_eq!(
        snapshot_digest(),
        fixture["snapshot_sha256"]
            .as_str()
            .expect("snapshot_sha256"),
        "内嵌快照与 Python 侧同名文件必须逐字节一致"
    );
}

#[test]
fn rules_match_python() {
    let fixture = fixture();
    let expected = fixture["rules"].as_array().expect("rules");
    let rules = gitleaks_default_rules();
    assert_eq!(
        rules.len(),
        fixture["rule_count"].as_u64().expect("rule_count") as usize,
        "规则条数"
    );

    for (rule, case) in rules.iter().zip(expected) {
        let label = case["rule_id"].as_str().expect("rule_id");
        assert_eq!(rule.rule_id, label, "rule_id");
        assert_eq!(
            rule.pattern_source,
            case["pattern"].as_str().unwrap(),
            "pattern（{label}）"
        );
        assert_eq!(
            rule.description,
            case["description"].as_str().unwrap_or_default(),
            "description（{label}）"
        );
        assert_eq!(
            rule.keywords,
            case["keywords"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            "keywords（{label}）"
        );
        assert_eq!(
            rule.secret_group,
            case["secret_group"].as_u64().map(|value| value as usize),
            "secret_group（{label}）"
        );
        assert_eq!(
            rule.min_entropy,
            case["min_entropy"].as_f64(),
            "min_entropy（{label}）"
        );
        let sources = |key: &str| {
            case[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rule.allowlist_sources,
            sources("allowlist"),
            "allowlist（{label}）"
        );
        assert_eq!(
            rule.match_allowlist_sources,
            sources("match_allowlist"),
            "match_allowlist（{label}）"
        );
        assert_eq!(
            sorted_owned(&rule.stopwords),
            sorted_owned(&sources("stopwords")),
            "stopwords（{label}）"
        );
    }
}

#[test]
fn normalize_matches_python() {
    let fixture = fixture();
    for case in fixture["normalize"].as_array().expect("normalize") {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            normalize_gitleaks_pattern(input),
            case["expected"].as_str().expect("expected"),
            "归一化（{input}）"
        );
    }
}

#[test]
fn scan_matches_python() {
    let fixture = fixture();
    let rules = gitleaks_default_rules();
    let cases = fixture["scan"].as_array().expect("scan");
    assert!(!cases.is_empty(), "数据集为空");

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let text = case["text"].as_str().expect("text");
        let mut accepted: Vec<(usize, usize)> = Vec::new();
        let produced: Vec<Value> = scan_gitleaks_rules(text, rules, &mut accepted)
            .into_iter()
            .map(|hit| {
                serde_json::json!({
                    "rule_id": hit.rule_id,
                    "start": hit.start,
                    "end": hit.end,
                    "value": hit.value,
                })
            })
            .collect();
        assert_eq!(Value::Array(produced), case["expected"], "扫描（{label}）");
    }
}

#[test]
fn custom_file_overrides_and_appends() {
    let fixture = fixture();
    let custom = &fixture["custom"];
    let toml_text = custom["toml"].as_str().expect("toml");

    let path = temp_path("gitleaks-custom-parity.toml");
    std::fs::write(&path, toml_text).expect("写入临时 gitleaks.toml");
    let merged = load_gitleaks_rules(Some(path.to_str().expect("utf-8 路径")));
    let _ = std::fs::remove_file(&path);

    let expected_ids: Vec<String> = custom["rule_ids"]
        .as_array()
        .expect("rule_ids")
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        merged
            .iter()
            .map(|rule| rule.rule_id.clone())
            .collect::<Vec<_>>(),
        expected_ids,
        "合并后的规则顺序（同 id 覆盖、新 id 追加）"
    );

    let overridden = merged
        .iter()
        .find(|rule| rule.rule_id == "gitleaks:aws-access-token")
        .expect("被覆盖的规则仍在原位");
    assert_eq!(
        overridden.description,
        custom["overridden"]["description"].as_str().unwrap(),
        "自定义文件应覆盖同 id 规则"
    );

    let appended = merged
        .iter()
        .find(|rule| rule.rule_id == "gitleaks:custom-rule")
        .expect("新 id 应追加");
    assert_eq!(
        appended.pattern_source,
        custom["appended"]["pattern"].as_str().unwrap(),
        "追加规则的模式"
    );

    let skipped = merged
        .iter()
        .find(|rule| rule.rule_id == "gitleaks:and-condition-rule")
        .expect("AND 豁免的规则本身仍生效");
    assert!(
        skipped.allowlist.is_empty(),
        "condition=AND 的豁免条目应被保守跳过"
    );
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("omnicrawl-{}-{}", std::process::id(), name));
    path
}
