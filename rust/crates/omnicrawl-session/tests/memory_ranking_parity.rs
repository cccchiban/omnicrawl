//! 记忆排序纯逻辑的跨语言 parity：期望值来自 Python `omnicrawl/state/memory_ranking.py`。

use std::collections::BTreeSet;

use omnicrawl_session::{
    classify_storage_directory, directories_overlap, directory_match_score, extract_search_tokens,
    make_summary, merge_memory_content, normalize_for_compare, parse_memory_datetime,
    score_related_entry, score_search_entry, MemoryIndexEntry,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/memory_ranking_parity.json");
const EPSILON: f64 = 1e-9;

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn cases(fixture: &Value, name: &str) -> Vec<Value> {
    fixture[name]
        .as_array()
        .unwrap_or_else(|| panic!("fixture 缺少 {name}"))
        .clone()
}

fn entry_of(value: &Value) -> MemoryIndexEntry {
    let timestamp = fixture()["now"].as_str().expect("now").to_string();
    let storage_directory = value["storage_directory"]
        .as_str()
        .expect("storage_directory");
    let id = value["id"].as_str().expect("id");
    MemoryIndexEntry::from_dict(&json!({
        "id": id,
        "path": format!("{storage_directory}/{id}.md"),
        "storage_directory": storage_directory,
        "timestamp": timestamp,
        "touch_count": value["touch_count"],
        "related_directories": value["related_directories"],
        "summary": value["summary"],
    }))
    .expect("索引条目可解析")
}

fn assert_close(actual: f64, expected: &Value, label: &str) {
    let expected = expected
        .as_f64()
        .unwrap_or_else(|| panic!("{label} 期望不是数字"));
    assert!(
        (actual - expected).abs() <= EPSILON,
        "{label} 期望 {expected}，实际 {actual}"
    );
}

#[test]
fn classification_matches_python() {
    for case in cases(&fixture(), "classify") {
        let text = case["input"].as_str().expect("输入是字符串");
        assert_eq!(
            json!(classify_storage_directory(text)),
            case["expected"],
            "{case}"
        );
    }
}

#[test]
fn summaries_match_python() {
    for case in cases(&fixture(), "summaries") {
        let text = case["input"].as_str().expect("输入是字符串");
        assert_eq!(json!(make_summary(text, 120)), case["expected"], "{case}");
    }
}

#[test]
fn merges_match_python() {
    for case in cases(&fixture(), "merges") {
        let old = case["old"].as_str().expect("old");
        let new = case["new"].as_str().expect("new");
        assert_eq!(
            json!(merge_memory_content(old, new)),
            case["expected"],
            "{case}"
        );
    }
}

#[test]
fn directory_scores_match_python() {
    for case in cases(&fixture(), "directory_scores") {
        let entry = entry_of(&case["entry"]);
        let directory = case["directory"].as_str().expect("directory");
        assert_close(
            directory_match_score(&entry, directory),
            &case["expected"],
            &format!("目录打分 {case}"),
        );
    }
}

#[test]
fn overlaps_match_python() {
    let to_list = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .expect("目录列表")
            .iter()
            .map(|item| item.as_str().expect("目录是字符串").to_string())
            .collect()
    };
    for case in cases(&fixture(), "overlaps") {
        let left = to_list(&case["left"]);
        let right = to_list(&case["right"]);
        assert_eq!(
            json!(directories_overlap(&left, &right)),
            case["expected"],
            "{case}"
        );
    }
}

#[test]
fn tokens_match_python() {
    for case in cases(&fixture(), "tokens") {
        let text = case["input"].as_str().expect("输入是字符串");
        let tokens: Vec<String> = extract_search_tokens(text).into_iter().collect();
        assert_eq!(json!(tokens), case["expected"], "{case}");
    }
}

#[test]
fn normalize_matches_python() {
    for case in cases(&fixture(), "normalize") {
        let text = case["input"].as_str().expect("输入是字符串");
        assert_eq!(
            json!(normalize_for_compare(text)),
            case["expected"],
            "{case}"
        );
    }
}

#[test]
fn search_scores_match_python() {
    let fixture = fixture();
    let now = parse_memory_datetime(fixture["now"].as_str().expect("now")).expect("now 可解析");
    // related_scores 里每个条目重复 9 次（3 组目录 × 3 个深度），按 9 分组即得原条目顺序。
    let sample_entries: Vec<Value> = fixture["related_scores"]
        .as_array()
        .expect("related_scores")
        .chunks(9)
        .map(|chunk| chunk[0]["entry"].clone())
        .collect();
    assert_eq!(sample_entries.len(), 3, "fixture 应包含 3 个条目样本");

    for case in cases(&fixture, "search_scores") {
        let query = case["query"].as_str().expect("query");
        let directories: Vec<String> = case["candidate_directories"]
            .as_array()
            .expect("candidate_directories")
            .iter()
            .map(|item| item.as_str().expect("目录").to_string())
            .collect();
        let expected = case["entries"].as_array().expect("entries").clone();
        for (index, spec) in sample_entries.iter().enumerate() {
            let entry = entry_of(spec);
            assert_close(
                score_search_entry(&entry, query, &directories, now),
                &expected[index],
                &format!("搜索分 {case} 第 {index} 条"),
            );
        }
    }
}

#[test]
fn related_scores_match_python() {
    for case in cases(&fixture(), "related_scores") {
        let entry = entry_of(&case["entry"]);
        let directories: BTreeSet<String> = case["directories"]
            .as_array()
            .expect("directories")
            .iter()
            .map(|item| item.as_str().expect("目录").to_string())
            .collect();
        let depth = case["depth"].as_u64().expect("depth") as usize;
        assert_close(
            score_related_entry(&entry, &directories, depth),
            &case["expected"],
            &format!("关联分 {case}"),
        );
    }
}
