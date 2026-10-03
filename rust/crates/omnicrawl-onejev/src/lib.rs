//! OneJev 本地自部署：把结构化决策模型换成「本机跑一份」。
//!
//! 与云端渠道并列的第三种决策服务来源：OneJev 是 OmniJev 发布的 System One 决策模型，
//! 官方用 Python 服务 `qev serve` 暴露 TypeSafe 兼容接口（`POST /v1/systemone`），
//! 因此自部署不是「加一个 HTTP 客户端」就能了事，需要三件事一起做：
//!
//! * [`sizes`]：可部署的尺寸清单（0.8B / 4B / 9B / 27B / 27B-FP8），驱动下载、删除、
//!   显存提示与模型名，界面与配置都不另留一份硬编码表；
//! * [`download`]：从 Hugging Face 拉取某个尺寸的权重到本机数据目录（`[onejev] model_dir`），
//!   带就绪判断与删除；
//! * [`env`] + [`server`]：专用虚拟环境（torch CUDA + qev）与本地服务进程的生命周期
//!   （拉起 / 健康检查 / 停止 / 切尺寸），宿主据此把决策请求打到 `127.0.0.1:<port>`。
//!
//! 权重与虚拟环境都放在用户数据目录（默认 `~/.omnicrawl/onejev`），不进工作区、不进仓库。

pub mod download;
pub mod env;
pub mod paths;
pub mod server;
pub mod sizes;

pub use download::{delete_model, download_model, model_ready, DownloadOutcome, ProgressCallback};
pub use env::{
    ensure_environment, environment_state, EnvironmentState, InstallOutcome, InstallProgress,
};
pub use paths::{default_root, resolve_root};
pub use server::{
    LaunchSpec, OneJevServer, ServerOutcome, ServerState, DEFAULT_HEALTH_TIMEOUT_SECONDS,
    LOCAL_BASE_URL, LOCAL_HOST, LOCAL_PORT,
};
pub use sizes::{find_size, size_labels, OneJevSize, ONEJEV_SIZES};