//! 跨进程写锁的行为测试。
//!
//! 三条不变量：另一个进程持锁时本进程取锁会超时（文案与 Python 一致）、锁文件里记的是持有者 pid、
//! 以及锁释放后可以立即再次取得。另有一个 opt-in 用例验证与 Python 实现的**互操作**：
//! 只有设置了 `OMNICRAWL_PYTHON`（解释器路径）时才真正跑，默认跳过。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use omnicrawl_session::{
    parse_datetime, process_lock_for_root, DurableWritePolicy, SessionStore, LOCK_FILE_NAME,
};

const SHORT_TIMEOUT: f64 = 0.3;

fn temp_root(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let root = std::env::temp_dir().join(format!("omnicrawl-lock-{name}-{stamp}"));
    std::fs::create_dir_all(&root).expect("创建临时根目录");
    root
}

fn quick_policy() -> DurableWritePolicy {
    DurableWritePolicy {
        fsync: false,
        lock_timeout_seconds: SHORT_TIMEOUT,
        lock_poll_seconds: 0.02,
    }
}

/// 子进程入口：真正的测试跑到时它只会在没有环境变量时直接返回。
#[test]
fn holder_process_helper() {
    let Ok(root) = std::env::var("OMNICRAWL_LOCK_ROOT") else {
        return;
    };
    let hold_ms: u64 = std::env::var("OMNICRAWL_LOCK_HOLD_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1_000);

    let lock = process_lock_for_root(Path::new(&root));
    let _guard = lock.acquire(5.0, 0.02).expect("子进程取锁失败");
    println!("locked");
    std::io::stdout().flush().ok();
    std::thread::sleep(Duration::from_millis(hold_ms));
}

#[test]
fn another_process_holding_the_lock_blocks_us() {
    let root = temp_root("cross-process");
    let mut child = Command::new(std::env::current_exe().expect("取当前测试可执行文件"))
        .args(["--exact", "holder_process_helper", "--nocapture"])
        .env("OMNICRAWL_LOCK_ROOT", &root)
        .env("OMNICRAWL_LOCK_HOLD_MS", "1500")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("拉起持锁子进程");

    let stdout = child.stdout.take().expect("子进程 stdout 可用");
    // reader 持有管道读端，必须活到子进程退出之后，否则子进程写输出会 BrokenPipe。
    let mut reader = BufReader::new(stdout);
    wait_for_lock_line(&mut reader);

    // 子进程持锁期间，本进程的写操作必须超时失败。
    let store = SessionStore::open_with_policy(&root, quick_policy());
    let started = Instant::now();
    let outcome = store.start_session("D:/work/demo", "被锁住", parse_time());
    let elapsed = started.elapsed();
    let error = outcome.expect_err("另一个进程持锁时不应成功");
    assert!(
        error
            .message()
            .starts_with("获取会话存储写锁超时（0.3s）："),
        "超时文案不符合预期：{}",
        error.message()
    );
    assert!(
        error.message().contains(LOCK_FILE_NAME),
        "超时文案应带上锁文件路径：{}",
        error.message()
    );
    assert!(
        elapsed >= Duration::from_secs_f64(SHORT_TIMEOUT),
        "不应该在超时之前返回：{elapsed:?}"
    );

    // 子进程到点自行退出；锁由 OS 释放，本进程应能立刻取到。
    let status = child.wait().expect("等待子进程结束");
    assert!(status.success(), "持锁子进程异常退出：{status:?}");
    let created = store
        .start_session("D:/work/demo", "解锁后", parse_time())
        .expect("子进程退出后应能取锁");
    assert_eq!(created.event_count, 1);
    let lock_text = std::fs::read_to_string(root.join(LOCK_FILE_NAME)).expect("锁文件存在");
    assert_eq!(lock_text, format!("pid={}\n", std::process::id()));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn lock_file_records_holder_and_is_reusable_after_release() {
    let root = temp_root("pid");
    let lock = process_lock_for_root(&root);
    {
        let _guard = lock.acquire(1.0, 0.02).expect("取锁");
    }
    // Windows 的字节区间锁是强制锁：持锁期间连读都会被拒，释放后再核对内容。
    let text = std::fs::read_to_string(root.join(LOCK_FILE_NAME)).expect("锁文件存在");
    assert_eq!(text.trim_end(), format!("pid={}", std::process::id()));
    // 释放后可以再次取得（OS 锁随句柄关闭释放）。
    let _again = lock.acquire(1.0, 0.02).expect("释放后应能再取锁");
    std::fs::remove_dir_all(&root).ok();
}

/// 与 Python 实现的互操作：Python 持锁时，内核取锁必须超时。
///
/// 需要 `OMNICRAWL_PYTHON` 指向解释器（本机是 `D:/ProgramData/Anaconda3/python.exe`）；
/// 未设置时跳过，避免在没装 Python 的环境里报假失败。
#[test]
fn python_holder_blocks_the_kernel() {
    let Some(python) = std::env::var("OMNICRAWL_PYTHON").ok() else {
        eprintln!("跳过：未设置 OMNICRAWL_PYTHON");
        return;
    };
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("仓库根目录")
        .to_path_buf();
    let root = temp_root("python-holder");

    let script = r#"
import sys, time
from pathlib import Path
from omnicrawl.state.session_locking import ProcessFileLock
lock = ProcessFileLock(Path(sys.argv[1]) / ".session_store.lock", timeout_seconds=5.0)
lock.acquire()
print("locked", flush=True)
time.sleep(1.5)
lock.release()
"#;
    let mut child = Command::new(&python)
        .args(["-c", script, root.to_string_lossy().as_ref()])
        .current_dir(&repository)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("拉起 Python 持锁进程");

    let stdout = child.stdout.take().expect("Python stdout 可用");
    let mut reader = BufReader::new(stdout);
    wait_for_lock_line(&mut reader);

    let store = SessionStore::open_with_policy(&root, quick_policy());
    let error = store
        .start_session("D:/work/demo", "被 Python 锁住", parse_time())
        .expect_err("Python 持锁时内核不应成功");
    assert!(
        error
            .message()
            .starts_with("获取会话存储写锁超时（0.3s）："),
        "超时文案不符合预期：{}",
        error.message()
    );

    let _ = child.wait();
    std::fs::remove_dir_all(&root).ok();
}

/// 等子进程打出 `locked`；测试框架会先输出自己的行，所以逐行找。
fn wait_for_lock_line(reader: &mut BufReader<std::process::ChildStdout>) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("子进程在拿到锁之前就结束了"),
            Ok(_) if line.trim() == "locked" => return,
            Ok(_) => continue,
            Err(error) => panic!("读取子进程输出失败：{error}"),
        }
    }
}

fn parse_time() -> chrono::DateTime<chrono::Utc> {
    parse_datetime("2026-09-18T03:05:29.123456+00:00").expect("固定时间可解析")
}
