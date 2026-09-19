//! 提示历史的跨语言 parity：期望值来自 Python 真实现。
//!
//! 覆盖展示清洗、条目构造与解析（含各类报错），以及在真实临时目录上的
//! PromptHistoryStore 轨迹（追加 / 查询 / 读取 / 坏行文件）。
//! 临时根在数据集里是 `<ROOT>` 占位；追加一律传固定时刻，落盘行才逐字节可比。

use std::fs;
use std::path::PathBuf;

use omnicrawl_session::{
    clean_prompt_display, parse_datetime, PromptHistoryEntry, PromptHistoryStore,
};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/prompt_history_parity.json");
const ROOT_PLACEHOLDER: &str = "<ROOT>";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

struct Env {
    root: PathBuf,
}

fn env() -> &'static Env {
    static ENV: std::sync::OnceLock<Env> = std::sync::OnceLock::new();
    ENV.get_or_init(|| {
        let raw =
            std::env::temp_dir().join(format!("omnicrawl-prompt-parity-{}", std::process::id()));
        fs::create_dir_all(raw.join("proj")).expect("建 proj");
        fs::create_dir_all(raw.join("other")).expect("建 other");
        // 走与内核同一套归一化：Windows 上要去掉 `\\?\` 前缀，否则与 Python 的 resolve() 形态不同。
        let root = PathBuf::from(
            omnicrawl_session::normalize_project_path(&raw.to_string_lossy()).expect("解析工作根"),
        );
        Env { root }
    })
}

impl Env {
    fn unmask(&self, text: &str) -> String {
        text.replace(ROOT_PLACEHOLDER, &self.root.to_string_lossy())
    }

    /// 还原写进 JSON 文本的路径：字符串里要的是转义形态（`\\`）。
    fn unmask_json_text(&self, text: &str) -> String {
        let escaped = self.root.to_string_lossy().replace('\\', "\\\\");
        text.replace(ROOT_PLACEHOLDER, &escaped)
    }

    fn mask(&self, text: &str) -> String {
        let base = self.root.to_string_lossy().to_string();
        text.replace(&base.replace('\\', "\\\\"), ROOT_PLACEHOLDER)
            .replace(&base, ROOT_PLACEHOLDER)
    }

