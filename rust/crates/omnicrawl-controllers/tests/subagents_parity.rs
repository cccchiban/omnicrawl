//! `agent/controllers/subagents/` 判定层的跨语言对照。
//!
//! 期望值来自 Python 真实现：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `fixtures/controllers_parity.json` 的 `subagents` 段后，本套件重放登记键、会话投影、
//! 产物摘要、丢弃保护、失败描述与结果投影逐条比对。git 操作、线程池与 Session 落盘属于宿主。

use std::collections::HashMap;

use omnicrawl_controllers::subagents::{orchestration, worktrees};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn section() -> Value {
    fixture()["subagents"].clone()
}

fn text(value: &Value) -> String {
    value.as_str().expect("字符串").to_string()
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().expect("数组").iter().map(text).collect()
}

fn session_of(value: &Value) -> worktrees::WorktreeSession {
    worktrees::WorktreeSession {
        task_id: text(&value["task_id"]),
        branch_name: text(&value["branch_name"]),
        worktree_path: text(&value["worktree_path"]),
        base_ref: text(&value["base_ref"]),
        repo_root: text(&value["repo_root"]),
    }
}

fn artifacts_of(value: &Value) -> worktrees::WorktreeArtifacts {
    worktrees::WorktreeArtifacts {
        branch_name: text(&value["branch_name"]),
        worktree_path: text(&value["worktree_path"]),
        base_ref: text(&value["base_ref"]),
        has_changes: value["has_changes"].as_bool().expect("has_changes"),
        changed_files: strings(&value["changed_files"]),
        diff_stat: text(&value["diff_stat"]),
        diff_text: text(&value["diff_text"]),
    }
}

fn sample_session() -> worktrees::WorktreeSession {
    worktrees::WorktreeSession {
        task_id: "task-1".to_string(),
        branch_name: "feat/one".to_string(),
        worktree_path: "C:/wt/one".to_string(),
        base_ref: "main".to_string(),
        repo_root: "C:/repo".to_string(),
    }
}

#[test]
fn worktree_lookup_matches_python() {
    let data = section();
    let session = sample_session();
    let mut table: HashMap<String, String> = HashMap::new();
    for key in worktrees::registration_keys(&session) {
        table.insert(key, session.branch_name.clone());
    }
    for case in data["worktree_lookup"].as_array().expect("worktree_lookup") {
        let key = case["key"].as_str().unwrap_or_default().to_string();
        let expected = case["branch"].as_str();
        let found = table.get(&worktrees::lookup_key(Some(&key)));
        assert_eq!(
            found.map(String::as_str),
            expected,
            "{}",
            case["label"].as_str().unwrap_or_default()
        );
    }
}

#[test]
fn worktree_registration_matches_python() {
    let data = section();
    let case = &data["worktree_registration"][0];
    assert_eq!(
        worktrees::registration_keys(&sample_session()).to_vec(),
        strings(&case["keys"])
    );
}

