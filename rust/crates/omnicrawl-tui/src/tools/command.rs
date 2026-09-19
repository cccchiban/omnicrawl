//! `bash` / `powershell` 工具：显式解释器执行、超时回收与输出采样。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `run_shell_command` /
//! `command_invocation` / `_run_command_invocation`：命令必须通过明确的 Bash 或
//! PowerShell 可执行文件启动（不回退默认 Shell），Windows 上 Bash 优先 Git Bash；
//! 输出按「退出码 + 解释器 + stdout/stderr」组织，单段超长时头尾采样并把完整输出
//! 落到 `.omnicrawl/.agent_tmp/files/`。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::arguments::limited_int;
use super::error::{ToolError, ToolOutcome};
use super::sample::{command_output_path, sample_command_output};

pub const DEFAULT_COMMAND_TIMEOUT_SECONDS: i64 = 360;
pub const MAX_COMMAND_TIMEOUT_SECONDS: i64 = 360;
/// 与 Python `workspace/temp.py` 的默认 Agent 临时目录一致。
pub const AGENT_TEMP_DIRECTORY: &str = ".omnicrawl/.agent_tmp";
/// 未识别参数时的提示前缀（与 Python 同文案）。
const UNSUPPORTED_KEYS_PREFIX: &str = "显式命令工具不支持参数：";
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    PowerShell,
}

impl Shell {
    fn label(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::PowerShell => "PowerShell",
        }
    }
}

/// 已解析的命令执行方式：可执行文件与参数，外加展示用标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub args: Vec<String>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub ok: bool,
    pub output: String,
}

/// 取消令牌：置位后终止已登记的进程树，用于 `Esc` 取消回合。
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelState>,
}

#[derive(Debug, Default)]
struct CancelState {
    cancelled: AtomicBool,
    children: Mutex<Vec<u32>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// 置位并立即回收正在运行的子进程树。
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let children: Vec<u32> = self
            .inner
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for pid in children {
            kill_process_tree_by_pid(pid);
        }
    }

    fn register(&self, pid: u32) {
        self.inner
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(pid);
    }

    fn unregister(&self, pid: u32) {
        let mut children = self
            .inner
            .children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        children.retain(|candidate| *candidate != pid);
    }
}

/// 命令执行器：工作区根、默认超时与取消令牌之外无状态。
pub struct CommandRunner {
    root: PathBuf,
    timeout_seconds: i64,
}

impl CommandRunner {
    pub fn new(root: impl Into<PathBuf>, timeout_seconds: i64) -> Self {
        Self {
            root: root.into(),
            timeout_seconds: timeout_seconds.clamp(1, MAX_COMMAND_TIMEOUT_SECONDS),
        }
    }

    /// 执行 `bash` / `powershell` 工具的一次调用。
    pub fn run_shell(
        &self,
        arguments: &Map<String, Value>,
        shell: Shell,
        cancel: &CancelToken,
    ) -> Result<CommandOutcome, ToolError> {
        let unsupported: Vec<String> = arguments
            .keys()
            .filter(|key| {
                !matches!(
                    key.as_str(),
                    "command" | "diagnostic_command" | "timeout_seconds"
                )
            })
            .cloned()
            .collect();
        if !unsupported.is_empty() {
            let mut names = unsupported;
            names.sort();
            return Err(ToolError::new(format!(
                "{UNSUPPORTED_KEYS_PREFIX}{}。",
                names.join("、")
            )));
        }

        let command = match arguments.get("command") {
            Some(Value::String(text)) => text.trim().to_string(),
            _ => String::new(),
        };
        if command.is_empty() {
            return Err(ToolError::new("command 不能为空。"));
        }
        let diagnostic_command = match arguments.get("diagnostic_command") {
            None => String::new(),
            Some(Value::String(text)) => text.trim().to_string(),
            Some(_) => {
                return Err(ToolError::new("diagnostic_command 必须是字符串。"));
            }
        };
        let timeout = limited_int(
            arguments,
            "timeout_seconds",
            self.timeout_seconds,
            1,
            MAX_COMMAND_TIMEOUT_SECONDS,
        );

        let primary = self.run_invocation(&invocation(&command, shell)?, timeout, cancel)?;
        if diagnostic_command.is_empty() {
            return Ok(primary);
        }
        let diagnostic =
            self.run_invocation(&invocation(&diagnostic_command, shell)?, timeout, cancel)?;
        Ok(CommandOutcome {
            ok: primary.ok && diagnostic.ok,
            output: format!(
                "主命令结果：\n{}\n\n诊断命令结果：\n{}",
                primary.output, diagnostic.output
            ),
        })
    }

    fn run_invocation(
        &self,
        invocation: &Invocation,
        timeout_seconds: i64,
        cancel: &CancelToken,
    ) -> Result<CommandOutcome, ToolError> {
        let mut command = Command::new(&invocation.args[0]);
        command
            .args(&invocation.args[1..])
            .current_dir(&self.root)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::new(format!("命令执行失败：{error}")))?;

