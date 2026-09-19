//! 会话存储的跨语言 parity：同一串操作，Python 真实现与 Rust 存储各跑一遍，
//! 比对「步骤产出 + 磁盘上的文件字节 + 目录骨架」。
//!
//! fixture 由 `rust/tools/gen_session_store_fixture.py` 生成：它在临时目录里跑真
//! `SessionStore`，把结果连同每个文件的确切内容一起记下来。两边都会做同样的归一化
//! （会话 id、事件 id 是随机值；`session_started` 里的运行时身份依赖各自环境）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use omnicrawl_session::{SessionStore, SessionStoreError};
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/session_store_parity.json");

/// 数据集里的工作区路径是 Windows 形式（`D:\work\demo`）：重放前换成本机等价路径，
/// 三种书写形态（含文件内容里的转义形态）都要跟着换，否则非 Windows 上会被当成相对路径。
fn workspace_mapping() -> BTreeMap<String, String> {
    let native = std::env::temp_dir()
        .join("omnicrawl-parity-workspace")
        .to_string_lossy()
        .to_string();
    BTreeMap::from([
        (
            "D:\\\\work\\\\demo".to_string(),
            native.replace('\\', "\\\\"),
        ),
        ("D:\\work\\demo".to_string(), native.clone()),
        ("D:/work/demo".to_string(), native),
    ])
}

#[test]
fn store_matches_python() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    let base_time = parse_time(fixture["base_time"].as_str().expect("base_time"));

    for scenario in fixture["scenarios"].as_array().expect("缺少 scenarios") {
        // 输入与期望值里的工作区路径一起换成本机等价路径。
        let scenario = replace(scenario, &workspace_mapping());
        let name = scenario["name"].as_str().expect("场景名");
        let root = temp_root(name);
        let store = SessionStore::open(&root);
        let mut context: Option<String> = None;
        let mut outputs = Vec::new();
        for step in scenario["steps"].as_array().expect("缺少 steps") {
            outputs.push(run_step(&store, &root, step, &mut context, base_time));
        }

        let mut actual = json!({
            "outputs": outputs,
            "files": snapshot(&root),
            "dirs": layout(&root),
        });
        actual = normalize(&actual);

        // 两侧都归一化：随机 id 与运行时身份在各自环境里取值不同，只比对形状与内容。
        let expected = normalize(&json!({
            "outputs": scenario["outputs"],
            "files": scenario["files"],
            "dirs": scenario["dirs"],
        }));
        for key in ["outputs", "files", "dirs"] {
            assert_eq!(actual[key], expected[key], "场景 {name} 的 {key} 不一致");
        }

        fs::remove_dir_all(&root).ok();
    }
}

