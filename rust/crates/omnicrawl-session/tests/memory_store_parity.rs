//! 记忆存储「索引 + 读取」的跨语言 parity：期望值来自 Python `omnicrawl/state/memory.py`。
//!
//! 读取加深会把时间戳刷新成「现在」，因此两侧都比对归一化后的时间戳占位。

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{SecondsFormat, Utc};
use omnicrawl_session::{MemoryRecord, MemoryStore};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/memory_store_parity.json");
const TIMESTAMP_SLOT: &str = "<timestamp>";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// 把 ISO-8601 时间戳替换成占位符（不引正则，按形状扫描）。
fn normalize(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < characters.len() {
        if let Some(length) = timestamp_length(&characters, index) {
            result.push_str(TIMESTAMP_SLOT);
            index += length;
            continue;
        }
        result.push(characters[index]);
        index += 1;
    }
    result
}

/// 从 `index` 起是否是 `YYYY-MM-DDTHH:MM:SS[.ffffff][Z|±HH:MM]`，返回长度。
fn timestamp_length(characters: &[char], index: usize) -> Option<usize> {
    let digit = |offset: usize| {
        characters
            .get(index + offset)
            .is_some_and(|character| character.is_ascii_digit())
    };
    let expect = |offset: usize, expected: char| characters.get(index + offset) == Some(&expected);
    for offset in [0, 1, 2, 3] {
        if !digit(offset) {
            return None;
        }
    }
    if !expect(4, '-') {
        return None;
    }
    for offset in [5, 6] {
        if !digit(offset) {
            return None;
        }
    }
    if !expect(7, '-') {
        return None;
    }
    for offset in [8, 9] {
        if !digit(offset) {
            return None;
        }
    }
    if !expect(10, 'T') {
        return None;
    }
    for offset in [11, 12] {
        if !digit(offset) {
            return None;
        }
    }
    if !expect(13, ':') {
        return None;
    }
    for offset in [14, 15] {
        if !digit(offset) {
            return None;
        }
    }
    if !expect(16, ':') {
        return None;
    }
    for offset in [17, 18] {
        if !digit(offset) {
            return None;
        }
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
        Some(sign @ ('+' | '-')) => {
            let body_ok = (0..2).all(|offset| digit(length + 1 + offset))
                && characters.get(index + length + 3) == Some(&':')
                && digit(length + 4)
                && digit(length + 5);
            if body_ok && matches!(sign, '+' | '-') {
                Some(length + 6)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn record_to_json(record: &MemoryRecord) -> Value {
    json!({
        "id": record.id,
        "timestamp": record
            .timestamp
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Micros, false),
        "related_directories": record.related_directories,
        "content": record.content,
    })
}

fn materialize(root: &Path, files: &Value) {
    for (relative, content) in files.as_object().expect("initial_files") {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建目录");
        }
        std::fs::write(&path, content.as_str().expect("文件内容"))
            .unwrap_or_else(|error| panic!("写入 {relative} 失败：{error}"));
    }
}

fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

fn collect(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) {
    let entries = std::fs::read_dir(directory).expect("读取目录");
    for entry in entries.flatten() {
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
        let text = std::fs::read_to_string(&path).expect("读取文件");
        files.insert(relative, normalize(&text));
    }
}

#[test]
fn memory_store_read_chain_matches_python() {
    let fixture = fixture();
    let root = std::env::temp_dir().join(format!(
        "omnicrawl-memory-store-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&root).expect("创建临时根目录");
    materialize(&root, &fixture["initial_files"]);

    let store = MemoryStore::open(&root);
    let mut outputs = Vec::new();
    for step in fixture["steps"].as_array().expect("steps") {
        let kind = step["kind"].as_str().expect("kind");
        match kind {
            "read" => {
                let ids = step["ids"].as_array().expect("ids").clone();
                let records = store.read(&ids).expect("读取记忆失败");
                outputs.push(json!({
                    "kind": kind,
                    "ids": ids,
                    "records": records.iter().map(record_to_json).collect::<Vec<Value>>(),
                }));
            }
            "load_ids" => {
                let ids: Vec<Value> = store
                    .load_entries()
                    .expect("读取索引失败")
                    .iter()
                    .map(|entry| json!(entry.id))
                    .collect();
                outputs.push(json!({"kind": kind, "ids": ids}));
            }
            other => panic!("未知步骤：{other}"),
        }
    }

    // 输出的时间戳同样归一化后比对（读取加深会把时间戳刷成「现在」）。
    let actual = serde_json::from_str::<Value>(&normalize(
        &serde_json::to_string(&json!(outputs)).expect("输出可序列化"),
    ))
    .expect("归一化后的输出可解析");
    assert_eq!(
        actual, fixture["expected_outputs"],
        "步骤输出与 Python 不一致"
    );

    let files = snapshot(&root);
    let expected: BTreeMap<String, String> = fixture["expected_files"]
        .as_object()
        .expect("expected_files")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().expect("内容").to_string()))
        .collect();
    assert_eq!(files, expected, "文件快照与 Python 不一致");

    std::fs::remove_dir_all(&root).ok();
}
