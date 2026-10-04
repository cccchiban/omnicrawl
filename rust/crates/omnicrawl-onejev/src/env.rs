//! 自部署专用虚拟环境：`python -m venv` + 按本机 GPU 选 CUDA 轮子安装 torch / qev。
//!
//! 推理底座是官方 `qev`（`pip install "qev[torch] @ git+https://github.com/OmniJev/OneJev.git"`），
//! 它需要 torch ≥ 2.6 与 Python ≥ 3.10。本机系统 Python 可能是 3.9（torch 最高只到 2.8），
//! 因此这里**优先挑 3.10–3.13 的解释器建环境**，挑不到再退到默认解释器并由 pip 自己裁决。
//!
//! 解释器候选里既有 `python` 这种单个程序，也有 `py -3.12` 这种「启动器 + 参数」：
//! 统一用 [`PythonLauncher`] 承载，绝不把带空格的候选串当成可执行文件路径。
//!
//! CUDA 加速靠 PyTorch 官方索引：按 `nvidia-smi` 报出的驱动版本选 cu126 / cu128 通道；
//! 有驱动就装 CUDA 轮子（`--index-url` 指向 torch 通道），没有驱动就装 CPU 轮子
//! （默认 PyPI）——两条路都不改用户系统环境，全部落在本 crate 的 `venv/` 里。
//!
//! 安装是长任务（torch 轮子约 3 GB），所有对外入口都是「阻塞 + 进度回调」，
//! 由宿主放到后台线程里跑；pip 的输出按行回传，界面因此看得到实时进展。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use omnicrawl_config::core::runtime::ConfigEnvironment;

use crate::paths;

/// 安装过程一共几步（界面据此画确定进度）。
const INSTALL_STEPS: u64 = 3;

/// pip 的下载重试次数。
///
/// torch 轮子 124 MB（CPU）到 3 GB（CUDA）：默认 5 次在上百 MB 的传输上不够用，
/// 网络抖一下就会以 `urllib3 ... Connection broken` 整体失败。
const PIP_RETRIES: &str = "10";

/// pip 的 socket 超时（秒）。默认 15 秒对大轮子太紧，放宽到 120 秒。
const PIP_SOCKET_TIMEOUT_SECONDS: &str = "120";

/// 用户在哪一步：界面据此显示「缺什么」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentState {
    /// 环境已就绪（解释器与 qev 都在）。
    Ready,
    /// 缺解释器或 qev。
    Missing,
    /// 系统里找不到可用的 Python 解释器。
    NoPython,
}

/// 一个可用的系统解释器：程序 + 前置参数。
///
/// `py -3.12` 这类候选是「启动器 + 参数」，必须拆开交给 [`Command`]；
/// 把它整串当程序名会得到 `program not found`（本机曾因此准备环境必失败）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonLauncher {
    /// 可执行文件（`py` / `python` / `python3`）。
    pub program: String,
    /// 前置参数（`-3.12`），没有则为空。
    pub args: Vec<String>,
    /// 解释器版本 `(major, minor)`。
    pub version: (u32, u32),
}

impl PythonLauncher {
    /// 界面与报错文案里的解释器写法（如 `py -3.12 (3.12)`）。
    pub fn display(&self) -> String {
        let mut text = self.program.clone();
        for arg in &self.args {
            text.push(' ');
            text.push_str(arg);
        }
        format!("{text} ({}.{})", self.version.0, self.version.1)
    }

    /// 按该解释器起一条命令（前后参数由调用方补）。
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        hide_window(&mut command);
        command
    }
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
/// 阶段说明直接进状态行；pip 逐行输出，因此字节位通常为 0（只有阶段与日志行）。
pub type InstallProgress<'a> = &'a mut dyn FnMut(&str, u64, u64);

