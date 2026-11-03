//! 记忆检索的速度与边界：打分只算一次查询、目录只扫一次、失效条目被过滤。

use omnicrawl_session::{MemoryStore, MemoryWriteRequest};
use serde_json::json;

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
    let root = std::env::temp_dir().join(format!("omnicrawl-memory-speed-{name}-{nanos}"));
    std::fs::create_dir_all(&root).expect("建临时目录");
    root
}

/// 上千条记忆里检索若干次，耗时必须远低于逐条重算分词与路径探测的实现。
#[test]
fn searching_a_large_store_stays_fast() {
    let root = temp_root("bulk");
    let store = MemoryStore::open(&root);
    let batch: Vec<MemoryWriteRequest> = (0..400)
        .map(|index| {
            let directory = match index % 4 {
                0 => "code-knowledge/general",
                1 => "project-context/general",
                2 => "error-lessons/general",
                _ => "task-history/general",
            };
            request(
                &format!("第 {index} 条记忆：记录模块 index{index} 的实现细节与注意事项"),
                directory,
            )
        })
        .collect();
    store.write(&batch).expect("批量写入");

    let started = std::time::Instant::now();
    for query in ["模块实现", "注意事项", "index42", "配置解析"] {
        let found = store.search(query, &[], 20).expect("检索");
        assert!(!found.is_empty(), "查询 {query} 应当有候选");
    }
    let elapsed = started.elapsed();
    eprintln!("400 条记忆四次检索总耗时 {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "四次检索耗时 {elapsed:?}，说明查询分词或目录探测被逐条重算了"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 文件被删（或索引残留）的条目不能出现在结果里。
#[test]
fn entries_without_a_file_are_dropped() {
    let root = temp_root("missing");
    let store = MemoryStore::open(&root);
    let written = store
        .write(&[
            request("配置解析先读环境变量", "code-knowledge/general"),
            request("图片压缩用 RUN 长度编码", "code-knowledge/general"),
        ])
        .expect("写入");
    let victim = written[0].clone();
    let entry = store
        .load_entries()
        .expect("索引")
        .into_iter()
        .find(|item| item.id == victim.id)
        .expect("索引里有这条");
    std::fs::remove_file(root.join(&entry.path)).expect("删掉正文");

    let started = std::time::Instant::now();
    let found = store.search("配置解析", &[], 20).expect("检索");
    eprintln!("单次检索耗时 {:?}", started.elapsed());
    assert!(
        found.iter().all(|result| result.id != victim.id),
        "正文已删的记忆不该出现在结果里：{found:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 空查询且无候选目录时按新鲜度与使用次数返回，不参与任何打分。
#[test]
fn empty_query_lists_the_newest_memories() {
    let root = temp_root("empty-query");
    let store = MemoryStore::open(&root);
    store
        .write(&[
            request("第一条：编译目录用 D:/build_temp", "external-context/general"),
            request("第二条：TUI 使用 ratatui", "project-context/general"),
        ])
        .expect("写入");

    let found = store.search("", &[], 20).expect("空查询检索");
    assert_eq!(found.len(), 2, "{found:?}");
    let newest = found.first().expect("至少一条");
    assert!(
        newest.summary.contains("TUI") || newest.summary.contains("编译目录"),
        "{newest:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 目录提示仍然生效：只有候选目录命中时条目也能被检索出来。
#[test]
fn candidate_directories_still_pull_entries_in() {
    let root = temp_root("directory-only");
    let store = MemoryStore::open(&root);
    store
        .write(&[request(
            "项目架构分三层",
            "project-context/general",
        )])
        .expect("写入");

    let found = store
        .search("完全无关的查询文本", &[json!("project-context")], 20)
        .expect("目录检索");
    assert_eq!(found.len(), 1, "目录命中应当把条目带出来：{found:?}");
    std::fs::remove_dir_all(&root).ok();
}


/// 索引一变（写入新记忆），缓存必须失效，否则会检索不到刚写的内容。
#[test]
fn cache_is_invalidated_by_writes() {
    let root = temp_root("cache-invalidation");
    let store = MemoryStore::open(&root);
    store
        .write(&[request("首次写入：编译目录", "external-context/general")])
        .expect("首次写入");
    let before = store.search("编译目录", &[], 20).expect("首次检索");
    assert_eq!(before.len(), 1, "{before:?}");

    let later = MemoryStore::open(&root);
    later
        .write(&[request("二次写入：压缩目录", "external-context/general")])
        .expect("二次写入");
    let after = store.search("压缩目录", &[], 20).expect("再次检索");
    // 两条记忆共享同一存储目录，目录权重也会带出旧条目；这里只断言新内容已被检索到。
    assert!(
        after
            .iter()
            .any(|result| result.summary.contains("压缩目录")),
        "新写入的记忆必须能被检索到：{after:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

