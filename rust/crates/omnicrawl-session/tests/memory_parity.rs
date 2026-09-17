//! 记忆层格式的跨语言 parity：期望值来自 Python `omnicrawl/state/memory.py` 的格式层。
//!
//! 时间戳两侧都按各自本地时区渲染（同一台机器上一致），比对前统一归一化到 UTC 占位，
//! 避免运行机器时区不同导致假失败。

use omnicrawl_session::{
    body_of, dedupe_directories, dedupe_strings, format_memory_markdown, normalize_content,
    normalize_directory, MemoryIndexEntry, SessionStoreError,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/memory_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn cases(fixture: &Value, name: &str) -> Vec<Value> {
    fixture[name]
        .as_array()
        .unwrap_or_else(|| panic!("fixture 缺少 {name}"))
        .clone()
}

fn compare_text(actual: Result<String, SessionStoreError>, case: &Value, label: &str) {
    match (actual, case.get("expected"), case.get("error")) {
        (Ok(value), Some(expected), None) => {
            assert_eq!(json!(value), *expected, "{label} 用例 {case}");
        }
        (Err(error), None, Some(expected)) => {
            assert_eq!(
                json!(error.message()),
                *expected,
                "{label} 用例 {case} 的错误文案"
            );
        }
        (actual, _, _) => panic!("{label} 与 fixture 期望不符：{actual:?} / {case}"),
    }
}

#[test]
fn directories_match_python() {
    for case in cases(&fixture(), "directories") {
        let input = case["input"].as_str();
        match input {
            Some(text) => compare_text(normalize_directory(text), &case, "记忆目录"),
            None => assert!(
                case.get("error").is_some(),
                "非字符串目录输入只应是错误用例：{case}"
            ),
        }
    }
}

#[test]
fn paths_match_python() {
    for case in cases(&fixture(), "paths") {
        let text = case["input"].as_str().expect("路径输入是字符串");
        compare_text(
            omnicrawl_session::memory::normalize_relative_file_path(text),
            &case,
            "记忆文件路径",
        );
    }
}

#[test]
fn dedupe_matches_python() {
    let fixture = fixture();
    for case in cases(&fixture, "dedupe_directories") {
        let input = case["input"].as_array().expect("目录列表").clone();
        assert_eq!(
            json!(dedupe_directories(&input)),
            case["expected"],
            "关联目录去重 {case}"
        );
    }
    for case in cases(&fixture, "dedupe_strings") {
        let input = case["input"].as_array().expect("字符串列表").clone();
        assert_eq!(
            json!(dedupe_strings(&input)),
            case["expected"],
            "字符串去重 {case}"
        );
    }
}

#[test]
fn contents_match_python() {
    for case in cases(&fixture(), "contents") {
        let text = case["input"].as_str().expect("正文是字符串");
        assert_eq!(json!(normalize_content(text)), case["expected"], "{case}");
    }
}

#[test]
fn markdown_matches_python() {
    let fixture = fixture();
    let slot = fixture["timestamp_slot"].as_str().expect("占位");
    for case in cases(&fixture, "markdown") {
        let timestamp =
            omnicrawl_session::parse_memory_datetime(case["timestamp"].as_str().expect("时间戳"))
                .expect("时间戳可解析");
        let related: Vec<String> = case["related_directories"]
            .as_array()
            .expect("关联目录")
            .iter()
            .map(|item| item.as_str().expect("目录是字符串").to_string())
            .collect();
        let content = case["content"].as_str().expect("正文");
        let markdown = format_memory_markdown(timestamp, &related, content);
        // 把本地渲染的时间戳换成 UTC 渲染后再比对（跨机时区无关）。
        let local_text = omnicrawl_session::format_memory_datetime(timestamp);
        let canonical = markdown.replace(&local_text, slot);
        assert_eq!(json!(canonical), case["expected"], "用例 {}", case["name"]);

        assert!(
            markdown.contains(&local_text),
            "Markdown 里应当带上渲染后的时间戳：{markdown}"
        );
    }
}

#[test]
fn bodies_match_python() {
    for case in cases(&fixture(), "bodies") {
        let text = case["text"].as_str().expect("文本");
        assert_eq!(
            json!(body_of(text)),
            case["expected"],
            "用例 {}",
            case["name"]
        );
    }
}

#[test]
fn index_entries_match_python() {
    let fixture = fixture();
    let slot = fixture["timestamp_slot"].as_str().expect("占位");
    for case in cases(&fixture, "index_entries") {
        let parsed = MemoryIndexEntry::from_dict(&case["input"]);
        match (parsed, case.get("expected"), case.get("error")) {
            (Ok(entry), Some(expected), None) => {
                let mut actual = entry.to_dict();
                assert!(
                    actual["timestamp"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty()),
                    "索引条目应带渲染后的时间戳"
                );
                if let Some(object) = actual.as_object_mut() {
                    object.insert("timestamp".to_string(), json!(slot));
                }
                assert_eq!(actual, *expected, "索引用例 {case}");
            }
            (Err(error), None, Some(expected)) => {
                assert_eq!(
                    json!(error.message()),
                    *expected,
                    "索引用例 {case} 的错误文案"
                );
            }
            (actual, _, _) => panic!("索引条目与 fixture 期望不符：{actual:?} / {case}"),
        }
    }
}
