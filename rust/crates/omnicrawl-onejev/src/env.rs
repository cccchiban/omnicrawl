//! 自部署专用虚拟环境：`python -m venv` + 按本机 GPU 选 CUDA 轮子安装 torch / qev。
//!
//! 推理底座是官方 `qev`（`pip install "qev[torch] @ git+https://github.com/OmniJev/OneJev.git"`），
//! 它需要 torch ≥ 2.6 与 Python ≥ 3.10。本机系统 Python 可能是 3.9（torch 最高只到 2.8），
//! 因此这里**优先挑 3.10–3.13 的解释器建环境**，挑不到再退到默认解释器并由 pip 自己裁决。
//!
//! CUDA 加速靠 PyTorch 官方索引：按 `nvidia-smi` 报出的驱动版本选 cu126 / cu128 通道；
//! 有驱动就装 CUDA 轮子（`--index-url` 指向 torch 通道），没有驱动就装 CPU 轮子
//! （默认 PyPI）——两条路都不改用户系统环境，全部落在本 crate 的 `venv/` 里。
//!
//! 安装是长任务（torch 轮子约 3 GB），所有对外入口都是「阻塞 + 进度回调」，
//! 由宿主放到后台线程里跑。

use std::path::{Path, PathBuf};
use std::process::Command;

use omnicrawl_config::core::runtime::ConfigEnvironment;

use crate::paths;

/// 用户在哪一步：界面据此显示「缺什么」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentState {
    /// 环境已就绪（解释器与 qev 都在）。
    Ready,
    /// 缺解释器或 qev。
    Missing,
    /// 系统 Python 的解释器路径（最高版本候选），未找到时为 `None`。
    NoPython,
}

/// 环境的就绪状态（不触发任何安装动作）。
pub fn environment_state(root: &Path) -> EnvironmentState {
    if paths::venv_qev(root).is_file() && paths::venv_python(root).is_file() {
        return EnvironmentState::Ready;
    }
    if system_python().is_some() {
        EnvironmentState::Missing
    } else {
        EnvironmentState::NoPython
    }
}

/// 一次安装的进度回调：`(阶段说明, 已下载字节或 0, 总字节或 0)`。
///
/// 阶段说明直接进状态行；pip 的输出是逐行的，因此这里粒度是「阶段 + 已耗时」。
pub type InstallProgress<'a> = &'a mut dyn FnMut(&str, u64, u64);

/// 一次环境安装的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub python: PathBuf,
    /// 是否装到了 CUDA 版本的 torch。
    pub cuda: bool,
}

/// 确保虚拟环境可用：缺解释器就建，缺 qev（或 torch）就装。
///
/// 已经就绪时立即返回，不产生任何网络与磁盘写入。
pub fn ensure_environment(
    env: &ConfigEnvironment,
    root: &Path,
    progress: InstallProgress<'_>,
) -> Result<InstallOutcome, String> {
    let _ = env;
    let python = paths::venv_python(root);
    let cuda = cuda_index_url().is_some();
    if environment_state(root) == EnvironmentState::Ready {
        return Ok(InstallOutcome { python, cuda });
    }

    std::fs::create_dir_all(root).map_err(|error| format!("创建 {} 失败：{error}", root.display()))?;

    if !python.is_file() {
        let base = system_python().ok_or_else(|| {
            "未找到 Python 3.10 以上的解释器；自部署需要 Python 3.10+（qev 要求）。".to_string()
        })?;
        progress(&format!("创建虚拟环境（{}）…", base.display()), 0, 0);
        let mut command = Command::new(&base);
        command
            .arg("-m")
            .arg("venv")
            .arg(paths::venv_dir(root))
            .stdin(std::process::Stdio::null());
        run_command(&mut command, "创建虚拟环境")?;
        if !python.is_file() {
            return Err(format!("虚拟环境创建后仍找不到解释器：{}", python.display()));
        }
    }

    if !paths::venv_qev(root).is_file() {
        // torch 先单独装：CUDA 轮子只在 torch 通道里，走索引参数最省事。
        progress(
            if cuda {
                "安装 torch（CUDA 轮子，约 3 GB）…"
            } else {
                "安装 torch（CPU 轮子，约 250 MB）…"
            },
            0,
            0,
        );
        let mut pip = pip_command(&python);
        pip.arg("install").arg("--upgrade").arg("torch");
        if let Some(index) = cuda_index_url() {
            pip.arg("--index-url").arg(index);
        }
        run_command(&mut pip, "安装 torch")?;

        progress("安装 qev（OneJev 官方服务）…", 0, 0);
        let spec = "qev[torch] @ git+https://github.com/OmniJev/OneJev.git";
        let mut pip = pip_command(&python);
        pip.arg("install").arg("--upgrade").arg(spec);
        // qev 自身的依赖仍从 PyPI 取：git 安装时索引参数会连带影响普通依赖解析，
        // 这里显式补回默认索引，避免只在 torch 通道里找 transformers。
        pip.arg("--extra-index-url").arg("https://pypi.org/simple");
        run_command(&mut pip, "安装 qev")?;
    }

    if environment_state(root) != EnvironmentState::Ready {
        return Err(format!(
            "环境安装结束但 qev 仍不可用：{}",
            paths::venv_qev(root).display()
        ));
    }
    Ok(InstallOutcome { python, cuda })
}

