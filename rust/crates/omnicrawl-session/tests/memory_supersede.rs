//! 记忆写入的 DELETE（取代）语义：`supersedes` 点名的记忆在写入后被删除。

use omnicrawl_session::{MemoryStore, MemoryWriteRequest};

fn request(content: &str, storage_directory: &str) -> MemoryWriteRequest {
    MemoryWriteRequest {
        content: content.to_string(),
        related_directories: vec![storage_directory.to_string()],
        storage_directory: Some(storage_directory.to_string()),
        source_event: None,
        supersedes: Vec::new(),
    }
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("omnicrawl-memory-supersede-{name}-{nanos}"));
    std::fs::create_dir_all(&root).expect("建临时目录");
    root
}

#[test]
fn supersedes_removes_the_old_memory_and_keeps_the_new_one() {
    let root = temp_root("basic");
    let store = MemoryStore::open(&root);

    // 先写一条"编译目录用 rust/target"的记忆。
    let first = store
        .write(&[request(
            "用户指定本机编译目录统一用 rust/target",
            "external-context/general",
        )])
        .expect("首次写入");
    assert_eq!(first.len(), 1);
    let old_id = first[0].id.clone();

    // 新事实推翻旧事实：内容与旧记忆差异较大，字面相似度不会命中重复。
    let mut replacing = request(
        "用户改口径：本机编译目录统一用 D:\\build_temp，不再使用 rust/target",
        "external-context/general",
    );
    replacing.supersedes = vec![old_id.clone()];
    let second = store.write(&[replacing]).expect("取代写入");
    assert_eq!(second.len(), 1);
    assert_ne!(second[0].id, old_id, "新事实应当是新条目");

    // 旧记忆必须消失：读不到、也搜不到。
    let read_old = store
        .read(&[serde_json::Value::String(old_id.clone())])
        .expect("读取旧记忆");
    assert!(read_old.is_empty(), "被取代的记忆应当已被删除");

    let results = store
        .search("编译目录 build_temp", &[], 20)
        .expect("检索记忆");
    assert!(
        results.iter().all(|item| item.id != old_id),
        "被取代的记忆不该再被检索到：{:?}",
        results.iter().map(|item| &item.id).collect::<Vec<_>>()
    );
    assert!(
        results.iter().any(|item| item.id == second[0].id),
        "新记忆应当可被检索到"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn supersedes_rejects_unknown_id() {
    let root = temp_root("unknown");
    let store = MemoryStore::open(&root);
    store
        .write(&[request("一条普通记忆", "task-history/general")])
        .expect("首次写入");

    let mut bad = request("另一条记忆", "task-history/general");
    bad.supersedes = vec!["19990101-000000".to_string()];
    let error = store.write(&[bad]).expect_err("不存在的 id 应当报错");
    assert!(
        error.message().contains("要取代的记忆不存在"),
        "错误文案应说明原因：{}",
        error.message()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn empty_supersedes_keeps_existing_behaviour() {
    let root = temp_root("none");
    let store = MemoryStore::open(&root);
    let first = store
        .write(&[request("用户偏好中文输出", "user-preferences/general")])
        .expect("首次写入");
    let second = store
        .write(&[request(
            "用户偏好用中文回答技术问题",
            "user-preferences/general",
        )])
        .expect("再次写入");
    assert_eq!(second.len(), 1);

    // 不填 supersedes 时保持既有行为：旧条目仍在（可能被合并或并存，但不会被删）。
    let remaining = store
        .read(&[serde_json::Value::String(first[0].id.clone())])
        .expect("读取");
    assert!(
        !remaining.is_empty() || second[0].id == first[0].id,
        "未声明取代时不应删除既有记忆"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 直接的按 id 删除：文件消失、索引条目摘除、重复删除不再报错。
#[test]
fn delete_removes_the_file_and_the_index_entry() {
    let root = temp_root("delete");
    let store = MemoryStore::open(&root);
    let written = store
        .write(&[request("待删的临时记忆：编译目录口径", "project-context/build")])
        .expect("写入");
    let id = written[0].id.clone();
    assert!(store.load_entries().expect("读索引").iter().any(|e| e.id == id));

    let deleted = store
        .delete(&[serde_json::Value::String(id.clone())])
        .expect("删除");
    assert_eq!(deleted, vec![id.clone()]);
    assert!(
        store.load_entries().expect("读索引").iter().all(|e| e.id != id),
        "索引里不该再有这条"
    );
    assert!(
        store.read(&[serde_json::Value::String(id.clone())]).expect("读取").is_empty(),
        "按 id 已经读不到"
    );

    // 再删一次：静默跳过，返回空列表而不是报错。
    assert!(
        store.delete(&[serde_json::Value::String(id)]).expect("重复删除不报错").is_empty()
    );
    let _ = std::fs::remove_dir_all(&root);
}
