//! 对照：Rust 记忆工具 vs Python 真实现。
//!
//! 数据集由 `python rust/tools/gen_memory_tools_fixture.py` 生成（期望值取自
//! `omnicrawl/agent/toolkit/memory_tools.py` 与 `omnicrawl/state/memory.py`）。
//! 记忆 id 与时间戳由存储层各自生成，两侧不逐字相同——数据集与测试都把它们规范化成
//! `{ID}` / `{TS}`：存储层的生成规则已由 `omnicrawl-session` 的 `memory_store` 对照覆盖，
//! 这里对照的是工具层的参数适配、作用域解析与输出形状。
//!
//! 用例按生成顺序重放：读取会加深记忆（刷新时间戳与触碰次数），顺序本身也是对照的一部分。

use std::path::PathBuf;

use regex::Regex;
use serde_json::{Map, Value};

use omnicrawl_tui::tools::{memory, MemoryOptions};

const FIXTURE: &str = include_str!("fixtures/memory_tools_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn normalize(text: &str) -> String {
    let id = Regex::new(r#""id": "[^"]*""#).expect("id 正则应当合法");
    let timestamp = Regex::new(r#""timestamp": "[^"]*""#).expect("时间戳正则应当合法");
    let with_id = id.replace_all(text, r#""id": "{ID}""#);
    timestamp
        .replace_all(&with_id, r#""timestamp": "{TS}""#)
        .to_string()
}

/// 把数据集里的 `{ID1}`、`{ID2}` 占位符换成本次运行真实生成的记忆 id。
fn substitute(value: &Value, ids: &[String]) -> Value {
    match value {
        Value::String(text) => {
            let mut text = text.clone();
            for (index, memory_id) in ids.iter().enumerate() {
                text = text.replace(&format!("{{ID{}}}", index + 1), memory_id);
            }
            Value::String(text)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| substitute(item, ids)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute(item, ids)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn prepare_options() -> (MemoryOptions, PathBuf) {
    let root = std::env::temp_dir().join("omnicrawl-tui-memory-parity");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    let options = MemoryOptions {
        workspace_root: root.clone(),
        project_directory: "project-memory".to_string(),
        project_enabled: true,
        ..MemoryOptions::default()
    };
    (options, root)
}

#[test]
fn memory_tools_match_python() {
    let data = fixture();
    let (options, root) = prepare_options();

    let seeded =
        memory::memory_write(&options, &arguments(&data["seed"])).expect("预置记忆应当写入成功");
    let records: Value = serde_json::from_str(&seeded).expect("预置输出是 JSON");
    let ids: Vec<String> = records
        .as_array()
        .expect("预置输出是数组")
        .iter()
        .map(|record| record["id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(ids.len(), 3, "预置应当写入三条记忆：{seeded}");
    assert!(root.join("project-memory/index.json").is_file());

    for case in data["cases"].as_array().expect("用例") {
        let tool = case["tool"].as_str().unwrap_or_default();
        let args = arguments(&substitute(&case["arguments"], &ids));
        let expected = case["output"].as_str().unwrap_or_default();
        let outcome = match tool {
            "memory_search" => memory::memory_search(&options, &args),
            "memory_read" => memory::memory_read(&options, &args),
            "memory_expand_related" => memory::memory_expand_related(&options, &args),
            "memory_write" => memory::memory_write(&options, &args),
            other => panic!("数据集里出现未知工具：{other}"),
        };
        match outcome {
            Ok(output) => {
                assert!(
                    case["ok"].as_bool().unwrap_or(false),
                    "{tool} 本应失败，实际成功：{output}"
                );
                assert_eq!(
                    normalize(&output),
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
                    normalize(&error.formatted()),
                    expected,
                    "用例 {tool} {:?}",
                    case["arguments"]
                );
            }
        }
    }
}
