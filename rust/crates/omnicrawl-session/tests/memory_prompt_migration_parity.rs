//! 提示词段落与旧目录迁移的跨语言 parity：期望值来自 Python `omnicrawl/state/memory.py`。
//!
//! 迁移会生成记忆 id 与带时间戳的备份目录名，因此两侧统一归一化：
//! `.migrated-<时间戳>` 里的时间戳换成 `<stamp>`，其余记忆 id 形状换成 `<memory-id>`；
//! 绝对路径里的临时目录换成 `<root>`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use omnicrawl_session::{migrate_legacy_memory, MemoryStore};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/memory_prompt_migration_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn temp_root(prefix: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&root).expect("创建临时根目录");
    root
}

/// 记忆 id 与备份时间戳统一占位（`.migrated-` 前缀优先识别）。
fn normalize_ids(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < characters.len() {
        if let Some(length) = id_length(&characters, index) {
            // 紧跟在 `.migrated-` 后面的是备份时间戳，不是记忆 id。
            if result.ends_with(".migrated-") {
                result.push_str("<stamp>");
            } else {
                result.push_str("<memory-id>");
            }
            index += length;
            continue;
        }
        result.push(characters[index]);
        index += 1;
    }
    result
}

/// 8 位数字 + `-` + 6 位数字，可选再接 `-` 加 3 位数字。
fn id_length(characters: &[char], start: usize) -> Option<usize> {
    let digit = |offset: usize| {
        characters
            .get(start + offset)
            .is_some_and(|character| character.is_ascii_digit())
    };
    if (0..8).any(|offset| !digit(offset)) || characters.get(start + 8) != Some(&'-') {
        return None;
    }
    if (9..15).any(|offset| !digit(offset)) {
        return None;
    }
    if characters.get(start + 15) == Some(&'-') && (16..19).all(digit) {
        return Some(19);
    }
    Some(15)
}

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("创建目录");
    }
    std::fs::write(path, content).expect("写入文件");
}

fn materialize(root: &Path, prefix: &str, files: &Value) {
    for (relative, content) in files.as_object().expect("文件表") {
        write_file(
            root,
            &format!("{prefix}{relative}"),
            content.as_str().expect("文件内容"),
        );
    }
}

fn snapshot_paths(root: &Path) -> BTreeMap<String, String> {
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
        files.insert(
            normalize_ids(&relative),
            std::fs::read_to_string(&path).unwrap_or_default(),
        );
    }
}

#[test]
fn prompt_section_matches_python() {
    for case in fixture()["prompt"].as_array().expect("prompt") {
        let root = temp_root("omnicrawl-memory-prompt");
        let entries: Vec<Value> = case["entries"].as_array().expect("entries").clone();
        write_file(
            &root,
            "index.json",
            &serde_json::to_string(&json!({"memories": entries})).expect("索引可序列化"),
        );

        let overrides = &case["overrides"];
        let store = MemoryStore::open(&root);
        let text = store
            .format_prompt_section(
                overrides
                    .get("scope_label")
                    .and_then(Value::as_str)
                    .unwrap_or("长期"),
                overrides
                    .get("search_tool")
                    .and_then(Value::as_str)
                    .unwrap_or("memory_search"),
                overrides
                    .get("read_tool")
                    .and_then(Value::as_str)
                    .unwrap_or("memory_read"),
                overrides
                    .get("expand_tool")
                    .and_then(Value::as_str)
                    .unwrap_or("memory_expand_related"),
                overrides
                    .get("write_tool")
                    .and_then(Value::as_str)
                    .unwrap_or("memory_write"),
            )
            .expect("生成提示词段落失败");
        assert_eq!(json!(text), case["expected"], "提示词用例 {}", case["name"]);
        std::fs::remove_dir_all(&root).ok();
    }
}

#[test]
fn migration_matches_python() {
    for case in fixture()["migration"].as_array().expect("migration") {
        let name = case["name"].as_str().expect("name");
        let root = temp_root("omnicrawl-memory-migrate");
        if case["source_is_file"].as_bool().unwrap_or(false) {
            write_file(&root, "memory", "不是目录");
        } else {
            materialize(&root, "memory/", &case["source_files"]);
            materialize(&root, "project/memory/", &case["destination_files"]);
        }

        let expected = &case["expected"];
        let source = root.join("memory");
        // 源与目标同一路径的用例：目标就是源目录本身。
        let destination = if case["same_path"].as_bool().unwrap_or(false) {
            source.clone()
        } else {
            root.join("project").join("memory")
        };
        let outcome = migrate_legacy_memory(&source, &destination);

        if let Some(error) = expected.get("error").and_then(Value::as_str) {
            let message = outcome.expect_err("该用例应当报错").message().to_string();
            let normalized =
                normalize_ids(&message).replace(&root.to_string_lossy().to_string(), "<root>");
            assert_eq!(normalized, error, "迁移用例 {name} 的错误文案");
            std::fs::remove_dir_all(&root).ok();
            continue;
        }

        let result = outcome.expect("迁移应当成功");
        assert_eq!(json!(result.migrated), expected["migrated"], "用例 {name}");
        assert_eq!(
            json!(result.imported_count),
            expected["imported_count"],
            "用例 {name}"
        );
        assert_eq!(
            json!(result.backup_path.is_some()),
            expected["has_backup"],
            "用例 {name}"
        );

        let actual = snapshot_paths(&root);
        let expected_tree = expected["tree"].as_object().expect("tree");
        let actual_keys: Vec<&String> = actual.keys().collect();
        let expected_keys: Vec<&String> = expected_tree.keys().collect();
        assert_eq!(actual_keys, expected_keys, "用例 {name} 的文件集合");

        // 只有数据集里记了内容的条目才做逐字比对：其余用例会新生成 id 与时间戳。
        for (path, content) in expected_tree {
            let expected_content = content.as_str().expect("内容");
            if !expected_content.is_empty() {
                assert_eq!(
                    actual.get(path),
                    Some(&expected_content.to_string()),
                    "用例 {name} 的文件内容：{path}"
                );
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
