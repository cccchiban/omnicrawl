//! 工作区轮次快照的跨语言 parity：数据集是冻结的对照契约。
//!
//! 数据集是脚本化场景（建仓库、写文件、捕获快照、回退、读文件），Rust 侧执行同一份脚本，
//! 比对每步结果：快照用补丁 sha256 + 未跟踪清单表示，回退记返回值或错误文案。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use omnicrawl_session::{
    normalize_project_path, split_lines_python, WorktreeSnapshot, WorktreeSnapshotStore,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/turn_snapshot_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn work_root() -> PathBuf {
    let raw =
        std::env::temp_dir().join(format!("omnicrawl-snapshot-parity-{}", std::process::id()));
    fs::create_dir_all(&raw).expect("建工作根");
    PathBuf::from(normalize_project_path(&raw.to_string_lossy()).expect("解析工作根"))
}

fn git(args: &[String], cwd: &Path) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("启动 git")
}

fn run_git(args: &[String], cwd: &Path) {
    let output = git(args, cwd);
    assert!(
        output.status.success(),
        "git {args:?} 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn apply_setup(root: &Path, step: &Value) {
    match step["op"].as_str().expect("setup op") {
        "git" => {
            let args: Vec<String> = step["args"]
                .as_array()
                .expect("args")
                .iter()
                .map(|item| item.as_str().expect("arg").to_string())
                .collect();
            run_git(&args, root);
        }
        "write" => {
            let path = root.join(step["path"].as_str().expect("path"));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("建父目录");
            }
            fs::write(&path, step["content"].as_str().expect("content")).expect("写文件");
        }
        "delete" => {
            fs::remove_file(root.join(step["path"].as_str().expect("path"))).expect("删除文件");
        }
        other => panic!("未知的 setup op：{other}"),
    }
}

/// Python `Path.read_text()` 走通用换行：`\r\n` 与 `\r` 都归成 `\n`。
fn python_read_text(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    Some(text.replace("\r\n", "\n").replace('\r', "\n"))
}

fn current_untracked(root: &Path) -> Vec<String> {
    let output = git(
        &[
            "ls-files".to_string(),
            "--others".to_string(),
            "--exclude-standard".to_string(),
        ],
        root,
    );
    split_lines_python(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

#[test]
fn snapshot_scenarios_match_python() {
    let fixture = fixture();
    let root_root = work_root();
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "数据集为空");

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let root = root_root.join(label);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("建用例目录");

        let store = WorktreeSnapshotStore::new();
        let mut snapshots: HashMap<String, WorktreeSnapshot> = HashMap::new();
        let mut results: Vec<Value> = Vec::new();

        for step in case["setup"].as_array().unwrap_or(&Vec::new()) {
            apply_setup(&root, step);
        }

        for step in case["steps"].as_array().expect("steps") {
            let kind = step["op"].as_str().expect("op");
            match kind {
                "git" | "write" | "delete" => {
                    apply_setup(&root, step);
                    results.push(json!({"op": kind, "ok": true, "value": Value::Null}));
                }
                "capture" => match store.capture(&root) {
                    Ok(snapshot) => {
                        results.push(json!({
                            "op": kind,
                            "ok": true,
                            "has_head": snapshot.has_head,
                            "patch_sha256": sha256_hex(&snapshot.patch),
                            "patch_len": snapshot.patch.len(),
                            "untracked": snapshot.untracked,
                        }));
                        snapshots
                            .insert(step["save"].as_str().expect("save").to_string(), snapshot);
                    }
                    Err(error) => results.push(json!({
                        "op": kind, "ok": false, "error": error.to_string(),
                    })),
                },
                "transition" => {
                    let expected = snapshots
                        .get(step["expected"].as_str().expect("expected"))
                        .expect("快照已捕获");
                    let target = snapshots
                        .get(step["target"].as_str().expect("target"))
                        .expect("快照已捕获");
                    match store.transition(&root, expected, target) {
                        Ok(missing) => results.push(json!({
                            "op": kind, "ok": true, "value": missing,
                        })),
                        Err(error) => results.push(json!({
                            "op": kind,
                            "ok": false,
                            "conflict": error.is_conflict(),
                            "error": error.to_string(),
                        })),
                    }
                }
                "read" => {
                    let path = root.join(step["path"].as_str().expect("path"));
                    results.push(json!({
                        "op": kind,
                        "path": step["path"],
                        "content": python_read_text(&path),
                    }));
                }
                "untracked" => {
                    results.push(json!({"op": kind, "value": current_untracked(&root)}));
                }
                other => panic!("未知的 op：{other}"),
            }
        }

        let produced = json!({
            "label": case["label"],
            "setup": case["setup"],
            "steps": case["steps"],
            "results": results,
        });
        assert_eq!(produced, *case, "场景（{label}）");
    }
}
