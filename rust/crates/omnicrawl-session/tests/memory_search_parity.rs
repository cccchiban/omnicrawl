//! 记忆检索入口的跨语言 parity：期望值来自 Python `omnicrawl/state/memory.py`。
//!
//! 注入的条目时间戳都是一年前的固定时刻，因此搜索打分的「新鲜度」项恒为 0、排序可确定性比对；
//! 返回的时间戳统一按 UTC 秒渲染，避免依赖运行机器时区。

use std::path::Path;

use chrono::{SecondsFormat, Utc};
use omnicrawl_session::MemoryStore;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/memory_search_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn materialize(root: &Path, files: &Value) {
    for (relative, content) in files.as_object().expect("initial_files") {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("创建目录");
        }
        std::fs::write(&path, content.as_str().expect("文件内容")).expect("写入初始文件");
    }
}

fn render(result: &omnicrawl_session::MemorySearchResult) -> Value {
    json!({
        "id": result.id,
        "summary": result.summary,
        "storage_directory": result.storage_directory,
        "related_directories": result.related_directories,
        "timestamp": result
            .timestamp
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, false),
    })
}

fn strings(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

#[test]
fn memory_search_and_expand_match_python() {
    let fixture = fixture();
    let root = std::env::temp_dir().join(format!(
        "omnicrawl-memory-search-{}",
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
        let results = match kind {
            "search" => store
                .search(
                    step["query"].as_str().expect("query"),
                    &strings(step.get("candidate_directories")),
                    step.get("max_results").and_then(Value::as_u64).unwrap_or(5) as u32,
                )
                .expect("检索失败"),
            "expand_related" => store
                .expand_related(
                    &strings(step.get("ids")),
                    step.get("max_depth").and_then(Value::as_u64).unwrap_or(1) as u32,
                    step.get("max_results").and_then(Value::as_u64).unwrap_or(5) as u32,
                )
                .expect("关联展开失败"),
            other => panic!("未知步骤：{other}"),
        };
        outputs.push(json!({
            "kind": kind,
            "results": results.iter().map(render).collect::<Vec<Value>>(),
        }));
    }

    assert_eq!(
        json!(outputs),
        fixture["expected_outputs"],
        "检索步骤输出与 Python 不一致"
    );
    std::fs::remove_dir_all(&root).ok();
}
