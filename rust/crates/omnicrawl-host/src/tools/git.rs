//! `git` 工具：结构化 git 操作，argv 直调不经 shell。
//!
//! 语义基准是 `omnicrawl/agent/toolkit/git_tools.py`：模型参数逐项进 argv（消除
//! shell 转义与注入）、拒绝 --git-dir/--work-tree/--no-verify 等逃逸参数、config
//! 只允许改本地配置、archive 输出只走 stdout、clone/init 目标必须在工作区内、
//! 输出按首尾有界化。风险分级（readonly/local/high）复用 `omnicrawl-controllers::approval`。

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use omnicrawl_controllers::approval::GIT_SUPPORTED_ACTIONS;
use omnicrawl_controllers::tool_args::read_required_string_list;
use omnicrawl_core::ToolResult;
use serde_json::{Map, Value};

use super::paths::WorkspacePaths;
use crate::process_control::{configure_process_group, kill_process_tree_by_pid};
use crate::sandbox::gate::IsolationGate;
// `Plan` 只有 Linux 分支会用到；在其它 Unix 目标上导入它就是未使用。
#[cfg(target_os = "linux")]
use crate::sandbox::gate::Plan;

pub const GIT_COMMAND_TIMEOUT_SECONDS: u64 = 360;
/// 等 git 退出的轮询间隔：与 `command.rs` 一致，20ms 对 git 这种秒级命令足够细。
const POLL_INTERVAL: Duration = Duration::from_millis(20);
pub const GIT_OUTPUT_HEAD_CHARS: usize = 4_000;
pub const GIT_OUTPUT_TAIL_CHARS: usize = 8_000;
pub const GIT_OUTPUT_MAX_CHARS: usize = GIT_OUTPUT_HEAD_CHARS + GIT_OUTPUT_TAIL_CHARS;

/// 逃逸类参数：会把操作目标指向工作区之外或绕过仓库校验。
const FORBIDDEN_ARGUMENT_TOKENS: [&str; 4] = [
    "--git-dir",
    "--work-tree",
    "--no-verify",
    "--no-commit-verify",
];
const FORBIDDEN_ARGUMENT_PREFIXES: [&str; 2] = ["--git-dir=", "--work-tree="];
/// config 作用域参数：--global/--system/--file 会写到工作区之外。
const CONFIG_FORBIDDEN_TOKENS: [&str; 3] = ["--global", "--system", "--file"];
const CONFIG_FORBIDDEN_PREFIXES: [&str; 1] = ["--file="];
/// archive 的 -o/--output 会把归档写到任意路径。
const ARCHIVE_FORBIDDEN_TOKENS: [&str; 2] = ["-o", "--output"];
const ARCHIVE_FORBIDDEN_PREFIXES: [&str; 1] = ["--output="];
/// clone/init 的目录位置参数必须位于工作区内。
const DIRECTORY_ACTIONS: [&str; 2] = ["clone", "init"];

