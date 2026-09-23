//! 连接器自动启动的真进程测试：真拉起、真采集日志、真回收。
//!
//! 对照数据集（`connectors_autostart_parity.rs`）用的是假启动器，覆盖不了「子系统真的被
//! 拉起来、真的被连后代一起回收」这件事。这里把本测试可执行文件再拉起来当连接器子进程：
//!
//! 1. 配置探测按真实 `config.toml` 走，只拉起已配置的平台；
//! 2. 子进程的 stdout 落到用户 `logs/<平台>.log`（`capture_logs` 的生产行为），父测试从
//!    日志里读到子进程自报的 PID；
//! 3. `close()` 之后子进程必须消失（Windows 走 Job Object / `taskkill`，Unix 走进程组），
//!    并且平台单例锁可以重新拿到——不然下次启动会被自己的残留进程挡住。
//!
//! 子进程测试用「argv 与本测试约定的四条参数完全一致」来识别自己是不是被拉起的连接器，
//! 这样 `cargo test -- --ignored` 不会把它当成一个长驻 120 秒的测试。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_connectors::autostart::{
    connector_log_path, default_log_root, ConnectorManagerOptions, ConnectorProcessManager,
    ConnectorSpec, TELEGRAM_PLATFORM,
};
use omnicrawl_workspace::connector_singleton::ConnectorInstanceLock;
use omnicrawl_workspace::pid_is_running;

/// 被父测试拉起时传给子进程的参数（与 `autostart_connector_child` 里的判断一致）。
const CHILD_TEST_NAME: &str = "autostart_connector_child";
const CHILD_ARGS: [&str; 4] = ["--exact", "--ignored", "--nocapture", CHILD_TEST_NAME];
/// 子进程写进 stdout（也就是日志文件）的就绪标记。
const READY_MARKER: &str = "connector-child-ready pid=";
/// 子进程最多驻留多久（秒）：父测试会提前把它回收，这只是兜底。
const CHILD_HOLD_SECONDS: u64 = 120;

fn telegram_config() -> &'static str {
    "[telegram]\nbot_token = \"123:abc\"\nallowed_user_ids = [1]\n"
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "oc-autostart-proc-{tag}-{}-{}",
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

/// 从日志里找出子进程自报的 PID。
fn ready_pid(log_path: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(log_path).ok()?;
    let index = text.find(READY_MARKER)?;
    let rest = &text[index + READY_MARKER.len()..];
    let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

#[test]
#[ignore = "由 autostart_process.rs 的父测试以真子进程方式拉起"]
fn autostart_connector_child() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args != CHILD_ARGS {
        // 不是被父测试拉起的连接器子进程（例如 `cargo test -- --ignored`），不做任何事。
        return;
    }
    println!("{READY_MARKER}{}", std::process::id());
    let _ = std::io::stdout().flush();
    std::thread::sleep(Duration::from_secs(CHILD_HOLD_SECONDS));
}

#[test]
fn started_connector_is_reaped_and_lock_released() {
    let root = TempDir::new("reap");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("创建工作区");
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, telegram_config().as_bytes()).expect("写配置");

    let env = ConfigEnvironment::new(root.path(), std::env::consts::OS);
    // 监督器要拿走一份环境；收尾时还要用同一份环境推导平台单例锁的路径。
    let lock_env = env.clone();
    let log_path = connector_log_path(&default_log_root(&env), TELEGRAM_PLATFORM);
    let _ = std::fs::remove_file(&log_path);

    let program = std::env::current_exe().expect("取测试可执行文件");
    let options = ConnectorManagerOptions::new(env)
        .with_specs(vec![ConnectorSpec::new(
            TELEGRAM_PLATFORM,
            program,
            CHILD_ARGS.iter().map(|item| item.to_string()).collect(),
        )])
        .with_capture_logs(true)
        .with_config_path(Some(config_path))
        .with_pythonpath_root(None);
    let manager = ConnectorProcessManager::new(workspace.as_path(), options);

    assert_eq!(
        manager.start(),
        vec![TELEGRAM_PLATFORM.to_string()],
        "只配置了 Telegram，应当只拉起 Telegram"
    );
    assert!(manager.diagnostics().is_empty(), "正常启动不应有告警");

    // 子进程先报告就绪（写进日志文件），父测试据此拿到它的 PID。
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut pid = None;
    while Instant::now() < deadline {
        pid = ready_pid(&log_path);
        if pid.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = pid.unwrap_or_else(|| {
        panic!(
            "连接器子进程未在期限内就绪，日志：{:?}",
            std::fs::read_to_string(&log_path).unwrap_or_default()
        )
    });
    assert!(pid > 0, "子进程 PID 应当为正：{pid}");
    assert!(pid_is_running(pid), "子进程应当仍在运行");
    assert_ne!(pid, std::process::id() as i64, "子进程应当是另一个进程");

    manager.close();
    let deadline = Instant::now() + Duration::from_secs(15);
    while pid_is_running(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_is_running(pid), "close() 之后子进程应当已被回收");

    // 平台单例锁必须回到可用状态：否则下次启动会被「已有实例」挡住。
    let mut lock = ConnectorInstanceLock::new(&lock_env, TELEGRAM_PLATFORM);
    assert!(lock.try_acquire(), "收尾后应当能重新拿到平台单例");
    lock.release();
}
