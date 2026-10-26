//! 决策 REST 服务的进程生命周期：常驻监听、按记录复用、显式停止。
//!
//! 与 OneJev 的本地服务同一套约定：
//!
//! * 服务是**本机共用**的常驻进程，按 PID 文件与服务日志认定——端口上有没有人在听不靠连接去猜：
//!   那种探测在服务未跑时会把调用方卡满超时（TCP 连接在回环上也可能被静默丢弃），
//!   而它恰好位于设置面板与启动路径上；
//! * 进程以「脱离宿主」的方式创建（`spawn_shared_service`），本实例退出（含崩溃）都不回收它；
//! * 停止只能显式做（[`stop`]），按 PID 文件回收整棵进程树。
//!
//! 复用判据是**服务是否已就绪**，而不是「本进程起没起过」：CLI / Skill 随时可能来调用，
//! 它既不该自己拉一份服务，也不该因为宿主没起而失败。

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_host::process_control::{
    kill_process_tree_by_pid, pid_is_running, spawn_shared_service,
};

use crate::config::DecisionSettings;

/// 等待服务就绪的轮询间隔。
pub const READY_POLL_MILLIS: u64 = 200;
/// 等待服务就绪的默认上限（秒）：服务只做转发，启动是毫秒级。
pub const DEFAULT_READY_TIMEOUT_SECONDS: u64 = 20;

/// PID 文件名（放在用户配置目录下，与配置同源）。
pub const PID_FILENAME: &str = "decision_api.pid";
/// 服务日志文件名。
pub const LOG_FILENAME: &str = "decision_api.log";

/// 服务状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    /// 没有在跑的服务。
    Stopped,
    /// 正在启动（还没打出就绪标记）。
    Starting,
    /// 已就绪（进程在跑且日志里已有监听标记）。
    Ready,
    /// 启动失败。
    Failed(String),
}

impl ServiceState {
    /// 界面/日志文案。
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
    /// 服务地址（就绪时有意义）。
    pub address: String,
}

impl EnsureOutcome {
    /// 服务是否可用。
    pub fn ready(&self) -> bool {
        self.state == ServiceState::Ready
    }
}

/// `serve` 打出这行才算真正接管端口（绑定监听之后才打印）。
const READY_LINE: &str = "[decision] 决策接口监听";

/// 服务是否已就绪：PID 记录里的进程还活着，且日志里已出现监听标记。
///
/// 只读 PID 文件与日志，**不发起任何网络探测**：探测在服务未跑时会阻塞到超时，
/// 而 [`ensure_running`] 位于设置面板与启动路径上。
pub fn service_ready(environment: &ConfigEnvironment) -> bool {
    if !pid_file_alive(environment) {
        return false;
    }
    log_has_ready_line(&log_path(environment))
}

/// 只读判定：PID 文件里的服务进程是否还活着（不清理过期记录，见 [`live_pid`]）。
fn pid_file_alive(environment: &ConfigEnvironment) -> bool {
    read_pid(environment).is_some_and(pid_is_running)
}

