//! 常驻服务的进程生命周期：按记录复用、拉起、显式停止。
//!
//! 与决策接口、抓取接口同一套约定：
//!
//! * 服务是**本机共用**的常驻进程，按 PID 文件与服务日志认定——端口上有没有人在听不靠连接去猜：
//!   那种探测在服务未跑时会把调用方卡满超时，而它恰好位于启动路径上；
//! * 进程以「脱离宿主」的方式创建（`spawn_shared_service`），本实例退出（含崩溃）都不回收它；
//! * 停止只能显式做（[`stop`]），按 PID 文件回收整棵进程树。
//!
//! 复用判据是**服务是否已就绪**：cron 与传感器脚本随时可能来调用，它既不该自己拉一份服务，
//! 也不该因为宿主没起而失败。

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use omnicrawl_config::core::runtime::{ConfigEnvironment, user_config_dir};
use omnicrawl_host::process_control::{
    kill_process_tree_by_pid, pid_is_running, spawn_shared_service,
};

use crate::config::TriggerSettings;

/// 等待服务就绪的轮询间隔。
pub const READY_POLL_MILLIS: u64 = 200;
/// 等待服务就绪的默认上限（秒）。
pub const DEFAULT_READY_TIMEOUT_SECONDS: u64 = 20;

/// PID 文件名（放在用户配置目录下，与配置同源）。
pub const PID_FILENAME: &str = "trigger_api.pid";
/// 服务日志文件名。
pub const LOG_FILENAME: &str = "trigger_api.log";

/// 服务状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Ready,
    Failed(String),
}

impl ServiceState {
    pub fn label(&self) -> String {
        match self {
            Self::Stopped => "未运行".to_string(),
            Self::Starting => "正在启动".to_string(),
            Self::Ready => "已就绪".to_string(),
            Self::Failed(reason) => format!("启动失败：{reason}"),
        }
    }
}

/// 一次启停动作的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureOutcome {
    pub state: ServiceState,
    pub message: String,
    pub address: String,
}

impl EnsureOutcome {
    pub fn ready(&self) -> bool {
        self.state == ServiceState::Ready
    }
}

/// `serve` 打出这行才算真正接管端口（绑定监听之后才打印）。
const READY_LINE: &str = "[trigger] 触发接口监听";

/// 服务是否已就绪：PID 记录里的进程还活着，且日志里已出现监听标记。
///
/// 只读 PID 文件与日志，**不发起任何网络探测**。
pub fn service_ready(environment: &ConfigEnvironment) -> bool {
    if !pid_file_alive(environment) {
        return false;
    }
    log_has_ready_line(&log_path(environment))
}

fn pid_file_alive(environment: &ConfigEnvironment) -> bool {
    read_pid(environment).is_some_and(pid_is_running)
}

