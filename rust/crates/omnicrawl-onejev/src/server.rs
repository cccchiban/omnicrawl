//! 本地 `qev serve` 进程的生命周期：拉起 / 健康检查 / 停止 / 切尺寸。
//!
//! 服务是「一个尺寸一份进程」：切尺寸 = 停掉旧的、按新权重拉起、等健康检查通过，
//! 期间决策请求会失败（审查通道 fail-closed、重排 fail-open），因此切换动作本身要
//! 串行化，且界面要能看到「正在启动」这一状态。
//!
//! 服务地址固定为 `http://127.0.0.1:8766`——决策请求打到 `{base}/v1/systemone`，
//! 由 [`omnicrawl_host::decision_wire`] 按 `onejev` 请求方式拼路径。
//!
//! **本机所有 OmniCrawl 实例共用这一份服务**：它按端口与 PID 文件认定，而不是按
//! 「本进程起没起过」。因此：
//!
//! * 启动前先探端口，已在监听就直接复用（不重复拉起、不重复占显存）；
//! * 正在被别的实例拉起（端口还没监听）时等待其就绪，而不是再起第二个进程；
//! * 进程以「脱离宿主」的方式创建，本实例退出（含崩溃）都不回收它——停它只能在
//!   界面里显式点「停止服务」，或用 PID 文件里的 PID。
//!
//! 进程回收走 [`omnicrawl_host::process_control`]：这里创建的常驻进程刻意**不**纳入
//! kill-on-close 的 Job Object（那会让宿主退出时带走别人的服务）；显式停止走
//! [`stop_shared`]，按 PID 回收整棵进程树。

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use omnicrawl_host::process_control::{
    kill_process_tree_by_pid, pid_is_running, spawn_shared_service,
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
/// 认定「服务正在被谁拉起」的等待轮询间隔。
pub const READY_POLL_MILLIS: u64 = 500;

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
    ///
    /// 设备在这里就落地成 `cpu` / `cuda`：`auto` 需要探测 venv 里 torch 的实际
    /// 能力（见 [`resolve_device`]），不能拖到 `qev` 里赌它会自己回退。
    pub fn new(root: &Path, size: &OneJevSize, device: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            model_dir: paths::model_dir(root, size.repo_id),
            model_name: size.repo_id.rsplit('/').next().unwrap_or(size.key).to_string(),
            device: resolve_device(device, root),
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

    /// 换成实际在跑的模型名（复用其他实例的服务时，界面要显示那份服务的真实尺寸）。
    fn with_model_name(mut self, model_name: &str) -> Self {
        self.model_name = model_name.to_string();
        self
    }
}

/// 设备取值归一：`cpu` / `cuda` / `auto`（其余按 `auto` 处理）。
///
/// 这里**不决定**最终设备：`auto` 落地成哪个设备取决于 venv 里 torch 的实际能力
/// （见 [`resolve_device`]），只有拿到数据根的启动流程才判断得了。
pub fn normalize_device(device: &str) -> String {
    match device.trim().to_ascii_lowercase().as_str() {
        "cpu" => "cpu".to_string(),
        "cuda" => "cuda".to_string(),
        _ => "auto".to_string(),
    }
}

/// 决定传给 `qev serve --device` 的值。
///
/// `auto` 必须按 venv 里 torch 的**实际能力**落地：装了 CPU-only 轮子时传 `cuda`
/// 会让服务在加载权重时崩掉（`Torch not compiled with CUDA enabled`），而不是回退 CPU。
/// 探测不出结果（缺 venv / torch 跑不起来）时按 CPU 处理：CPU 一定能跑，
/// 不因探测失败而起不来。
pub fn resolve_device(configured: &str, root: &Path) -> String {
    match normalize_device(configured).as_str() {
        "cpu" => "cpu".to_string(),
        "cuda" => "cuda".to_string(),
        _ => match crate::env::torch_cuda_available(root) {
            Some(true) => "cuda".to_string(),
            _ => "cpu".to_string(),
        },
    }
}

/// 服务启动过程中的阶段回调（界面据此显示「正在加载权重」这类进度）。
///
/// 服务启动没有可量化的字节数：`qev serve` 加载权重后才绑定端口，
/// 因此这里只报阶段与已等待秒数。
pub type LaunchProgress<'a> = &'a mut dyn FnMut(&str);

/// 本地 `qev serve` 共享服务的句柄。
///
/// 服务不属于某个 OmniCrawl 实例：本结构只记录「本实例起过的那份进程」，用于在切尺寸
/// 时能停掉旧进程；端口上已就绪的服务（无论是本实例还是别的实例起的）一律直接复用。
pub struct OneJevServer {
    /// 本实例拉起的进程；进程以脱离宿主的方式创建，`Drop` 不会回收它。
    child: Option<Child>,
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

    /// 启动或切尺寸：端口上已有服务就直接复用，本实例起过的同规格进程则等它就绪。
    ///
    /// `wait` 为 `true` 时阻塞等健康检查（调用方在后台线程里跑）；为 `false` 时
    /// 只把进程拉起来并把状态置为 [`ServerState::Starting`]。
    ///
    /// 环境（venv + qev）不由这里安装：装机是显式动作，由下载流程保证；
    /// 这里缺环境就报「未就绪」，让界面提示用户先去准备环境。
    pub fn start(&mut self, spec: LaunchSpec, wait: bool, timeout_seconds: u64) -> ServerOutcome {
        self.start_with_progress(spec, wait, timeout_seconds, &mut |_| {})
    }

    /// 带进度回调的启动：界面据此显示「正在加载权重（已等待 N 秒）」。
    pub fn start_with_progress(
        &mut self,
        spec: LaunchSpec,
        wait: bool,
        timeout_seconds: u64,
        progress: LaunchProgress<'_>,
    ) -> ServerOutcome {
        // 端口已就绪：本机（很可能是别的 OmniCrawl 实例）已经跑着一份可用服务。
        // 多实例共用同一份权重与服务，这里直接复用，不再拉起第二份进程。
        if healthy(LOCAL_HOST, LOCAL_PORT, 1) {
            let running = running_model(&spec.root).unwrap_or_else(|| spec.model_name.clone());
            // 先算文案再移动 `spec`：它要连实际模型名一起记下来（界面显示真实在跑的那一档）。
            let message = if running == spec.model_name {
                format!("本地决策服务已在运行（{running}），本实例直接复用。")
            } else {
                format!(
                    "本地决策服务已在运行（{running}），本实例直接复用（配置的 {} 未生效：一份服务只跑一个尺寸）。",
                    spec.model_name
                )
            };
            self.adopt_ready(spec.with_model_name(&running));
            return ServerOutcome {
                state: self.state.clone(),
                message,
            };
        }

        // 本实例起过同规格的进程：它还在拉起途中（端口未绑定），等它。
        if self.child.is_some() && self.spec.as_ref() == Some(&spec) {
            let state = if wait {
                let pid = self.child.as_ref().map(|child| child.id());
                self.wait_ready(pid, timeout_seconds, progress)
            } else {
                self.state = ServerState::Starting;
                self.state.clone()
            };
            return ServerOutcome {
                message: match &state {
                    ServerState::Ready => format!(
                        "本地决策服务已就绪（{}，{}）。",
                        spec.model_name, spec.device
                    ),
                    ServerState::Starting => {
                        format!("决策服务正在启动（{}）。", spec.model_name)
                    }
                    _ => format!("本地决策服务未就绪：{}", state.label()),
                },
                state,
            };
        }

        // 别的实例正在拉起：PID 文件里有活进程但端口还没监听。等它就绪后复用，
        // 而不是再起一份（同尺寸两份权重会白白占满显存）。
        if let Some(pid) = live_pid(&spec.root) {
            if !wait {
                self.state = ServerState::Starting;
                self.spec = Some(spec);
                return ServerOutcome {
                    state: self.state.clone(),
                    message: format!(
                        "本地决策服务正由其他 OmniCrawl 实例启动（pid {pid}），就绪后本实例直接复用。"
                    ),
                };
            }
            let state = self.wait_ready(Some(pid), timeout_seconds, progress);
            if state == ServerState::Ready {
                let running = running_model(&spec.root).unwrap_or(spec.model_name.clone());
                self.adopt_ready(spec.with_model_name(&running));
                return ServerOutcome {
                    state: self.state.clone(),
                    message: format!("已复用其他 OmniCrawl 实例启动的本地决策服务（{running}）。"),
                };
            }
            return ServerOutcome {
                message: format!("本地决策服务未就绪：{}", state.label()),
                state,
            };
        }

        // 走到这里说明端口上没有服务：本实例若还记着旧进程（换模型、或它已退出），
        // 先收掉再按新规格拉起——避免两份权重同时占显存。
        self.stop_own();

        if environment_state(&spec.root) != EnvironmentState::Ready {
            let reason = "自部署环境未就绪：请先执行「准备运行环境」。".to_string();
            self.state = ServerState::Failed(reason.clone());
            return ServerOutcome {
                state: self.state.clone(),
                message: reason,
            };
        }
        self.spawn(spec, wait, timeout_seconds, progress)
    }

    /// 记下「本实例正在用某份已在监听的服务」：句柄不持有该进程，只用于界面展示与切尺寸判定。
    fn adopt_ready(&mut self, spec: LaunchSpec) {
        self.spec = Some(spec);
        self.state = ServerState::Ready;
    }

    /// 拉起 `qev serve` 进程。
    fn spawn(
        &mut self,
        spec: LaunchSpec,
        wait: bool,
        timeout_seconds: u64,
        progress: LaunchProgress<'_>,
    ) -> ServerOutcome {
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
        if let Some(parent) = spec.log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(log) = std::fs::File::create(&spec.log_path) {
            if let Ok(clone) = log.try_clone() {
                command.stdout(Stdio::from(log)).stderr(Stdio::from(clone));
            }
        }
        // 服务是本机所有实例共用的常驻进程：脱离宿主创建，不纳入宿主的 kill-on-close Job，
        // 因此本实例退出（含崩溃）都不会带走正在被其他实例使用的服务。
        let child = match spawn_shared_service(&mut command) {
            Ok(child) => child,
            Err(error) => {
                self.state = ServerState::Failed(format!("拉起服务失败：{error}"));
                return ServerOutcome {
                    state: self.state.clone(),
                    message: format!("拉起本地决策服务失败：{error}"),
                };
            }
        };
        write_pid(&spec.root, child.id(), &spec.model_name);
        self.child = Some(child);
        self.spec = Some(spec.clone());
        if !wait {
            self.state = ServerState::Starting;
            return ServerOutcome {
                state: self.state.clone(),
                message: format!("决策服务正在启动（{}）。", spec.model_name),
            };
        }
        let pid = self.child.as_ref().map(|child| child.id());
        let state = self.wait_ready(pid, timeout_seconds, progress);
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

    /// 等本地服务就绪；超时、进程提前退出都算失败。
    ///
    /// `pid` 是「这次等的是谁」：本实例拉起的子进程，或 PID 文件里那个由别的实例拉起的进程。
    /// 只看这一个进程的存活，不去猜端口是谁的；每 500ms 报一次「已等待 N 秒」，
    /// 界面因此不会看起来卡死。本实例子进程提前退出时把服务日志尾部带进失败文案
    /// （`qev` 的报错都在那里）。
    fn wait_ready(
        &mut self,
        pid: Option<u32>,
        timeout_seconds: u64,
        progress: LaunchProgress<'_>,
    ) -> ServerState {
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1));
        let started = Instant::now();
        let mut reported = 0u64;
        while Instant::now() < deadline {
            if let Some(id) = pid {
                if !pid_is_running(id) {
                    let exited = self
                        .child
                        .as_ref()
                        .map(|child| child.id() == id)
                        .unwrap_or(false);
                    if exited {
                        self.child = None;
                    }
                    let detail = self
                        .spec
                        .as_ref()
                        .and_then(|spec| read_log_tail(&spec.log_path, 400))
                        .unwrap_or_default();
                    let reason = if detail.is_empty() {
                        format!("服务进程已退出（pid {id}）")
                    } else {
                        format!("服务进程已退出（pid {id}）：{detail}")
                    };
                    self.state = ServerState::Failed(reason);
                    return self.state.clone();
                }
            }
            if healthy(LOCAL_HOST, LOCAL_PORT, 1) {
                self.state = ServerState::Ready;
                return self.state.clone();
            }
            let waited = started.elapsed().as_secs();
            if waited > reported {
                reported = waited;
                let name = self
                    .spec
                    .as_ref()
                    .map(|spec| spec.model_name.clone())
                    .unwrap_or_default();
                progress(&format!("正在加载权重并绑定端口（{name}，已等待 {waited} 秒）…"));
            }
            std::thread::sleep(Duration::from_millis(READY_POLL_MILLIS));
        }
        self.state = ServerState::Failed(format!("等待健康检查超时（{timeout_seconds} 秒）"));
        self.state.clone()
    }

    /// 停掉共享服务：按 PID 文件回收进程树，无论它由哪个实例拉起。
    ///
    /// 这是「停止服务」按钮的语义——面板上点它就是要那份服务真的停掉并释放显存；
    /// 也是切尺寸前的清理动作。本机只有一份服务，谁起的都能停。
    pub fn stop_shared(&mut self, root: &Path) -> ServerOutcome {
        let own_pid = self.child.as_ref().map(|child| child.id());
        let pid = read_pid(root).or(own_pid);
        self.stop_own();
        // 刚收掉的正是本实例的进程：PID 记录已被 `stop_own` 清掉，别再按「记录过期」报告。
        if let Some(own) = own_pid {
            return ServerOutcome {
                state: self.state.clone(),
                message: format!("已停止本地决策服务（pid {own}），显存已释放。"),
            };
        }
        let Some(pid) = pid else {
            let message = if healthy(LOCAL_HOST, LOCAL_PORT, 1) {
                "端口上仍有服务在监听，但没有可用的 PID 记录：请手工确认后再处理。".to_string()
            } else {
                "本地决策服务当前未运行。".to_string()
            };
            return ServerOutcome {
                state: self.state.clone(),
                message,
            };
        };
        if !pid_is_running(pid) {
            clear_pid(root);
            return ServerOutcome {
                state: self.state.clone(),
                message: "本地决策服务当前未运行（PID 记录已过期，已清理）。".to_string(),
            };
        }
        kill_process_tree_by_pid(pid);
        clear_pid(root);
        // 端口释放是异步的：给一点时间让进程退出，界面状态才不会立刻又读到「在跑」。
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !healthy(LOCAL_HOST, LOCAL_PORT, 1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(READY_POLL_MILLIS));
        }
        let released = !healthy(LOCAL_HOST, LOCAL_PORT, 1);
        ServerOutcome {
            state: if released {
                ServerState::Stopped
            } else {
                ServerState::Failed("停止请求已发出，但端口仍在监听".to_string())
            },
            message: if released {
                format!("已停止本地决策服务（pid {pid}），显存已释放。")
            } else {
                format!("已向 pid {pid} 发出停止请求，但端口仍在监听，可能还在退出。")
            },
        }
    }

    /// 收掉本实例拉起的进程并把状态复位；不影响其他实例的服务。
    ///
    /// 等端口真正释放再返回：紧接着按新规格拉起时，端口还占着会让「端口已就绪」
    /// 判定误认为是旧服务。
    fn stop_own(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            let recorded = self
                .spec
                .as_ref()
                .and_then(|spec| read_pid(&spec.root))
                .is_some_and(|recorded| recorded == pid);
            kill_process_tree_by_pid(pid);
            let _ = child.wait();
            // PID 文件记的正是这个进程时才清：别把别的实例刚写进去的记录抹掉。
            if recorded {
                if let Some(spec) = self.spec.as_ref() {
                    clear_pid(&spec.root);
                }
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && healthy(LOCAL_HOST, LOCAL_PORT, 1) {
                std::thread::sleep(Duration::from_millis(READY_POLL_MILLIS));
            }
        }
        self.spec = None;
        self.state = ServerState::Stopped;
    }

    /// 重启：先停掉当前的共享服务，再按新规格启动（配置改了尺寸/设备时用）。
    ///
    /// 切尺寸必须换进程，因此这里连**其他实例启动的**那份服务一起停：一份服务只能跑一个尺寸，
    /// 换尺寸就意味着旧的那份不再可用（界面上的切档是显式动作）。
    pub fn restart(&mut self, spec: LaunchSpec, timeout_seconds: u64) -> ServerOutcome {
        let stop = self.stop_shared(&spec.root);
        if stop.state != ServerState::Stopped {
            return stop;
        }
        self.start_with_progress(spec, true, timeout_seconds, &mut |_| {})
    }
}

