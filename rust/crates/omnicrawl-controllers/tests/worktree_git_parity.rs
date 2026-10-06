//! `agent/subagents/worktree.py` 判定面的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `worktree_git` 段——分支名直接调真函数，
//! 脏树门禁与变更统计用 `_run_git` 桩驱动真流程。本套件用同一批输入重放 Rust 实现。

use omnicrawl_controllers::subagents::worktrees::{
    dirty_main_tree_error, sanitize_branch_fragment, worktree_changes, WorktreeChanges,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["worktree_git"].clone()
}

#[test]
fn sanitize_branch_fragment_matches_python() {
    let data = section();
    for case in data["fragment"].as_array().expect("fragment") {
        let value = case["value"].as_str().expect("value");
        assert_eq!(
            sanitize_branch_fragment(value),
            case["expected"].as_str().expect("expected"),
            "分支名片段（{value:?}）"
        );
    }
}

#[test]
fn dirty_main_tree_error_matches_python() {
    let data = section();
    for case in data["dirty"].as_array().expect("dirty") {
        let label = case["label"].as_str().expect("label");
        let status = case["status"].as_str().expect("status");
        assert_eq!(
            dirty_main_tree_error(status).as_deref(),
            case["error"].as_str(),
            "脏树拒绝文案（{label}）"
        );
    }
}

#[test]
fn worktree_changes_matches_python() {
    let data = section();
    for case in data["changes"].as_array().expect("changes") {
        let label = case["label"].as_str().expect("label");
        let expected = WorktreeChanges {
            uncommitted: case["uncommitted"].as_u64().expect("uncommitted") as usize,
            new_commits: case["new_commits"].as_u64().expect("new_commits") as usize,
        };
        let produced = worktree_changes(
            case["directory_exists"]
                .as_bool()
                .expect("directory_exists"),
            case["status"].as_str().expect("status"),
            case["query_failed"].as_bool().expect("query_failed"),
            case["raw_count"].as_str().expect("raw_count"),
        );
        if case["directory_exists"]
            .as_bool()
            .expect("directory_exists")
        {
            assert_eq!(produced, Some(expected), "变更统计（{label}）");
        } else {
            assert_eq!(produced, None, "目录不存在时视为无变更（{label}）");
        }
        // 目录不存在时不该去问 git。
        let calls = case["git_calls"].as_u64().expect("git_calls");
        assert_eq!(
            calls == 0,
            !case["directory_exists"].as_bool().unwrap(),
            "{label}"
        );
    }
}
