//! 宿主启动时自动管理 Telegram 与飞书连接器子进程（对应 `omnicrawl/connectors/autostart.py`）。
//!
//! 连接器本身各自拥有独立的 Agent、网络客户端和关闭流程，不应在线程中直接嵌入宿主进程。
//! 这里只负责三件事：无网络地读取两个连接器的本地配置并判断平台是否已配置；对已配置的平台
//! 启动独立子进程；在宿主退出时终止子进程及其后代，避免留下轮询进程。
//!
//! 连接器的 App Secret、Bot Token 等凭证只由子进程从继承的环境变量或 `config.toml` 读取，
//! 绝不出现在命令行参数中。
//!
//! 与 Python 的差异（见 crate `README.md`）：
//!
//! - **子进程命令由宿主注入**：Python 固定 `[sys.executable, "-m", module]`；内核没有 Python
//!   模块进程，[`ConnectorManagerOptions::specs`] 缺省给「当前可执行文件 + `connector <平台>`」，
//!   宿主可以换成真正的连接器二进制。
//! - **诊断不落日志**：Python 用 `logging`；Rust 侧把告警收进
//!   [`ConnectorProcessManager::diagnostics`]（文案逐条对齐），由宿主决定写到哪里。
//! - **PYTHONPATH**：Python 把 `omnicrawl` 包的父目录塞进子进程 `PYTHONPATH`；内核没有 Python
//!   包，[`ConnectorManagerOptions::pythonpath_root`] 由宿主注入，缺省不动 `PYTHONPATH`。
//! - **日志句柄生命周期**：Python 在 watcher 观察到退出后关闭父进程持有的日志句柄；Rust 把
//!   日志文件直接交给子进程的 stdout/stderr，父进程不保留句柄。对使用方可见的行为一致：
//!   子进程直接写日志文件、文件在退出后保留。
//! - **监督线程 join**：Python 的 join 有 5 秒上限并告警；Rust 直接 join（终止请求已发出）。
//! - **自定义平台**：宿主给出的非 Telegram / 非飞书平台没有已知的凭证来源，按「已配置」处理。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use omnicrawl_config::core::context::LAUNCH_CWD_ENV;
use omnicrawl_config::core::runtime::{load_config_data, user_config_dir, ConfigEnvironment};
use omnicrawl_config::value::toml_to_json_object;
use omnicrawl_session::redact_sensitive_text;
use omnicrawl_workspace::connector_singleton::{
    connector_lock_path, sanitize_platform_name, ConnectorInstanceLock,
};
use omnicrawl_workspace::process_control::{
    assign_process_to_kill_on_close_job, close_windows_handle, terminate_process_tree,
    ManagedProcess, ProcessState,
};
use omnicrawl_workspace::resolve_path;

use crate::feishu::config::{load_feishu_config, ConfigSource};
use crate::telegram::config::load_telegram_config;

/// 默认自动启动；设置为 0/false/no/off 可在需要单独运行连接器或排障时关闭。
pub const AUTO_START_ENV: &str = "OMNICRAWL_AUTO_START_CONNECTORS";
/// 关闭自动启动的取值（大小写不敏感，去首尾空白）。
pub const DISABLED_VALUES: [&str; 5] = ["0", "false", "no", "off", "disabled"];
/// 显式开启自动启动的取值。
pub const ENABLED_VALUES: [&str; 5] = ["1", "true", "yes", "on", "enabled"];
/// 子进程已被要求自行退出时，等待其监督线程收尾的上限（秒）。
pub const WATCHER_JOIN_TIMEOUT_SECONDS: f64 = 5.0;
/// 连接器子进程运行日志目录名（用户配置目录下，跨工作区共享）。
pub const CONNECTOR_LOG_DIRNAME: &str = "logs";
/// Telegram 平台的显示名（与 Python 一致，同时是单例锁与日志文件名的一部分）。
pub const TELEGRAM_PLATFORM: &str = "Telegram";
/// 飞书平台的显示名。
pub const FEISHU_PLATFORM: &str = "飞书";
/// 缺省子命令：`<当前可执行文件> connector telegram|feishu`。
pub const CONNECTOR_SUBCOMMAND: &str = "connector";
/// 监督线程观察子进程退出的轮询间隔（秒）。
pub const WATCH_POLL_SECONDS: f64 = 0.05;