/// 一次环境安装的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub python: PathBuf,
    /// 是否装到了 CUDA 版本的 torch。
    pub cuda: bool,
    /// 建环境用的解释器（报错与界面提示都用它）。
    pub launcher: PythonLauncher,
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
    // 就绪时没有解释器也要给一个可用的 launcher：后续服务启动按 venv 里的解释器走。
    let launcher = system_python().ok_or_else(|| {
        "未找到 Python 3.10 以上的解释器；自部署需要 Python 3.10+（qev 要求）。\
         可安装 python.org 版 Python 3.12，或用 `py -3.12 -V` 确认启动器可用。"
            .to_string()
    })?;
    if environment_state(root) == EnvironmentState::Ready {
        return Ok(InstallOutcome {
            python,
            cuda,
            launcher,
        });
    }

    std::fs::create_dir_all(root).map_err(|error| format!("创建 {} 失败：{error}", root.display()))?;

    if !python.is_file() {
        progress(
            &format!("1/{INSTALL_STEPS} 创建虚拟环境（{}）…", launcher.display()),
            1,
            INSTALL_STEPS,
        );
        let mut command = launcher.command();
        command
            .arg("-m")
            .arg("venv")
            .arg(paths::venv_dir(root))
            .stdin(Stdio::null());
        let log = paths::install_log_dir(root).join("1-venv.log");
        run_command(&mut command, "创建虚拟环境", Some(&log), &mut |_line| {})?;
        if !python.is_file() {
            return Err(format!(
                "虚拟环境创建后仍找不到解释器：{}。请确认 `{}` 能正常创建 venv。",
                python.display(),
                launcher.display()
            ));
        }
    }

    if !paths::venv_qev(root).is_file() {
        // torch 先单独装：CUDA 轮子只在 torch 通道里，走索引参数最省事。
        let torch_stage = if cuda {
            format!("2/{INSTALL_STEPS} 安装 torch（CUDA 轮子，约 3 GB）…")
        } else {
            format!("2/{INSTALL_STEPS} 安装 torch（CPU 轮子，约 250 MB）…")
        };
        progress(&torch_stage, 2, INSTALL_STEPS);
        let log = paths::install_log_dir(root).join("2-torch.log");
        // 权重轮子动辄上百 MB 到几 GB：默认 15 秒 socket 超时 + 5 次重试在这种体量下太紧，
        // 网络抖一下就会以 `urllib3 ... Connection broken` 整体失败。这里放宽两者，
        // 让 pip 自己在同一命令内把断点续传与重试做完。
        let mut pip = pip_command(&python);
        pip.arg("install")
            .arg("--upgrade")
            .arg("--retries")
            .arg(PIP_RETRIES)
            .arg("--timeout")
            .arg(PIP_SOCKET_TIMEOUT_SECONDS)
            .arg("torch");
        if let Some(index) = cuda_index_url() {
            pip.arg("--index-url").arg(index);
        }
        // pip 的下载进度是 `\r` 刷新的，逐行回传才能在界面上看到「在动」。
        let mut on_line = |line: &str| progress(line, 2, INSTALL_STEPS);
        run_command(&mut pip, "安装 torch", Some(&log), &mut on_line)?;

        progress(&format!("3/{INSTALL_STEPS} 安装 qev（OneJev 官方服务）…"), 3, INSTALL_STEPS);
        let spec = "qev[torch] @ git+https://github.com/OmniJev/OneJev.git";
        let log = paths::install_log_dir(root).join("3-qev.log");
        let mut pip = pip_command(&python);
        pip.arg("install")
            .arg("--upgrade")
            .arg("--retries")
            .arg(PIP_RETRIES)
            .arg("--timeout")
            .arg(PIP_SOCKET_TIMEOUT_SECONDS)
            .arg(spec);
        // qev 自身的依赖仍从 PyPI 取：git 安装时索引参数会连带影响普通依赖解析，
        // 这里显式补回默认索引，避免只在 torch 通道里找 transformers。
        pip.arg("--extra-index-url").arg("https://pypi.org/simple");
        let mut on_line = |line: &str| progress(line, 3, INSTALL_STEPS);
        run_command(&mut pip, "安装 qev", Some(&log), &mut on_line)?;
    }

    if environment_state(root) != EnvironmentState::Ready {
        return Err(format!(
            "环境安装结束但 qev 仍不可用：{}",
            paths::venv_qev(root).display()
        ));
    }
    progress("运行环境已就绪。", INSTALL_STEPS, INSTALL_STEPS);
    Ok(InstallOutcome {
        python,
        cuda,
        launcher,
    })
}

