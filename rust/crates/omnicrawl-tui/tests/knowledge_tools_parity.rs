//! 对照：Rust 知识库工具 vs Python 真实现。
//!
//! 数据集是冻结的对照契约（期望值取自
//! `omnicrawl/knowledge/__init__.py` 与 `omnicrawl/agent/toolkit/knowledge_tools.py`）。
//! 用例按生成顺序重放：写/追加类用例会改变知识库状态，顺序本身也是对照的一部分。

use std::path::PathBuf;

use serde_json::{Map, Value};

use omnicrawl_tui::tools::{knowledge, KnowledgeBase, RerankOptions};

const FIXTURE: &str = include_str!("fixtures/knowledge_tools_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn prepare_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("omnicrawl-tui-kb-parity-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时知识库");
    root
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

#[test]
fn knowledge_tools_match_python() {
    let data = fixture();
    let root = prepare_root("tools");
    for entry in data["initial"].as_array().expect("初始状态") {
        let path = root.join(entry["path"].as_str().unwrap_or_default());
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("创建目录");
        std::fs::write(path, entry["text"].as_str().unwrap_or_default()).expect("写初始笔记");
    }
    let base = KnowledgeBase::new(&root);
    let today = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();

    for case in data["cases"].as_array().expect("用例") {
        let tool = case["tool"].as_str().unwrap_or_default();
        let args = arguments(&case["arguments"]);
        let expected = case["output"]
            .as_str()
            .unwrap_or_default()
            .replace(&today, "{TODAY}");
        let outcome = match tool {
            "kb_search" => knowledge::kb_search(&base, &RerankOptions::default(), &args),
            "kb_read" => knowledge::kb_read(&base, &args),
            "kb_write" => knowledge::kb_write(&base, &args),
            "kb_append" => knowledge::kb_append(&base, &args),
            "kb_list" => knowledge::kb_list(&base, &args),
            other => panic!("数据集里出现未知工具：{other}"),
        };
        match outcome {
            Ok(output) => {
                assert!(
                    case["ok"].as_bool().unwrap_or(false),
                    "{tool} 本应失败，实际成功：{output}"
                );
                assert_eq!(
                    output.replace(&today, "{TODAY}"),
                    expected,
                    "用例 {tool} {:?}",
                    case["arguments"]
                );
            }
            Err(error) => {
                assert!(
                    !case["ok"].as_bool().unwrap_or(true),
                    "{tool} 本应成功，实际失败：{}",
                    error.message
                );
                assert_eq!(
                    error.formatted().replace(&today, "{TODAY}"),
                    expected,
                    "用例 {tool} {:?}",
                    case["arguments"]
                );
            }
        }
    }
}

#[test]
fn index_file_skips_itself_and_reflects_notes() {
    let root = prepare_root("index");
    std::fs::write(root.join("a.md"), "---\ntitle: 甲\n---\n\n正文\n").expect("写笔记");
    std::fs::write(root.join("INDEX.md"), "手工内容\n").expect("写索引");
    let base = KnowledgeBase::new(&root);
    base.refresh_index().expect("刷新索引");
    let index = std::fs::read_to_string(root.join("INDEX.md")).expect("索引应当存在");
    assert!(index.starts_with("# Knowledge Base Index"), "{index}");
    assert!(index.contains("共 1 篇笔记。"), "{index}");
    assert!(index.contains("- [甲](a.md)"), "{index}");
    assert!(!index.contains("手工内容"), "{index}");
}