fn run_step(
    store: &SessionStore,
    root: &Path,
    step: &Value,
    context: &mut Option<String>,
    base_time: DateTime<Utc>,
) -> Value {
    let kind = step["kind"].as_str().expect("步骤类型");
    let session_id = step["session_id"]
        .as_str()
        .map(|value| match context.as_deref() {
            Some(current) => value.replace("$session", current),
            None => value.to_string(),
        });

    match kind {
        "ensure" => record(store.ensure(), kind),
        "start_session" => {
            let now = base_time + chrono::Duration::seconds(offset(step, 0));
            match store.start_session(
                step["workspace_root"].as_str().unwrap_or_default(),
                step.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                now,
            ) {
                Ok(created) => {
                    *context = Some(created.session_id.clone());
                    json!({
                        "kind": kind,
                        "session_id": created.session_id,
                        "title": created.title,
                        "event_count": created.event_count,
                        "last_event_type": created.last_event_type,
                    })
                }
                Err(error) => error_output(kind, &error),
            }
        }
        "append_event" => {
            let now = base_time + chrono::Duration::seconds(offset(step, 1));
            let payload = step
                .get("payload")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_else(Map::new);
            let outcome = store.append_event(
                session_id.as_deref().unwrap_or_default(),
                step["event_type"].as_str().unwrap_or_default(),
                payload,
                step.get("parent_id").and_then(Value::as_str),
                now,
            );
            match outcome {
                Ok(event) => json!({"kind": kind, "event": event.to_dict()}),
                Err(error) => error_output(kind, &error),
            }
        }
        "read_events" => match store.read_events(session_id.as_deref().unwrap_or_default()) {
            Ok(events) => json!({
                "kind": kind,
                "events": events.iter().map(|event| event.to_dict()).collect::<Vec<Value>>(),
            }),
            Err(error) => error_output(kind, &error),
        },
        "list_sessions" => match store.list_sessions() {
            Ok(entries) => json!({
                "kind": kind,
                "sessions": entries.iter().map(|entry| entry.to_dict()).collect::<Vec<Value>>(),
            }),
            Err(error) => error_output(kind, &error),
        },
        "corrupt_line" => {
            let path = root.join("sessions").join(format!(
                "{}.jsonl",
                session_id.as_deref().unwrap_or_default()
            ));
            let line = format!("{}\n", step["line"].as_str().unwrap_or_default());
            match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut handle| std::io::Write::write_all(&mut handle, line.as_bytes()))
            {
                Ok(()) => json!({"kind": kind, "line": step["line"]}),
                Err(error) => error_output(kind, &SessionStoreError::new(error.to_string())),
            }
        }
        "write_file" => {
            let path = root.join(step["path"].as_str().unwrap_or_default());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("创建父目录");
            }
            match fs::write(&path, step["text"].as_str().unwrap_or_default()) {
                Ok(()) => json!({"kind": kind, "path": step["path"]}),
                Err(error) => error_output(kind, &SessionStoreError::new(error.to_string())),
            }
        }
        other => panic!("fixture 里出现未知步骤：{other}"),
    }
}

fn record(outcome: Result<(), SessionStoreError>, kind: &str) -> Value {
    match outcome {
        Ok(()) => json!({"kind": kind}),
        Err(error) => error_output(kind, &error),
    }
}

fn error_output(kind: &str, error: &SessionStoreError) -> Value {
    json!({"kind": kind, "error": error.message()})
}

/// 与生成脚本同一默认值：建会话落在基准时刻，追加事件默认 +1 秒。
fn offset(step: &Value, fallback: i64) -> i64 {
    step.get("offset_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(fallback)
}

/// 同一个 id 会在多处出现，编号只认首次出现。
fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn parse_time(text: &str) -> DateTime<Utc> {
    omnicrawl_session::parse_datetime(text).expect("fixture 时间可解析")
}

fn temp_root(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!("omnicrawl-store-{name}-{stamp}"));
    fs::create_dir_all(&root).expect("创建临时根目录");
    root
}

fn snapshot(root: &Path) -> Value {
    let mut files = Map::new();
    collect(root, root, &mut files);
    Value::Object(files)
}

fn collect(root: &Path, current: &Path, files: &mut Map<String, Value>) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
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
        let text = fs::read_to_string(&path).unwrap_or_else(|_| "<binary>".to_string());
        files.insert(relative, json!(normalize_transcript(&text)));
    }
}

fn layout(root: &Path) -> Value {
    let mut dirs = Vec::new();
    collect_dirs(root, root, &mut dirs);
    dirs.sort();
    json!(dirs)
}

fn collect_dirs(root: &Path, current: &Path, dirs: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(
                path.strip_prefix(root)
                    .expect("相对路径")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            collect_dirs(root, &path, dirs);
        }
    }
}

/// 转录行里的运行时身份依赖各自环境，比对前替换成占位。
fn normalize_transcript(text: &str) -> String {
    if !text.starts_with('{') {
        return text.to_string();
    }
    let mut lines = String::new();
    for line in text.lines() {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            lines.push_str(line);
            lines.push('\n');
            continue;
        };
        if let Some(payload) = value.get_mut("payload").and_then(Value::as_object_mut) {
            if payload.contains_key("runtime") {
                payload.insert("runtime".to_string(), json!({"<normalized>": true}));
            }
        }
        lines.push_str(&serde_json::to_string(&value).expect("可序列化"));
        lines.push('\n');
    }
    lines
}