/// 服务日志里是否已经出现监听标记（逐行扫描：标记会被后续请求日志淹没）。
fn log_has_ready_line(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut line = Vec::new();
    let mut reader = std::io::BufReader::new(file);
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

/// 保证本机有一份可用的决策服务：已就绪就复用，否则拉起常驻进程并等它就绪。
///
/// `wait` 为 `false` 时只把进程拉起来（不阻塞等健康检查）；为 `true` 时等到就绪或超时。
pub fn ensure_running(
    environment: &ConfigEnvironment,
    settings: &DecisionSettings,
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
            message: format!("决策接口已就绪 {address}，本实例直接复用。"),
            address,
        };
    }

    // 别的实例正在拉起（PID 记录里进程还活着但还没打就绪标记）：等它就绪，不再起第二个进程。
    if let Some(pid) = live_pid(environment) {
        if !wait {
            return EnsureOutcome {
                state: ServiceState::Starting,
                message: format!("决策接口正由其他实例启动（pid {pid}），就绪后直接复用。"),
                address,
            };
        }
        let state = wait_ready(environment, pid, DEFAULT_READY_TIMEOUT_SECONDS);
        return EnsureOutcome {
            message: match &state {
                ServiceState::Ready => format!("已复用其他实例启动的决策接口（{address}）。"),
                _ => format!("决策接口未就绪：{}", state.label()),
            },
            state,
            address,
        };
    }

    match spawn_service(environment, settings) {
        Ok(pid) => {
            if !wait {
                return EnsureOutcome {
                    state: ServiceState::Starting,
                    message: format!("决策接口正在启动（{address}）。"),
                    address,
                };
            }
            let state = wait_ready(environment, pid, DEFAULT_READY_TIMEOUT_SECONDS);
            let message = match &state {
                ServiceState::Ready => format!("决策接口已就绪（{address}）。"),
                ServiceState::Failed(reason) => format!("决策接口启动失败：{reason}"),
                _ => format!("决策接口状态：{}。", state.label()),
            };
            EnsureOutcome {
                state,
                message,
                address,
            }
        }
        Err(error) => EnsureOutcome {
            state: ServiceState::Failed(error.clone()),
            message: format!("拉起决策接口失败：{error}"),
            address,
        },
    }
}

/// 停止本机共用的决策接口并清掉 PID 记录。
pub fn stop(environment: &ConfigEnvironment, settings: &DecisionSettings) -> EnsureOutcome {
    let address = settings.address();
    let Some(pid) = live_pid(environment) else {
        return EnsureOutcome {
            state: ServiceState::Stopped,
            message: "决策接口未运行。".to_string(),
            address,
        };
    };
    kill_process_tree_by_pid(pid);
    clear_pid(environment);
    EnsureOutcome {
        state: ServiceState::Stopped,
        message: format!("决策接口已停止（pid {pid}）。"),
        address,
    }
}

/// 拉起常驻服务进程；返回它的 PID。
fn spawn_service(
    environment: &ConfigEnvironment,
    _settings: &DecisionSettings,
) -> Result<u32, String> {
    let program = resolve_service_program();
    let mut command = Command::new(&program);
    // 子进程是常驻服务本体：不再经 ensure 路径，直接监听。
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(log) = open_log(environment) {
        command
            .stdout(Stdio::from(log.0))
            .stderr(Stdio::from(log.1));
    }
    // 服务是本机共用的常驻进程：脱离宿主创建，不纳入宿主的 kill-on-close Job。
    let child: Child = spawn_shared_service(&mut command)
        .map_err(|error| format!("拉起决策接口进程失败（{}）：{error}", program.display()))?;
    let pid = child.id();
    write_pid(environment, pid);
    Ok(pid)
}

