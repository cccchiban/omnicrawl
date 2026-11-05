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
use crate::process_control::{
    configure_process_group, kill_process_tree_by_pid, terminate_process_tree, KillOnCloseJob,
};
use crate::sandbox::gate::{IsolationGate, Plan};
use crate::sandbox::Sandbox;

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

    /// 新回合开始：清掉上一回合的取消标记，让本回合的命令能真正执行。
    ///
    /// 令牌随工具表长期存活（registry 重建与工作区切换都刻意沿用同一份，见
    /// `RegistryOptions::cancel`），所以「取消一个回合」不能把令牌永久置位——否则用户
    /// 按 Esc 之后继续对话，每个 `bash` 都会在生成进程的瞬间被判定为已取消。
    ///
    /// 只清标记、**不动已登记的子进程**：正在收尾的旧回合进程由各自的调用路径注销，
    /// `cancel` 之外还有 kill-on-close Job 兜底（宿主退出时由操作系统递归回收）；在这里
    /// 清空列表反而会让那些进程从账上消失，之后再也回收不掉。
    pub fn reset(&self) {
        self.inner.cancelled.store(false, Ordering::SeqCst);
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

/// 命令执行器：工作区根、默认超时与沙盒策略之外无状态。
pub struct CommandRunner {
    root: PathBuf,
    timeout_seconds: i64,
    gate: IsolationGate,
}

impl CommandRunner {
    pub fn new(root: impl Into<PathBuf>, timeout_seconds: i64) -> Self {
        let root = root.into();
        // 没有注入策略时按关闭处理：`CommandRunner::new` 的调用方（测试、`monitor`）
        // 不该因为沙盒而改变行为。
        let sandbox = Arc::new(Sandbox::disabled(
            root.clone(),
            root.join(AGENT_TEMP_DIRECTORY),
        ));
        Self::with_sandbox(root, timeout_seconds, sandbox)
    }

    /// 带上沙盒策略构造。`mode = appcontainer` 时命令以 AppContainer 身份执行。
    pub fn with_sandbox(
        root: impl Into<PathBuf>,
        timeout_seconds: i64,
        sandbox: Arc<Sandbox>,
    ) -> Self {
        Self {
            root: root.into(),
            timeout_seconds: timeout_seconds.clamp(1, MAX_COMMAND_TIMEOUT_SECONDS),
            gate: IsolationGate::new(sandbox),
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

        let primary = self.run_invocation(&command, shell, timeout, cancel)?;
        if diagnostic_command.is_empty() {
            return Ok(primary);
        }
        let diagnostic = self.run_invocation(&diagnostic_command, shell, timeout, cancel)?;
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
        text: &str,
        shell: Shell,
        timeout_seconds: i64,
        cancel: &CancelToken,
    ) -> Result<CommandOutcome, ToolError> {
        let plan = self.gate.plan().map_err(|error| {
            // 沙盒准备失败时**不放行到宿主身份**：能跑但没隔离，比跑不了危险得多。
            ToolError::coded(
                format!("沙盒无法启用，命令未执行：{error}"),
                "SANDBOX_UNAVAILABLE",
                false,
            )
        })?;
        let invocation = match &plan {
            Plan::Isolated(isolation) => {
                invocation_for_isolation(text, shell, &|name| std::env::var(name).ok(), isolation)
            }
            Plan::Host => invocation_for(text, shell, &|name| std::env::var(name).ok(), false),
        }?;
        if let Plan::Isolated(isolation) = plan {
            return self.run_isolated(&invocation, timeout_seconds, cancel, &isolation);
        }
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
        // Unix 下让命令自成进程组：取消 / 超时按组回收时，`bash -c '… & …'`
        // 拉起的后代也一起被终止，不止直接子进程。
        configure_process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::new(format!("命令执行失败：{error}")))?;

        // 命令树纳入 kill-on-close Job：宿主退出（含崩溃）时由操作系统递归回收，
        // 不依赖按 PID 重新查找，也不受受限桌面会话下的 `taskkill` 权限限制。
        let mut job = KillOnCloseJob::assign(&child);
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
                kill_process_tree(&mut child, job.take());
                cancelled = true;
                break None;
            }
            if Instant::now() >= deadline {
                kill_process_tree(&mut child, job.take());
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
        let return_code = status
            .and_then(|status| status.code())
            .map(i64::from)
            .unwrap_or(-1);
        Ok(self.finish(&invocation, return_code, stdout_text, stderr_text, false))
    }

    /// 沙盒路径：以 AppContainer 身份执行同一条命令。
    ///
    /// 与宿主路径刻意保持同一套「退出码 + 解释器 + stdout/stderr」组织方式，
    /// 只在开头多一行 `[sandbox:appcontainer]` 标记，让人一眼看出这条命令在隔离内跑。
    fn run_isolated(
        &self,
        invocation: &Invocation,
        timeout_seconds: i64,
        cancel: &CancelToken,
        isolation: &crate::sandbox::Isolation,
    ) -> Result<CommandOutcome, ToolError> {
        let mut spawned = isolation.spawn(&invocation.args).map_err(|error| {
            ToolError::coded(
                format!("沙盒内命令启动失败：{error}"),
                "SANDBOX_SPAWN_FAILED",
                false,
            )
        })?;
        let pid = spawned.child.id();
        cancel.register(pid);

        let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(1) as u64);
        let mut cancelled = false;
        let exit_code = loop {
            if let Some(code) = spawned.child.try_wait() {
                break Some(code);
            }
            if cancel.is_cancelled() {
                spawned.child.kill();
                cancelled = true;
                break None;
            }
            if Instant::now() >= deadline {
                spawned.child.kill();
                cancel.unregister(pid);
                return Err(ToolError::new(format!(
                    "命令执行超过 {timeout_seconds} 秒，已终止。"
                )));
            }
            thread::sleep(POLL_INTERVAL);
        };
        cancel.unregister(pid);

        let stdout_bytes = join_bytes_reader(spawned.stdout);
        let stderr_bytes = join_bytes_reader(spawned.stderr);
        if cancelled {
            return Ok(CommandOutcome {
                ok: false,
                output: "命令已取消。".to_string(),
            });
        }
        let return_code = exit_code.map(i64::from).unwrap_or(-1);
        Ok(self.finish(
            invocation,
            return_code,
            decode_console_bytes(&stdout_bytes),
            decode_console_bytes(&stderr_bytes),
            true,
        ))
    }

    /// 两条执行路径共用的结果组织。
    fn finish(
        &self,
        invocation: &Invocation,
        return_code: i64,
        stdout_text: String,
        stderr_text: String,
        isolated: bool,
    ) -> CommandOutcome {
        let mut parts = vec![format!("退出码：{return_code}")];
        if isolated {
            parts.push("[sandbox:appcontainer] 本命令以隔离身份执行".to_string());
        }
        parts.push(format!("Shell：{}", invocation.label));
        if !stdout_text.trim().is_empty() {
            parts.push(format!(
                "stdout:
{}",
                self.sampled(stdout_text.trim().to_string())
            ));
        }
        if !stderr_text.trim().is_empty() {
            parts.push(format!(
                "stderr:
{}",
                self.sampled(stderr_text.trim().to_string())
            ));
        }
        CommandOutcome {
            ok: return_code == 0,
            output: parts.join(
                "

",
            ),
        }
    }

    fn sampled(&self, text: String) -> String {
        let save_path = command_output_path(&self.root, AGENT_TEMP_DIRECTORY);
        sample_command_output(&text, Some(&save_path))
    }
}

