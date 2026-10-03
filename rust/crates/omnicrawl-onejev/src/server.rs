//! 本地 `qev serve` 进程的生命周期：拉起 / 健康检查 / 停止 / 切尺寸。
//!
//! 服务是「一个尺寸一份进程」：切尺寸 = 停掉旧的、按新权重拉起、等健康检查通过，
//! 期间决策请求会失败（审查通道 fail-closed、重排 fail-open），因此切换动作本身要
//! 串行化，且界面要能看到「正在启动」这一状态。
//!
//! 服务地址固定为 `http://127.0.0.1:8766`——决策请求打到 `{base}/v1/systemone`，
//! 由 [`omnicrawl_host::decision_wire`] 按 `onejev` 请求方式拼路径。
//!
//! 进程回收走 [`omnicrawl_host::process_control`]：Windows 用 Job Object 的
//! kill-on-close（宿主崩溃也不留孤儿），Unix 用进程组 `SIGKILL`。

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use omnicrawl_host::process_control::{
    configure_process_group, terminate_process_tree, KillOnCloseJob,
};

use crate::env::{environment_state, EnvironmentState};
use crate::paths;
use crate::sizes::OneJevSize;

/// 本地服务监听地址与端口（`qev serve` 默认 8000，这里避开常见占用）。
pub const LOCAL_HOST: &str = "127.0.0.1";
pub const LOCAL_PORT: u16 = 8766;
/// 本地决策服务基地址（决策渠道的 `base_url` 写这个值）。
pub const LOCAL_BASE_URL: &str = "http://127.0.0.1:8766";
/// 健康检查等待上限（首次加载权重 + 捕获 CUDA 图，大尺寸会久一些）。
pub const DEFAULT_HEALTH_TIMEOUT_SECONDS: u64 = 600;

/// 服务的对外状态（界面状态行与服务守卫共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// 没有在跑的服务。
    Stopped,
    /// 正在启动（等地一个健康响应）。
    Starting,
    /// 已在监听。
    Ready,
    /// 启动失败。
    Failed(String),
}

impl ServerState {
    /// 界面文案。
    pub fn label(&self) -> String {
        match self {
            Self::Stopped => "未运行".to_string(),
            Self::Starting => "正在启动".to_string(),
            Self::Ready => "已就绪".to_string(),
            Self::Failed(reason) => format!("启动失败：{reason}"),
        }
    }
}

/// 一次启停/切换动作的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOutcome {
    pub state: ServerState,
    pub message: String,
}

/// 启动参数：权重目录、设备、显存相关开关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub root: PathBuf,
    /// 本地模型目录（含 config.json 与权重分片）。
    pub model_dir: PathBuf,
    /// `qev serve --name`：服务对外报出的模型名（决策渠道的 `model` 用它）。
    pub model_name: String,
    /// `cuda` / `cpu`。
    pub device: String,
    /// 服务日志文件路径。
    pub log_path: PathBuf,
}

impl LaunchSpec {
    /// 按尺寸与设备组装启动参数。
    pub fn new(root: &Path, size: &OneJevSize, device: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            model_dir: paths::model_dir(root, size.repo_id),
            model_name: size.repo_id.rsplit('/').next().unwrap_or(size.key).to_string(),
            device: normalize_device(device),
            log_path: paths::server_log_path(root),
        }
    }

    /// `qev serve --model` 的参数值：优先本地目录，服务因此可离线启动。
    pub fn model_argument(&self) -> String {
        if self.model_dir.is_dir() {
            self.model_dir.display().to_string()
        } else {
            // 权重不在本机时让 qev 自己按仓库 id 取（走 HF 缓存），至少不会静默用错模型。
            self.model_name.clone()
        }
    }
}

/// 设备取值归一：只认 `cuda` / `cpu`，其余（含 `auto`）按 CUDA 处理。
pub fn normalize_device(device: &str) -> String {
    if device.trim().eq_ignore_ascii_case("cpu") {
        "cpu".to_string()
    } else {
        "cuda".to_string()
    }
}

/// 本地服务句柄：宿主持有它，`Drop` 时回收进程树。
pub struct OneJevServer {
    child: Option<Child>,
    job: Option<KillOnCloseJob>,
    spec: Option<LaunchSpec>,
    state: ServerState,
}

impl Default for OneJevServer {
    fn default() -> Self {
        Self::new()
    }
}

impl OneJevServer {
    pub fn new() -> Self {
        Self {
            child: None,
            job: None,
            spec: None,
            state: ServerState::Stopped,
        }
    }

