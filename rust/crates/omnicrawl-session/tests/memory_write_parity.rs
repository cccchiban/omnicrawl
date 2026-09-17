//! 记忆写入的跨语言 parity：期望值来自 Python `omnicrawl/state/memory.py` 的写入路径。
//!
//! 记忆 id 由时间戳生成、时间戳是「现在」，因此两侧用同一套规则归一化：先按出现顺序把 id 换成
//! `<memory-id-N>`（先扫输出、再扫文件名与正文，替换时先长后短），再把时间戳换成占位。

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{SecondsFormat, Utc};
use omnicrawl_session::{MemoryStore, MemoryWriteRequest};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/memory_write_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// 扫描记忆 id：8 位数字 + `-` + 6 位数字，可选再接 `-` 加 3 位数字。
fn find_memory_ids(text: &str, found: &mut Vec<String>) {
    let characters: Vec<char> = text.chars().collect();
    for start in 0..characters.len() {
        let digit = |offset: usize| {
            characters
                .get(start + offset)
                .is_some_and(|character| character.is_ascii_digit())
        };
        if (0..8).any(|offset| !digit(offset)) {
            continue;
        }
        if characters.get(start + 8) != Some(&'-') {
            continue;
        }
        if (9..15).any(|offset| !digit(offset)) {
            continue;
        }
        let mut length = 15;
        if characters.get(start + 15) == Some(&'-') && (16..19).all(digit) {
            length = 19;
        }
        let candidate: String = characters[start..start + length].iter().collect();
        if !found.contains(&candidate) {
            found.push(candidate);
        }
    }
}

/// 把 ISO-8601 时间戳替换成占位符（不引正则，按形状扫描）。
fn replace_timestamps(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < characters.len() {
        match timestamp_length(&characters, index) {
            Some(length) => {
                result.push_str("<timestamp>");
                index += length;
            }
            None => {
                result.push(characters[index]);
                index += 1;
            }
        }
    }
    result
}

fn timestamp_length(characters: &[char], index: usize) -> Option<usize> {
    let digit = |offset: usize| {
        characters
            .get(index + offset)
            .is_some_and(|character| character.is_ascii_digit())
    };
    let expect = |offset: usize, expected: char| characters.get(index + offset) == Some(&expected);
    if (0..4).any(|offset| !digit(offset))
        || !expect(4, '-')
        || (5..7).any(|offset| !digit(offset))
        || !expect(7, '-')
        || (8..10).any(|offset| !digit(offset))
        || !expect(10, 'T')
        || (11..13).any(|offset| !digit(offset))
        || !expect(13, ':')
        || (14..16).any(|offset| !digit(offset))
        || !expect(16, ':')
        || (17..19).any(|offset| !digit(offset))
    {
        return None;
    }

    let mut length = 19;
    if expect(length, '.') {
        let mut fraction = length + 1;
        while characters
            .get(index + fraction)
            .is_some_and(|character| character.is_ascii_digit())
        {
            fraction += 1;
        }
        if fraction > length + 1 {
            length = fraction;
        }
    }
    match characters.get(index + length) {
        Some('Z') => Some(length + 1),
        Some('+' | '-') => {
            let body_ok = (0..2).all(|offset| digit(length + 1 + offset))
                && characters.get(index + length + 3) == Some(&':')
                && digit(length + 4)
                && digit(length + 5);
            if body_ok {
                Some(length + 6)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn discover_ids(outputs_text: &str, files: &BTreeMap<String, String>) -> Vec<String> {
    let mut found = Vec::new();
    find_memory_ids(outputs_text, &mut found);
    for (path, content) in files {
        find_memory_ids(path, &mut found);
        find_memory_ids(content, &mut found);
    }
    found
}

fn normalize(
    outputs_text: &str,
    files: &BTreeMap<String, String>,
) -> (Value, BTreeMap<String, String>) {
    let ids = discover_ids(outputs_text, files);
    let mut mapping: Vec<(String, String)> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), format!("<memory-id-{}>", index + 1)))
        .collect();
    // 先替换更长的 id：同秒生成的记忆 id 互为前缀，短的先替换会串到长的里面去。
    mapping.sort_by_key(|(id, _)| std::cmp::Reverse(id.len()));

    let apply = |text: &str| {
        let mut replaced = text.to_string();
        for (id, placeholder) in &mapping {
            replaced = replaced.replace(id.as_str(), placeholder.as_str());
        }
        replace_timestamps(&replaced)
    };

    let outputs = serde_json::from_str(&apply(outputs_text)).expect("归一化后的输出可解析");
    let normalized_files = files
        .iter()
        .map(|(path, content)| (apply(path), apply(content)))
        .collect();
    (outputs, normalized_files)
}

fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

fn collect(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) {
    for entry in std::fs::read_dir(directory).expect("读取目录").flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("相对路径")
            .to_string_lossy()
            .replace('\\', "/");
        files.insert(relative, std::fs::read_to_string(&path).expect("读取文件"));
    }
}

#[test]
fn memory_store_write_chain_matches_python() {
    let fixture = fixture();
    let root = std::env::temp_dir().join(format!(
        "omnicrawl-memory-write-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&root).expect("创建临时根目录");

    let store = MemoryStore::open(&root);
    let mut outputs = Vec::new();
    for step in fixture["steps"].as_array().expect("steps") {
        let requests: Vec<MemoryWriteRequest> = step["requests"]
            .as_array()
            .expect("requests")
            .iter()
            .map(|item| MemoryWriteRequest {
                content: item["content"].as_str().expect("content").to_string(),
                related_directories: item["related_directories"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|value| value.as_str().expect("目录").to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                storage_directory: item
                    .get("storage_directory")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                source_event: None,
            })
            .collect();

        match store.write(&requests) {
            Err(error) => outputs.push(json!({"kind": "write", "error": error.message()})),
            Ok(records) => {
                let records: Vec<Value> = records
                    .iter()
                    .map(|record| {
                        json!({
                            "id": record.id,
                            "timestamp": record
                                .timestamp
                                .with_timezone(&Utc)
                                .to_rfc3339_opts(SecondsFormat::Micros, false),
                            "related_directories": record.related_directories,
                            "content": record.content,
                        })
                    })
                    .collect();
                outputs.push(json!({"kind": "write", "records": records}));
            }
        }
    }

    let files = snapshot(&root);
    let outputs_text = serde_json::to_string(&json!(outputs)).expect("输出可序列化");
    let (actual_outputs, actual_files) = normalize(&outputs_text, &files);

    assert_eq!(
        actual_outputs, fixture["expected_outputs"],
        "写入步骤输出与 Python 不一致"
    );

    let expected: BTreeMap<String, String> = fixture["expected_files"]
        .as_object()
        .expect("expected_files")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().expect("内容").to_string()))
        .collect();
    assert_eq!(actual_files, expected, "写入后的文件快照与 Python 不一致");

    std::fs::remove_dir_all(&root).ok();
}