/// 一个可由子进程启动的消息平台连接器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorSpec {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl ConnectorSpec {
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args,
        }
    }

    /// 完整命令行（程序名 + 参数），供诊断与对照数据集使用。
    pub fn command_line(&self) -> Vec<String> {
        let mut parts = vec![self.program.to_string_lossy().to_string()];
        parts.extend(self.args.iter().cloned());
        parts
    }
}

/// 缺省的两个平台：Telegram 与飞书，都走「当前可执行文件 + `connector <平台>`」。
pub fn default_connector_specs(program: impl Into<PathBuf>) -> Vec<ConnectorSpec> {
    let program = program.into();
    vec![
        ConnectorSpec::new(
            TELEGRAM_PLATFORM,
            program.clone(),
            vec![CONNECTOR_SUBCOMMAND.to_string(), "telegram".to_string()],
        ),
        ConnectorSpec::new(
            FEISHU_PLATFORM,
            program,
            vec![CONNECTOR_SUBCOMMAND.to_string(), "feishu".to_string()],
        ),
    ]
}

/// 子进程输出目标（对映 Python 的 `DEVNULL` / 追加文件 / `subprocess.STDOUT`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// 丢弃（`DEVNULL`）。
    Null,
    /// 追加到日志文件（`open(path, "ab")`）。
    Append(PathBuf),
    /// 并入子进程的 stdout（`subprocess.STDOUT`）。
    MergeIntoStdout,
}

/// 一次子进程启动请求：把 Python 传给 `Popen` 的实参摊平成可对照的数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub stdin_null: bool,
    pub stdout: OutputTarget,
    pub stderr: OutputTarget,
    /// Windows 的 `CREATE_NEW_PROCESS_GROUP`；Unix 用独立进程组（`start_new_session`）。
    pub new_process_group: bool,
    /// 日志文件路径（`capture_logs` 时有值），便于诊断与对照。
    pub log_path: Option<PathBuf>,
}

impl SpawnRequest {
    pub fn command_line(&self) -> Vec<String> {
        let mut parts = vec![self.program.to_string_lossy().to_string()];
        parts.extend(self.args.iter().cloned());
        parts
    }

    pub fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// 子进程启动器：真实实现走 `std::process`，测试替身记录请求而不真的起进程。
pub trait ProcessSpawner: Send + Sync {
    fn spawn(&self, request: &SpawnRequest) -> std::io::Result<Box<dyn ManagedProcess>>;
}

/// 真实的子进程启动器。
pub struct StdProcessSpawner;

impl ProcessSpawner for StdProcessSpawner {
    fn spawn(&self, request: &SpawnRequest) -> std::io::Result<Box<dyn ManagedProcess>> {
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            // Python 传的是完整环境副本，因此这里清空再灌入请求里的那份。
            .env_clear()
            .envs(request.env.iter().cloned());
        command.stdin(if request.stdin_null {
            Stdio::null()
        } else {
            Stdio::inherit()
        });
        let (stdout, stderr) = stdio_pair(&request.stdout, &request.stderr)?;
        command.stdout(stdout);
        command.stderr(stderr);
        if request.new_process_group {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;

                // Windows 没有 Unix 的 session / process group 语义；Job Object 负责递归回收，
                // CREATE_NEW_PROCESS_GROUP 作为兼容性兜底。
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                command.creation_flags(CREATE_NEW_PROCESS_GROUP);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;

                // 让 Unix 的 killpg 只作用于当前连接器及其后代，不误伤宿主。
                command.process_group(0);
            }
        }
        let child = command.spawn()?;
        Ok(Box::new(child) as Box<dyn ManagedProcess>)
    }
}

/// 打开（必要时创建）追加写的日志文件。
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// 把一对输出目标摊成 `Stdio`。
///
/// `MergeIntoStdout` 对映 Python 的 `subprocess.STDOUT`：并入**子进程自己的 stdout**，因此
/// stderr 复用 stdout 已经打开的那个日志文件（复制一个句柄），而不是继承父进程的 stderr。
fn stdio_pair(stdout: &OutputTarget, stderr: &OutputTarget) -> std::io::Result<(Stdio, Stdio)> {
    let (out, out_file) = match stdout {
        OutputTarget::Null => (Stdio::null(), None),
        OutputTarget::Append(path) => {
            let file = open_append(path)?;
            (Stdio::from(file.try_clone()?), Some(file))
        }
        OutputTarget::MergeIntoStdout => (Stdio::inherit(), None),
    };
    let err = match stderr {
        OutputTarget::Null => Stdio::null(),
        OutputTarget::Append(path) => Stdio::from(open_append(path)?),
        OutputTarget::MergeIntoStdout => match out_file {
            Some(file) => Stdio::from(file),
            None => match stdout {
                OutputTarget::Null => Stdio::null(),
                _ => Stdio::inherit(),
            },
        },
    };
    Ok((out, err))
}

