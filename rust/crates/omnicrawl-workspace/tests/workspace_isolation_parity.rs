//! 与 Python `omnicrawl/workspace/` 真实现的对照测试。
//!
//! 数据集由 `python rust/tools/gen_workspace_isolation_fixture.py` 生成：同一批输入（含合成
//! 出来的 gitdir 目录树、隔离区元数据、清扫条目）喂给真实现，这里照 `tree` 描述重建目录后
//! 重放 Rust 实现并逐字段比对。改了任一侧都要重跑生成脚本。
//!
//! 数据集里的路径用 `<ROOT>` 占位，两侧各自替换成自己的临时根目录，因此临时目录的名字
//! 不影响比对。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use omnicrawl_workspace::agent_isolation::{
    cleanup_eligible, isolation_metadata_path, read_isolation_metadata, resolve_worktree_head,
    session_from_sweep_entry, IsolationSession,
};
use omnicrawl_workspace::slug::{is_safe_slug, validate_slug};
use omnicrawl_workspace::{count_changed, patch_files, resolve_path};
use serde_json::Value;

const ROOT_TOKEN: &str = "<ROOT>";
const MTIME_TOKEN: &str = "<MTIME>";

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("workspace_isolation_parity.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 {} 失败：{error}", path.display()));
    serde_json::from_str(&text).expect("数据集必须是合法 JSON")
}

fn group<'a>(data: &'a Value, name: &str) -> &'a Vec<Value> {
    data.get(name)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("数据集缺少分组：{name}"))
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("缺少字符串字段 {key}：{value}"))
        .to_string()
}

fn float_of(value: &Value, key: &str) -> f64 {
    value
        .get(key)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("缺少数值字段 {key}：{value}"))
}

/// 临时目录：测试结束即删除，避免污染系统临时目录。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "oc-workspace-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("创建临时目录");
        Self {
            path: resolve_path(&base),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 与生成脚本的 `norm()` 同一口径：把路径折成 `<ROOT>/...` 或 `<OUTSIDE>`。
fn norm(path: &Path, root: &Path) -> String {
    let resolved = resolve_path(path);
    let root_resolved = resolve_path(root);
    match resolved.strip_prefix(&root_resolved) {
        Ok(relative) => {
            let text = slashes(relative);
            if text.is_empty() {
                ROOT_TOKEN.to_string()
            } else {
                format!("{ROOT_TOKEN}/{text}")
            }
        }
        Err(_) => "<OUTSIDE>".to_string(),
    }
}

/// 按数据集里的 `tree` 描述建出目录与文件（写字节，保证两侧换行一致）。
fn materialize(root: &Path, tree: &Value) {
    let Some(entries) = tree.as_object() else {
        return;
    };
    let root_text = slashes(root);
    for (relative, content) in entries {
        let content = content.as_str().unwrap_or_default();
        let text = content.replace(ROOT_TOKEN, &root_text);
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("创建目录");
        }
        std::fs::write(&target, text.as_bytes()).expect("写入文件");
    }
}

fn expand(root: &Path, text: &str) -> PathBuf {
    PathBuf::from(text.replace(ROOT_TOKEN, &slashes(root)))
}

