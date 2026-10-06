//! 会话记录解码与诊断的跨语言 parity：数据集是冻结的对照契约。
//!
//! 覆盖事件字典迁移、事件解码（字典与 JSONL 单行）、转录整份读取（真实临时文件）、
//! 索引文档解析与构造、Python `str.splitlines()` 的切行规则。
//! 诊断明细里的 `json_error` 来自各自的 JSON 库：两侧都替换成占位符后再比对。

use std::fs;
use std::path::PathBuf;

use omnicrawl_session::{
    build_index_document, decode_session_event_dict, decode_session_event_line, migrate_event_dict,
    parse_index_document, read_session_events_with_diagnostics, split_lines_python,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/session_records_parity.json");
const DISPLAY_PATH: &str = "sessions/20260918-030529-abcdef.jsonl";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// 把诊断明细里的 JSON 库错误文本换成占位符（两侧库不同，文本必然不同）。
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

fn placeholder(fixture: &Value) -> String {
    fixture["json_error_placeholder"]
        .as_str()
        .expect("json_error_placeholder")
        .to_string()
}

#[test]
fn migrate_matches_python() {
    let fixture = fixture();
    for case in fixture["migrate"].as_array().expect("migrate") {
        let input = &case["input"];
        let label = case["label"].as_str().unwrap_or("");
        let produced = match migrate_event_dict(input) {
            Ok((data, tag)) => json!({
                "label": case["label"],
                "input": input,
                "ok": true,
                "value": {"data": data, "tag": tag},
            }),
            Err(error) => json!({
                "label": case["label"],
                "input": input,
                "ok": false,
                "error": error.to_string(),
            }),
        };
        assert_eq!(produced, *case, "事件迁移（{label}）");
    }
}

#[test]
fn decode_dict_matches_python() {
    let fixture = fixture();
    let placeholder = placeholder(&fixture);
    for case in fixture["decode_dict"].as_array().expect("decode_dict") {
        let input = &case["input"];
        let label = case["label"].as_str().unwrap_or("");
        let (event, diagnostics) = decode_session_event_dict(
            &input["data"],
            input["path"].as_str(),
            input["line_no"].as_u64().map(|value| value as usize),
            input["expected_session_id"].as_str(),
        );
        let mut produced = json!({
            "label": case["label"],
            "input": input,
            "event": event.map(|item| item.to_dict()).unwrap_or(Value::Null),
            "diagnostics": diagnostics.iter().map(|item| item.to_dict()).collect::<Vec<Value>>(),
        });
        flatten_json_error(&mut produced, &placeholder);
        assert_eq!(produced, *case, "事件解码（{label}）");
    }
}

#[test]
fn decode_line_matches_python() {
    let fixture = fixture();
    let placeholder = placeholder(&fixture);
    for case in fixture["decode_line"].as_array().expect("decode_line") {
        let input = &case["input"];
        let label = case["label"].as_str().unwrap_or("");
        let (event, diagnostics) = decode_session_event_line(
            input["line"].as_str().expect("line"),
            input["path"].as_str(),
            input["line_no"].as_u64().map(|value| value as usize),
            input["expected_session_id"].as_str(),
            input["is_last_nonempty_line"].as_bool().unwrap_or(false),
            input["file_ends_with_newline"].as_bool().unwrap_or(true),
        );
        let mut produced = json!({
            "label": case["label"],
            "input": input,
            "event": event.map(|item| item.to_dict()).unwrap_or(Value::Null),
            "diagnostics": diagnostics.iter().map(|item| item.to_dict()).collect::<Vec<Value>>(),
        });
        flatten_json_error(&mut produced, &placeholder);
        assert_eq!(produced, *case, "单行解码（{label}）");
    }
}

#[test]
fn read_file_matches_python() {
    let fixture = fixture();
    let placeholder = placeholder(&fixture);
    let root = records_root();
    for case in fixture["read_file"].as_array().expect("read_file") {
        let label = case["label"].as_str().expect("label");
        let path = root.join(format!("{label}.jsonl"));
        match case["text"].as_str() {
            Some(text) => fs::write(&path, text).expect("写转录"),
            None => {
                let _ = fs::remove_file(&path);
            }
        }
        let result = read_session_events_with_diagnostics(
            &path,
            case["session_id"].as_str(),
            Some(DISPLAY_PATH),
        );
        let mut produced = json!({
            "label": case["label"],
            "session_id": case["session_id"],
            "path_exists": case["path_exists"],
            "text": case["text"],
            "result": result.expect("读取转录").to_dict(),
        });
        flatten_json_error(&mut produced, &placeholder);
        assert_eq!(produced, *case, "转录读取（{label}）");
    }
}

#[test]
fn parse_index_matches_python() {
    let fixture = fixture();
    for case in fixture["parse_index"].as_array().expect("parse_index") {
        let input = &case["input"];
        let label = case["label"].as_str().unwrap_or("");
        let produced = match parse_index_document(input) {
            Ok((sessions, version)) => json!({
                "label": case["label"],
                "input": input,
                "ok": true,
                "value": {"sessions": sessions, "version": version},
            }),
            Err(error) => json!({
                "label": case["label"],
                "input": input,
                "ok": false,
                "error": error.to_string(),
            }),
        };
        assert_eq!(produced, *case, "索引解析（{label}）");
    }
}

#[test]
fn build_index_matches_python() {
    let fixture = fixture();
    for case in fixture["build_index"].as_array().expect("build_index") {
        let input = case["input"].as_array().expect("input");
        assert_eq!(
            build_index_document(input),
            case["expected"],
            "索引构造（{}）",
            case["label"].as_str().unwrap_or("")
        );
    }
}

#[test]
fn splitlines_matches_python() {
    let fixture = fixture();
    for case in fixture["splitlines"].as_array().expect("splitlines") {
        let input = case["input"].as_str().expect("input");
        let produced: Vec<Value> = split_lines_python(input)
            .into_iter()
            .map(Value::String)
            .collect();
        assert_eq!(
            Value::Array(produced),
            case["expected"],
            "切行（{input:?}）"
        );
    }
}

fn records_root() -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("omnicrawl-session-records-{}", std::process::id()));
    fs::create_dir_all(&root).expect("建临时目录");
    root
}