    pub fn state(&self) -> &ServerState {
        &self.state
    }

    /// 当前服务的模型名（决策渠道的 `model` 值来源）。
    pub fn model_name(&self) -> Option<&str> {
        self.spec.as_ref().map(|spec| spec.model_name.as_str())
    }

    /// 当前服务的权重目录。
    pub fn model_dir(&self) -> Option<&Path> {
        self.spec.as_ref().map(|spec| spec.model_dir.as_path())
    }

    /// 启动或切尺寸：已在运行的规格一致时直接返回，否则重启。
    ///
    /// `wait` 为 `true` 时阻塞等健康检查（调用方在后台线程里跑）；为 `false` 时
    /// 只把进程拉起来并把状态置为 [`ServerState::Starting`]。
    ///
    /// 环境（venv + qev）不由这里安装：装机是显式动作，由下载流程保证；
    /// 这里缺环境就报「未就绪」，让界面提示用户先去准备环境。
    pub fn start(&mut self, spec: LaunchSpec, wait: bool, timeout_seconds: u64) -> ServerOutcome {
        if self.child.is_some() && self.spec.as_ref() == Some(&spec) {
            let healthy = healthy(LOCAL_HOST, LOCAL_PORT, 1);
            self.state = if healthy {
                ServerState::Ready
            } else if wait {
                self.wait_healthy(timeout_seconds)
            } else {
                ServerState::Starting
            };
            return ServerOutcome {
                message: match self.state {
                    ServerState::Ready => format!("决策服务已在运行（{}）。", spec.model_name),
                    _ => format!("决策服务正在启动（{}）。", spec.model_name),
                },
                state: self.state.clone(),
            };
        }
        // 换模型：先把旧进程停掉，避免两份权重同时占显存。
        self.stop();

        if environment_state(&spec.root) != EnvironmentState::Ready {
            let reason = "自部署环境未就绪：请先执行「准备运行环境」。".to_string();
            self.state = ServerState::Failed(reason.clone());
            return ServerOutcome {
                state: self.state.clone(),
                message: reason,
            };
        }
        self.spawn(spec, wait, timeout_seconds)
    }

    /// 拉起 `qev serve` 进程。
    fn spawn(&mut self, spec: LaunchSpec, wait: bool, timeout_seconds: u64) -> ServerOutcome {
        if !spec.model_dir.is_dir() {
            self.state = ServerState::Failed("权重未下载".to_string());
            return ServerOutcome {
                state: self.state.clone(),
                message: format!(
                    "未找到 {} 的权重：请先在决策模型页下载该尺寸。",
                    spec.model_name
                ),
            };
        }
        let python = paths::venv_python(&spec.root);
        let mut command = Command::new(&python);
        command
            .arg("-m")
            .arg("qev.cli")
            .arg("serve")
            .arg("--model")
            .arg(spec.model_argument())
            .arg("--name")
            .arg(&spec.model_name)
            .arg("--device")
            .arg(&spec.device)
            .arg("--host")
            .arg(LOCAL_HOST)
            .arg("--port")
            .arg(LOCAL_PORT.to_string());
        // 权重已在本机目录：关掉联网探测，避免服务启动时再去问一次 HF。
        command
            .env("HF_HOME", paths::hf_cache_dir(&spec.root))
            .env("HF_HUB_OFFLINE", "1")
            .env("PYTHONUTF8", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        if let Some(parent) = spec.log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(log) = std::fs::File::create(&spec.log_path) {
            if let Ok(clone) = log.try_clone() {
                command.stdout(Stdio::from(log)).stderr(Stdio::from(clone));
            }
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.state = ServerState::Failed(format!("拉起服务失败：{error}"));
                return ServerOutcome {
                    state: self.state.clone(),
                    message: format!("拉起本地决策服务失败：{error}"),
                };
            }
        };
        let job = KillOnCloseJob::assign(&child);
        self.job = job;
        self.child = Some(child);
        self.spec = Some(spec.clone());
        if !wait {
            self.state = ServerState::Starting;
            return ServerOutcome {
                state: self.state.clone(),
                message: format!("决策服务正在启动（{}）。", spec.model_name),
            };
        }
        let state = self.wait_healthy(timeout_seconds);
        let message = match &state {
            ServerState::Ready => format!(
                "本地决策服务已就绪（{}，{}）。",
                spec.model_name, spec.device
            ),
            ServerState::Failed(reason) => format!("本地决策服务启动失败：{reason}"),
            _ => format!("本地决策服务状态：{}。", state.label()),
        };
        ServerOutcome { state, message }
    }