fn seconds_since_epoch(time: std::time::SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[test]
fn slug_cases_match_python() {
    let data = fixture();
    for case in group(&data, "slug") {
        let name = text_of(case, "name");
        let max_length = case.get("max_length").and_then(Value::as_u64).unwrap_or(64) as usize;
        assert_eq!(
            is_safe_slug(&name, max_length),
            case.get("safe").and_then(Value::as_bool).unwrap_or(false),
            "is_safe_slug 不一致：{case}"
        );
        let actual = validate_slug(&name, "隔离区实例 ID", max_length);
        match case.get("error").and_then(Value::as_str) {
            Some(expected) => {
                let error = actual.expect_err("期望校验失败");
                assert_eq!(error.message(), expected, "错误文案不一致：{case}");
            }
            None => {
                let expected = text_of(case, "validated");
                assert_eq!(
                    actual.expect("期望校验通过"),
                    expected,
                    "校验结果不一致：{case}"
                );
            }
        }
    }
}

#[test]
fn diff_stats_match_python() {
    let data = fixture();
    for case in group(&data, "diff_stats") {
        let diff = text_of(case, "diff");
        assert_eq!(
            count_changed(&diff),
            case.get("changed").and_then(Value::as_u64).unwrap_or(0) as usize,
            "变更文件数不一致：{case}"
        );
        let expected: Vec<String> = case
            .get("files")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(patch_files(&diff), expected, "patch 文件列表不一致：{case}");
    }
}

#[test]
fn worktree_head_matches_python() {
    let data = fixture();
    let work = TempDir::new("head");
    for case in group(&data, "worktree_head") {
        materialize(work.path(), case.get("tree").unwrap_or(&Value::Null));
        let gitdir = expand(work.path(), &text_of(case, "gitdir"));
        let expected = case
            .get("expect")
            .and_then(Value::as_str)
            .map(str::to_string);
        assert_eq!(
            resolve_worktree_head(&gitdir),
            expected,
            "HEAD 解析结果不一致：{case}"
        );
    }
}

#[test]
fn sweep_entries_match_python() {
    let data = fixture();
    let work = TempDir::new("sweep");
    for case in group(&data, "sweep_entries") {
        materialize(work.path(), case.get("tree").unwrap_or(&Value::Null));
        let name = text_of(case, "name");
        let instance_id = text_of(case, "instance_id");
        let entry = work.path().join(&name);
        let session = session_from_sweep_entry(work.path(), &entry, &instance_id);
        let expected = case.get("expect").cloned().unwrap_or(Value::Null);
        let Some(session) = session else {
            assert!(expected.is_null(), "期望重建出会话但返回 None：{case}");
            continue;
        };
        if !expected.is_object() {
            panic!("期望应为对象：{case}");
        }
        assert_eq!(session.instance_id, text_of(&expected, "instance_id"));
        assert_eq!(session.mode, text_of(&expected, "mode"));
        assert_eq!(session.base_ref, text_of(&expected, "base_ref"));
        assert_eq!(session.branch_name, text_of(&expected, "branch_name"));
        assert_eq!(
            norm(&session.worktree_path, work.path()),
            text_of(&expected, "worktree_path"),
            "worktree_path 不一致：{case}"
        );
        assert_eq!(
            norm(&session.repo_root, work.path()),
            text_of(&expected, "repo_root"),
            "repo_root 不一致：{case}"
        );
        assert_eq!(
            norm(&session.main_workspace, work.path()),
            text_of(&expected, "main_workspace"),
            "main_workspace 不一致：{case}"
        );
        match expected.get("created_at").and_then(Value::as_str) {
            // 遗留目录的创建时间取目录 mtime（两侧都非确定性，只对照来源）。
            Some(MTIME_TOKEN) => {
                let mtime = std::fs::metadata(&entry)
                    .and_then(|meta| meta.modified())
                    .map(seconds_since_epoch)
                    .unwrap_or_default();
                assert!(
                    (session.created_at - mtime).abs() < 1.0,
                    "遗留目录创建时间应取 mtime：{case}"
                );
            }
            _ => {
                let expected_time = float_of(&expected, "created_at");
                assert_eq!(
                    session.created_at, expected_time,
                    "created_at 不一致：{case}"
                );
            }
        }
    }
}

#[test]
fn metadata_read_matches_python() {
    let data = fixture();
    let work = TempDir::new("meta");
    for case in group(&data, "metadata_read") {
        materialize(work.path(), case.get("tree").unwrap_or(&Value::Null));
        let entry = text_of(case, "entry");
        let expected = case.get("expect").cloned().unwrap_or(Value::Null);
        let actual = read_isolation_metadata(work.path(), &entry);
        match (actual, expected) {
            (None, Value::Null) => {}
            (Some(map), Value::Object(expected)) => {
                assert_eq!(
                    Value::Object(map),
                    Value::Object(expected),
                    "元数据不一致：{case}"
                );
            }
            (actual, expected) => panic!("元数据读取结果不一致：{actual:?} != {expected}"),
        }
    }
}

#[test]
fn metadata_path_matches_python() {
    let data = fixture();
    let work = TempDir::new("metapath");
    for case in group(&data, "metadata_path") {
        let entry = text_of(case, "entry");
        let actual = isolation_metadata_path(work.path(), &entry);
        assert_eq!(
            norm(&actual, work.path()),
            text_of(case, "expect"),
            "元数据路径不一致：{case}"
        );
    }
}

#[test]
fn cleanup_gates_match_python() {
    let data = fixture();
    let work = TempDir::new("gates");
    for case in group(&data, "cleanup_gates") {
        let raw = case.get("session").expect("缺少 session");
        let session = IsolationSession {
            instance_id: text_of(raw, "instance_id"),
            mode: text_of(raw, "mode"),
            repo_root: expand(work.path(), &text_of(raw, "repo_root")),
            worktree_path: expand(work.path(), &text_of(raw, "worktree_path")),
            base_ref: text_of(raw, "base_ref"),
            main_workspace: expand(work.path(), &text_of(raw, "main_workspace")),
            created_at: float_of(raw, "created_at"),
            branch_name: text_of(raw, "branch_name"),
        };
        let in_use: HashSet<String> = case
            .get("in_use")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let now = case.get("now").and_then(Value::as_f64);
        let (eligible, reason) = cleanup_eligible(&session, Some(&in_use), now, "origin");
        let expected = case
            .get("expect")
            .and_then(Value::as_array)
            .expect("expect 必须是数组");
        assert_eq!(
            eligible,
            expected.first().and_then(Value::as_bool).unwrap_or(false),
            "门禁结论不一致：{case}"
        );
        assert_eq!(
            reason,
            expected.get(1).and_then(Value::as_str).unwrap_or_default(),
            "门禁原因不一致：{case}"
        );
    }
}

/// 数据集本身的自检：路径一律用 `<ROOT>` 占位，不能混进某一侧的临时绝对路径。
#[test]
fn fixture_paths_are_placeholders() {
    let data = fixture();
    let serialized = serde_json::to_string(&data).expect("序列化数据集");
    assert!(
        serialized.contains(ROOT_TOKEN),
        "数据集应使用 {ROOT_TOKEN} 占位"
    );
    // 生成时根目录是 `%TEMP%` 下的真实路径；只有占位方案能保证两侧可比。
    assert!(
        !serialized.contains("AppData") && !serialized.contains("/tmp/"),
        "数据集里不应该出现生成机的绝对路径"
    );
}