    fn unmask_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.unmask(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.unmask_value(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), self.unmask_value(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn mask_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.mask(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.mask_value(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), self.mask_value(item)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

fn flatten_json_error(value: &mut Value, placeholder: &str) {
    match value {
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| flatten_json_error(item, placeholder)),
        Value::Object(map) => {
            if let Some(Value::Object(details)) = map.get_mut("details") {
                if details.contains_key("json_error") {
                    details.insert("json_error".to_string(), json!(placeholder));
                }
            }
            for item in map.values_mut() {
                flatten_json_error(item, placeholder);
            }
        }
        _ => {}
    }
}

#[test]
fn clean_display_matches_python() {
    let fixture = fixture();
    for case in fixture["pure"]["clean_display"]
        .as_array()
        .expect("clean_display")
    {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            clean_prompt_display(input),
            case["expected"].as_str().expect("expected"),
            "展示清洗（{} 字符）",
            input.chars().count()
        );
    }
}

#[test]
fn entry_create_matches_python() {
    let fixture = fixture();
    let env = env();
    for case in fixture["pure"]["entry_create"]
        .as_array()
        .expect("entry_create")
    {
        let input = &case["input"];
        let label = case["label"].as_str().unwrap_or("");
        let display = input["display"].as_str().expect("display");
        let project = env.unmask(input["project"].as_str().expect("project"));
        let session_id = input["session_id"].as_str().expect("session_id");
        let pasted: Option<Map<String, Value>> = input["pasted_contents"].as_object().cloned();
        let now = input["now"]
            .as_str()
            .map(|text| parse_datetime(text).expect("now"));

        let produced = match PromptHistoryEntry::create(display, &project, session_id, pasted, now)
        {
            Ok(entry) => json!({
                "label": case["label"],
                "input": input,
                "ok": true,
                "value": env.mask_value(&entry.to_dict()),
            }),
            Err(error) => json!({
                "label": case["label"],
                "input": input,
                "ok": false,
                "error": env.mask(&error.to_string()),
            }),
        };
        assert_eq!(produced, *case, "条目构造（{label}）");
    }
}

#[test]
fn entry_from_dict_matches_python() {
    let fixture = fixture();
    let env = env();
    for case in fixture["pure"]["entry_from_dict"]
        .as_array()
        .expect("entry_from_dict")
    {
        let input = env.unmask_value(&case["input"]);
        let label = case["label"].as_str().unwrap_or("");
        let produced = match PromptHistoryEntry::from_dict(&input) {
            Ok(entry) => json!({
                "label": case["label"],
                "input": case["input"],
                "ok": true,
                "value": env.mask_value(&entry.to_dict()),
            }),
            Err(error) => json!({
                "label": case["label"],
                "input": case["input"],
                "ok": false,
                "error": env.mask(&error.to_string()),
            }),
        };
        assert_eq!(produced, *case, "条目解析（{label}）");
    }
}

#[test]
fn store_traces_match_python() {
    let fixture = fixture();
    let env = env();
    let placeholder = fixture["json_error_placeholder"]
        .as_str()
        .expect("json_error_placeholder")
        .to_string();
    let cases = fixture["store"].as_array().expect("store");
    assert!(!cases.is_empty(), "数据集为空");

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let store = PromptHistoryStore::open(env.root.join(label).join("history.jsonl"), true);
        let mut results: Vec<Value> = Vec::new();

        for op in case["ops"].as_array().expect("ops") {
            let kind = op["op"].as_str().expect("op");
            match kind {
                "write_file" => {
                    fs::create_dir_all(store.root()).expect("建目录");
                    let text = env.unmask_json_text(op["text"].as_str().expect("text"));
                    fs::write(store.path(), text).expect("写 history.jsonl");
                    results.push(json!({"op": kind, "ok": true, "value": Value::Null}));
                }
                "append" => {
                    let project = env.unmask(op["project"].as_str().expect("project"));
                    let pasted: Option<Map<String, Value>> =
                        op["pasted_contents"].as_object().cloned();
                    let now = op["now"]
                        .as_str()
                        .map(|text| parse_datetime(text).expect("now"));
                    let produced = store.append(
                        op["display"].as_str().expect("display"),
                        &project,
                        op["session_id"].as_str().expect("session_id"),
                        pasted,
                        now,
                    );
                    results.push(match produced {
                        Ok(Some(entry)) => json!({
                            "op": kind, "ok": true, "value": env.mask_value(&entry.to_dict()),
                        }),
                        Ok(None) => json!({"op": kind, "ok": true, "value": Value::Null}),
                        Err(error) => json!({
                            "op": kind, "ok": false, "error": env.mask(&error.to_string()),
                        }),
                    });
                }
                "search" => {
                    let project = op["project"].as_str().map(|text| env.unmask(text));
                    let produced = store.search(
                        project.as_deref(),
                        op["session_id"].as_str(),
                        op["query"].as_str().unwrap_or(""),
                        op["limit"].as_i64().expect("limit"),
                    );
                    results.push(match produced {
                        Ok(entries) => json!({
                            "op": kind,
                            "ok": true,
                            "value": Value::Array(
                                entries.iter().map(|entry| env.mask_value(&entry.to_dict())).collect()
                            ),
                        }),
                        Err(error) => json!({
                            "op": kind, "ok": false, "error": env.mask(&error.to_string()),
                        }),
                    });
                }
                "read" => {
                    let (entries, diagnostics) =
                        store.read_entries_with_diagnostics().expect("读取提示历史");
                    let mut value = json!({
                        "entries": entries
                            .iter()
                            .map(|entry| env.mask_value(&entry.to_dict()))
                            .collect::<Vec<Value>>(),
                        "diagnostics": diagnostics
                            .iter()
                            .map(|item| item.to_dict())
                            .collect::<Vec<Value>>(),
                    });
                    flatten_json_error(&mut value, &placeholder);
                    results.push(json!({"op": kind, "ok": true, "value": value}));
                }
                other => panic!("未知的 op：{other}"),
            }
        }

        assert_eq!(
            Value::Array(results),
            case["results"],
            "步骤结果（{label}）"
        );

        let produced_file = if store.path().exists() {
            env.mask(&fs::read_to_string(store.path()).expect("读 history.jsonl"))
        } else {
            String::new()
        };
        assert_eq!(
            produced_file,
            case["file"].as_str().expect("file"),
            "落盘内容（{label}）"
        );
    }
}