pub fn git_tool(
    paths: &WorkspacePaths,
    arguments: &Map<String, Value>,
    gate: Option<&IsolationGate>,
) -> ToolResult {
    let action = arguments
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !GIT_SUPPORTED_ACTIONS.contains(&action.as_str()) {
        let shown = if action.is_empty() { "(空)" } else { &action };
        return error_result(format!("不支持的 git 子命令：{shown}。"));
    }

    let args = read_required_string_list(arguments, "args");
    let message = match arguments.get("message") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.trim().to_string(),
        Some(_) => return error_result("message 必须是字符串。".to_string()),
    };
    let file_paths = read_required_string_list(arguments, "paths");

    if let Some(token) = forbidden_token(&args) {
        return error_result(format!("git 工具不允许参数：{token}。"));
    }
    if action == "config"
        && (args
            .iter()
            .any(|arg| CONFIG_FORBIDDEN_TOKENS.contains(&arg.as_str()))
            || args.iter().any(|arg| {
                CONFIG_FORBIDDEN_PREFIXES
                    .iter()
                    .any(|prefix| arg.starts_with(prefix))
            }))
    {
        return error_result("git config 不允许修改全局/系统配置或指定 --file。".to_string());
    }
    if action == "archive"
        && (args
            .iter()
            .any(|arg| ARCHIVE_FORBIDDEN_TOKENS.contains(&arg.as_str()))
            || args.iter().any(|arg| {
                ARCHIVE_FORBIDDEN_PREFIXES
                    .iter()
                    .any(|prefix| arg.starts_with(prefix))
            }))
    {
        return error_result("git archive 不允许 -o/--output，输出只能走 stdout。".to_string());
    }
    if action == "commit" && message.is_empty() && !args.iter().any(|arg| arg == "--no-edit") {
        return error_result("commit 必须提供 message，或显式传入 --no-edit。".to_string());
    }

    if let Some(error) = validate_workspace_paths(paths.root(), &file_paths) {
        return error_result(error);
    }
    if DIRECTORY_ACTIONS.contains(&action.as_str()) {
        if let Some(error) = validate_directory_positional(paths.root(), &action, &args) {
            return error_result(error);
        }
    }

    // argv[0] 必须是 git 本身：Windows 下第一个参数会被当作可执行文件。
    let mut argv: Vec<String> = vec![
        "git".to_string(),
        "-c".to_string(),
        "color.ui=never".to_string(),
        "--no-pager".to_string(),
        action.clone(),
    ];
    argv.extend(args.iter().cloned());
    if !message.is_empty() && matches!(action.as_str(), "commit" | "tag") {
        argv.push("-m".to_string());
        argv.push(message);
    }
    if !file_paths.is_empty() {
        argv.push("--".to_string());
        argv.extend(file_paths.iter().cloned());
    }

    run_git(argv, action, paths.root(), gate)
}

/// 按沙盒计划执行一次 `git`。
///
/// 三条路径的差别只在**怎么起进程**；参数校验、输出组织、错误文案完全共享，
/// 否则沙盒内外会给出两种不同的失败文本，排查时找不到方向。
#[cfg(not(windows))]
fn run_git(
    argv: Vec<String>,
    action: String,
    root: &Path,
    gate: Option<&IsolationGate>,
) -> ToolResult {
    // Linux：`git` 是普通可执行文件，只需经 `sandbox-exec` 施加文件与网络规则。
    let plan = match gate.map(IsolationGate::plan) {
        None => None,
        Some(Ok(plan)) => Some(plan),
        Some(Err(error)) => {
            return error_result(format!("沙盒无法启用，git 未执行：{error}"));
        }
    };
    #[cfg(target_os = "linux")]
    let (args, policy) = match &plan {
        Some(Plan::Landlock(isolation)) => (
            isolation.wrap(&argv),
            Some(isolation.policy_text().to_string()),
        ),
        _ => (argv, None),
    };
    #[cfg(not(target_os = "linux"))]
    let (args, policy) = (argv, None::<String>);

    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .current_dir(root)
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        // 不向终端交互：需要凭据时直接失败，需要编辑器时用 true 直接成功。
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true");
    // 沙盒内必须清掉宿主代理变量并改指本机代理，否则 `git clone https://…`
    // 会直奔用户代理，`[sandbox.net]` 的判定被整段跳过。
    #[cfg(target_os = "linux")]
    if let Some(Plan::Landlock(isolation)) = &plan {
        isolation.apply_environment(&mut command);
    }
    // 沙盒 launcher 从 stdin 读策略：必须给它管道，写完立刻关。
    // 两个输出管道**始终**要接管：下面用独立的读线程收输出，不接管就什么都读不到。
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if policy.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    // 自成进程组：超时按组回收时，git 拉起的钩子/凭据助手一并终止。
    configure_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return error_result("未找到 git 可执行文件，请确认 Git 已安装。".to_string());
        }
        Err(error) => return error_result(format!("无法启动 git：{error}。")),
    };
    if let Some(text) = policy {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(text.as_bytes());
            let _ = stdin.flush();
        }
    }
    // `wait_with_output` 没有上限：`git push` 卡在网络上会一直占着这条工具调用。
    // 超时后整组终止（`configure_process_group` 让 git 自成一组，后代一并回收）。
    //
    // 读管道必须先起线程再等退出：管道缓冲区满时 git 会阻塞在 write 上，
    // 先等退出就会死锁。这也是 `command.rs` 的做法。
    let stdout = child.stdout.take().map(spawn_output_reader);
    let stderr = child.stderr.take().map(spawn_output_reader);
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(GIT_COMMAND_TIMEOUT_SECONDS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return error_result(format!("等待 git 结束失败：{error}。")),
        }
        if Instant::now() >= deadline {
            kill_process_tree_by_pid(pid);
            return error_result(format!(
                "git {action} 超过 {GIT_COMMAND_TIMEOUT_SECONDS} 秒，已终止。"
            ));
        }
        thread::sleep(POLL_INTERVAL);
    };
    let stdout_text = stdout.map(join_output_reader).unwrap_or_default();
    let stderr_text = stderr.map(join_output_reader).unwrap_or_default();
    finish_git(status, &stdout_text, &stderr_text, &action)
}