/// 解沙盒子进程的输出字节。
///
/// 宿主路径继承宿主自己的控制台，输出本来就是 UTF-8；沙盒进程的 stdout 是管道，
/// 程序按**控制台代码页**（中文系统上是 936/GBK）写字节。因此先按 UTF-8 严格解，
/// 失败再按 GBK——顺序不能反：GBK 也能"解开"UTF-8 的中文，但会得到乱码。
fn decode_console_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let (decoded, _, _) = encoding_rs::GBK.decode(bytes);
            decoded.to_string()
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(mut stream: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stream.read_to_end(&mut buffer);
        String::from_utf8_lossy(&buffer).to_string()
    })
}

fn join_bytes_reader(handle: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
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
    invocation_for(command, shell, env, false)
}

/// 与 [`invocation_with`] 相同，但可以按「是否在沙盒内」选择解释器。
pub fn invocation_for(
    command: &str,
    shell: Shell,
    env: &dyn Fn(&str) -> Option<String>,
    isolated: bool,
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
            let executable = if isolated {
                find_sandboxed_powershell(env)
            } else {
                find_powershell_executable(env)
            }
            .ok_or_else(|| ToolError::new("未找到 PowerShell 可执行文件。"))?;
            Ok(powershell_invocation(&executable, command, isolated))
        }
    }
}