/// 决策接口二进制的位置：`$OMNICRAWL_DECISION_BINARY` > 与当前可执行文件同目录 > PATH。
///
/// 不能直接用 `current_exe()`：调用方可能是 TUI 或宿主，它们的进程名不是本服务——
/// 用进程名当命令会拉起调用方自己。
pub fn resolve_service_program() -> PathBuf {
    const ENV: &str = "OMNICRAWL_DECISION_BINARY";
    if let Ok(value) = std::env::var(ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let name = if cfg!(windows) {
        "omnicrawl-decision.exe"
    } else {
        "omnicrawl-decision"
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

/// 等服务就绪（日志里出现监听标记）；进程提前退出即算失败。
fn wait_ready(environment: &ConfigEnvironment, pid: u32, timeout_seconds: u64) -> ServiceState {
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    while Instant::now() < deadline {
        if service_ready(environment) {
            return ServiceState::Ready;
        }
        if !pid_is_running(pid) {
            return ServiceState::Failed("服务进程已退出，请查看决策接口日志。".to_string());
        }
        std::thread::sleep(Duration::from_millis(READY_POLL_MILLIS));
    }
    ServiceState::Failed(format!("等待 {timeout_seconds} 秒仍未就绪。"))
}

/// PID 文件路径。
pub fn pid_path(environment: &ConfigEnvironment) -> PathBuf {
    user_config_dir(environment).join(PID_FILENAME)
}

/// 服务日志路径。
pub fn log_path(environment: &ConfigEnvironment) -> PathBuf {
    user_config_dir(environment).join(LOG_FILENAME)
}

/// 记下服务进程的 PID。
fn write_pid(environment: &ConfigEnvironment, pid: u32) {
    let path = pid_path(environment);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{pid}\n"));
}

/// 读 PID 文件里的进程号。
fn read_pid(environment: &ConfigEnvironment) -> Option<u32> {
    let text = std::fs::read_to_string(pid_path(environment)).ok()?;
    text.lines()
        .next()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

/// 清掉 PID 文件。
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
    use omnicrawl_config::features::decision_model::DecisionApiConfig;

    fn environment(tag: &str) -> ConfigEnvironment {
        let root =
            std::env::temp_dir().join(format!("oc-decision-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(omnicrawl_config::core::runtime::USER_CONFIG_DIRNAME))
            .expect("建立临时配置目录");
        ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
    }

    fn settings(enabled: bool) -> DecisionSettings {
        DecisionSettings {
            api: DecisionApiConfig {
                enabled,
                host: "127.0.0.1".to_string(),
                // 高位端口：测试期间几乎不可能被占用。
                port: 18_767,
            },
            channel: None,
        }
    }

    #[test]
    fn unavailable_reasons_are_reported_without_starting_a_process() {
        let environment = environment("unavailable");
        let disabled = ensure_running(&environment, &settings(false), false);
        assert!(!disabled.ready());
        assert!(disabled.message.contains("未启用"), "{}", disabled.message);

        // 已启用但没有决策渠道：同样不该监听。
        let no_channel = ensure_running(&environment, &settings(true), false);
        assert!(!no_channel.ready());
        assert!(
            no_channel.message.contains("决策渠道"),
            "{}",
            no_channel.message
        );
        assert!(
            read_pid(&environment).is_none(),
            "拒绝启动时不该留下 PID 记录"
        );
    }

    #[test]
    fn stop_is_a_noop_without_a_pid_record() {
        let environment = environment("stop");
        let outcome = stop(&environment, &settings(true));
        assert_eq!(outcome.state, ServiceState::Stopped);
        assert!(outcome.message.contains("未运行"));
    }

    #[test]
    fn stale_pid_records_are_dropped() {
        let environment = environment("stale");
        // 一个几乎不可能存活的 PID：读出来时必须被判为「不在跑」并清掉记录。
        write_pid(&environment, 4_294_967_295);
        assert_eq!(live_pid(&environment), None);
        assert!(read_pid(&environment).is_none(), "过期记录应被清理");
    }

    #[test]
    fn a_ready_service_is_reused_instead_of_starting_a_second_process() {
        let environment = environment("reuse");
        let mut settings = settings(true);
        // 记录与就绪标记都在：即使没有决策渠道也必须复用，而不是起进程或报不可用。
        settings.channel = Some(omnicrawl_host::review::DecisionReviewOptions {
            mode: "jev".to_string(),
            model: "jev-latest".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "k".to_string(),
            retry_count: 0,
        });
        // 只有 PID 记录、还没有就绪标记：此时不算就绪。
        write_pid(&environment, std::process::id());
        assert!(!service_ready(&environment), "只有 PID 记录时不该算就绪");
        std::fs::write(
            log_path(&environment),
            "[decision] 决策接口监听 http://127.0.0.1:18767\n",
        )
        .expect("写服务日志");

        let outcome = ensure_running(&environment, &settings, false);
        assert!(outcome.ready(), "{}", outcome.message);
        assert!(outcome.message.contains("复用"), "{}", outcome.message);
        assert_eq!(
            read_pid(&environment),
            Some(std::process::id()),
            "复用不该覆盖已有记录"
        );
    }
}