fn spawn_output_reader<R: std::io::Read + Send + 'static>(
    mut stream: R,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stream, &mut buffer);
        buffer
    })
}

fn join_output_reader(handle: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

/// Windows：`git` 是 MSYS 程序，在 AppContainer 内起不来，因此**明确拒绝**而非悄悄放行。
///
/// 依据是 `sandbox_windows_design.md` §18 的实测：`git --version` 能过，但任何真正干活的
/// 子命令都因打不开 `/dev/null`（Windows 的 `NUL`）而失败，`CreateFileW` 返回
/// `ERROR_ACCESS_DENIED`。这**不是配置问题**，授权目录解决不了。
///
/// 关键：必须拒绝，不能"沙盒模式下退回宿主身份执行"——那等于让最该被约束的操作
/// （`push`、`config`、`submodule`）完全不受约束，比"跑不了"危险得多。
#[cfg(windows)]
fn run_git(
    argv: Vec<String>,
    action: String,
    root: &Path,
    gate: Option<&IsolationGate>,
) -> ToolResult {
    let isolated = match gate.map(IsolationGate::plan) {
        None => false,
        Some(Ok(plan)) => plan.is_isolated(),
        Some(Err(error)) => {
            return error_result(format!("沙盒无法启用，git 未执行：{error}"));
        }
    };
    if isolated {
        return error_result(GIT_UNAVAILABLE_IN_SANDBOX.to_string());
    }
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(root)
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true");
    // 与 Linux 路径同一套超时语义：`git push` 卡住不能无限占着工具调用。
    // 必须显式接管两个输出管道，否则 `child.stdout` 为 `None`，读线程拿不到任何内容。
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    configure_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return error_result("未找到 git 可执行文件，请确认 Git 已安装。".to_string());
        }
        Err(error) => return error_result(format!("无法启动 git：{error}。")),
    };
    let stdout = child.stdout.take().map(spawn_output_reader);
    let stderr = child.stderr.take().map(spawn_output_reader);
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(GIT_COMMAND_TIMEOUT_SECONDS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return error_result(format!("等待 git 结束失败：{error}。")),
        }
        if Instant::now() >= deadline {
            kill_process_tree_by_pid(pid);
            return error_result(format!(
                "git {action} 超过 {GIT_COMMAND_TIMEOUT_SECONDS} 秒，已终止。"
            ));
        }
        thread::sleep(POLL_INTERVAL);
    };
    let stdout_bytes = stdout.map(join_output_reader).unwrap_or_default();
    let stderr_bytes = stderr.map(join_output_reader).unwrap_or_default();
    finish_git(status, &stdout_bytes, &stderr_bytes, &action)
}

/// 共享的退出码与输出组织：沙盒内外文案必须一致。
fn finish_git(
    status: std::process::ExitStatus,
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
    action: &str,
) -> ToolResult {
    let stdout = String::from_utf8_lossy(stdout_bytes).to_string();
    let stderr = String::from_utf8_lossy(stderr_bytes).to_string();
    if !status.success() {
        let detail = stderr.trim();
        let text = if detail.is_empty() {
            let code = status.code().unwrap_or(-1);
            format!("git {action} 失败（退出码 {code}）。")
        } else {
            detail.to_string()
        };
        return ToolResult {
            ok: false,
            full_output: bound_text(&text),
            output: bound_text(&text),
            error_code: None,
            retryable: false,
        };
    }
    ToolResult {
        ok: true,
        full_output: bound_text(&stdout),
        output: bound_text(&stdout),
        error_code: None,
        retryable: false,
    }
}