/// 沙盒内的调用：解释器由容器实测选出（`pwsh` 7.* 优先，不通则回退 5.1）。
///
/// 与 [`invocation_for`] 的差别只在「谁选了可执行文件」：这里的选择需要容器身份才能验证，
/// 因此由 [`crate::sandbox::Isolation`] 在首次使用时探测并缓存。
pub fn invocation_for_isolation(
    command: &str,
    shell: Shell,
    env: &dyn Fn(&str) -> Option<String>,
    isolation: &crate::sandbox::Isolation,
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
            let executable = isolation
                .powershell()
                .ok_or_else(|| ToolError::new("沙盒内未找到可用的 PowerShell。"))?;
            Ok(powershell_invocation(&executable, command, true))
        }
    }
}

/// 组装一次 PowerShell 调用：UTF-8 前缀、沙盒内的代理改写、退出码透传。
fn powershell_invocation(executable: &Path, command: &str, isolated: bool) -> Invocation {
    let utf8_prefix = "$__OmniCrawlUtf8 = [System.Text.UTF8Encoding]::new($false); \
                       [Console]::InputEncoding = $__OmniCrawlUtf8; \
                       [Console]::OutputEncoding = $__OmniCrawlUtf8; \
                       $OutputEncoding = $__OmniCrawlUtf8; ";
    // 沙盒内把 .NET 的默认代理指向 `HTTP_PROXY`。
    //
    // 这是一个真实的绕过口，本设置只能**收窄**不能关闭：.NET 的
    // `HttpWebRequest`/`WebClient` 默认读**用户级 WinINET 设置**
    // （`HKCU\...\Internet Settings\ProxyServer`），那可能是用户自己的代理
    // （本机实测是 `127.0.0.1:10808`）。而我们为了代理可达给容器加了回环豁免，
    // 该豁免按容器粒度、无法限定端口，于是用户代理也随之可达。
    //
    // `DefaultWebProxy` 覆盖走到它的客户端（`HttpClient`/`WebClient`）；但 Windows PowerShell
    // 5.1 的 `Invoke-WebRequest` 取的是 `GetSystemWebProxy()`（读 WinINET），实测仍指向
    // 用户代理——`allowlist` 下 `example.com` 就是这样漏出去的（连续 3 轮复现）。
    // 容器内写不了注册表，宿主侧改 `HKCU` 会动用户自己的浏览器设置，因此这一层
    // 只能做到「应用层代理级」，详见设计文档 §20。
    //
    // 只在沙盒内设置：宿主侧的代理偏好是用户自己的事。
    let proxy_prefix = if isolated {
        "$__OmniCrawlProxy = $env:HTTP_PROXY; \
         if ($__OmniCrawlProxy) { \
             $__OmniCrawlWebProxy = New-Object System.Net.WebProxy($__OmniCrawlProxy); \
             [System.Net.WebRequest]::DefaultWebProxy = $__OmniCrawlWebProxy; \
             [System.Net.ServicePointManager]::SecurityProtocol = \
                 [System.Net.SecurityProtocolType]::Tls12 -bor \
                 [System.Net.SecurityProtocolType]::Tls11 -bor \
                 [System.Net.SecurityProtocolType]::Tls; \
         } else { [System.Net.WebRequest]::DefaultWebProxy = $null }; "
    } else {
        ""
    };
    let script = format!(
        "$ErrorActionPreference = 'Stop'; {utf8_prefix}{proxy_prefix}& {{\n{command}\n}}\n\
         $__OmniCrawlExitCode = $LASTEXITCODE; \
         if ($null -ne $__OmniCrawlExitCode) {{ exit $__OmniCrawlExitCode }}"
    );
    let mut args = vec![
        executable.to_string_lossy().to_string(),
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
    ];
    if isolated {
        // 只有沙盒内需要显式 Bypass：AppContainer 读不到执行策略注册表
        // （`Get-ExecutionPolicy` 报「不允许所请求的注册表访问权」），策略判定
        // 因此落到 Restricted，把 `npm.cmd` 这类包装脚本挡在 `AuthorizationManager`
        // 之前，报「AuthorizationManager 检查失败」。宿主侧不改执行策略——那是用户
        // 自己的安全设置，不该被工具悄悄放宽。
        args.push("-ExecutionPolicy".to_string());
        args.push("Bypass".to_string());
    }
    args.push("-Command".to_string());
    args.push(script);
    Invocation {
        args,
        label: Shell::PowerShell.label().to_string(),
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

/// 沙盒内的 PowerShell（无法在容器内实测时的回退口径）：安装目录里最新的 `pwsh`，
/// 没有则用 `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`。
///
/// 真正进沙盒执行时以 [`crate::sandbox::Isolation::powershell`] 的容器实测结果为准：
/// PowerShell 7 在 AppContainer 里能否加载 cmdlet 取决于 `PSModulePath` 是否含它自己的
/// 模块目录，只有跑一次才知道，因此探测放在隔离层。
pub fn find_sandboxed_powershell(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    crate::sandbox::runtime::powershell_candidates(env)
        .into_iter()
        .next()
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

/// 回收一整棵进程树：纳入了 Job 时关句柄让它递归终止，否则按 PID（Unix 下按进程组）回收。
///
/// 句柄在正常路径上由作用域结束时的 `Drop` 关闭，因此「命令留在后台的常驻进程」
/// 也会随本次调用结束时被清掉——与 Python `tool_process` 的 `finally` 语义一致。
fn kill_process_tree(child: &mut Child, job: Option<KillOnCloseJob>) {
    terminate_process_tree(child, job, true);
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

    /// 宿主侧不得改动用户的执行策略；只有沙盒内才补 `Bypass`。
    ///
    /// 两者分开不是为了形式：`-ExecutionPolicy Bypass` 等于绕过用户自己的安全设置，
    /// 在宿主上这么做是越权。沙盒内则是唯一可行选择——AppContainer 读不到策略注册表，
    /// 不补 Bypass 就连 `npm.cmd` 都跑不起来。
    #[test]
    fn only_the_sandbox_adds_execution_policy_bypass() {
        let env = |name: &str| match name {
            "PATH" => std::env::var("PATH").ok(),
            "SystemRoot" => std::env::var("SystemRoot").ok(),
            _ => None,
        };
        let hosted = invocation_with("Get-Location", Shell::PowerShell, &env).expect("宿主侧可用");
        assert!(
            !hosted.args.iter().any(|arg| arg == "-ExecutionPolicy"),
            "宿主侧不得改执行策略：{:?}",
            hosted.args
        );
        let isolated =
            invocation_for("Get-Location", Shell::PowerShell, &env, true).expect("沙盒侧可用");
        let policy = isolated
            .args
            .iter()
            .position(|arg| arg == "-ExecutionPolicy")
            .expect("沙盒内必须补 -ExecutionPolicy");
        assert_eq!(isolated.args[policy + 1], "Bypass");
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

    #[test]
    fn reset_clears_the_cancel_mark_for_the_next_turn() {
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(cancel.is_cancelled());
        cancel.reset();
        assert!(
            !cancel.is_cancelled(),
            "新回合必须能继续执行命令（Esc 后继续对话的回归）"
        );
    }

    #[test]
    fn a_turn_after_a_cancelled_one_really_runs() {
        // 端到端最小回归：取消 → 复位 → 同一条命令必须真的跑起来并拿到正常退出码。
        let env = |name: &str| std::env::var(name).ok();
        if find_bash_executable(&env).is_none() {
            // 没装 Git Bash 的机器上只能验到标记层；跨平台 CI 上必须真跑。
            return;
        }
        let root = std::env::temp_dir().join("omnicrawl-tui-command-cancel-reset");
        let _ = std::fs::create_dir_all(&root);
        let runner = CommandRunner::new(&root, DEFAULT_COMMAND_TIMEOUT_SECONDS);
        let cancel = CancelToken::new();
        cancel.cancel();
        let cancelled = runner
            .run_shell(&args(json!({"command": "echo hi"})), Shell::Bash, &cancel)
            .expect("取消必须回结果而不是错误");
        assert!(!cancelled.ok);
        assert_eq!(cancelled.output, "命令已取消。");

        cancel.reset();
        let resumed = runner
            .run_shell(&args(json!({"command": "echo hi"})), Shell::Bash, &cancel)
            .expect("复位后必须能执行");
        assert!(resumed.ok, "{}", resumed.output);
        assert!(resumed.output.contains("退出码：0"), "{}", resumed.output);
    }
}
