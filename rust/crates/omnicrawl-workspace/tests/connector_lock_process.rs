//! 连接器单例锁的真文件锁与真进程测试。
//!
//! 对照数据集（`connector_singleton_parity.rs`）只能覆盖「同一进程内重放纯函数」的那部分；
//! 这里补上真正要动 OS 锁与真进程的两件事：
//!
//! 1. 同一进程内的两个句柄互斥（`try_lock_file` 刻意做成按打开实例排斥，因此在 Windows 的
//!    字节区间锁与 POSIX 的 `flock` 上都成立）；
//! 2. 另一个进程持锁时拿不到、持有者退出（包括被强杀）后能接管——后者正是「崩溃残留不会
//!    把平台永久锁死」的依据。
//!
//! 第 2 条用「把本测试可执行文件再拉起来当持有者」的方式实现，不依赖平台特有的长驻命令。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use omnicrawl_workspace::connector_singleton::{
    ConnectorInstanceLock, LOCK_FILENAME_PREFIX, LOCK_FILENAME_SUFFIX,
};
use omnicrawl_workspace::pid_is_running;

/// 子进程要拿的锁文件路径（父进程注入）。
const CHILD_LOCK_ENV: &str = "OMNICRAWL_TEST_CONNECTOR_LOCK_FILE";
/// 子进程拿到锁之后写的就绪标记（父进程据此判断「已经持锁」）。
const CHILD_READY_ENV: &str = "OMNICRAWL_TEST_CONNECTOR_READY_FILE";
/// 被父进程拉起的那个测试名。
const CHILD_TEST_NAME: &str = "connector_lock_holder_child";
/// 子进程持锁上限（秒）：父进程提前把它杀掉，这只是兜底，免得孤儿进程永久占着锁。
const CHILD_HOLD_SECONDS: u64 = 30;

fn platform_lock_path(directory: &Path) -> PathBuf {
    directory.join(format!(
        "{LOCK_FILENAME_PREFIX}Telegram{LOCK_FILENAME_SUFFIX}"
    ))
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "oc-lock-{tag}-{}-{}",
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
fn two_handles_in_one_process_exclude_each_other() {
    let root = TempDir::new("same-process");
    let path = platform_lock_path(root.path());

    let mut first = ConnectorInstanceLock::at("Telegram", path.clone());
    assert!(first.try_acquire(), "第一个实例应当拿到单例");
    assert!(first.is_owner());
    assert!(path.exists(), "持锁期间锁文件应当存在");

    let mut second = ConnectorInstanceLock::at("Telegram", path.clone());
    assert!(!second.try_acquire(), "第二个实例不应拿到同一平台的单例");
    assert!(!second.is_owner());

    // 释放之后锁文件应当被删掉，并且新的竞争方可以接管。
    first.release();
    assert!(!first.is_owner());
    assert!(!path.exists(), "释放后应当删除锁文件");

    assert!(second.try_acquire(), "释放后应当能接管");
    second.release();
    assert!(!path.exists(), "再次释放后同样不应留下锁文件");
}

#[test]
fn drop_releases_the_lock() {
    let root = TempDir::new("drop");
    let path = platform_lock_path(root.path());
    {
        let mut lock = ConnectorInstanceLock::at("Telegram", path.clone());
        assert!(lock.try_acquire());
    }
    // 显式 release 之外，Drop 也要把锁还回去（提前返回 / 异常路径都靠它）。
    let mut again = ConnectorInstanceLock::at("Telegram", path.clone());
    assert!(again.try_acquire(), "Drop 之后应当能重新拿到单例");
    again.release();
}

#[test]
#[ignore = "由 another_process_holding_the_lock_blocks_us 拉起，直接跑会被环境变量守卫挡住"]
fn connector_lock_holder_child() {
    let Some(lock_file) = std::env::var_os(CHILD_LOCK_ENV) else {
        // 单独执行（未带环境变量）时什么都不做。
        return;
    };
    let ready = std::env::var_os(CHILD_READY_ENV).expect("子进程缺少就绪标记路径");
    let mut lock = ConnectorInstanceLock::at("Telegram", PathBuf::from(lock_file));
    assert!(lock.try_acquire(), "子进程应当拿到平台单例");
    std::fs::write(&ready, std::process::id().to_string()).expect("写就绪标记");
    std::thread::sleep(Duration::from_secs(CHILD_HOLD_SECONDS));
    lock.release();
}

#[test]
fn another_process_holding_the_lock_blocks_us() {
    let root = TempDir::new("cross-process");
    let path = platform_lock_path(root.path());
    let ready = root.join("ready.txt");
    let executable = std::env::current_exe().expect("取测试可执行文件");

    let mut child = Command::new(executable)
        .args(["--exact", CHILD_TEST_NAME, "--ignored", "--nocapture"])
        .env(CHILD_LOCK_ENV, &path)
        .env(CHILD_READY_ENV, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起持有者子进程");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() && Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("持有者子进程提前退出：{status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready.exists(), "持有者子进程未在期限内就绪");
    assert!(
        pid_is_running(child.id() as i64),
        "持有者子进程应当仍在运行"
    );
    assert_eq!(
        std::fs::read_to_string(&ready).expect("读就绪标记").trim(),
        child.id().to_string(),
        "就绪标记里的 PID 应当就是持有者"
    );

    // 另一个进程持锁：读不到 / 读到活着的 PID 两条路径都应当得到「拿不到」。
    let mut lock = ConnectorInstanceLock::at("Telegram", path.clone());
    assert!(!lock.try_acquire(), "持有者还活着时不应拿到单例");
    assert!(!lock.is_owner());

    // 强杀持有者：OS 随进程释放文件锁，本进程应当能接管（崩溃残留不会永久锁死平台）。
    child.kill().expect("终止持有者子进程");
    child.wait().expect("等待持有者退出");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut acquired = false;
    while Instant::now() < deadline {
        if lock.try_acquire() {
            acquired = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(acquired, "持有者退出后应当能接管平台单例");

    lock.release();
    assert!(!path.exists(), "释放后应当删除锁文件");
}
