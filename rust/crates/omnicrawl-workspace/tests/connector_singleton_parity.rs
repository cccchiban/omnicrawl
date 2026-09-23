//! 与 Python `omnicrawl/workspace/connector_singleton.py` 真实现的对照测试。
//!
//! 数据集由 `python rust/tools/gen_workspace_connector_singleton_fixture.py` 生成：同一批平台名
//! 与锁文件内容喂给真实现（`connector_lock_path` / `_locked_pid` / `ConnectorInstanceLock`），
//! 这里重放 Rust 实现并逐字段比对。改了任一侧都要重跑生成脚本。
//!
//! 「另一进程持锁 → 拿不到」与「持有者退出 → 可接管」需要真起进程，放在
//! `connector_lock_process.rs`；锁文件里 CRLF 的解析差异由生成器抓出来过（Python 的
//! `read_text` 走通用换行），Rust 侧按同一口径切行。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_workspace::connector_singleton::{
    connector_lock_path, locked_pid, pid_is_running, ConnectorInstanceLock, LOCK_FILENAME_PREFIX,
    LOCK_FILENAME_SUFFIX, STALE_RECLAIM_WAIT_SECONDS,
};
use serde_json::Value;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("connector_singleton_parity.json");
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

fn bool_of(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("缺少布尔字段 {key}：{value}"))
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
            "oc-singleton-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("创建临时目录");
        Self {
            path: omnicrawl_workspace::resolve_path(&base),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn constants_match_python() {
    let data = fixture();
    let constants = data.get("constants").expect("缺少 constants");
    assert_eq!(LOCK_FILENAME_PREFIX, text_of(constants, "prefix"));
    assert_eq!(LOCK_FILENAME_SUFFIX, text_of(constants, "suffix"));
    assert_eq!(
        STALE_RECLAIM_WAIT_SECONDS,
        constants
            .get("stale_reclaim_wait_seconds")
            .and_then(Value::as_f64)
            .expect("缺少 stale_reclaim_wait_seconds")
    );
}

#[test]
fn lock_filenames_match_python() {
    let data = fixture();
    let root = TempDir::new("names");
    let env = ConfigEnvironment::new(root.path(), std::env::consts::OS);

    for case in group(&data, "lock_filenames") {
        let name = text_of(case, "name");
        let expected = text_of(case, "filename");
        let path = connector_lock_path(&env, &name);
        assert_eq!(
            path.file_name()
                .map(|item| item.to_string_lossy().to_string()),
            Some(expected.clone()),
            "锁文件名不一致：{name:?}"
        );
        // 默认位置就是「用户配置目录 / 锁文件名」，与 Python 的 user_config_dir() 同源。
        assert_eq!(
            path,
            user_config_dir(&env).join(&expected),
            "锁文件目录不一致：{name:?}"
        );
    }
}

#[test]
fn pid_lines_match_python() {
    let data = fixture();
    let root = TempDir::new("pid");
    for (index, case) in group(&data, "pid_lines").iter().enumerate() {
        let path = root.join(&format!("lock-{index}.lock"));
        std::fs::write(&path, text_of(case, "content").as_bytes()).expect("写锁文件");
        let expected = case.get("pid").and_then(Value::as_i64);
        assert_eq!(
            locked_pid(&path),
            expected,
            "PID 解析不一致：{:?}",
            text_of(case, "content")
        );
    }
}

#[test]
fn takeover_matches_python() {
    let data = fixture();
    let case = data.get("takeover").expect("缺少 takeover");
    let root = TempDir::new("takeover");
    let path = root.join(&format!(
        "{LOCK_FILENAME_PREFIX}Telegram{LOCK_FILENAME_SUFFIX}"
    ));
    std::fs::write(&path, text_of(case, "content").as_bytes()).expect("写陈旧锁文件");

    let mut lock = ConnectorInstanceLock::at("Telegram", path.clone());
    assert_eq!(
        lock.try_acquire(),
        bool_of(case, "acquired"),
        "已有锁文件时应当能拿到单例"
    );
    assert_eq!(lock.is_owner(), bool_of(case, "owner_flag"));
    assert_eq!(path.exists(), bool_of(case, "lock_file_exists_while_held"));

    lock.release();
    assert_eq!(
        path.exists(),
        bool_of(case, "lock_file_exists_after_release"),
        "释放单例锁后应当删除锁文件"
    );
}

#[test]
fn pid_running_cases_match_python() {
    let data = fixture();
    for case in group(&data, "pid_running") {
        let pid = case
            .get("pid")
            .and_then(Value::as_i64)
            .expect("缺少 pid 字段");
        assert_eq!(
            pid_is_running(pid),
            bool_of(case, "running"),
            "PID 存活判定不一致：{pid}"
        );
    }
    // 真进程的存活 / 退出判定由 connector_lock_process.rs 覆盖（那里会起真子进程）。
}