        let pid = child.id();
        cancel.register(pid);
        let stdout = child.stdout.take().map(spawn_reader);
        let stderr = child.stderr.take().map(spawn_reader);

        let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1) as u64);
        let mut cancelled = false;
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break Some(status);
            }
            if cancel.is_cancelled() {
                kill_process_tree(&mut child);
                cancelled = true;
                break None;
            }
            if Instant::now() >= deadline {
                kill_process_tree(&mut child);
                cancel.unregister(pid);
                return Err(ToolError::new(format!(
                    "命令执行超过 {timeout_seconds} 秒，已终止。"
                )));
            }
            thread::sleep(POLL_INTERVAL);
        };
        cancel.unregister(pid);

        let stdout_text = stdout.map(join_reader).unwrap_or_default();
        let stderr_text = stderr.map(join_reader).unwrap_or_default();
        if cancelled {
            return Ok(CommandOutcome {
                ok: false,
                output: "命令已取消。".to_string(),
            });
        }
        let return_code = status.and_then(|status| status.code()).unwrap_or(-1);

        let mut parts = vec![
            format!("退出码：{return_code}"),
            format!("Shell：{}", invocation.label),
        ];
        if !stdout_text.trim().is_empty() {
            parts.push(format!(
                "stdout:\n{}",
                self.sampled(stdout_text.trim().to_string())
            ));
        }
        if !stderr_text.trim().is_empty() {
            parts.push(format!(
                "stderr:\n{}",
                self.sampled(stderr_text.trim().to_string())
            ));
        }
        Ok(CommandOutcome {
            ok: return_code == 0,
            output: parts.join("\n\n"),
        })
    }

    fn sampled(&self, text: String) -> String {
        let save_path = command_output_path(&self.root, AGENT_TEMP_DIRECTORY);
        sample_command_output(&text, Some(&save_path))
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut stream: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stream.read_to_end(&mut buffer);
        String::from_utf8_lossy(&buffer).to_string()
    })
}

fn join_reader(handle: thread::JoinHandle<String>) -> String {
    handle.join().unwrap_or_default()
}

/// 把工具要求的 Shell 转成可执行调用（与 Python `command_invocation` 同形）。
pub fn invocation(command: &str, shell: Shell) -> Result<Invocation, ToolError> {
    invocation_with(command, shell, &|name| std::env::var(name).ok())
}