/// 监督器构造参数（Python 侧的关键字参数面 + 宿主注入点）。
pub struct ConnectorManagerOptions {
    /// 配置来源：`~/.OmniCrawl`、`AI_CONFIG_FILE` 与环境变量。
    pub environment: ConfigEnvironment,
    /// 平台与命令行（缺省「当前可执行文件 + `connector <平台>`」）。
    pub specs: Vec<ConnectorSpec>,
    /// 子进程启动器。
    pub spawner: Arc<dyn ProcessSpawner>,
    /// 是否把子进程 stdout/stderr 落盘到用户 `logs/` 目录。
    pub capture_logs: bool,
    /// 配置文件路径（缺省按 `ConfigEnvironment` 解析）。
    pub config_path: Option<PathBuf>,
    /// 单例锁目录（缺省用户配置目录）。
    pub lock_root: Option<PathBuf>,
    /// 日志目录（缺省 `<用户配置目录>/logs`）。
    pub log_root: Option<PathBuf>,
    /// 子进程 `PYTHONPATH` 前置目录（宿主注入；缺省不动 `PYTHONPATH`）。
    pub pythonpath_root: Option<PathBuf>,
}

impl ConnectorManagerOptions {
    /// 以当前可执行文件为连接器程序建立缺省参数。
    pub fn new(environment: ConfigEnvironment) -> Self {
        let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("omnicrawl-host"));
        Self {
            environment,
            specs: default_connector_specs(program),
            spawner: Arc::new(StdProcessSpawner),
            capture_logs: false,
            config_path: None,
            lock_root: None,
            log_root: None,
            pythonpath_root: None,
        }
    }

    pub fn with_spawner(mut self, spawner: Arc<dyn ProcessSpawner>) -> Self {
        self.spawner = spawner;
        self
    }

    pub fn with_specs(mut self, specs: Vec<ConnectorSpec>) -> Self {
        self.specs = specs;
        self
    }

    pub fn with_capture_logs(mut self, capture_logs: bool) -> Self {
        self.capture_logs = capture_logs;
        self
    }

    pub fn with_config_path(mut self, config_path: Option<PathBuf>) -> Self {
        self.config_path = config_path;
        self
    }

    pub fn with_lock_root(mut self, lock_root: Option<PathBuf>) -> Self {
        self.lock_root = lock_root;
        self
    }

    pub fn with_log_root(mut self, log_root: Option<PathBuf>) -> Self {
        self.log_root = log_root;
        self
    }

    pub fn with_pythonpath_root(mut self, pythonpath_root: Option<PathBuf>) -> Self {
        self.pythonpath_root = pythonpath_root;
        self
    }
}

/// 管理宿主自动拉起的连接器子进程。
///
/// 该类不负责连接器内部的网络通信，也不创建连接器 Agent。配置探测和子进程启动失败均降级为
/// 诊断告警；因此外部平台故障不会阻止本地宿主启动。
pub struct ConnectorProcessManager {
    workspace_root: PathBuf,
    options: Arc<ConnectorManagerOptions>,
    state: Arc<Mutex<ManagerState>>,
    diagnostics: Arc<Mutex<Vec<String>>>,
}

/// 监督器持有的子进程及其平台相关回收资源。
struct Entry {
    shared: Arc<EntryShared>,
    watcher: Option<JoinHandle<()>>,
}

struct EntryShared {
    name: String,
    /// 进程句柄用 `Arc` 共享：启动失败的回收路径与监督线程可能同时持有同一份。
    process: Arc<Mutex<Box<dyn ManagedProcess>>>,
    job_handle: Mutex<Option<isize>>,
}

struct ManagerState {
    closed: bool,
    processes: Vec<Entry>,
}

/// 监督线程的最小句柄：只做「观察退出」「判断是否已关闭」与「记录告警」。
struct ManagerHandle {
    state: Arc<Mutex<ManagerState>>,
    diagnostics: Arc<Mutex<Vec<String>>>,
}

