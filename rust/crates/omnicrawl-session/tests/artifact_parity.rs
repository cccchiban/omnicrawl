//! 会话 artifact 转存与脱敏的跨语言 parity：期望值来自 Python `session_artifacts.py`。
//!
//! 产物名基于 sha256 前缀，因此无需归一化：每步返回载荷与最终文件快照都可逐字比对。

use std::collections::BTreeMap;
use std::path::Path;

use omnicrawl_session::{
    preview_text, redact_sensitive_html, tool_output_summary, SessionArtifactStore,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/artifact_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn snapshot(root: &Path) -> BTreeMap<String, String> {
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
        files.insert(relative, std::fs::read_to_string(&path).unwrap_or_default());
    }
}

#[test]
fn artifact_store_matches_python() {
    let fixture = fixture();
    let session_id = fixture["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    let root = std::env::temp_dir().join(format!(
        "omnicrawl-artifact-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&root).expect("创建临时根目录");
    let store = SessionArtifactStore::new(&root, root.join("artifacts"));

    let mut outputs: Vec<Value> = Vec::new();
    let mut first_artifact: Option<String> = None;

    for step in fixture["steps"].as_array().expect("steps") {
        let kind = step["kind"].as_str().expect("kind");
        let value = match kind {
            "event" => {
                let event_type = step["event_type"].as_str().expect("event_type");
                let payload = step["payload"].clone();
                match store.prepare_event_payload(&session_id, event_type, Some(&payload)) {
                    Ok(payload) => {
                        if first_artifact.is_none() {
                            if let Some(path) = payload.get("artifact_path").and_then(Value::as_str)
                            {
                                first_artifact = Some(path.to_string());
                            }
                        }
                        json!({"kind": kind, "payload": payload})
                    }
                    Err(error) => json!({"kind": kind, "error": error.message()}),
                }
            }
            "subagent" => match store.prepare_subagent_result(
                &session_id,
                step["task_id"].as_str().expect("task_id"),
                step["agent_type"].as_str().expect("agent_type"),
                step["description"].as_str().expect("description"),
                step["result_text"].as_str().expect("result_text"),
                step["summary_chars"].as_u64().expect("summary_chars") as usize,
            ) {
                Ok(result) => json!({"kind": kind, "result": result}),
                Err(error) => json!({"kind": kind, "error": error.message()}),
            },
            "read" => {
                let path = match step["artifact_path"].as_str() {
                    Some(path) => path.to_string(),
                    None => first_artifact.clone().expect("已产出 artifact"),
                };
                match store.read_text(step["session_id"].as_str().expect("session_id"), &path) {
                    Ok(text) => json!({"kind": kind, "artifact_path": path, "text": text}),
                    Err(error) => json!({"kind": kind, "error": error.message()}),
                }
            }
            "preview" => json!({
                "kind": kind,
                "text": preview_text(
                    step["text"].as_str().expect("text"),
                    step["max_chars"].as_u64().expect("max_chars") as usize,
                ),
            }),
            "summary" => json!({
                "kind": kind,
                "text": tool_output_summary(step["text"].as_str().expect("text")),
            }),
            "html" => json!({
                "kind": kind,
                "text": redact_sensitive_html(step["html"].as_str().expect("html")),
            }),
            other => panic!("未知步骤：{other}"),
        };
        outputs.push(value);
    }

    // 逐步比对，失败时只看得到出错那一步（整份输出太大，直接比会淹没信息）。
    let expected_outputs = fixture["expected_outputs"]
        .as_array()
        .expect("expected_outputs")
        .clone();
    assert_eq!(outputs.len(), expected_outputs.len(), "步骤数不一致");
    for (index, (actual, want)) in outputs.iter().zip(expected_outputs.iter()).enumerate() {
        assert_eq!(actual, want, "第 {index} 步与 Python 不一致");
    }

    let files = snapshot(&root);
    let expected: BTreeMap<String, String> = fixture["expected_files"]
        .as_object()
        .expect("expected_files")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().expect("内容").to_string()))
        .collect();
    assert_eq!(files, expected, "artifact 文件快照与 Python 不一致");

    std::fs::remove_dir_all(&root).ok();
}