#[test]
fn worktree_items_match_python() {
    let data = section();
    for case in data["worktree_items"].as_array().expect("worktree_items") {
        let sessions: Vec<worktrees::WorktreeSession> = case["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .map(session_of)
            .collect();
        assert_eq!(
            Value::Array(worktrees::list_items(&sessions)),
            case["value"],
            "{}",
            case["label"]
        );
    }
}

#[test]
fn worktree_artifact_lines_match_python() {
    let data = section();
    for case in data["worktree_artifact_lines"]
        .as_array()
        .expect("worktree_artifact_lines")
    {
        let artifacts = if case["artifacts"].is_null() {
            None
        } else {
            Some(artifacts_of(&case["artifacts"]))
        };
        assert_eq!(
            worktrees::artifact_lines(artifacts.as_ref()),
            strings(&case["value"]),
            "{}",
            case["label"]
        );
    }
    for case in data["worktree_artifact_error"]
        .as_array()
        .expect("worktree_artifact_error")
    {
        assert_eq!(
            vec![worktrees::artifact_failure_line("boom")],
            strings(&case["value"]),
            "{}",
            case["label"]
        );
    }
}

#[test]
fn worktree_discard_guard_matches_python() {
    let data = section();
    for case in data["worktree_discard"]
        .as_array()
        .expect("worktree_discard")
    {
        let changes = worktrees::WorktreeChanges {
            uncommitted: case["uncommitted"].as_u64().expect("uncommitted") as usize,
            new_commits: case["new_commits"].as_u64().expect("new_commits") as usize,
        };
        let guard = worktrees::discard_guard(changes);
        match case["error"].as_str() {
            Some(error) => assert_eq!(guard.as_deref(), Some(error), "{}", case["label"]),
            None => {
                assert_eq!(guard, None, "{}", case["label"]);
                assert_eq!(
                    case["cleanup_called"].as_bool(),
                    Some(true),
                    "{}",
                    case["label"]
                );
            }
        }
    }
}

#[test]
fn worktree_error_texts_match_python() {
    let data = section();
    let guard = &data["worktree_discard_guard_error"][0];
    assert_eq!(
        worktrees::change_check_failure_error("git failed").message(),
        guard["error"].as_str().expect("error")
    );
    let missing = &data["worktree_discard_missing"][0];
    assert_eq!(
        worktrees::missing_session_error("missing").message(),
        missing["error"].as_str().expect("error")
    );
    for case in data["worktree_apply"].as_array().expect("worktree_apply") {
        let expected = match case["error"].as_str() {
            None => continue,
            Some(error) => error,
        };
        let actual = match case["label"].as_str().unwrap_or_default() {
            "应用失败" => worktrees::apply_failure_error("nope"),
            _ => worktrees::missing_session_error("missing"),
        };
        assert_eq!(actual.message(), expected, "{}", case["label"]);
    }
}

#[test]
fn run_failure_matches_python() {
    let data = section();
    for case in data["run_failure"].as_array().expect("run_failure") {
        let task = if case["task"].is_null() {
            None
        } else {
            Some(&case["task"])
        };
        assert_eq!(
            orchestration::describe_run_failure(&case["payload"], task),
            text(&case["value"]),
            "{}",
            case["label"]
        );
    }
}

#[test]
fn fork_task_message_matches_python() {
    let data = section();
    for case in data["fork_task_message"]
        .as_array()
        .expect("fork_task_message")
    {
        let description = text(&case["description"]);
        let prompt = text(&case["prompt"]);
        assert_eq!(
            orchestration::fork_task_message(&description, &prompt),
            case["value"],
            "{}",
            case["label"]
        );
    }
}

#[test]
fn fork_snapshot_matches_python() {
    let data = section();
    for case in data["fork_snapshot"].as_array().expect("fork_snapshot") {
        assert_eq!(
            orchestration::fork_context_snapshot(&case["messages"]).expect("应成功"),
            case["value"]
                .as_array()
                .expect("value")
                .clone()
                .into_iter()
                .collect::<Vec<Value>>(),
            "{}",
            case["label"]
        );
    }
}

#[test]
fn local_public_result_matches_python() {
    let data = section();
    for case in data["public_result"].as_array().expect("public_result") {
        let result = orchestration::local_public_result(
            case["text"].as_str().expect("text"),
            case["summary_chars"].as_u64().expect("summary_chars") as usize,
        );
        assert_eq!(result.summary, text(&case["summary"]), "{}", case["label"]);
        assert_eq!(
            Value::Array(result.artifacts),
            case["artifacts"],
            "{}",
            case["label"]
        );
    }
}

#[test]
fn inject_notifications_matches_python() {
    let data = section();
    for case in data["notifications"].as_array().expect("inject") {
        let mut messages: Vec<Value> = case["messages"].as_array().expect("messages").clone();
        let notifications: Vec<Value> = case["notifications"]
            .as_array()
            .expect("notifications")
            .clone();
        let injected = orchestration::inject_notifications(&mut messages, &notifications);
        assert_eq!(
            injected,
            case["changed"].as_bool().expect("changed"),
            "{}",
            case["label"]
        );
        assert_eq!(Value::Array(messages), case["value"], "{}", case["label"]);
    }
}

#[test]
fn wants_review_matches_python() {
    let data = section();
    for case in data["wants_review"].as_array().expect("wants_review") {
        let expected = case["keep_full_text"].as_bool().unwrap_or(false);
        assert_eq!(
            orchestration::wants_review(&case["arguments"]),
            expected,
            "{}",
            case["label"]
        );
    }
}

#[test]
fn require_completed_result_matches_python() {
    let data = section();
    for case in data["run_task"].as_array().expect("run_task") {
        let expected_error = case["error"].as_str();
        if !case["has_definition"].as_bool().expect("has_definition") {
            let available = strings(&case["available"]);
            assert_eq!(
                orchestration::missing_definition_error("review", &available).message(),
                expected_error.expect("error"),
                "{}",
                case["label"]
            );
            continue;
        }
        let ok = case["result_ok"].as_bool().expect("result_ok");
        let output = text(&case["output"]);
        let actual = orchestration::require_completed_result(ok, &output);
        match expected_error {
            Some(error) => assert_eq!(
                actual.expect_err("应失败").message(),
                error,
                "{}",
                case["label"]
            ),
            None => assert_eq!(
                actual.expect("应成功"),
                text(&case["value"]),
                "{}",
                case["label"]
            ),
        }
    }
}