impl ManagerHandle {
    fn watch_process(&self, shared: &Arc<EntryShared>) {
        let return_code = loop {
            let mut process = shared
                .process
                .lock()
                .unwrap_or_else(|item| item.into_inner());
            match process.state() {
                ProcessState::Exited(code) => break code,
                ProcessState::Signalled => break -1,
                ProcessState::Running => {}
            }
            drop(process);
            std::thread::sleep(std::time::Duration::from_secs_f64(WATCH_POLL_SECONDS));
        };

        // 句柄只能由 watcher 或 close 其中一方取得，避免 Windows 下两个线程同时 CloseHandle。
        let job_handle = shared
            .job_handle
            .lock()
            .unwrap_or_else(|item| item.into_inner())
            .take();
        close_windows_handle(job_handle);
        let closed = self
            .state
            .lock()
            .unwrap_or_else(|item| item.into_inner())
            .closed;
        if !closed && return_code != 0 {
            self.diagnostics
                .lock()
                .unwrap_or_else(|item| item.into_inner())
                .push(format!(
                    "{} 连接器已退出（代码 {return_code}），TUI 将继续运行。",
                    shared.name
                ));
        }
    }
}

impl ConnectorProcessManager {
    pub fn new(workspace_root: impl Into<PathBuf>, options: ConnectorManagerOptions) -> Self {
        let workspace_root = resolve_path(&workspace_root.into());
        Self {
            workspace_root,
            options: Arc::new(options),
            state: Arc::new(Mutex::new(ManagerState {
                closed: false,
                processes: Vec::new(),
            })),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 已成功启动的平台名称，供启动诊断和测试使用。
    pub fn started_connectors(&self) -> Vec<String> {
        self.lock_state()
            .processes
            .iter()
            .map(|entry| entry.shared.name.clone())
            .collect()
    }

    /// 子进程 stdout/stderr 是否重定向到用户日志目录的开关。
    pub fn capture_logs(&self) -> bool {
        self.options.capture_logs
    }

    /// 已收集的告警 / 信息文案（Python 侧走 `logging`）。
    pub fn diagnostics(&self) -> Vec<String> {
        self.diagnostics
            .lock()
            .unwrap_or_else(|item| item.into_inner())
            .clone()
    }

    /// 探测配置并启动已配置的平台，返回成功启动的平台名称。
    pub fn start(&self) -> Vec<String> {
        let decision = auto_start_decision(&self.options.environment);
        if let Some(warning) = decision.warning {
            self.record(warning);
        }
        if !decision.enabled {
            self.record(format!(
                "已通过 {AUTO_START_ENV} 关闭 {TELEGRAM_PLATFORM}/{FEISHU_PLATFORM}自动启动。"
            ));
            return Vec::new();
        }
        for (spec, configured) in self.configured_connectors() {
            if !configured {
                continue;
            }
            if let Err(error) = self.start_one(&spec) {
                self.record(format!(
                    "{} 连接器自动启动异常，TUI 将继续运行：{}",
                    spec.name,
                    safe_error_text(&error)
                ));
            }
        }
        self.started_connectors()
    }

    /// 终止全部连接器进程及其子进程，并等待监督线程退出。
    ///
    /// 关闭顺序先标记监督器、再发出进程树终止请求，最后等待 watcher；这样 watcher 不会在
    /// 主 Agent 已关闭后继续持有远程连接器资源，也不会因重复调用 `close` 而重复关闭
    /// Windows Job Object 句柄。
    pub fn close(&self) {
        // 保留 `processes`（Python 的 `close` 同样不清空列表，`started_connectors` 在关闭后
        // 仍能报告本次会话拉起过哪些平台），只把监督线程句柄取走。先收集、后终止：终止请求
        // 会阻塞，不能在持有状态锁时发出，否则与 watcher 的「观察到退出后回读 closed」相锁。
        let (shared_entries, watchers) = {
            let mut state = self.lock_state();
            if state.closed {
                return;
            }
            state.closed = true;
            let shared_entries: Vec<Arc<EntryShared>> = state
                .processes
                .iter()
                .map(|entry| Arc::clone(&entry.shared))
                .collect();
            let watchers: Vec<(String, JoinHandle<()>)> = state
                .processes
                .iter_mut()
                .filter_map(|entry| {
                    entry
                        .watcher
                        .take()
                        .map(|watcher| (entry.shared.name.clone(), watcher))
                })
                .collect();
            (shared_entries, watchers)
        };

        for shared in &shared_entries {
            // 句柄只能由 watcher 或 close() 其中一方取得，避免 Windows 下两个线程同时
            // CloseHandle 造成无效句柄或误关其他资源。
            let job_handle = self.take_job_handle(shared);
            let mut process = shared
                .process
                .lock()
                .unwrap_or_else(|item| item.into_inner());
            terminate_process_tree(&mut **process, job_handle, true);
        }

        for (name, watcher) in watchers {
            if watcher.join().is_err() {
                self.record(format!(
                    "{name} 连接器监督线程未在 {WATCHER_JOIN_TIMEOUT_SECONDS:.1} 秒内退出。"
                ));
            }
        }
    }

    /// 启动单个平台；锁住注册过程，避免 `close` 与启动竞态。
    ///
    /// 启动前先获取该平台的跨进程单例锁：若已有其他进程在运行同一平台的连接器（例如另一个
    /// 宿主或手工启动的实例），直接跳过，避免同一平台出现多个长连接子进程。
    fn start_one(&self, spec: &ConnectorSpec) -> Result<(), String> {
        if self.lock_state().closed {
            // 监督器已进入关闭流程，不启动新进程。
            return Ok(());
        }

        // 单例锁失败（已有实例或获取超时）按「该平台已在运行」处理，跳过本次启动；这是
        // 多进程并存时的预期行为，不作为异常上报。
        let mut instance_lock = ConnectorInstanceLock::at(&spec.name, self.lock_path(&spec.name));
        if !instance_lock.try_acquire() {
            self.record(format!(
                "连接器 {} 已有实例在运行，跳过本次自动启动。",
                spec.name
            ));
            return Ok(());
        }

        let outcome = self.spawn_and_register(spec);
        instance_lock.release();
        outcome
    }

    fn spawn_and_register(&self, spec: &ConnectorSpec) -> Result<(), String> {
        let mut log_path: Option<PathBuf> = None;
        let mut stdout = OutputTarget::Null;
        let mut stderr = OutputTarget::Null;
        if self.options.capture_logs {
            // 连接器子进程输出落盘到用户配置目录 logs/ 下，宿主全屏界面保持干净，同时排障
            // 时可读完整运行日志（白名单拦截、断线重连等）。
            let path = self.connector_log_path(&spec.name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("创建连接器日志目录失败：{error}"))?;
            }
            stdout = OutputTarget::Append(path.clone());
            stderr = OutputTarget::MergeIntoStdout;
            log_path = Some(path);
        }

        let request = SpawnRequest {
            program: spec.program.clone(),
            args: spec.args.clone(),
            cwd: self.workspace_root.clone(),
            env: child_environment(
                &current_environment(),
                self.options.pythonpath_root.as_deref(),
            ),
            stdin_null: true,
            stdout,
            stderr,
            new_process_group: true,
            log_path: log_path.clone(),
        };
        let process = match self.options.spawner.spawn(&request) {
            Ok(process) => Arc::new(Mutex::new(process)),
            Err(error) => {
                // 连接器不直接向宿主终端写日志，避免破坏全屏界面；启动失败只降级为告警。
                return Err(format!(
                    "{} 连接器自动启动失败，TUI 将继续运行：{}",
                    spec.name,
                    safe_error_text(&error.to_string())
                ));
            }
        };

        // 拿不到 Job 句柄时静默退化到普通进程组回收（Python 侧那条「进程树保护初始化失败」
        // 告警实际上不可达：底层把 AttributeError/OSError 都吞掉并返回 None，因此这里不加
        // 诊断，保持两侧告警集合一致）。
        let job_handle = {
            let guard = process.lock().unwrap_or_else(|item| item.into_inner());
            assign_process_to_kill_on_close_job(&**guard)
        };

        let shared = Arc::new(EntryShared {
            name: spec.name.clone(),
            process: Arc::clone(&process),
            job_handle: Mutex::new(job_handle),
        });

        let mut state = self.lock_state();
        if state.closed {
            // 启动期间监督器已进入关闭流程：直接回收刚启动的进程，避免留下无监督的孤儿。
            drop(state);
            let mut guard = process.lock().unwrap_or_else(|item| item.into_inner());
            terminate_process_tree(&mut **guard, job_handle, true);
            return Ok(());
        }

        let handle = ManagerHandle {
            state: Arc::clone(&self.state),
            diagnostics: Arc::clone(&self.diagnostics),
        };
        let watch_shared = Arc::clone(&shared);
        let watcher = std::thread::Builder::new()
            .name(format!(
                "omnicrawl-{}-connector-watch",
                spec.name.to_lowercase()
            ))
            .spawn(move || handle.watch_process(&watch_shared));
        match watcher {
            Ok(watcher) => {
                state.processes.push(Entry {
                    shared,
                    watcher: Some(watcher),
                });
                Ok(())
            }
            Err(error) => {
                drop(state);
                // 监督线程起不来时必须回收刚启动的进程：否则宿主虽继续启动，却会遗留一个
                // 没有监督者的连接器孤儿。
                let mut guard = process.lock().unwrap_or_else(|item| item.into_inner());
                terminate_process_tree(&mut **guard, job_handle, true);
                Err(format!(
                    "{} 连接器监督线程创建失败：{}",
                    spec.name,
                    safe_error_text(&error.to_string())
                ))
            }
        }
    }

    fn take_job_handle(&self, shared: &Arc<EntryShared>) -> Option<isize> {
        shared
            .job_handle
            .lock()
            .unwrap_or_else(|item| item.into_inner())
            .take()
    }

    /// 无网络判断 Telegram / 飞书是否具备启动所需的最小凭证。
    fn configured_connectors(&self) -> Vec<(ConnectorSpec, bool)> {
        let mut results: Vec<(ConnectorSpec, bool)> = Vec::new();
        let data = match self.config_json() {
            Ok(data) => data,
            Err(error) => {
                for spec in &self.options.specs {
                    self.record(format!(
                        "检查 {} 自动启动配置失败，跳过该连接器：{}",
                        spec.name,
                        safe_error_text(&error)
                    ));
                    results.push((spec.clone(), false));
                }
                return results;
            }
        };

        for spec in &self.options.specs {
            let configured = if spec.name == TELEGRAM_PLATFORM {
                let environment = self.options.environment.clone();
                let query = move |name: &str| environment.get(name);
                let section = data.get("telegram").cloned();
                match load_telegram_config(&query, section.as_ref()) {
                    Ok(config) => {
                        !config.bot_token.trim().is_empty() && !config.allowed_user_ids.is_empty()
                    }
                    Err(error) => {
                        self.record(format!(
                            "检查 Telegram 自动启动配置失败，跳过该连接器：{}",
                            safe_error_text(&error)
                        ));
                        false
                    }
                }
            } else if spec.name == FEISHU_PLATFORM {
                let environment = self.options.environment.clone();
                let query = move |name: &str| environment.get(name);
                let source = ConfigSource {
                    environment: &query,
                    data: &data,
                };
                match load_feishu_config(source) {
                    Ok(config) => {
                        !config.app_id.trim().is_empty() && !config.app_secret.trim().is_empty()
                    }
                    Err(error) => {
                        self.record(format!(
                            "检查飞书自动启动配置失败，跳过该连接器：{}",
                            safe_error_text(&error)
                        ));
                        false
                    }
                }
            } else {
                // 宿主自定义平台：内核不知道它的凭证来源，按「已配置」处理。
                true
            };
            results.push((spec.clone(), configured));
        }
        results
    }

    fn config_json(&self) -> Result<serde_json::Value, String> {
        let data = load_config_data(
            &self.options.environment,
            self.options.config_path.as_deref(),
        )
        .map_err(|error| error.message().to_string())?;
        Ok(toml_to_json_object(&data))
    }

    fn lock_path(&self, platform_name: &str) -> PathBuf {
        match &self.options.lock_root {
            Some(root) => root.join(format!(
                "connector-{}.lock",
                sanitize_platform_name(platform_name)
            )),
            None => connector_lock_path(&self.options.environment, platform_name),
        }
    }

    fn connector_log_path(&self, platform_name: &str) -> PathBuf {
        let root = self
            .options
            .log_root
            .clone()
            .unwrap_or_else(|| default_log_root(&self.options.environment));
        connector_log_path(&root, platform_name)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ManagerState> {
        self.state.lock().unwrap_or_else(|item| item.into_inner())
    }

    fn record(&self, message: String) {
        self.diagnostics
            .lock()
            .unwrap_or_else(|item| item.into_inner())
            .push(message);
    }
}

/// 创建并启动自动连接器监督器。
///
/// 连接器的配置读取只访问本地 TOML 和环境变量，不会创建 Agent、建立网络连接或加载飞书
/// WebSocket。调用方必须在应用退出时调用返回对象的 `close`。生产入口默认把子进程
/// stdout/stderr 落盘到用户 `logs/` 目录，便于在宿主全屏之外排查连接器问题。
pub fn start_configured_connectors(
    mut options: ConnectorManagerOptions,
    workspace_root: impl Into<PathBuf>,
) -> ConnectorProcessManager {
    options.capture_logs = true;
    let manager = ConnectorProcessManager::new(workspace_root, options);
    manager.start();
    manager
}

/// `_auto_start_enabled` 的结果：开关结论 + 需要上报的告警文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoStartDecision {
    pub enabled: bool,
    pub warning: Option<String>,
}

/// 解析自动启动开关：缺省开启；未知取值按「启用」处理并给出告警。
pub fn auto_start_decision(env: &ConfigEnvironment) -> AutoStartDecision {
    let raw_value = env.get_trimmed(AUTO_START_ENV).to_lowercase();
    if raw_value.is_empty() {
        return AutoStartDecision {
            enabled: true,
            warning: None,
        };
    }
    if DISABLED_VALUES.contains(&raw_value.as_str()) {
        return AutoStartDecision {
            enabled: false,
            warning: None,
        };
    }
    let warning = if ENABLED_VALUES.contains(&raw_value.as_str()) {
        None
    } else {
        // 单引号与 Python 的 `%r` 对普通 ASCII 取值一致（`%r` 会转义控制字符，Rust 侧
        // 的 `{:?}` 用双引号，这里按 Python 的形状输出）。
        Some(format!(
            "{AUTO_START_ENV}='{raw_value}' 不是有效的开关值，将按启用处理。可使用 0/false/off 关闭。"
        ))
    };
    AutoStartDecision {
        enabled: true,
        warning,
    }
}

/// 构造子进程环境：清掉继承的启动目录变量，并按需前置 `PYTHONPATH`。
///
/// 连接器子进程以工作区为 cwd 启动，先清掉继承的 `LAUNCH_CWD_ENV`，否则会沿用主进程最初的
/// 启动目录；`pythonpath_root` 由宿主注入（Python 侧固定是 `omnicrawl` 包的父目录），
/// `None` 表示不动 `PYTHONPATH`。
pub fn child_environment(
    base: &[(String, String)],
    pythonpath_root: Option<&Path>,
) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = base
        .iter()
        .filter(|(key, _)| key != LAUNCH_CWD_ENV)
        .cloned()
        .collect();
    let Some(root) = pythonpath_root else {
        return environment;
    };
    let package_parent = root.to_string_lossy().to_string();
    let existing = environment
        .iter()
        .find(|(key, _)| key == "PYTHONPATH")
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default();
    let separator = if cfg!(windows) { ';' } else { ':' };
    let value = if existing.is_empty() {
        package_parent
    } else {
        format!("{package_parent}{separator}{existing}")
    };
    match environment.iter_mut().find(|(key, _)| key == "PYTHONPATH") {
        Some(slot) => slot.1 = value,
        None => environment.push(("PYTHONPATH".to_string(), value)),
    }
    environment
}

/// 当前进程环境（保持插入顺序）。
pub fn current_environment() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// 连接器运行日志文件路径（`<日志目录>/<平台>.log`）。
pub fn connector_log_path(log_root: &Path, platform_name: &str) -> PathBuf {
    log_root.join(format!("{}.log", sanitize_platform_name(platform_name)))
}

/// 默认日志目录：用户配置目录下的 `logs/`。
pub fn default_log_root(env: &ConfigEnvironment) -> PathBuf {
    user_config_dir(env).join(CONNECTOR_LOG_DIRNAME)
}

/// 错误文案脱敏 + 截断（对映 Python `_safe_error_text`）。
pub fn safe_error_text(text: &str) -> String {
    let redacted = redact_sensitive_text(text).trim().to_string();
    if redacted.is_empty() {
        "未知错误".to_string()
    } else if redacted.chars().count() > 300 {
        redacted.chars().take(300).collect()
    } else {
        redacted
    }
}