/// 服务日志里是否已经出现监听标记（逐行扫描：标记会被后续请求日志淹没）。
fn log_has_ready_line(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return false,
            Ok(_) => {
                if line
                    .windows(READY_LINE.len())
                    .any(|window| window == READY_LINE.as_bytes())
                {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
}

/// 保证本机有一份可用的触发接口：已就绪就复用，否则拉起常驻进程并等它就绪。
pub fn ensure_running(
    environment: &ConfigEnvironment,
    settings: &TriggerSettings,
    wait: bool,
) -> EnsureOutcome {
    let address = settings.address();
    if let Some(reason) = settings.unavailable_reason() {
        return EnsureOutcome {
            state: ServiceState::Failed(reason.clone()),
            message: reason,
            address,
        };
    }
    if service_ready(environment) {
        return EnsureOutcome {
            state: ServiceState::Ready,
            message: format!("触发接口已就绪 {address}，本实例直接复用。"),
            address,
        };
    }
    // 别的实例正在拉起：等它就绪，不再起第二个进程。
    if let Some(pid) = live_pid(environment) {
        if !wait {
            return EnsureOutcome {
                state: ServiceState::Starting,
                message: format!("触发接口正由其他实例启动（pid {pid}），就绪后直接复用。"),
                address,
            };
        }
        let state = wait_ready(environment, pid, DEFAULT_READY_TIMEOUT_SECONDS);
        return EnsureOutcome {
            message: match &state {
                ServiceState::Ready => format!("已复用其他实例启动的触发接口（{address}）。"),
                _ => format!("触发接口未就绪：{}", state.label()),
            },
            state,
            address,
        };
    }

    match spawn_service(environment) {
        Ok(pid) => {
            if !wait {
                return EnsureOutcome {
                    state: ServiceState::Starting,
                    message: format!("触发接口正在启动（{address}）。"),
                    address,
                };
            }
            let state = wait_ready(environment, pid, DEFAULT_READY_TIMEOUT_SECONDS);
            let message = match &state {
                ServiceState::Ready => format!("触发接口已就绪（{address}）。"),
                ServiceState::Failed(reason) => format!("触发接口启动失败：{reason}"),
                _ => format!("触发接口状态：{}。", state.label()),
            };
            EnsureOutcome {
                state,
                message,
                address,
            }
        }
        Err(error) => EnsureOutcome {
            state: ServiceState::Failed(error.clone()),
            message: format!("拉起触发接口失败：{error}"),
            address,
        },
    }
}

/// 停止本机共用的触发接口并清掉 PID 记录。
pub fn stop(environment: &ConfigEnvironment, settings: &TriggerSettings) -> EnsureOutcome {
    let address = settings.address();
    let Some(pid) = live_pid(environment) else {
        return EnsureOutcome {
            state: ServiceState::Stopped,
            message: "触发接口未运行。".to_string(),
            address,
        };
    };
    kill_process_tree_by_pid(pid);
    clear_pid(environment);
    EnsureOutcome {
        state: ServiceState::Stopped,
        message: format!("触发接口已停止（pid {pid}）。"),
        address,
    }
}

/// 拉起常驻服务进程；返回它的 PID。
///
/// 三路 stdio 都必须指向 null 或日志文件，**不能**是 `inherit`：Windows 上 Rust 的
/// `Command` 用 `bInheritHandles=TRUE` 创建进程，任何未显式重定向的槽位都会把调用方的
/// 管道复制给这个常驻进程。于是调用方（例如 cron 里的 `subprocess.run(capture_output=True)`）
/// 会一直等 stdout 的 EOF，而 serve 既不写也不关——脚本永久卡死，看着像 ensure 没返回。
fn spawn_service(environment: &ConfigEnvironment) -> Result<u32, String> {
    let program = resolve_service_program();
    let mut command = Command::new(&program);
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some((stdout, stderr)) = open_log(environment) {
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
    }
    // 服务是本机共用的常驻进程：脱离宿主创建，不纳入宿主的 kill-on-close Job。
    let child: Child = spawn_shared_service(&mut command)
        .map_err(|error| format!("拉起触发接口进程失败（{}）：{error}", program.display()))?;
    let pid = child.id();
    write_pid(environment, pid);
    Ok(pid)
}

/// 触发接口二进制的位置：`$OMNICRAWL_TRIGGER_BINARY` > 与当前可执行文件同目录 > PATH。
pub fn resolve_service_program() -> PathBuf {
    const ENV: &str = "OMNICRAWL_TRIGGER_BINARY";
    if let Ok(value) = std::env::var(ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let name = if cfg!(windows) {
        "omnicrawl-trigger.exe"
    } else {
        "omnicrawl-trigger"
    };
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            let sibling = directory.join(name);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from(name)
}

/// 打开服务日志（追加）。
fn open_log(environment: &ConfigEnvironment) -> Option<(std::fs::File, std::fs::File)> {
    let path = log_path(environment);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let clone = file.try_clone().ok()?;
    Some((file, clone))
}

/// 等服务就绪；进程提前退出即算失败。
fn wait_ready(environment: &ConfigEnvironment, pid: u32, timeout_seconds: u64) -> ServiceState {
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    while Instant::now() < deadline {
        if service_ready(environment) {
            return ServiceState::Ready;
        }
        if !pid_is_running(pid) {
            return ServiceState::Failed("服务进程已退出，请查看触发接口日志。".to_string());
        }
        std::thread::sleep(Duration::from_millis(READY_POLL_MILLIS));
    }
    ServiceState::Failed(format!("等待 {timeout_seconds} 秒仍未就绪。"))
}

pub fn pid_path(environment: &ConfigEnvironment) -> PathBuf {
    user_config_dir(environment).join(PID_FILENAME)
}

pub fn log_path(environment: &ConfigEnvironment) -> PathBuf {
    user_config_dir(environment).join(LOG_FILENAME)
}

/// 写入服务 PID 记录（`ensure` 与 `serve` 两侧都写：谁先就绪谁负责登记）。
pub fn write_pid(environment: &ConfigEnvironment, pid: u32) {
    let path = pid_path(environment);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{pid}\n"));
}

fn read_pid(environment: &ConfigEnvironment) -> Option<u32> {
    let text = std::fs::read_to_string(pid_path(environment)).ok()?;
    text.lines()
        .next()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

fn clear_pid(environment: &ConfigEnvironment) {
    let _ = std::fs::remove_file(pid_path(environment));
}

/// 现存活的服务 PID：记录里的进程还在才算数（已退出时顺手清掉记录）。
fn live_pid(environment: &ConfigEnvironment) -> Option<u32> {
    let pid = read_pid(environment)?;
    if pid_is_running(pid) {
        Some(pid)
    } else {
        clear_pid(environment);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(tag: &str) -> ConfigEnvironment {
        let root =
            std::env::temp_dir().join(format!("oc-trigger-server-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(omnicrawl_config::core::runtime::USER_CONFIG_DIRNAME))
            .expect("建立临时配置目录");
        ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
    }

    #[test]
    fn not_ready_without_pid_file() {
        let env = environment("nopid");
        assert!(!service_ready(&env));
        assert_eq!(
            stop(&env, &crate::config::load_settings(&env)).state,
            ServiceState::Stopped
        );
    }

    #[test]
    fn ready_line_must_appear_in_log() {
        let env = environment("logline");
        let path = log_path(&env);
        std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建目录");
        std::fs::write(&path, "启动中\n").expect("写日志");
        assert!(!log_has_ready_line(&path));
        std::fs::write(
            &path,
            "启动中\n[trigger] 触发接口监听 http://127.0.0.1:8769\n",
        )
        .expect("写日志");
        assert!(log_has_ready_line(&path));
    }

    #[test]
    fn dead_pid_is_cleared() {
        let env = environment("deadpid");
        std::fs::create_dir_all(pid_path(&env).parent().expect("有父目录")).expect("建目录");
        // 极不可能存在的 PID：读出来必须被清掉。
        std::fs::write(pid_path(&env), "4294000000\n").expect("写 PID");
        assert!(live_pid(&env).is_none());
        assert!(!pid_path(&env).exists());
    }
}