/// 读 PID 文件；不存在或内容不合法返回 `None`。
pub fn read_pid(root: &Path) -> Option<u32> {
    read_pid_record(root).map(|record| record.0)
}

/// PID 文件里记的服务进程：`(pid, 模型名)`；模型名可能缺失（旧记录）时为空串。
pub fn read_pid_record(root: &Path) -> Option<(u32, String)> {
    let text = std::fs::read_to_string(paths::server_pid_path(root)).ok()?;
    let mut lines = text.lines();
    let pid = lines.next()?.trim().parse::<u32>().ok().filter(|pid| *pid > 0)?;
    let model = lines.next().map(|line| line.trim().to_string()).unwrap_or_default();
    Some((pid, model))
}

/// 当前在跑的服务用的是哪个模型（本机共用的那份，可能是别的实例起的）。
pub fn running_model(root: &Path) -> Option<String> {
    let (_, model) = read_pid_record(root)?;
    (!model.is_empty()).then_some(model)
}

/// 现存活的服务 PID：PID 文件里的进程还在才算数（进程已退出时顺手清掉记录）。
fn live_pid(root: &Path) -> Option<u32> {
    let pid = read_pid(root)?;
    if pid_is_running(pid) {
        Some(pid)
    } else {
        clear_pid(root);
        None
    }
}