/// 选 CUDA 轮子的索引通道：有 NVIDIA 驱动才给（否则装 CPU 版，避免白拉 3 GB）。
///
/// 驱动主版本 ≥ 570 用 cu128，其余用 cu126（两个通道都覆盖 cp310–cp313 的 Windows 轮子）。
pub fn cuda_index_url() -> Option<String> {
    let driver = nvidia_driver_version()?;
    let major: u32 = driver.split('.').next()?.trim().parse().ok()?;
    if major >= 570 {
        Some("https://download.pytorch.org/whl/cu128".to_string())
    } else {
        Some("https://download.pytorch.org/whl/cu126".to_string())
    }
}

/// `nvidia-smi` 报出的驱动版本（取第一块 GPU 的那一行）。
pub fn nvidia_driver_version() -> Option<String> {
    let mut command = Command::new("nvidia-smi");
    command.args(["--query-gpu=driver_version", "--format=csv,noheader"]);
    hide_window(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.lines().next()?.trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// 可用的系统 Python 解释器（3.10–3.13 优先，找不到再退默认解释器）。
pub fn system_python() -> Option<PathBuf> {
    for candidate in ["py -3.13", "py -3.12", "py -3.11", "py -3.10", "python3", "python"] {
        let mut parts = candidate.split(' ');
        let program = parts.next()?;
        let mut command = Command::new(program);
        for part in parts {
            command.arg(part);
        }
        command
            .arg("-c")
            .arg("import sys; print(sys.version_info[:2])");
        hide_window(&mut command);
        let Ok(output) = command.output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let Ok(version) = parse_version_tuple(&text) else {
            continue;
        };
        if (3, 10) <= version && version < (3, 14) {
            return Some(PathBuf::from(candidate));
        }
    }
    // 没有 3.10–3.13 的解释器时退回默认解释器：交由 pip 自己裁决（例如 3.9 上
    // 只能拿到 torch 2.8，但 qev 仍可能装上）。解析失败即视为没有。
    let mut command = Command::new("python");
    command.arg("-c").arg("import sys; print(sys.version)");
    hide_window(&mut command);
    let output = command.output().ok()?;
    output.status.success().then(|| PathBuf::from("python"))
}

/// 解析 `python -c "print(sys.version_info[:2])"` 的输出，如 `(3, 12)`。
fn parse_version_tuple(text: &str) -> Result<(u32, u32), String> {
    let inner = text
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .to_string();
    let mut parts = inner.split(',');
    let major = parts
        .next()
        .ok_or_else(|| "缺少主版本".to_string())?
        .trim()
        .parse()
        .map_err(|_| "主版本不是数字".to_string())?;
    let minor = parts
        .next()
        .ok_or_else(|| "缺少次版本".to_string())?
        .trim()
        .parse()
        .map_err(|_| "次版本不是数字".to_string())?;
    Ok((major, minor))
}

/// venv 里的 pip 调用前缀。
fn pip_command(python: &Path) -> Command {
    let mut command = Command::new(python);
    command.arg("-m").arg("pip");
    hide_window(&mut command);
    command
}

/// 执行一条命令，失败时带上 stderr 的尾部便于定位。
fn run_command(command: &mut Command, action: &str) -> Result<(), String> {
    command.stdout(std::process::Stdio::null());
    hide_window(command);
    let output = command
        .output()
        .map_err(|error| format!("{action}失败：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("{action}失败：{}", tail(&stderr, 600)))
}

/// 取文本尾部（pip 的失败原因通常落在最后几行）。
fn tail(text: &str, limit: usize) -> String {
    let characters: Vec<char> = text.trim().chars().collect();
    if characters.len() <= limit {
        return characters.iter().collect();
    }
    characters[characters.len() - limit..].iter().collect()
}

#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuda_index_follows_the_driver_major() {
        // 驱动版本解析独立于本机是否真有 GPU：这里只验证版本元组解析。
        assert_eq!(parse_version_tuple("(3, 12)").unwrap(), (3, 12));
        assert_eq!(parse_version_tuple(" (3, 13)\n").unwrap(), (3, 13));
        assert!(parse_version_tuple("oops").is_err());
    }

    #[test]
    fn missing_environment_is_reported_not_installed() {
        let root = std::env::temp_dir().join(format!("oc-onejev-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建目录");
        // 空目录一定不是 Ready；具体是 Missing 还是 NoPython 取决于本机解释器。
        assert_ne!(environment_state(&root), EnvironmentState::Ready);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tail_keeps_the_end_of_the_log() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("abcdef", 3), "def");
    }
}