/// 删除专用虚拟环境（venv 整体），保留权重与下载缓存。
///
/// 「准备运行环境」按本机 GPU 能力选 torch 轮子，装错了（例如当时驱动不可用而落到 CPU 版）
/// 只能删掉重来；权重是 GB 级唯一副本，因此这里**只动 venv**。
pub fn remove_environment(root: &Path) -> Result<bool, String> {
    let venv = paths::venv_dir(root);
    if !venv.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&venv)
        .map_err(|error| format!("删除运行环境失败：{}（{error}）", venv.display()))?;
    Ok(true)
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

/// 解释器候选：先按版本从高到低问 `py` 启动器，再退到 PATH 上的 `python3` / `python`。
///
/// `py` 未安装或某个版本缺失都会让该候选失效（`py -3.13` 在本机返回 103），
/// 因此必须逐个探测而不是只看 `py` 自己能不能跑起来。
const PYTHON_CANDIDATES: [&[&str]; 6] = [
    &["py", "-3.13"],
    &["py", "-3.12"],
    &["py", "-3.11"],
    &["py", "-3.10"],
    &["python3"],
    &["python"],
];

/// 可用的系统 Python 解释器（3.10–3.13 优先，找不到再退默认解释器）。
pub fn system_python() -> Option<PythonLauncher> {
    for candidate in PYTHON_CANDIDATES {
        if let Some(launcher) = probe_python(candidate) {
            if (3, 10) <= launcher.version && launcher.version < (3, 14) {
                return Some(launcher);
            }
        }
    }
    // 没有 3.10–3.13 的解释器时退回默认解释器：交由 pip 自己裁决（例如 3.9 上
    // 只能拿到 torch 2.8，但 qev 仍可能装上）。版本过低时给出可读的失败原因。
    for candidate in [&["python"][..], &["python3"][..]] {
        if let Some(launcher) = probe_python(candidate) {
            return Some(launcher);
        }
    }
    None
}