/// 记下服务进程的 PID 与模型名（本机全部实例靠它认定「谁在跑、跑的是哪一档」）。
fn write_pid(root: &Path, pid: u32, model: &str) {
    let path = paths::server_pid_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{pid}\n{model}\n"));
}

/// 清掉 PID 文件（服务已停止或记录已过期）。
fn clear_pid(root: &Path) {
    let _ = std::fs::remove_file(paths::server_pid_path(root));
}

/// 读服务日志的尾部（`qev` 的报错都在这里，失败文案带上它才有可操作性）。
fn read_log_tail(path: &Path, limit: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let characters: Vec<char> = trimmed.chars().collect();
    let tail: String = if characters.len() <= limit {
        characters.iter().collect()
    } else {
        characters[characters.len() - limit..].iter().collect()
    };
    Some(tail.replace(['\r', '\n'], " "))
}

// 这里刻意没有 `Drop`：本结构持有的进程是本机共用的常驻服务，别的 OmniCrawl 实例
// 可能正在用它。收掉进程只发生在两个显式场合——切尺寸前（[`OneJevServer::stop_own`]）
// 与「停止服务」（[`OneJevServer::stop_shared`]）。

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
    fn spec_resolves_auto_to_cpu_when_torch_cannot_use_cuda() {
        let root = std::env::temp_dir().join(format!("oc-onejev-spec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let size = ONEJEV_SIZES[0];
        let spec = LaunchSpec::new(&root, &size, "AUTO");
        // 没有 venv 就探测不出 CUDA：必须落到 CPU，否则 qev 会以
        // 「Torch not compiled with CUDA enabled」崩在加载权重这一步。
        assert_eq!(spec.device, "cpu", "探测不出 CUDA 时按 CPU 落地");
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
    fn device_normalization_keeps_auto_distinct() {
        assert_eq!(normalize_device(" CPU "), "cpu");
        assert_eq!(normalize_device("CUDA"), "cuda");
        // auto 不能在这里被折叠成 cuda：落地要等探测 venv 里 torch 的能力。
        assert_eq!(normalize_device("auto"), "auto");
        assert_eq!(normalize_device(""), "auto");
        assert_eq!(normalize_device("tpu"), "auto");
    }

    #[test]
    fn explicit_device_choices_are_never_overridden() {
        let root = std::env::temp_dir().join(format!("oc-onejev-dev-{}", std::process::id()));
        // 用户显式选的设备不受探测结果影响：装 CPU 轮子也能按 cuda 试（可能失败，
        // 但那是用户的显式选择），没装 CUDA 轮子时选 cpu 依旧保持 cpu。
        assert_eq!(resolve_device("cpu", &root), "cpu");
        assert_eq!(resolve_device("cuda", &root), "cuda");
    }

    #[test]
    fn stopped_server_reports_stopped_and_has_no_model() {
        let root = std::env::temp_dir().join(format!("oc-onejev-stopped-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时根目录");
        let mut server = OneJevServer::new();
        assert_eq!(server.state(), &ServerState::Stopped);
        assert!(server.model_name().is_none());
        // 没有服务在跑时停止是无害的空操作，且不改动状态语义。
        let outcome = server.stop_shared(&root);
        assert_eq!(outcome.state, ServerState::Stopped);
        assert!(outcome.message.contains("未运行"));
        // 没有监听服务时健康检查必须为假（端口选一个高位空闲口）。
        assert!(!healthy(LOCAL_HOST, 1, 1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn start_without_weights_fails_with_a_readable_reason() {
        let root = std::env::temp_dir().join(format!("oc-onejev-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 端口上真跑着服务时这条用例的前提（无服务）不成立：跳过而不是误判。
        if healthy(LOCAL_HOST, LOCAL_PORT, 1) {
            eprintln!("跳过：{LOCAL_PORT} 端口已有服务在跑");
            return;
        }
        let mut server = OneJevServer::new();
        let spec = LaunchSpec::new(&root, &ONEJEV_SIZES[0], "cpu");
        // 环境未就绪 + 权重缺失：必须给出可操作提示，且不起进程。
        let outcome = server.start(spec, false, 1);
        assert!(matches!(outcome.state, ServerState::Failed(_) | ServerState::Stopped));
        assert!(!outcome.message.is_empty());
    }

    #[test]
    fn pid_file_round_trips_and_drops_stale_records() {
        let root = std::env::temp_dir().join(format!("oc-onejev-pid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时根目录");

        // 没有 PID 文件时读不到，也不会被误当成「已有实例在跑」。
        assert_eq!(read_pid(&root), None);
        assert_eq!(live_pid(&root), None);
        assert_eq!(running_model(&root), None);

        // 记下本进程：它是活的，因此算「服务在跑」，并且记着它跑的是哪一档。
        write_pid(&root, std::process::id(), "OneJev-0.8B");
        assert_eq!(read_pid(&root), Some(std::process::id()));
        assert_eq!(live_pid(&root), Some(std::process::id()));
        assert_eq!(running_model(&root).as_deref(), Some("OneJev-0.8B"));

        // 进程已退出的记录要顺手清掉，否则别的实例会一直误判「服务正在启动」。
        std::fs::write(crate::paths::server_pid_path(&root), "4294967295\nOneJev-4B\n")
            .expect("写死记录");
        assert_eq!(live_pid(&root), None);
        assert_eq!(read_pid(&root), None, "过期记录应被清理");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 多实例共用的核心约定：端口上已有服务时启动只复用、不再拉起第二个进程。
    #[test]
    fn start_reuses_a_service_that_is_already_listening() {
        let root = std::env::temp_dir().join(format!("oc-onejev-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时根目录");
        let listener = std::net::TcpListener::bind((LOCAL_HOST, LOCAL_PORT));
        let Ok(listener) = listener else {
            eprintln!("跳过：{LOCAL_PORT} 端口被占用，无法构造「已有服务」场景");
            return;
        };

        let mut server = OneJevServer::new();
        // 环境与权重都不在：若实现改为「先起进程」，这里会以失败收场。
        let spec = LaunchSpec::new(&root, &ONEJEV_SIZES[0], "cpu");
        let outcome = server.start(spec, false, 1);
        assert_eq!(outcome.state, ServerState::Ready, "{}", outcome.message);
        assert!(outcome.message.contains("复用"), "{}", outcome.message);
        assert!(!outcome.message.contains("正在启动"), "{}", outcome.message);

        drop(listener);
        let _ = std::fs::remove_dir_all(&root);
    }
}