/// Windows 沙盒内 `git` 不可用的说明与替代做法。
///
/// 与 `registry.rs` 的 `BASH_UNAVAILABLE_IN_SANDBOX` 同源（都是 `NUL` 设备导致的），
/// 但 `git` 有自己的替代路径：用 `bash` 工具跑系统 `git` 也不可行（同因），因此给出的是
/// 「关掉沙盒或改用文件工具」这两条真实可走的路。
#[cfg(windows)]
const GIT_UNAVAILABLE_IN_SANDBOX: &str =
    "沙盒真隔离下无法执行 `git`：它是 MSYS 程序，启动时要打开 `/dev/null`（Windows 的 `NUL` 设备），\
     而 AppContainer 身份对该设备没有访问权，任何真正干活的子命令都会失败。\
     这不是配置问题，无法通过授权目录解决。可选做法：把 `[sandbox] mode` 设为 off 后重试；\
     或改用 `read`/`write_file`/`Edit_file`/`grep`/`find` 完成同样的改动（这些工具仍在沙盒判定之下）。";

fn error_result(message: String) -> ToolResult {
    ToolResult {
        ok: false,
        full_output: message.clone(),
        output: message,
        error_code: None,
        retryable: false,
    }
}

fn forbidden_token(args: &[String]) -> Option<String> {
    args.iter()
        .find(|arg| {
            FORBIDDEN_ARGUMENT_TOKENS.contains(&arg.as_str())
                || FORBIDDEN_ARGUMENT_PREFIXES
                    .iter()
                    .any(|prefix| arg.starts_with(prefix))
        })
        .cloned()
}

fn validate_workspace_paths(root: &Path, file_paths: &[String]) -> Option<String> {
    for path in file_paths {
        let candidate = super::paths::resolve_lenient(&root.join(path));
        if !candidate.starts_with(root) {
            return Some(format!("路径越界（必须在工作区内）：{path}。"));
        }
    }
    None
}

fn validate_directory_positional(root: &Path, action: &str, args: &[String]) -> Option<String> {
    let positionals: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    let directory = positionals.last()?;
    let candidate = super::paths::resolve_lenient(&root.join(directory.as_str()));
    if !candidate.starts_with(root) {
        return Some(format!(
            "{action} 目标目录越界（必须在工作区内）：{directory}。"
        ));
    }
    None
}