pub fn invocation_with(
    command: &str,
    shell: Shell,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Invocation, ToolError> {
    match shell {
        Shell::Bash => {
            let executable = find_bash_executable(env).ok_or_else(|| {
                ToolError::new(
                    "未找到可用的 Git Bash。请安装 Git for Windows，或设置 PATH 后重试。",
                )
            })?;
            Ok(Invocation {
                args: vec![
                    executable.to_string_lossy().to_string(),
                    "-o".to_string(),
                    "pipefail".to_string(),
                    "-lc".to_string(),
                    command.to_string(),
                ],
                label: Shell::Bash.label().to_string(),
            })
        }
        Shell::PowerShell => {
            let executable = find_powershell_executable(env)
                .ok_or_else(|| ToolError::new("未找到 PowerShell 可执行文件。"))?;
            let utf8_prefix = "$__OmniCrawlUtf8 = [System.Text.UTF8Encoding]::new($false); \
                               [Console]::InputEncoding = $__OmniCrawlUtf8; \
                               [Console]::OutputEncoding = $__OmniCrawlUtf8; \
                               $OutputEncoding = $__OmniCrawlUtf8; ";
            let script = format!(
                "$ErrorActionPreference = 'Stop'; {utf8_prefix}& {{\n{command}\n}}\n\
                 $__OmniCrawlExitCode = $LASTEXITCODE; \
                 if ($null -ne $__OmniCrawlExitCode) {{ exit $__OmniCrawlExitCode }}"
            );
            Ok(Invocation {
                args: vec![
                    executable.to_string_lossy().to_string(),
                    "-NoLogo".to_string(),
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    script,
                ],
                label: Shell::PowerShell.label().to_string(),
            })
        }
    }
}

/// Bash：优先 Git 自带的 bash（Windows 上的 `bash.exe` 可能只是 WSL 启动器）。
pub fn find_bash_executable(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(git) = which("git", env) {
        if let Some(git_root) = git.parent().and_then(Path::parent) {
            candidates.push(git_root.join("bin").join("bash.exe"));
            candidates.push(git_root.join("usr").join("bin").join("bash.exe"));
        }
    }
    if cfg!(windows) {
        for key in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(root) = env(key).filter(|value| !value.trim().is_empty()) {
                let root = PathBuf::from(root);
                candidates.push(root.join("Git").join("bin").join("bash.exe"));
                candidates.push(root.join("Git").join("usr").join("bin").join("bash.exe"));
            }
        }
    } else if let Some(bash) = which("bash", env) {
        candidates.push(bash);
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// PowerShell 7（pwsh）优先，其次 Windows PowerShell。
pub fn find_powershell_executable(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    ["pwsh", "powershell"]
        .into_iter()
        .find_map(|name| which(name, env))
}

/// `shutil.which` 的最小实现：按 PATH 找候选，Windows 上补常见可执行后缀。
fn which(program: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let path = env("PATH")?;
    let suffixes: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for directory in std::env::split_paths(&path) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        for suffix in suffixes {
            let candidate = directory.join(format!("{program}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 回收整棵进程树：Windows 用 `taskkill /T`，其它平台先 TERM 再交由 `kill` 兜底。
fn kill_process_tree(child: &mut Child) {
    kill_process_tree_by_pid(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// 按 pid 回收整棵进程树（`monitor` 的停止路径复用同一条规则）。
pub(crate) fn kill_process_tree_by_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // Unix 侧只回收直接子进程：进程组回收需要 setsid/libc，留待后续补齐。
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 文本型工具适配：命令类工具自带 ok 语义。
pub fn command_outcome_result(outcome: CommandOutcome) -> omnicrawl_core::ToolResult {
    super::error::command_result(outcome.ok, outcome.output)
}

impl From<CommandOutcome> for ToolOutcome {
    fn from(outcome: CommandOutcome) -> Self {
        Ok(outcome.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn bash_invocation_uses_pipefail_and_login_shell() {
        let env = |name: &str| match name {
            "PATH" => std::env::var("PATH").ok(),
            _ => None,
        };
        match invocation_with("echo hi", Shell::Bash, &env) {
            Ok(invocation) => {
                assert_eq!(invocation.label, "Bash");
                assert_eq!(&invocation.args[1..], ["-o", "pipefail", "-lc", "echo hi"]);
            }
            Err(error) => {
                // 未装 Git Bash 的机器上必须是明确报错，而不是回退到别的 Shell。
                assert!(error.message.contains("Git Bash"), "{}", error.message);
            }
        }
    }

    #[test]
    fn powershell_invocation_wraps_command_with_utf8_prefix() {
        let env = |name: &str| match name {
            "PATH" => std::env::var("PATH").ok(),
            _ => None,
        };
        match invocation_with("Get-Location", Shell::PowerShell, &env) {
            Ok(invocation) => {
                assert_eq!(invocation.label, "PowerShell");
                assert_eq!(
                    &invocation.args[1..5],
                    ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]
                );
                let script = &invocation.args[5];
                assert!(script.starts_with("$ErrorActionPreference = 'Stop'; "));
                assert!(script.contains("[Console]::OutputEncoding = $__OmniCrawlUtf8;"));
                assert!(script.contains("& {\nGet-Location\n}"));
                assert!(script.ends_with(
                    "if ($null -ne $__OmniCrawlExitCode) { exit $__OmniCrawlExitCode }"
                ));
            }
            Err(error) => assert!(error.message.contains("PowerShell"), "{}", error.message),
        }
    }

    #[test]
    fn missing_shell_reports_python_texts() {
        let bash = invocation_with("echo hi", Shell::Bash, &no_env).unwrap_err();
        assert_eq!(
            bash.message,
            "未找到可用的 Git Bash。请安装 Git for Windows，或设置 PATH 后重试。"
        );
        let powershell = invocation_with("echo hi", Shell::PowerShell, &no_env).unwrap_err();
        assert_eq!(powershell.message, "未找到 PowerShell 可执行文件。");
    }

    #[test]
    fn argument_validation_matches_python() {
        let root = std::env::temp_dir().join("omnicrawl-tui-command-args");
        let runner = CommandRunner::new(&root, DEFAULT_COMMAND_TIMEOUT_SECONDS);
        let cancel = CancelToken::new();

        let empty = runner
            .run_shell(&args(json!({"command": "   "})), Shell::Bash, &cancel)
            .unwrap_err();
        assert_eq!(empty.message, "command 不能为空。");

        let extra = runner
            .run_shell(
                &args(json!({"command": "echo hi", "cwd": "."})),
                Shell::Bash,
                &cancel,
            )
            .unwrap_err();
        assert_eq!(extra.message, "显式命令工具不支持参数：cwd。");

        let diagnostic_type = runner
            .run_shell(
                &args(json!({"command": "echo hi", "diagnostic_command": 5})),
                Shell::Bash,
                &cancel,
            )
            .unwrap_err();
        assert_eq!(diagnostic_type.message, "diagnostic_command 必须是字符串。");
    }

    #[test]
    fn cancel_token_marks_and_reports() {
        let cancel = CancelToken::new();
        assert!(!cancel.is_cancelled());
        cancel.register(999_999);
        cancel.cancel();
        assert!(cancel.is_cancelled());
        cancel.unregister(999_999);
    }
}