    /// 等健康检查通过；超时或进程提前退出都算失败。
    fn wait_healthy(&mut self, timeout_seconds: u64) -> ServerState {
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
        while Instant::now() < deadline {
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    let log = self
                        .spec
                        .as_ref()
                        .map(|spec| spec.log_path.display().to_string())
                        .unwrap_or_default();
                    self.state = ServerState::Failed(format!(
                        "服务进程已退出（{status}），日志：{log}"
                    ));
                    self.child = None;
                    self.job = None;
                    return self.state.clone();
                }
            }
            if healthy(LOCAL_HOST, LOCAL_PORT, 1) {
                self.state = ServerState::Ready;
                return self.state.clone();
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        self.state = ServerState::Failed(format!("等待健康检查超时（{timeout_seconds} 秒）"));
        self.state.clone()
    }

    /// 停止服务并释放显存（等待进程真正结束）。
    pub fn stop(&mut self) -> ServerOutcome {
        let running = self.child.is_some();
        if let Some(mut child) = self.child.take() {
            terminate_process_tree(&mut child, self.job.take(), true);
        }
        self.spec = None;
        self.state = ServerState::Stopped;
        ServerOutcome {
            state: self.state.clone(),
            message: if running {
                "已停止本地决策服务，显存已释放。".to_string()
            } else {
                "本地决策服务当前未运行。".to_string()
            },
        }
    }

    /// 重启：规格不变也强制换一次进程（配置改了设备时用）。
    pub fn restart(&mut self, spec: LaunchSpec, timeout_seconds: u64) -> ServerOutcome {
        self.stop();
        self.spawn(spec, true, timeout_seconds)
    }
}

impl Drop for OneJevServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            terminate_process_tree(&mut child, self.job.take(), false);
        }
    }
}

/// 健康检查：连得上端口即视为已监听（`qev` 在加载权重前就绑定端口？否——uvicorn
/// 绑定端口在加载之后，因此「连得上」等于「模型已加载、可以接请求」）。
pub fn healthy(host: &str, port: u16, timeout_seconds: u64) -> bool {
    let address = format!("{host}:{port}");
    match address.parse() {
        Ok(socket) => TcpStream::connect_timeout(
            &socket,
            Duration::from_secs(timeout_seconds.max(1)),
        )
        .is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sizes::ONEJEV_SIZES;

    #[test]
    fn spec_prefers_the_local_model_directory() {
        let root = std::env::temp_dir().join(format!("oc-onejev-spec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let size = ONEJEV_SIZES[0];
        let spec = LaunchSpec::new(&root, &size, "AUTO");
        assert_eq!(spec.device, "cuda", "auto 按 CUDA 处理");
        assert_eq!(spec.model_name, "OneJev-0.8B");
        assert_eq!(spec.model_argument(), "OneJev-0.8B", "权重不在本机时退仓库名");
        std::fs::create_dir_all(spec.model_dir.clone()).expect("建权重目录");
        assert_eq!(
            spec.model_argument(),
            spec.model_dir.display().to_string(),
            "权重在本机时用本地目录（离线可启动）"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn device_only_knows_cpu_and_cuda() {
        assert_eq!(normalize_device(" CPU "), "cpu");
        assert_eq!(normalize_device("cuda"), "cuda");
        assert_eq!(normalize_device("auto"), "cuda");
        assert_eq!(normalize_device(""), "cuda");
    }

    #[test]
    fn stopped_server_reports_stopped_and_has_no_model() {
        let mut server = OneJevServer::new();
        assert_eq!(server.state(), &ServerState::Stopped);
        assert!(server.model_name().is_none());
        let outcome = server.stop();
        assert_eq!(outcome.state, ServerState::Stopped);
        assert!(outcome.message.contains("未运行"));
        // 没有监听服务时健康检查必须为假（端口选一个高位空闲口）。
        assert!(!healthy(LOCAL_HOST, 1, 1));
    }

    #[test]
    fn start_without_weights_fails_with_a_readable_reason() {
        let root = std::env::temp_dir().join(format!("oc-onejev-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut server = OneJevServer::new();
        let spec = LaunchSpec::new(&root, &ONEJEV_SIZES[0], "cpu");
        // 环境未就绪 + 权重缺失：必须给出可操作提示，且不起进程。
        let outcome = server.start(spec, false, 1);
        assert!(matches!(outcome.state, ServerState::Failed(_) | ServerState::Stopped));
        assert!(!outcome.message.is_empty());
    }
}