/// 有界输出：超出上限时保留首尾字符并提示截断（字符数口径与 Python 一致）。
pub fn bound_text(text: &str) -> String {
    if text.chars().count() <= GIT_OUTPUT_MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(GIT_OUTPUT_HEAD_CHARS).collect();
    let total = text.chars().count();
    let tail: String = text
        .chars()
        .skip(total.saturating_sub(GIT_OUTPUT_TAIL_CHARS))
        .collect();
    format!("{head}\n…（git 输出已截断，共 {total} 字符）\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace(name: &str) -> (WorkspacePaths, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-git-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建目录");
        (WorkspacePaths::new(&root), root)
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn validation_errors_match_python_texts() {
        let (paths, _root) = workspace("validate");
        let unsupported = git_tool(&paths, &args(json!({"action": "push"})), None);
        // push 属于高风险但受支持；这里用真正不支持的动作。
        assert!(unsupported.ok || !unsupported.output.contains("不支持的 git 子命令"));

        let bad_action = git_tool(&paths, &args(json!({"action": "frobnicate"})), None);
        assert_eq!(bad_action.output, "不支持的 git 子命令：frobnicate。");

        let escape = git_tool(
            &paths,
            &args(json!({"action": "status", "args": ["--git-dir=/tmp/x"]})),
            None,
        );
        assert_eq!(escape.output, "git 工具不允许参数：--git-dir=/tmp/x。");

        let config = git_tool(
            &paths,
            &args(json!({"action": "config", "args": ["--global", "user.name", "x"]})),
            None,
        );
        assert_eq!(
            config.output,
            "git config 不允许修改全局/系统配置或指定 --file。"
        );

        let archive = git_tool(
            &paths,
            &args(json!({"action": "archive", "args": ["-o", "out.zip"]})),
            None,
        );
        assert_eq!(
            archive.output,
            "git archive 不允许 -o/--output，输出只能走 stdout。"
        );

        let commit = git_tool(&paths, &args(json!({"action": "commit"})), None);
        assert_eq!(
            commit.output,
            "commit 必须提供 message，或显式传入 --no-edit。"
        );

        let message_type = git_tool(&paths, &args(json!({"action": "commit", "message": 5})), None);
        assert_eq!(message_type.output, "message 必须是字符串。");

        let outside = git_tool(
            &paths,
            &args(json!({"action": "add", "paths": ["../outside.txt"]})),
            None,
        );
        assert_eq!(
            outside.output,
            "路径越界（必须在工作区内）：../outside.txt。"
        );

        let clone_outside = git_tool(
            &paths,
            &args(json!({"action": "clone", "args": ["https://example.invalid/x", "../out"]})),
            None,
        );
        assert!(
            clone_outside
                .output
                .starts_with("clone 目标目录越界（必须在工作区内）："),
            "{}",
            clone_outside.output
        );
    }

    #[test]
    fn bound_text_keeps_head_and_tail() {
        let text: String = "字".repeat(GIT_OUTPUT_MAX_CHARS + 100);
        let bounded = bound_text(&text);
        assert!(bounded.contains("（git 输出已截断，共 "), "{bounded}");
        assert!(bounded.chars().count() < GIT_OUTPUT_MAX_CHARS + 100);
        assert_eq!(bound_text("短文本"), "短文本");
    }

    #[test]
    fn readonly_status_runs_in_a_real_repository() {
        let (paths, root) = workspace("status");
        // 需要真 git：先 init，再跑只读 status。
        let init = git_tool(&paths, &args(json!({"action": "init"})), None);
        if !init.ok && init.output.contains("未找到 git 可执行文件") {
            eprintln!("跳过：本机没有 git");
            return;
        }
        assert!(init.ok, "{}", init.output);
        let status = git_tool(&paths, &args(json!({"action": "status"})), None);
        assert!(status.ok, "{}", status.output);
        assert!(
            status.output.contains("On branch") || status.output.contains("No commits"),
            "{}",
            status.output
        );
        assert!(root.join(".git").is_dir());
    }

    /// 沙盒关闭时 git 照常执行：先校验参数、再跑子命令，两层都不能被沙盒改动。
    #[test]
    fn sandbox_off_keeps_git_working() {
        let (paths, _root) = workspace("sandbox-off");
        let sandbox = std::sync::Arc::new(crate::sandbox::Sandbox::disabled(
            paths.root().to_path_buf(),
            paths.root().join(".omnicrawl/.agent_tmp"),
        ));
        let gate = IsolationGate::new(sandbox);
        let init = git_tool(&paths, &args(json!({"action": "init"})), Some(&gate));
        if !init.ok && init.output.contains("未找到 git 可执行文件") {
            eprintln!("跳过：本机没有 git");
            return;
        }
        assert!(init.ok, "{}", init.output);
        // 参数校验必须在沙盒判定之前照旧生效。
        let escape = git_tool(
            &paths,
            &args(json!({"action": "status", "args": ["--git-dir=/tmp/x"]})),
            Some(&gate),
        );
        assert_eq!(escape.output, "git 工具不允许参数：--git-dir=/tmp/x。");
    }

    /// Windows 沙盒内 `git` 必须**明确拒绝**，绝不能让它在宿主身份下悄悄跑起来。
    ///
    /// 这是本工具最重要的一条安全断言：`git push`/`config`/`submodule` 是最需要约束的
    /// 操作，一旦"沙盒模式下退回宿主"就完全失守。
    #[cfg(windows)]
    #[test]
    fn windows_sandbox_refuses_git_instead_of_falling_back() {
        let (paths, _root) = workspace("sandbox-windows");
        let mut config = omnicrawl_config::features::sandbox::SandboxConfig::default();
        config.enabled = true;
        config.mode = omnicrawl_config::features::sandbox::SandboxMode::AppContainer;
        let sandbox = std::sync::Arc::new(crate::sandbox::Sandbox::new(
            config,
            paths.root().to_path_buf(),
            paths.root().join(".omnicrawl/.agent_tmp"),
        ));
        let gate = IsolationGate::new(sandbox);
        // 容器真实建不起来时也要给出"沙盒无法启用"而不是退化成宿主执行。
        let result = git_tool(&paths, &args(json!({"action": "status"})), Some(&gate));
        assert!(!result.ok, "{}", result.output);
        assert!(
            result.output.contains("沙盒"),
            "拒绝理由必须提到沙盒：{}",
            result.output
        );
    }
}
