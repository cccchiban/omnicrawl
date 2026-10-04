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
//!
//! 路径归一化与 TTS 侧同一套语义（目标不存在时也要给出绝对路径，Windows 去掉 `\\?\` 前缀）。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime::ConfigEnvironment;

/// 默认数据根目录（`<home>/.omnicrawl/onejev`）。
pub fn default_root(env: &ConfigEnvironment) -> PathBuf {
    env.home().join(".omnicrawl").join("onejev")
}

/// 解析数据根目录：配置为空时用默认目录。
pub fn resolve_root(env: &ConfigEnvironment, configured: &str) -> PathBuf {
    let text = configured.trim();
    if text.is_empty() {
        return default_root(env);
    }
    resolve_lenient(&expand_user(env, text))
}

/// 展开配置里的 `~`（与 `omnicrawl-config` 内部同名实现同语义，那边不对外导出）。
fn expand_user(env: &ConfigEnvironment, raw: &str) -> PathBuf {
    if raw == "~" {
        return env.home().to_path_buf();
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let rest = rest.trim_start_matches(['/', '\\']);
            if rest == "." || rest.is_empty() {
                return env.home().to_path_buf();
            }
            return env.home().join(rest);
        }
    }
    PathBuf::from(raw)
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

/// 虚拟环境里 `qev` 可执行文件（服务优先用它，避免依赖 PATH）。
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

/// Hugging Face 下载缓存目录（下载与离线启动共用）。
pub fn hf_cache_dir(root: &Path) -> PathBuf {
    root.join("cache").join("huggingface")
}

/// 服务日志文件（`qev serve` 的 stdout/stderr 落这里，便于排障）。
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

/// 近似 Python `Path.resolve()`：目标不存在时也要给出归一化路径。
pub fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return strip_verbatim(canonical);
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = path.to_path_buf();
    while let Some(parent) = current.parent() {
        match current.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => return path.to_path_buf(),
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(canonical) = parent.canonicalize() {
            let mut resolved = strip_verbatim(canonical);
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        current = parent.to_path_buf();
    }
    path.to_path_buf()
}

/// 去掉 Windows 规范化路径的 `\\?\` 前缀（保留 UNC 形式）。
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            return PathBuf::from(rest.to_string());
        }
    }
    path
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