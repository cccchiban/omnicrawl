//! 飞书入站队列的跨语言 parity：期望值来自 Python 真实现。
//!
//! 每个用例是一份动作脚本（入队 / 确认 / 回放 / 重启 / 损坏文件），Rust 侧用可注入时钟
//! 重放同一份脚本，比对每步结果与最终 `pending.jsonl` / `done.jsonl` / `state.json` 的字节。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use omnicrawl_connectors::feishu::FeishuInbox;
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/feishu_inbox_parity.json");
const FILE_NAMES: [&str; 3] = ["pending.jsonl", "done.jsonl", "state.json"];

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn work_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("omnicrawl-inbox-parity-{}", std::process::id()));
    fs::create_dir_all(&root).expect("建工作根");
    root
}

fn build_inbox(root: &Path, memory: bool, config: &Value, clock: Arc<Mutex<f64>>) -> FeishuInbox {
    FeishuInbox::with_clock(
        if memory {
            None
        } else {
            Some(root.to_path_buf())
        },
        config["ttl"].as_f64().expect("ttl"),
        config["max_records"].as_u64().expect("max_records") as usize,
        config["compact_keep"].as_u64().expect("compact_keep") as usize,
        Box::new(move || *clock.lock().expect("时钟锁")),
    )
}

fn record_to_value(record: &omnicrawl_connectors::feishu::InboxRecord) -> Value {
    json!({
        "seq": record.seq,
        "event_id": record.event_id,
        "dedupe_key": record.dedupe_key,
        "payload": Value::Object(record.payload.clone()),
        "created_at": record.created_at,
        "version": record.version,
    })
}

fn run_case(root_root: &Path, spec: &Value) -> Value {
    let label = spec["label"].as_str().expect("label");
    let root = root_root.join(label);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("建用例目录");

    let config = &spec["config"];
    let memory = config["memory"].as_bool().unwrap_or(false);
    let clock = Arc::new(Mutex::new(1000.0_f64));
    let mut inbox = build_inbox(&root, memory, config, Arc::clone(&clock));
    let mut results: Vec<Value> = Vec::new();

    for step in spec["ops"].as_array().expect("ops") {
        let kind = step["op"].as_str().expect("op");
        match kind {
            "enqueue" => {
                let payload: Option<Map<String, Value>> =
                    step.get("payload").and_then(Value::as_object).cloned();
                let value = inbox.enqueue(
                    step["event_id"].as_str().expect("event_id"),
                    step.get("dedupe_key").and_then(Value::as_str),
                    payload,
                );
                results.push(json!({"op": kind, "value": value}));
            }
            "confirm" => {
                inbox.confirm(step["key"].as_str().expect("key"));
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            "recover" => {
                let records: Vec<Value> = inbox.recover().iter().map(record_to_value).collect();
                results.push(json!({"op": kind, "value": records}));
            }
            "is_duplicate" => {
                let value = inbox.is_duplicate(step["key"].as_str().expect("key"));
                results.push(json!({"op": kind, "value": value}));
            }
            "pending_count" => {
                results.push(json!({"op": kind, "value": inbox.pending_count()}));
            }
            "count_done" => {
                results.push(json!({"op": kind, "value": inbox.done_count()}));
            }
            "memory_only" => {
                results.push(json!({"op": kind, "value": inbox.memory_only()}));
            }
            "set_now" => {
                *clock.lock().expect("时钟锁") = step["value"].as_f64().expect("value");
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            "close" => {
                inbox.close();
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            "reopen" => {
                inbox.close();
                inbox = build_inbox(&root, memory, config, Arc::clone(&clock));
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            "write_file" => {
                fs::write(
                    root.join(step["name"].as_str().expect("name")),
                    step["text"].as_str().expect("text"),
                )
                .expect("写文件");
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            other => panic!("未知的 op：{other}"),
        }
    }

    inbox.close();
    let mut files = Map::new();
    for name in FILE_NAMES {
        let path = root.join(name);
        let text = if path.exists() {
            Value::String(fs::read_to_string(&path).expect("读文件"))
        } else {
            Value::Null
        };
        files.insert(name.to_string(), text);
    }

    json!({
        "label": spec["label"],
        "config": spec["config"],
        "ops": spec["ops"],
        "results": results,
        "files": Value::Object(files),
    })
}

#[test]
fn inbox_scenarios_match_python() {
    let fixture = fixture();
    let root_root = work_root();
    let cases = fixture.as_array().expect("cases");
    assert!(!cases.is_empty(), "数据集为空");

    for spec in cases {
        let label = spec["label"].as_str().unwrap_or("");
        let produced = run_case(&root_root, spec);
        assert_eq!(produced["results"], spec["results"], "步骤结果（{label}）");
        assert_eq!(produced["files"], spec["files"], "落盘内容（{label}）");
    }
}
