//! OneJev 本地自部署：把结构化决策模型换成「本机跑一份」。
//!
//! 与云端渠道并列的第三种决策服务来源：OneJev 是 OmniJev 发布的 System One 决策模型，
//! 官方用 Python 服务 `qev serve` 经 HTTP 暴露 TypeSafe 兼容接口；本项目改为自有 worker
//! 直接驱动同一份引擎、在命名管道（Windows）/ Unix socket 上服务，不经 TCP 与 HTTP。
//! 因此自部署不是「加一个 HTTP 客户端」就能了事，需要三件事一起做：
//!
//! * [`sizes`]：可部署的尺寸清单（0.8B / 4B / 9B / 27B / 27B-FP8），驱动下载、删除、
//!   显存提示与模型名，界面与配置都不另留一份硬编码表；
//! * [`download`]：从 Hugging Face 拉取某个尺寸的权重到本机数据目录（`[onejev] model_dir`），
//!   带就绪判断与删除；
//! * [`env`] + [`server`]：专用虚拟环境（torch CUDA + qev）与本地服务进程的生命周期
//!   （拉起 / 就绪判定 / 停止 / 切尺寸）。这份服务由**本机全部 OmniCrawl 实例共用**：
//!   实例启动时按配置自动准备，已在跑就复用，且不随实例退出被回收。
//!
//! 权重与虚拟环境都放在用户数据目录（默认 `~/.omnicrawl/onejev`），不进工作区、不进仓库。

pub mod download;
pub mod env;
pub mod paths;
pub mod progress;
pub mod server;
pub mod sizes;

pub use download::{delete_model, download_model, model_ready, DownloadOutcome, ProgressCallback};
pub use env::{
    ensure_environment, environment_state, remove_environment, system_python, EnvironmentState,
    InstallOutcome, InstallProgress, PythonLauncher,
};
pub use paths::{default_root, resolve_root};
pub use progress::{human_bytes, ProgressHandle, ProgressSnapshot, ProgressUnit};
pub use server::{
    read_pid, read_pid_record, running_model, service_ready, LaunchSpec, OneJevServer,
    ServerOutcome, ServerState, DEFAULT_HEALTH_TIMEOUT_SECONDS,
};
pub use sizes::{find_size, size_labels, OneJevSize, ONEJEV_SIZES};