/// 会话 id 与事件 id 是随机值，按出现顺序换成 `<session-id-N>`、`<event-id-N>`。
fn normalize(value: &Value) -> Value {
    let mut session_ids = Vec::new();
    let mut event_ids = Vec::new();
    collect_random_ids(value, &mut session_ids, &mut event_ids);
    // 编号各自从 1 开始：会话 id 与事件 id 是两套独立的随机值。
    let mut mapping: BTreeMap<String, String> = BTreeMap::new();
    for (index, value) in dedupe(session_ids).into_iter().enumerate() {
        mapping.insert(value, format!("<session-id-{}>", index + 1));
    }
    // 事件 id 不编号：它只出现在转录行里，编号会依赖两个实现的遍历顺序；
    // 位置与个数仍被比对（转录行是逐字节比对的），distinct 不参与断言。
    for value in dedupe(event_ids) {
        mapping.insert(value, "<event-id>".to_string());
    }
    let mut replaced = replace(value, &mapping);
    normalize_pids(&mut replaced);
    normalize_runtime(&mut replaced);
    replaced
}

/// `session_started` 载荷里的运行时身份依赖各自环境（Python 记源码指纹，内核记自己版本）。
fn normalize_runtime(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("runtime") {
                map.insert("runtime".to_string(), json!({"<normalized>": true}));
            }
            map.values_mut().for_each(normalize_runtime);
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_runtime),
        _ => {}
    }
}

/// 锁文件记的是持有者 pid，进程之间天然不同，比对前替换成占位。
fn normalize_pids(value: &mut Value) {
    match value {
        Value::String(text) => {
            let mut out = String::new();
            let mut rest = text.as_str();
            while let Some(position) = rest.find("pid=") {
                out.push_str(&rest[..position + 4]);
                rest = &rest[position + 4..];
                let digits =
                    rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                if digits > 0 {
                    out.push_str("<pid>");
                    rest = &rest[digits..];
                }
            }
            out.push_str(rest);
            *text = out;
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_pids),
        Value::Object(map) => map.values_mut().for_each(normalize_pids),
        _ => {}
    }
}

fn collect_random_ids(value: &Value, session_ids: &mut Vec<String>, event_ids: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            session_ids.extend(find_session_ids(text));
            event_ids.extend(find_event_ids(text));
        }
        Value::Array(items) => {
            for item in items {
                collect_random_ids(item, session_ids, event_ids);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_random_ids(&Value::String(key.clone()), session_ids, event_ids);
                collect_random_ids(item, session_ids, event_ids);
            }
        }
        _ => {}
    }
}

fn replace(value: &Value, mapping: &BTreeMap<String, String>) -> Value {
    match value {
        Value::String(text) => {
            let mut text = text.clone();
            for (original, replacement) in mapping {
                text = text.replace(original, replacement);
            }
            Value::String(text)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| replace(item, mapping)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| {
                    let key = match replace(&Value::String(key.clone()), mapping) {
                        Value::String(text) => text,
                        _ => key.clone(),
                    };
                    (key, replace(item, mapping))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `YYYYMMDD-HHMMSS-xxxxxx`。
fn find_session_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index + 22 <= bytes.len() {
        let window = &bytes[index..index + 22];
        let valid = window[..8].iter().all(u8::is_ascii_digit)
            && window[8] == b'-'
            && window[9..15].iter().all(u8::is_ascii_digit)
            && window[15] == b'-'
            && window[16..22]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
        if valid {
            found.push(text[index..index + 22].to_string());
            index += 22;
        } else {
            index += 1;
        }
    }
    found
}

/// 恰好 24 位小写十六进制（sha256 是 64 位，不会被误伤）。
fn find_event_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_hexdigit() || bytes[index].is_ascii_uppercase() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len()
            && bytes[index].is_ascii_hexdigit()
            && !bytes[index].is_ascii_uppercase()
        {
            index += 1;
        }
        if index - start == 24 {
            found.push(text[start..index].to_string());
        }
    }
    found
}