/// 探测一个候选：能跑起来、能报出 `(major, minor)` 才算数。
fn probe_python(candidate: &[&str]) -> Option<PythonLauncher> {
    let (program, args) = candidate.split_first()?;
    let mut command = Command::new(program);
    command.args(args);
    command
        .arg("-c")
        .arg("import sys; print(sys.version_info[:2])");
    hide_window(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let version = parse_version_tuple(&text).ok()?;
    Some(PythonLauncher {
        program: program.to_string(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        version,
    })
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

/// venv 里的 torch 能否真正用 CUDA。
///
/// 只信 `torch.cuda.is_available()`：它同时覆盖「装的是 CPU-only 轮子」与
/// 「有 CUDA 轮子但驱动不可用」两种情况，而这两种情况传 `--device cuda` 都会
/// 让服务在加载权重时崩掉。返回 `None` 表示探测不了（缺 venv、缺 torch 或命令失败）。
pub fn torch_cuda_available(root: &Path) -> Option<bool> {
    let python = paths::venv_python(root);
    if !python.is_file() {
        return None;
    }
    let mut command = Command::new(&python);
    command
        .arg("-c")
        .arg("import torch; print(1 if torch.cuda.is_available() else 0)");
    hide_window(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    match text.trim().lines().last()?.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// venv 里的 pip 调用前缀。
fn pip_command(python: &Path) -> Command {
    let mut command = Command::new(python);
    command.arg("-m").arg("pip");
    hide_window(&mut command);
    command
}

/// 执行一条命令并把 stdout 的每一行交给 `on_line`；失败时给出**可读的原因**并把完整输出落盘。
///
/// stdout 与 stderr 分两根管道：stderr 由独立线程收干（否则管道写满会让子进程卡死，
/// 这正是「界面显示在跑、其实早就停了」的成因之一），stdout 在本线程按行消费。
///
/// 失败时的返回文案同时进状态行，因此**不能**只丢一坨 traceback：`pip` 的报错形如
/// 「ERROR: <一句话原因> + traceback + 最后一行异常」。界面只有两行，所以这里挑出
/// 这批行里信息量最大的几条（`ERROR:` 摘要行与 traceback 末行异常），而不是从中间截断。
/// 完整 stdout/stderr 一律写进 `logs/<标签>.log`，并在文案里给出路径。
fn run_command(
    command: &mut Command,
    action: &str,
    log_path: Option<&Path>,
    on_line: &mut dyn FnMut(&str),
) -> Result<(), String> {
    hide_window(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("{action}失败：{error}"))?;
    let stderr = child.stderr.take();
    let collector = thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stream) = stderr {
            let _ = stream.read_to_string(&mut text);
        }
        text
    });
    let mut stdout_lines: Vec<String> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut reader = LineStream::new(stdout, 400);
        while let Some(line) = reader.next_line() {
            stdout_lines.push(line.clone());
            on_line(&line);
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("{action}失败：{error}"))?;
    let stderr_text = collector.join().unwrap_or_default();

    // 完整输出落盘：装 torch 的失败细节（网络中断的具体异常）只有全量日志里才有。
    let saved = log_path.and_then(|path| {
        write_install_log(path, &stdout_lines, &stderr_text)
            .ok()
            .map(|()| path.to_path_buf())
    });

    if status.success() {
        return Ok(());
    }
    let reason = failure_reason(&stderr_text, &stdout_lines);
    match saved {
        Some(path) => Err(format!(
            "{action}失败：{reason}（完整日志：{}）",
            path.display()
        )),
        None => Err(format!("{action}失败：{reason}")),
    }
}

/// 从子进程输出里挑出「真正的原因」，供界面两行状态行展示。
///
/// pip 失败时的 stderr 结构：`ERROR: <摘要>` → `Traceback (...)` → `...` →
/// `<模块>.<异常类>: <消息>`。中间那些 `File "...", line N, in f` 帧对用户毫无价值，
/// 而**末行异常**才是根因（例如 `urllib3.exceptions.ProtocolError: Connection broken`）。
fn failure_reason(stderr_text: &str, stdout_lines: &[String]) -> String {
    let mut error_lines: Vec<&str> = Vec::new();
    let mut exception_line: Option<&str> = None;
    for line in stderr_text.lines().chain(stdout_lines.iter().map(|s| s.as_str())) {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        // pip 的摘要行：`ERROR: Could not find a version ...`
        if text.starts_with("ERROR:") {
            error_lines.push(text);
        }
        // traceback 的末行异常：`<Something>Error: ...` / `xxx.exceptions.Yyy: ...`
        if is_exception_line(text) {
            exception_line = Some(text);
        }
    }
    // 优先级：异常末行（最具体）→ ERROR 摘要 → 原样尾部。
    if let Some(line) = exception_line {
        return truncate_chars(line, 300);
    }
    if let Some(line) = error_lines.last() {
        return truncate_chars(line, 300);
    }
    let joined: Vec<&str> = stderr_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if joined.is_empty() {
        return "没有输出（子进程未给出原因）".to_string();
    }
    truncate_chars(&joined.join(" | "), 300)
}

/// 判断一行是不是 traceback 的「异常末行」（如 `urllib3.exceptions.ProtocolError: ...`）。
///
/// 判据：含 `: `，且冒号前那一段长得像异常类型（以 `Error`/`Exception`/`Warning` 结尾，
/// 或形如 `pkg.exceptions.Thing`）。据此把 `File "..."` 帧与缩进代码行排除掉。
fn is_exception_line(line: &str) -> bool {
    let Some((head, _)) = line.split_once(": ") else {
        return false;
    };
    let head = head.trim();
    if head.contains(' ') || head.contains('"') {
        return false;
    }
    let last = head.rsplit('.').next().unwrap_or(head);
    last.ends_with("Error")
        || last.ends_with("Exception")
        || last.ends_with("Warning")
        || head.contains(".exceptions.")
}

/// 把一次子进程的完整输出写进日志文件（stdout 与 stderr 分节，便于排查）。
fn write_install_log(path: &Path, stdout_lines: &[String], stderr_text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "=== stdout ===")?;
    for line in stdout_lines {
        writeln!(file, "{line}")?;
    }
    writeln!(file, "=== stderr ===")?;
    file.write_all(stderr_text.as_bytes())?;
    Ok(())
}

/// 把 pip 那种混用 `\n` 与 `\r` 的输出切成行。
///
/// pip 的下载进度条只用 `\r` 刷新，按 `\n` 切会一直攒到下载结束才出现；
/// 这里两种分隔符都当行尾，界面因此能看到实时百分比。超长行截断，避免把进度条
/// 重复拼出的巨串塞进状态行。
struct LineStream<R: Read> {
    reader: R,
    pending: String,
    limit: usize,
}

impl<R: Read> LineStream<R> {
    fn new(reader: R, limit: usize) -> Self {
        Self {
            reader,
            pending: String::new(),
            limit,
        }
    }

    fn next_line(&mut self) -> Option<String> {
        loop {
            if let Some(index) = self
                .pending
                .find(['\n', '\r'])
            {
                let rest = self.pending.split_off(index + 1);
                let mut line = std::mem::replace(&mut self.pending, rest);
                line.truncate(index);
                let line = line.trim().to_string();
                if !line.is_empty() {
                    return Some(truncate_chars(&line, self.limit));
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            match self.reader.read(&mut chunk) {
                Ok(0) => {
                    let line = self.pending.trim().to_string();
                    self.pending.clear();
                    return (!line.is_empty()).then(|| truncate_chars(&line, self.limit));
                }
                Ok(read) => {
                    self.pending
                        .push_str(&String::from_utf8_lossy(&chunk[..read]));
                }
                Err(_) => return None,
            }
        }
    }
}

/// 取文本前 `limit` 个字符（按字符而不是字节切，避免多字节字符被劈开）。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
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
    use std::io::Cursor;

    #[test]
    fn version_tuple_parsing_is_strict() {
        assert_eq!(parse_version_tuple("(3, 12)").unwrap(), (3, 12));
        assert_eq!(parse_version_tuple(" (3, 13)\n").unwrap(), (3, 13));
        assert!(parse_version_tuple("oops").is_err());
    }

    #[test]
    fn launcher_keeps_program_and_arguments_apart() {
        // 带版本参数的候选必须拆成「程序 + 参数」，整串当程序名会 program not found。
        let launcher = PythonLauncher {
            program: "py".to_string(),
            args: vec!["-3.12".to_string()],
            version: (3, 12),
        };
        assert_eq!(launcher.display(), "py -3.12 (3.12)");
        let command = launcher.command();
        assert_eq!(command.get_program().to_string_lossy(), "py");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-3.12"]);
    }

    #[test]
    fn candidates_cover_launcher_and_plain_programs() {
        assert_eq!(PYTHON_CANDIDATES[0], ["py", "-3.13"]);
        assert_eq!(PYTHON_CANDIDATES[5], ["python"]);
        // 探测本机解释器：只要有 3.10+ 就必须给出与候选拆解一致的 launcher。
        if let Some(launcher) = system_python() {
            assert!(!launcher.program.is_empty());
            assert!(
                launcher.args.iter().all(|arg| arg.starts_with('-')),
                "前置参数只能是选项：{:?}",
                launcher.args
            );
        }
    }

    #[test]
    fn probed_launcher_can_actually_spawn() {
        // 回归测试：曾经把 `py -3.12` 整串当程序名交给 Command，于是准备环境必然
        // 以 "program not found" 失败。这里真的拉起一次，确认拆解后的程序名可执行。
        let Some(launcher) = system_python() else {
            return; // 本机没有可用解释器时跳过（不是失败）。
        };
        let mut command = launcher.command();
        command.arg("-c").arg("import sys; print(sys.version_info[0])");
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("{} 起不来：{error}", launcher.display()));
        assert!(
            output.status.success(),
            "{} 执行失败：{}",
            launcher.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).trim().starts_with('3'),
            "应报出主版本"
        );
    }

    #[test]
    fn missing_candidates_do_not_abort_the_probe() {
        // 缺失的候选（如未安装 3.13）只会被跳过，后面的候选仍要能命中——
        // 本机 `py -3.13` 返回 103，探测必须继续往下走。
        assert!(probe_python(&["definitely-not-a-python-binary"]).is_none());
    }

    #[test]
    fn remove_environment_only_touches_the_venv() {
        let root = std::env::temp_dir().join(format!("oc-onejev-rm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 布局：venv（运行环境）+ models（权重）+ cache（下载缓存）。
        let python = paths::venv_python(&root);
        std::fs::create_dir_all(python.parent().expect("解释器所在目录")).expect("建 venv");
        std::fs::write(&python, b"stub").expect("放个解释器占位");
        let model = paths::model_dir(&root, "OmniJev/OneJev-0.8B");
        std::fs::create_dir_all(&model).expect("建权重目录");
        std::fs::write(model.join("config.json"), b"{}").expect("放个权重文件");
        std::fs::create_dir_all(paths::hf_cache_dir(&root)).expect("建缓存目录");

        assert!(remove_environment(&root).expect("删除应当成功"), "有 venv 时应报告已删");
        assert!(!paths::venv_dir(&root).exists(), "venv 应被删掉");
        assert!(model.is_dir(), "权重必须保留");
        assert!(paths::hf_cache_dir(&root).is_dir(), "下载缓存必须保留");

        // 幂等：再删一次报告「没有可删的」，不报错。
        assert!(!remove_environment(&root).expect("重复删除不该失败"));
        let _ = std::fs::remove_dir_all(&root);
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
    fn line_stream_splits_on_carriage_returns() {
        // pip 的进度条只用 `\r` 刷新，必须能切出中间态。
        let mut stream = LineStream::new(Cursor::new(b"a\rDownloading 10%\rDownloading 90%\n"), 400);
        assert_eq!(stream.next_line().as_deref(), Some("a"));
        assert_eq!(stream.next_line().as_deref(), Some("Downloading 10%"));
        assert_eq!(stream.next_line().as_deref(), Some("Downloading 90%"));
        assert_eq!(stream.next_line(), None);
    }

    #[test]
    fn line_stream_truncates_and_skips_blanks() {
        let long = "x".repeat(50);
        let mut stream = LineStream::new(Cursor::new(format!("\n\n{long}\n").into_bytes()), 10);
        let line = stream.next_line().expect("应有行");
        assert_eq!(line.chars().count(), 10);
        assert_eq!(stream.next_line(), None, "空行不产出");
    }

    #[test]
    fn failure_reason_prefers_the_traceback_exception_line() {
        // 用户报错现场：pip 装 torch 时连接被重置。以前 tail(600) 从中间截断，
        // 界面只剩下 `File "...\\urllib3\\response.py", line 560, in read` 这种
        // 毫无信息量的中间帧；现在必须挑出末行异常。
        let stderr = "\
ERROR: Exception:
Traceback (most recent call last):
  File \"C:\\\\...\\\\pip\\\\_internal\\\\commands\\\\install.py\", line 438, in run
    return self._handle_target_dir(
  File \"C:\\\\...\\\\pip\\\\_vendor\\\\urllib3\\\\response.py\", line 560, in read
    with self._error_catcher():
urllib3.exceptions.ProtocolError: ('Connection broken: ConnectionResetError(10054)', ...)
";
        let reason = failure_reason(stderr, &[]);
        assert!(
            reason.starts_with("urllib3.exceptions.ProtocolError"),
            "应挑出 traceback 末行异常：{reason}"
        );
        assert!(
            reason.contains("Connection broken"),
            "异常消息要保留：{reason}"
        );
        assert!(
            !reason.contains("_error_catcher"),
            "不该退化成中间帧：{reason}"
        );
    }

    #[test]
    fn failure_reason_falls_back_to_error_summary() {
        // 没有 traceback 时（如版本不匹配）取 ERROR 摘要行。
        let stderr = "\
ERROR: Could not find a version that satisfies the requirement torch (from versions: none)
ERROR: No matching distribution found for torch
";
        let reason = failure_reason(stderr, &[]);
        assert!(
            reason.contains("No matching distribution"),
            "取最后一条 ERROR 摘要：{reason}"
        );
    }

    #[test]
    fn failure_reason_reads_stdout_when_stderr_is_empty() {
        // 有些失败只有 stdout 有内容。
        let reason = failure_reason("", &["Collecting torch".to_string(), "ERROR: 目标不可达".to_string()]);
        assert!(reason.contains("目标不可达"), "{reason}");
    }

    #[test]
    fn failure_reason_reports_absence_of_output() {
        assert!(failure_reason("", &[]).contains("没有输出"));
    }

    #[test]
    fn exception_line_detection_ignores_traceback_frames() {
        // traceback 中间帧与缩进代码行都不能被当成异常末行。
        assert!(!is_exception_line(
            "  File \"C:\\\\x\\\\pip\\\\install.py\", line 438, in run"
        ));
        assert!(!is_exception_line("    return self._handle_target_dir("));
        assert!(!is_exception_line("with self._error_catcher():"));
        assert!(is_exception_line("urllib3.exceptions.ProtocolError: Connection broken"));
        assert!(is_exception_line("ConnectionResetError: [WinError 10054] 远程主机强迫关闭"));
        assert!(is_exception_line("pip._vendor.urllib3.exceptions.ReadTimeoutError: x"));
    }

    #[test]
    fn install_log_records_both_streams() {
        let path = std::env::temp_dir().join(format!("oc-onejev-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        write_install_log(
            &path,
            &["Downloading torch".to_string()],
            "ERROR: boom\n",
        )
        .expect("写日志");
        let text = std::fs::read_to_string(&path).expect("读回日志");
        assert!(text.contains("=== stdout ==="), "{text}");
        assert!(text.contains("Downloading torch"), "{text}");
        assert!(text.contains("=== stderr ==="), "{text}");
        assert!(text.contains("ERROR: boom"), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_command_reports_reason_and_writes_log() {
        // 端到端：真起一个子进程，让它在 stderr 打一段「pip 风格」的 traceback，
        // 验证 (1) 返回文案是末行异常而不是中间帧，(2) 完整输出落盘，(3) 退出码非零被识别。
        let python = match system_python() {
            Some(launcher) => launcher,
            None => return, // 本机无解释器时跳过
        };
        let script = concat!(
            "import sys\n",
            "print('Collecting torch')\n",
            "sys.stderr.write('ERROR: Exception:\\n')\n",
            "sys.stderr.write('Traceback (most recent call last):\\n')\n",
            "sys.stderr.write('  File \"x.py\", line 1, in <module>\\n')\n",
            "sys.stderr.write('    with self._error_catcher():\\n')\n",
            "sys.stderr.write(\"urllib3.exceptions.ProtocolError: ('Connection broken',)\\n\")\n",
            "sys.exit(1)\n",
        );
        let log = std::env::temp_dir().join(format!("oc-onejev-run-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);

        let mut command = python.command();
        command.arg("-c").arg(script);
        let error = run_command(&mut command, "安装 torch", Some(&log), &mut |_| {})
            .expect_err("子进程非零退出应当报错");

        assert!(error.starts_with("安装 torch失败："), "{error}");
        assert!(
            error.contains("urllib3.exceptions.ProtocolError"),
            "文案要点出真正原因：{error}"
        );
        assert!(
            !error.contains("_error_catcher"),
            "不该是中间帧：{error}"
        );
        assert!(
            error.contains(&log.display().to_string()) || error.contains("完整日志"),
            "文案要给出完整日志路径：{error}"
        );
        let text = std::fs::read_to_string(&log).expect("日志应落盘");
        assert!(text.contains("Collecting torch"), "{text}");
        assert!(text.contains("_error_catcher"), "日志里保留完整 traceback：{text}");
        let _ = std::fs::remove_file(&log);
    }
}
