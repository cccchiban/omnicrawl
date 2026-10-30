//! 自部署数据根目录与各子目录的解析。
//!
//! 默认根目录是 `~/.omnicrawl/onejev`（与本机其它 OmniCrawl 数据同族，不进工作区）：
//!
//! ```text
//! ~/.omnicrawl/onejev/
//!   models/OneJev-0.8B/   权重（HF 仓库快照）
//!   venv/                 专用虚拟环境（torch CUDA + qev）
//!   cache/huggingface/    HF 下载缓存（下载复用、离线启动都靠它）
//! ```

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime::ConfigEnvironment;

/// 载荷目录的环境变量名：显式指定时优先于所有自动搜索。
pub const RUNNER_DIR_ENV: &str = "OMNICRAWL_RUNNER_DIR";

/// 默认数据根目录（`<home>/.omnicrawl/onejev`）。
pub fn default_root(env: &ConfigEnvironment) -> PathBuf {
    env.home()
        .join(".omnicrawl")
        .join(omnicrawl_config::features::decision_model::ONEJEV_DEFAULT_DIRNAME)
}

/// 解析数据根目录：配置为空时用默认目录。
///
/// 转发到 `omnicrawl-config` 的同一实现：端点名是按这个根目录算哈希得来的，
/// 两边各写一份解析就会在 `~` 之类的写法上算出不同的根，最终连不上服务。
pub fn resolve_root(env: &ConfigEnvironment, configured: &str) -> PathBuf {
    omnicrawl_config::features::decision_model::resolve_onejev_root(env, configured)
}

/// 某尺寸的权重目录（根目录下的 `models/<仓库名>`）。
pub fn model_dir(root: &Path, repo_id: &str) -> PathBuf {
    let name = repo_id.rsplit('/').next().unwrap_or(repo_id);
    root.join("models").join(name)
}

/// 专用虚拟环境的目录。
pub fn venv_dir(root: &Path) -> PathBuf {
    root.join("venv")
}

/// 虚拟环境里的 Python 解释器。
pub fn venv_python(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        venv_dir(root).join("Scripts").join("python.exe")
    }
    #[cfg(not(windows))]
    {
        venv_dir(root).join("bin").join("python")
    }
}

/// 虚拟环境里 `qev` 可执行文件（判定环境是否装好时用它判断）。
pub fn venv_qev(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        venv_dir(root).join("Scripts").join("qev.exe")
    }
    #[cfg(not(windows))]
    {
        venv_dir(root).join("bin").join("qev")
    }
}

/// 本地判定 worker 的文件名（放在 `extensions/` 下，随宿主载荷分发）。
pub const WORKER_FILENAME: &str = "onejev_worker.py";

/// worker 脚本所在目录的搜索顺序（去重后按序探测）：
///
/// 1. `OMNICRAWL_RUNNER_DIR`（npm 启动器与打包脚本用它注入）；
/// 2. 可执行文件目录及其各级祖先下的 `extensions/`，以及仓库检出里的
///    `rust/assets/extensions/` 与旧布局的 `omnicrawl/extensions/`；
/// 3. 进程工作目录下的同三级（开发联调时直接从仓库根运行）。
fn runner_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };

    if let Some(value) = std::env::var_os(RUNNER_DIR_ENV) {
        let text = value.to_string_lossy().trim().to_string();
        if !text.is_empty() {
            push(PathBuf::from(text));
        }
    }

    if let Ok(executable) = std::env::current_exe() {
        let mut current = executable.parent().map(Path::to_path_buf);
        while let Some(directory) = current {
            push(directory.join("extensions"));
            push(directory.join("rust").join("assets").join("extensions"));
            push(directory.join("omnicrawl").join("extensions"));
            current = directory.parent().map(Path::to_path_buf);
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        push(cwd.join("extensions"));
        push(cwd.join("rust").join("assets").join("extensions"));
        push(cwd.join("omnicrawl").join("extensions"));
    }

    dirs
}

/// worker 脚本路径：在 [`runner_search_dirs`] 里找 `extensions/onejev_worker.py`。
///
/// 本机服务不再用官方 `qev serve`，而是由这个脚本直接驱动引擎并在管道上服务，
/// 所以它必须与二进制一起分发；找不到时返回候选路径，由调用方给出可读报错。
pub fn worker_script_path() -> PathBuf {
    runner_search_dirs()
        .into_iter()
        .map(|directory| directory.join(WORKER_FILENAME))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(WORKER_FILENAME))
}

/// Hugging Face 下载缓存目录（下载与离线启动共用）。
pub fn hf_cache_dir(root: &Path) -> PathBuf {
    root.join("cache").join("huggingface")
}

/// 服务日志文件（worker 的 stdout/stderr 落这里，便于排障）。
pub fn server_log_path(root: &Path) -> PathBuf {
    root.join("qev-server.log")
}

/// 服务 PID 文件：服务常驻且被本机全部实例共享，停它时靠这里找到进程。
pub fn server_pid_path(root: &Path) -> PathBuf {
    root.join("qev-server.pid")
}

/// 安装日志目录：venv / pip 的完整输出落这里。
///
/// 界面状态行只有两行，装 torch 的失败原因（traceback 的**最后一行**）永远挤不进去；
/// 完整输出必须落盘，用户才能自己翻到根因。
pub fn install_log_dir(root: &Path) -> PathBuf {
    root.join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_hangs_off_the_root() {
        let root = PathBuf::from("/data/onejev");
        assert_eq!(
            model_dir(&root, "OmniJev/OneJev-0.8B"),
            root.join("models").join("OneJev-0.8B")
        );
        assert_eq!(venv_dir(&root), root.join("venv"));
        assert!(venv_python(&root).starts_with(venv_dir(&root)));
        assert!(venv_qev(&root).starts_with(venv_dir(&root)));
        assert!(hf_cache_dir(&root).starts_with(root.join("cache")));
        assert!(server_log_path(&root).starts_with(&root));
    }

    #[test]
    fn empty_configuration_falls_back_to_default_root() {
        let env = ConfigEnvironment::from_process();
        assert_eq!(resolve_root(&env, "   "), default_root(&env));
        assert_eq!(default_root(&env).file_name().unwrap(), "onejev");
    }
}
