//! `agent/subagents/definitions.py` 的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_subagents_fixture.py` 重新生成
//! `fixtures/subagents_parity.json` 后，本套件在临时目录里重放解析用例、真实模板与
//! 四层来源发现。数据集里的临时根与仓库根是 `<ROOT>` / `<REPO>` 占位，两侧各自还原。

use std::path::{Path, PathBuf};

use omnicrawl_config::features::subagents::SubAgentConfig;
use omnicrawl_controllers::subagents::batch::{
    batch_status, batch_summary, is_cancellation_error, requires_shared_writer_lock, ActiveBatch,
};
use omnicrawl_controllers::subagents::coordinator::{
    build_failure_diagnostics, cancelled_payload, failure_payload, json_result_text, query_request,
    select_profile_tools, task_event_payload, terminal_event_payload, top_level_error,
    validate_arguments, worktree_control_request, FailureFacts, PreparedTaskView,
};
use omnicrawl_controllers::subagents::definitions::{
    parse_agent_definition, AgentDefinition, AgentDefinitionRegistry,
};
use omnicrawl_controllers::subagents::execution::{SubAgentExecutionContext, FORK_BOILERPLATE};
use omnicrawl_controllers::subagents::recovery::{
    rebuild_task_snapshots_from_session_events, LifecycleEvent,
};
use omnicrawl_controllers::subagents::verify::{
    parse_verify_arguments, verify_checks, verify_command_description, verify_command_schema,
    VERIFY_COMMAND_TOOL_NAME,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/subagents_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn section(name: &str) -> Value {
    fixture()[name].clone()
}

fn definitions() -> Value {
    fixture()["definitions"].clone()
}

fn norm(text: &str) -> String {
    text.replace('\\', "/")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("仓库根")
        .to_path_buf()
}

/// 临时根与仓库根的多种书写形式（`temp_dir` 可能是短名，`canonicalize` 展开成长名）。
struct Paths {
    root: PathBuf,
    root_variants: Vec<String>,
    repo_variants: Vec<String>,
}

impl Paths {
    fn new(tag: &str) -> Self {
        let raw =
            std::env::temp_dir().join(format!("omnicrawl-subagents-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&raw).expect("建临时根");
        // `temp_dir()` 可能是 8.3 短名，`canonicalize` 才给出长名；以长名为准做比对基准。
        let root = std::fs::canonicalize(&raw)
            .map(strip_verbatim)
            .unwrap_or_else(|_| raw.clone());
        let mut root_variants = variants(&root);
        for variant in variants(&raw) {
            if !root_variants.contains(&variant) {
                root_variants.push(variant);
            }
        }
        root_variants.sort_by_key(|item| std::cmp::Reverse(item.len()));
        Self {
            root_variants,
            repo_variants: variants(&repo_root()),
            root,
        }
    }

    /// 把真实路径换回数据集里的占位符，便于逐字比对。
    fn placeholder(&self, text: &str) -> String {
        let mut out = norm(text);
        for variant in &self.root_variants {
            out = out.replace(variant, "<ROOT>");
        }
        for variant in &self.repo_variants {
            out = out.replace(variant, "<REPO>");
        }
        out
    }

    fn child(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

fn variants(path: &Path) -> Vec<String> {
    let mut items = vec![norm(&path.to_string_lossy())];
    if let Ok(canonical) = std::fs::canonicalize(path) {
        let text = norm(&strip_verbatim(canonical).to_string_lossy());
        if !items.contains(&text) {
            items.push(text);
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.len()));
    items
}

/// Windows 的 `canonicalize` 会给出 `\\?\` 前缀，Python 的 `Path.resolve()` 不带。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

fn assert_definition(paths: &Paths, actual: &AgentDefinition, expected: &Value, label: &str) {
    assert_eq!(
        actual.name,
        expected["name"].as_str().unwrap(),
        "{label} name"
    );
    assert_eq!(
        actual.description,
        expected["description"].as_str().unwrap(),
        "{label} description"
    );
    assert_eq!(
        actual.system_prompt,
        expected["system_prompt"].as_str().unwrap(),
        "{label} system_prompt"
    );
    assert_eq!(actual.tools, strings(&expected["tools"]), "{label} tools");
    assert_eq!(
        actual.disallowed_tools,
        strings(&expected["disallowed_tools"]),
        "{label} disallowedTools"
    );
    assert_eq!(
        actual.model,
        expected["model"].as_str().unwrap(),
        "{label} model"
    );
    assert_eq!(
        actual.permission_mode,
        expected["permission_mode"].as_str().unwrap(),
        "{label} permissionMode"
    );
    assert_eq!(
        actual.background,
        expected["background"].as_bool().unwrap(),
        "{label} background"
    );
    assert_eq!(
        actual.isolation,
        expected["isolation"].as_str().unwrap(),
        "{label} isolation"
    );
    assert_eq!(
        actual.skills,
        strings(&expected["skills"]),
        "{label} skills"
    );
    assert_eq!(
        actual.mcp_servers,
        strings(&expected["mcp_servers"]),
        "{label} mcpServers"
    );
    assert_eq!(
        actual.git_mode,
        expected["git_mode"].as_str().unwrap(),
        "{label} gitMode"
    );
    assert_eq!(
        actual.source,
        expected["source"].as_str().unwrap(),
        "{label} source"
    );
    let actual_path = actual
        .source_path
        .as_ref()
        .map(|path| paths.placeholder(&path.to_string_lossy()))
        .unwrap_or_default();
    match expected["source_path"].as_str() {
        Some(expected_path) => assert_eq!(actual_path, expected_path, "{label} source_path"),
        None => assert!(actual_path.is_empty(), "{label} source_path 应为空"),
    }
}

#[test]
fn parse_definition_matches_python() {
    let paths = Paths::new("defs");
    let case_dir = paths.child("cases");
    std::fs::create_dir_all(&case_dir).expect("建用例目录");
    let cases = definitions()["parse"].as_array().expect("parse").clone();
    for case in cases {
        let label = case["label"].as_str().expect("label");
        let name = case["file"].as_str().expect("file");
        let mut payload = case["content"].as_str().expect("content").to_string();
        payload.push_str(&"x".repeat(case["content_pad"].as_u64().unwrap_or(0) as usize));
        let target = case_dir.join(name);
        std::fs::write(&target, payload).expect("写用例文件");
        let result = parse_agent_definition(&target, "project");
        match result {
            Ok(definition) => {
                assert!(
                    case["ok"].as_bool().unwrap(),
                    "{label}：Rust 侧解析成功但 Python 侧失败"
                );
                assert_definition(&paths, &definition, &case["definition"], label);
            }
            Err(error) => {
                assert!(
                    !case["ok"].as_bool().unwrap(),
                    "{label}：Rust 侧失败（{}）但 Python 侧成功",
                    error.message()
                );
                let actual = paths.placeholder(error.message());
                match case["error_prefix"].as_str() {
                    Some(prefix) => assert!(actual.starts_with(prefix), "{label}：{actual}"),
                    None => assert_eq!(actual, case["error"].as_str().unwrap(), "{label}"),
                }
            }
        }
    }
    std::fs::remove_dir_all(&paths.root).ok();
}

#[test]
fn templates_match_python() {
    let paths = Paths::new("templates");
    let cases = definitions()["templates"]
        .as_array()
        .expect("templates")
        .clone();
    for case in cases {
        let label = case["file"].as_str().expect("file");
        let path = repo_root().join(label);
        let definition = parse_agent_definition(&path, "builtin")
            .unwrap_or_else(|error| panic!("{label}：{}", error.message()));
        assert_definition(&paths, &definition, &case["definition"], label);
    }
    std::fs::remove_dir_all(&paths.root).ok();
}

#[test]
fn discovery_matches_python() {
    let paths = Paths::new("discover");
    let base = paths.child("discover");
    let data = definitions()["discover"].clone();
    for (relative, content) in data["files"].as_object().expect("files") {
        let target = base.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("建目录");
        }
        std::fs::write(&target, content.as_str().expect("内容")).expect("写文件");
    }
    let plugins: Vec<(String, PathBuf)> = data["plugins"]
        .as_array()
        .expect("plugins")
        .iter()
        .map(|item| {
            (
                item["name"].as_str().expect("name").to_string(),
                base.join(item["path"].as_str().expect("path")),
            )
        })
        .collect();

    let mut registry = AgentDefinitionRegistry::new(base.join("builtin"), Some(base.join("home")));
    registry.discover(&base.join("ws"), &plugins);

    let expected_definitions = data["definitions"].as_array().expect("definitions");
    let actual: Vec<(String, String, String)> = registry
        .list_all()
        .iter()
        .map(|item| {
            (
                item.name.clone(),
                item.source.clone(),
                item.source_path
                    .as_ref()
                    .map(|path| paths.placeholder(&path.to_string_lossy()))
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        actual.len(),
        expected_definitions.len(),
        "定义数量不一致：{actual:?}"
    );
    for (index, expected) in expected_definitions.iter().enumerate() {
        assert_eq!(actual[index].0, expected["name"].as_str().unwrap());
        assert_eq!(actual[index].1, expected["source"].as_str().unwrap());
        assert_eq!(actual[index].2, expected["source_path"].as_str().unwrap());
    }

    let expected_diagnostics = data["diagnostics"].as_array().expect("diagnostics");
    let actual: Vec<(String, String, String, String, String)> = registry
        .diagnostics()
        .iter()
        .map(|item| {
            (
                item.kind.clone(),
                paths.placeholder(&item.message),
                paths.placeholder(&item.path),
                paths.placeholder(&item.winner_path),
                paths.placeholder(&item.loser_path),
            )
        })
        .collect();
    assert_eq!(
        actual.len(),
        expected_diagnostics.len(),
        "诊断数量不一致：{actual:?}"
    );
    for (index, expected) in expected_diagnostics.iter().enumerate() {
        assert_eq!(actual[index].0, expected["kind"].as_str().unwrap());
        assert_eq!(actual[index].1, expected["message"].as_str().unwrap());
        assert_eq!(actual[index].2, expected["path"].as_str().unwrap());
        assert_eq!(actual[index].3, expected["winner_path"].as_str().unwrap());
        assert_eq!(actual[index].4, expected["loser_path"].as_str().unwrap());
    }

    for case in data["lookup"].as_array().expect("lookup") {
        let key = case["key"].as_str().expect("key");
        let actual = registry.get(key).map(|item| item.name.clone());
        match case["name"].as_str() {
            Some(name) => assert_eq!(actual.as_deref(), Some(name), "lookup {key:?}"),
            None => assert_eq!(actual, None, "lookup {key:?}"),
        }
    }
    std::fs::remove_dir_all(&paths.root).ok();
}

#[test]
fn execution_defaults_match_python() {
    let data = section("execution");
    assert_eq!(
        FORK_BOILERPLATE,
        data["fork_boilerplate"].as_str().unwrap(),
        "fork boilerplate"
    );
    let expected = &data["context_defaults"];
    let context = SubAgentExecutionContext::default();
    assert_eq!(context.context, expected["context"].as_str().unwrap());
    assert!(context.model_snapshot.is_none(), "默认没有模型快照");
    assert_eq!(
        expected["model_snapshot"].is_null(),
        context.model_snapshot.is_none()
    );
    assert!(context.fork_messages.is_empty());
    assert_eq!(
        expected["fork_messages"]
            .as_array()
            .expect("fork_messages")
            .len(),
        0
    );
    assert_eq!(
        context.parent_system_prompt,
        expected["parent_system_prompt"].as_str().unwrap()
    );
    assert_eq!(
        context.skill_context,
        expected["skill_context"].as_str().unwrap()
    );
    assert!(context.worktree_session.is_none());
    assert_eq!(
        expected["worktree_session"].is_null(),
        context.worktree_session.is_none()
    );
    assert_eq!(
        context.workspace_root,
        expected["workspace_root"].as_str().unwrap()
    );
    assert_eq!(context.isolation, expected["isolation"].as_str().unwrap());
    assert_eq!(context.task_id, expected["task_id"].as_str().unwrap());
    assert_eq!(context.batch_id, expected["batch_id"].as_str().unwrap());
}

#[test]
fn verify_checks_and_arguments_match_python() {
    let data = section("verify");
    assert_eq!(
        VERIFY_COMMAND_TOOL_NAME,
        data["tool_name"].as_str().unwrap()
    );
    assert_eq!(
        verify_command_description(),
        data["description"].as_str().unwrap()
    );
    assert!(!data["requires_confirmation"].as_bool().unwrap());
    assert_eq!(
        verify_command_schema("<PYTHON>", 120).expect("schema"),
        data["argument_schema"].as_str().unwrap()
    );

    let checks = verify_checks("<PYTHON>");
    let expected = data["checks"].as_array().expect("checks");
    assert_eq!(checks.len(), expected.len());
    for (index, item) in expected.iter().enumerate() {
        assert_eq!(
            checks[index].identifier,
            item["identifier"].as_str().unwrap()
        );
        assert_eq!(checks[index].label, item["label"].as_str().unwrap());
        assert_eq!(checks[index].argv, strings(&item["argv"]));
    }

    for case in data["parse"].as_array().expect("parse") {
        let label = case["label"].as_str().expect("label");
        let max_timeout = case["max_timeout_seconds"].as_i64().expect("max");
        let result = parse_verify_arguments(&case["arguments"], "<PYTHON>", max_timeout);
        match result {
            Ok((check, timeout)) => {
                assert!(
                    case["ok"].as_bool().unwrap(),
                    "{label}：Rust 侧成功但 Python 侧失败"
                );
                assert_eq!(check.identifier, case["check"].as_str().unwrap(), "{label}");
                assert_eq!(
                    timeout,
                    case["timeout_seconds"].as_i64().unwrap(),
                    "{label}"
                );
            }
            Err(error) => {
                assert!(
                    !case["ok"].as_bool().unwrap(),
                    "{label}：Rust 侧失败（{}）但 Python 侧成功",
                    error.message()
                );
                assert_eq!(error.message(), case["error"].as_str().unwrap(), "{label}");
            }
        }
    }
}

#[test]
fn recovery_snapshots_match_python() {
    let cases = section("recovery")["cases"].clone();
    for case in cases.as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let events: Vec<LifecycleEvent> = case["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|item| LifecycleEvent {
                event_type: item["type"].as_str().unwrap_or_default().to_string(),
                payload: item["payload"].clone(),
                created_at_seconds: item["created_at_seconds"].as_f64(),
            })
            .collect();
        let actual = rebuild_task_snapshots_from_session_events(
            &events,
            case["owner_id"].as_str().expect("owner_id"),
            case["session_id"].as_str().expect("session_id"),
        );
        assert_eq!(Value::Array(actual), case["snapshots"], "{label}");
    }
}
// `agent/subagents/tasks.py` 的跨语言对照。
//
// 数据集是冻结的对照契约：静态投影（`_bound_result` / `_bound_error`）直接调用，
// 五个脚本化场景用真 `SubAgentTaskManager` 跑一遍，时间戳统一换成 `<TIME>` 占位后比对。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Map;

use omnicrawl_controllers::subagents::tasks::{
    bound_error, bound_result, SubAgentTaskManager, SubAgentTaskSpec, TaskRunner,
};

const TIME_KEYS: [&str; 3] = ["created_at", "updated_at", "timestamp"];

fn scrub_times(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| {
                    let item = if TIME_KEYS.contains(&key.as_str()) && item.is_number() {
                        Value::String("<TIME>".to_string())
                    } else {
                        scrub_times(item)
                    };
                    (key.clone(), item)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(scrub_times).collect()),
        other => other.clone(),
    }
}

fn scrub(value: &Value) -> Value {
    scrub_times(value)
}

/// 通知与批次的完成顺序由线程调度决定，比对前按 task_id 规范化。
fn sorted_by_task(value: Value) -> Value {
    let mut items = value.as_array().cloned().unwrap_or_default();
    items.sort_by_key(|item| {
        item.get("task_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    });
    Value::Array(items)
}

fn completed_map(summary: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("status".to_string(), Value::String("completed".to_string()));
    map.insert("summary".to_string(), Value::String(summary.to_string()));
    map.insert("hidden".to_string(), Value::from(1));
    map
}

fn poll_status(manager: &SubAgentTaskManager, task_id: &str, status: &str) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(current) = manager.get(task_id, "owner-1", Some("session-1")) {
            if current["status"].as_str() == Some(status) {
                return Some(current);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[test]
fn bound_projections_match_python() {
    let data = section("tasks");
    for case in data["bound_result"].as_array().expect("bound_result") {
        let label = case["label"].as_str().expect("label");
        let input = case["input"].as_object();
        let actual = bound_result(input)
            .map(Value::Object)
            .unwrap_or(Value::Null);
        let expected = if case["value"].is_null() {
            Value::Null
        } else {
            case["value"].clone()
        };
        assert_eq!(actual, expected, "{label}");
    }
    for case in data["bound_error"].as_array().expect("bound_error") {
        let label = case["label"].as_str().expect("label");
        let input = case["input"].as_object();
        let actual = bound_error(input).map(Value::Object).unwrap_or(Value::Null);
        let expected = if case["value"].is_null() {
            Value::Null
        } else {
            case["value"].clone()
        };
        assert_eq!(actual, expected, "{label}");
    }
}

#[test]
fn task_manager_scripts_match_python() {
    let data = section("tasks");
    let scripts = data["scripted"].as_array().expect("scripted");

    // 场景一：同步完成、失败与异常。
    let case = &scripts[0];
    let label = case["label"].as_str().expect("label");
    assert_eq!(label, "同步完成失败与异常");
    let manager = SubAgentTaskManager::new(3600.0, 2);
    let specs = vec![
        SubAgentTaskSpec::new(
            "task-000000000001",
            "完成任务",
            "explore",
            "batch-000000000001",
        ),
        SubAgentTaskSpec::new(
            "task-000000000002",
            "失败任务",
            "explore",
            "batch-000000000001",
        ),
        SubAgentTaskSpec::new(
            "task-000000000003",
            "异常任务",
            "explore",
            "batch-000000000001",
        ),
    ];
    let runner: TaskRunner = Arc::new(|spec, _cancel| {
        if spec.task_id.ends_with('2') {
            let mut map = Map::new();
            map.insert("status".to_string(), Value::String("failed".to_string()));
            let mut error = Map::new();
            error.insert("code".to_string(), Value::String("MY_FAIL".to_string()));
            error.insert("message".to_string(), Value::String("失败原因".to_string()));
            map.insert("error".to_string(), Value::Object(error));
            return map;
        }
        if spec.task_id.ends_with('3') {
            panic!("boom");
        }
        completed_map("完成摘要")
    });
    let spawn = manager
        .spawn("owner-1", "session-1", &specs, runner, None)
        .expect("spawn");
    assert_eq!(spawn, case["spawn"]);
    let idle = manager.wait_for_idle("owner-1", Some("session-1"), 5.0);
    assert_eq!(Value::Bool(idle), case["idle"]);
    let listed = scrub(&Value::Array(manager.list("owner-1", Some("session-1"))));
    assert_eq!(listed, case["list"]);
    let first = manager.get("task-000000000001", "owner-1", Some("session-1"));
    let first = scrub(&first.unwrap_or(Value::Null));
    assert_eq!(first, case["get_first"]);
    let other = manager.get("task-000000000001", "owner-1", Some("session-2"));
    assert_eq!(
        Value::Bool(other.is_none()),
        case["get_other_session"].is_null()
    );
    let drained = sorted_by_task(scrub(&Value::Array(
        manager.drain_notifications("owner-1", Some("session-1")),
    )));
    assert_eq!(drained, case["notifications"]);
    let drained_again = scrub(&Value::Array(
        manager.drain_notifications("owner-1", Some("session-1")),
    ));
    assert_eq!(drained_again, case["notifications_again"]);
    assert_eq!(
        Value::Bool(manager.is_idle("owner-1", Some("session-1"))),
        case["is_idle"]
    );
    manager.close("owner-1", None);

    // 场景二：取消运行中的任务。
    let case = &scripts[1];
    assert_eq!(case["label"].as_str().unwrap(), "取消运行中任务");
    let manager = SubAgentTaskManager::new(3600.0, 1);
    let spec = SubAgentTaskSpec::new(
        "task-000000000011",
        "阻塞任务",
        "explore",
        "batch-000000000011",
    );
    let blocking: TaskRunner = Arc::new(|_spec, cancel| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancel.is_set() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        completed_map("迟到")
    });
    manager
        .spawn(
            "owner-1",
            "session-1",
            std::slice::from_ref(&spec),
            blocking,
            None,
        )
        .expect("spawn");
    let running =
        scrub(&poll_status(&manager, "task-000000000011", "running").unwrap_or(Value::Null));
    assert_eq!(running, case["running"]);
    let cancel = manager.cancel(
        "owner-1",
        Some("session-1"),
        Some("task-000000000011"),
        None,
    );
    assert_eq!(cancel, case["cancel"]);
    let idle = manager.wait_for_idle("owner-1", Some("session-1"), 5.0);
    assert_eq!(Value::Bool(idle), case["idle"]);
    let listed = scrub(&Value::Array(manager.list("owner-1", Some("session-1"))));
    assert_eq!(listed, case["list"]);
    manager.close("owner-1", None);

    // 场景三：取消排队任务与整批。
    let case = &scripts[2];
    assert_eq!(case["label"].as_str().unwrap(), "取消排队与批次");
    let manager = SubAgentTaskManager::new(3600.0, 1);
    let specs = vec![
        SubAgentTaskSpec::new(
            "task-000000000021",
            "阻塞一",
            "explore",
            "batch-000000000021",
        ),
        SubAgentTaskSpec::new(
            "task-000000000022",
            "排队二",
            "explore",
            "batch-000000000021",
        ),
    ];
    let blocking: TaskRunner = Arc::new(|_spec, cancel| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancel.is_set() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        completed_map("迟到")
    });
    manager
        .spawn("owner-1", "session-1", &specs, blocking, None)
        .expect("spawn");
    let running =
        scrub(&poll_status(&manager, "task-000000000021", "running").unwrap_or(Value::Null));
    assert_eq!(running, case["running"]);
    let queued = scrub(
        &manager
            .get("task-000000000022", "owner-1", Some("session-1"))
            .unwrap_or(Value::Null),
    );
    assert_eq!(queued, case["queued"]);
    let cancel_queued = manager.cancel(
        "owner-1",
        Some("session-1"),
        Some("task-000000000022"),
        None,
    );
    assert_eq!(cancel_queued, case["cancel_queued"]);
    let cancel_batch = manager.cancel(
        "owner-1",
        Some("session-1"),
        None,
        Some("batch-000000000021"),
    );
    assert_eq!(cancel_batch, case["cancel_batch"]);
    let idle = manager.wait_for_idle("owner-1", Some("session-1"), 5.0);
    assert_eq!(Value::Bool(idle), case["idle"]);
    let listed = scrub(&Value::Array(manager.list("owner-1", Some("session-1"))));
    assert_eq!(listed, case["list"]);
    manager.close("owner-1", None);

    // 场景四：参数校验与关闭后的拒绝。
    let case = &scripts[3];
    assert_eq!(case["label"].as_str().unwrap(), "校验与关闭");
    let manager = SubAgentTaskManager::new(3600.0, 2);
    let never: TaskRunner = Arc::new(|_spec, _cancel| completed_map("完成摘要"));
    let empty = manager.spawn("owner-1", "session-1", &[], never.clone(), None);
    assert_eq!(
        empty.expect_err("空批次应失败").message(),
        case["empty_spawn"]["error"].as_str().unwrap()
    );
    let spec = SubAgentTaskSpec::new(
        "task-000000000031",
        "重复任务",
        "explore",
        "batch-000000000031",
    );
    let quick: TaskRunner = Arc::new(|_spec, _cancel| {
        let mut map = Map::new();
        map.insert("status".to_string(), Value::String("completed".to_string()));
        map
    });
    manager
        .spawn(
            "owner-1",
            "session-1",
            std::slice::from_ref(&spec),
            quick,
            None,
        )
        .expect("spawn");
    let duplicate = manager.spawn(
        "owner-1",
        "session-1",
        std::slice::from_ref(&spec),
        never.clone(),
        None,
    );
    assert_eq!(
        duplicate.expect_err("重复 ID 应失败").message(),
        case["duplicate_spawn"]["error"].as_str().unwrap()
    );
    let missing = manager.cancel(
        "owner-1",
        Some("session-1"),
        Some("task-000000000039"),
        None,
    );
    assert_eq!(missing, case["cancel_missing"]);
    manager.wait_for_idle("owner-1", Some("session-1"), 5.0);
    manager.close("owner-1", None);
    let closed_spec = SubAgentTaskSpec::new(
        "task-000000000032",
        "关闭后",
        "explore",
        "batch-000000000032",
    );
    let closed = manager.spawn(
        "owner-1",
        "session-1",
        std::slice::from_ref(&closed_spec),
        never,
        None,
    );
    assert_eq!(
        closed.expect_err("关闭后应失败").message(),
        case["closed_spawn"]["error"].as_str().unwrap()
    );

    // 场景五：导入跨进程恢复快照。
    let case = &scripts[4];
    assert_eq!(case["label"].as_str().unwrap(), "导入恢复快照");
    let manager = SubAgentTaskManager::new(3600.0, 2);
    let snapshots = case["snapshots"].as_array().expect("snapshots").clone();
    let imported = manager.import_recovered_snapshots("owner-1", "session-1", &snapshots);
    assert_eq!(Value::from(imported), case["imported"]);
    let listed = scrub(&Value::Array(manager.list("owner-1", Some("session-1"))));
    assert_eq!(listed, case["list"]);
    let drained = sorted_by_task(scrub(&Value::Array(
        manager.drain_notifications("owner-1", Some("session-1")),
    )));
    assert_eq!(drained, case["notifications"]);
    manager.close("owner-1", None);
}

// ---- coordinator 判定层 ----------------------------------------------------

fn build_registry(paths: &Paths, data: &Value) -> AgentDefinitionRegistry {
    let base = paths.child("coordinator");
    let builtin = base.join("builtin");
    std::fs::create_dir_all(&builtin).expect("建定义目录");
    for (name, content) in data["definitions"].as_object().expect("definitions") {
        std::fs::write(builtin.join(name), content.as_str().expect("定义内容"))
            .expect("写定义文件");
    }
    let mut registry = AgentDefinitionRegistry::new(builtin, Some(base.join("home")));
    registry.discover(&base.join("ws"), &[]);
    registry
}

fn config_from(key: &str, data: &Value) -> SubAgentConfig {
    let mut config = SubAgentConfig::default();
    let Some(overrides) = data["configs"][key].as_object() else {
        return config;
    };
    for (name, value) in overrides {
        let flag = value.as_bool().expect("布尔开关");
        match name.as_str() {
            "enable_verify_agent" => config.enable_verify_agent = flag,
            "allow_standard_agent" => config.allow_standard_agent = flag,
            "allow_worktree" => config.allow_worktree = flag,
            "allow_shared_workspace_writes" => config.allow_shared_workspace_writes = flag,
            "allow_fork" => config.allow_fork = flag,
            other => panic!("未知配置覆盖：{other}"),
        }
    }
    config
}

#[test]
fn coordinator_validation_matches_python() {
    let paths = Paths::new("coordinator");
    let data = section("coordinator");
    let registry = build_registry(&paths, &data);
    for case in data["validate"].as_array().expect("validate") {
        let label = case["label"].as_str().expect("label");
        let config = config_from(case["config"].as_str().expect("config"), &data);
        let actual = validate_arguments(&case["arguments"], &config, &registry);
        match (actual, case["result"].as_array()) {
            (None, None) => {}
            (Some((code, message)), Some(expected)) => {
                assert_eq!(code, expected[0].as_str().unwrap(), "{label}");
                assert_eq!(
                    paths.placeholder(&message),
                    expected[1].as_str().unwrap(),
                    "{label}"
                );
            }
            (actual, expected) => panic!("{label}：{actual:?} vs {expected:?}"),
        }
    }
    std::fs::remove_dir_all(&paths.root).ok();
}

#[test]
fn coordinator_tool_selection_matches_python() {
    let paths = Paths::new("coordinator-tools");
    let data = section("coordinator");
    let registry = build_registry(&paths, &data);
    for case in data["tools"].as_array().expect("tools") {
        let label = case["label"].as_str().expect("label");
        let definition = registry
            .get(case["definition"].as_str().expect("definition"))
            .expect("定义存在");
        let parent = strings(&case["parent_tools"]);
        let verify = vec!["verify_command".to_string()];
        let selection = select_profile_tools(definition, &parent, &verify);
        assert_eq!(selection.names, strings(&case["tools"]), "{label} 工具表");
        for (name, expected) in case["confirmation"].as_object().expect("confirmation") {
            let effective = case["effective_confirmation"][name]
                .as_bool()
                .unwrap_or(true);
            let actual = if selection.clear_confirmation.contains(name) {
                false
            } else {
                effective
            };
            assert_eq!(Value::Bool(actual), *expected, "{label} {name} 审批");
        }
        let mut wrapped = selection.wrap_read_only_command.clone();
        if selection.wrap_read_only_git {
            wrapped.push("git".to_string());
        }
        wrapped.sort();
        assert_eq!(wrapped, strings(&case["wrapped"]), "{label} 包装");
    }
    std::fs::remove_dir_all(&paths.root).ok();
}

#[test]
fn coordinator_control_requests_match_python() {
    let data = section("coordinator");
    for case in data["worktree"].as_array().expect("worktree") {
        let label = case["label"].as_str().expect("label");
        let actual = worktree_control_request(&case["arguments"]);
        match (actual, case["kind"].as_str()) {
            (Ok(_), Some("ok")) => {}
            (Err((code, message)), None) => {
                assert_eq!(code, case["code"].as_str().unwrap(), "{label} 编号");
                assert_eq!(message, case["message"].as_str().unwrap(), "{label} 文案");
            }
            (actual, expected) => panic!("{label}：{actual:?} vs {expected:?}"),
        }
    }
    for case in data["query"].as_array().expect("query") {
        let label = case["label"].as_str().expect("label");
        let actual = query_request(&case["arguments"]);
        match (actual, case["kind"].as_str()) {
            (Ok(_), Some("ok")) => {}
            (Err((code, message)), None) => {
                assert_eq!(code, case["code"].as_str().unwrap(), "{label} 编号");
                assert_eq!(message, case["message"].as_str().unwrap(), "{label} 文案");
            }
            (actual, expected) => panic!("{label}：{actual:?} vs {expected:?}"),
        }
    }
}

#[test]
fn coordinator_projections_match_python() {
    let data = section("coordinator");
    let projection = data["projection"].clone();
    let task = PreparedTaskView {
        batch_id: "batch-1".to_string(),
        task_id: "task-1".to_string(),
        description: "描述".to_string(),
        agent_type: "review".to_string(),
        definition_source: "builtin".to_string(),
    };
    let diagnostic = json!({"category": "RATE_LIMIT", "retryable": true});
    assert_eq!(
        failure_payload(
            &task,
            "SUBAGENT_MODEL_ERROR",
            "子任务模型请求失败。",
            Some(&diagnostic)
        ),
        projection["failure"]
    );
    assert_eq!(
        cancelled_payload(&task, "任务已取消。"),
        projection["cancelled"]
    );
    assert_eq!(
        top_level_error("SUBAGENT_DISABLED", "未启用"),
        projection["top_level"]
    );
    assert_eq!(
        task_event_payload(&task, "started"),
        projection["task_event"]
    );
    let completed = json!({
        "status": "completed",
        "summary": "ok",
        "artifacts": [],
        "usage": {},
        "error": Value::Null,
    });
    assert_eq!(
        terminal_event_payload(&task, &completed),
        projection["terminal_event"]
    );
    assert_eq!(
        json_result_text(&json!({"b": 1, "a": [1, 2]})),
        projection["json_text"].as_str().unwrap()
    );

    for case in data["diagnostics"].as_array().expect("diagnostics") {
        let label = case["label"].as_str().expect("label");
        let value = &case["value"];
        let facts = FailureFacts {
            exception_type: value["exception_type"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            category: value["category"].as_str().unwrap_or_default().to_string(),
            retryable: value["retryable"].as_bool().unwrap_or(false),
            provider: value["provider"].as_str().unwrap_or_default().to_string(),
            status_code: value["status_code"].as_i64(),
            detail: value["detail"].as_str().unwrap_or_default().to_string(),
        };
        let actual = build_failure_diagnostics(
            Some(&facts),
            case["model"].as_str().expect("model"),
            case["wire_model"].as_str().expect("wire_model"),
        );
        assert_eq!(actual, *value, "{label}");
    }
}

// ---- 批次状态机与汇总 ------------------------------------------------------

#[test]
fn batch_state_machine_matches_python() {
    let data = section("batch");
    for case in data["scripts"].as_array().expect("scripts") {
        let label = case["label"].as_str().expect("label");
        let task_count = case["task_count"].as_u64().expect("task_count") as usize;
        let batch = ActiveBatch::new("batch-1", task_count);
        let mut actual: Vec<Value> = Vec::new();
        for operation in case["operations"].as_array().expect("operations") {
            let name = operation[0].as_str().expect("操作名");
            match name {
                "register" => {
                    let index = operation[1].as_u64().expect("索引") as usize;
                    actual.push(Value::Bool(batch.register(index)));
                }
                "cancel" => {
                    let reason = operation[1].as_str().expect("原因");
                    actual.push(Value::Bool(batch.begin_cancel(reason)));
                }
                "finish" => {
                    let index = operation[1].as_u64().expect("索引") as usize;
                    actual.push(Value::Bool(batch.mark_finished(index)));
                }
                "untracked" => {
                    let indexes: Vec<u64> = batch
                        .untracked_indexes()
                        .into_iter()
                        .map(|index| index as u64)
                        .collect();
                    actual.push(json!(indexes));
                }
                other => panic!("未知操作：{other}"),
            }
        }
        assert_eq!(Value::Array(actual), case["results"], "{label} 返回值");
        assert_eq!(
            batch.raw_cancel_reason(),
            case["cancel_reason"].as_str().unwrap(),
            "{label} 取消原因"
        );
        assert_eq!(Value::Bool(batch.is_done()), case["done"], "{label} 完成态");
        let finished: Vec<u64> = batch
            .untracked_indexes()
            .into_iter()
            .map(|index| index as u64)
            .collect();
        assert!(
            finished.iter().all(|index| (*index as usize) < task_count),
            "{label} 未登记索引越界"
        );
    }
}

#[test]
fn batch_helpers_match_python() {
    let data = section("batch");
    for case in data["shared_writer"].as_array().expect("shared_writer") {
        let label = case["label"].as_str().expect("label");
        let definition = AgentDefinition {
            permission_mode: case["permission_mode"].as_str().unwrap().to_string(),
            isolation: case["definition_isolation"].as_str().unwrap().to_string(),
            ..AgentDefinition::default()
        };
        let isolation = case["isolation"].as_str().unwrap();
        let tools = strings(&case["tool_names"]);
        assert_eq!(
            requires_shared_writer_lock(&definition, isolation, &tools),
            case["expected"].as_bool().unwrap(),
            "{label}"
        );
    }
    for case in data["cancellation"].as_array().expect("cancellation") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            is_cancellation_error(case["exception_type"].as_str().expect("类型")),
            case["expected"].as_bool().unwrap(),
            "{label}"
        );
    }
    for case in data["statuses"].as_array().expect("statuses") {
        let label = case["label"].as_str().expect("label");
        let results = case["results"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            batch_status(&results),
            case["status"].as_str().unwrap(),
            "{label} 状态"
        );
        let (ok, summary) = batch_summary("batch-1", results);
        assert_eq!(ok, case["ok"].as_bool().unwrap(), "{label} ok");
        assert_eq!(
            json_result_text(&summary),
            case["text"].as_str().unwrap(),
            "{label} 文本"
        );
    }
}
