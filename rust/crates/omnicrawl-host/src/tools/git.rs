//! `git` 工具：结构化 git 操作，argv 直调不经 shell。
//!
//! 语义基准是 `omnicrawl/agent/toolkit/git_tools.py`：模型参数逐项进 argv（消除
//! shell 转义与注入）、拒绝 --git-dir/--work-tree/--no-verify 等逃逸参数、config
//! 只允许改本地配置、archive 输出只走 stdout、clone/init 目标必须在工作区内、
//! 输出按首尾有界化。风险分级（readonly/local/high）复用 `omnicrawl-controllers::approval`。

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use omnicrawl_controllers::approval::{GIT_SUPPORTED_ACTIONS, GIT_TOOL_NAME};
use omnicrawl_controllers::tool_args::read_required_string_list;
use omnicrawl_core::ToolResult;
use serde_json::{Map, Value};

use super::paths::WorkspacePaths;

pub const GIT_COMMAND_TIMEOUT_SECONDS: u64 = 360;
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

pub fn git_tool(paths: &WorkspacePaths, arguments: &Map<String, Value>) -> ToolResult {
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

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(paths.root())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        // 不向终端交互：需要凭据时直接失败，需要编辑器时用 true 直接成功。
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true");
    let child = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return error_result("未找到 git 可执行文件，请确认 Git 已安装。".to_string());
        }
        Err(error) => return error_result(format!("无法启动 git：{error}。")),
    };
    let _ = Duration::from_secs(GIT_COMMAND_TIMEOUT_SECONDS);

    let stdout = String::from_utf8_lossy(&child.stdout).to_string();
    let stderr = String::from_utf8_lossy(&child.stderr).to_string();
    if !child.status.success() {
        let detail = stderr.trim();
        let text = if detail.is_empty() {
            let code = child.status.code().unwrap_or(-1);
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

/// git 工具名（供审批分流判断用）。
pub fn is_git_tool(name: &str) -> bool {
    name == GIT_TOOL_NAME
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
        let unsupported = git_tool(&paths, &args(json!({"action": "push"})));
        // push 属于高风险但受支持；这里用真正不支持的动作。
        assert!(unsupported.ok || !unsupported.output.contains("不支持的 git 子命令"));

        let bad_action = git_tool(&paths, &args(json!({"action": "frobnicate"})));
        assert_eq!(bad_action.output, "不支持的 git 子命令：frobnicate。");

        let escape = git_tool(
            &paths,
            &args(json!({"action": "status", "args": ["--git-dir=/tmp/x"]})),
        );
        assert_eq!(escape.output, "git 工具不允许参数：--git-dir=/tmp/x。");

        let config = git_tool(
            &paths,
            &args(json!({"action": "config", "args": ["--global", "user.name", "x"]})),
        );
        assert_eq!(
            config.output,
            "git config 不允许修改全局/系统配置或指定 --file。"
        );

        let archive = git_tool(
            &paths,
            &args(json!({"action": "archive", "args": ["-o", "out.zip"]})),
        );
        assert_eq!(
            archive.output,
            "git archive 不允许 -o/--output，输出只能走 stdout。"
        );

        let commit = git_tool(&paths, &args(json!({"action": "commit"})));
        assert_eq!(
            commit.output,
            "commit 必须提供 message，或显式传入 --no-edit。"
        );

        let message_type = git_tool(&paths, &args(json!({"action": "commit", "message": 5})));
        assert_eq!(message_type.output, "message 必须是字符串。");

        let outside = git_tool(
            &paths,
            &args(json!({"action": "add", "paths": ["../outside.txt"]})),
        );
        assert_eq!(
            outside.output,
            "路径越界（必须在工作区内）：../outside.txt。"
        );

        let clone_outside = git_tool(
            &paths,
            &args(json!({"action": "clone", "args": ["https://example.invalid/x", "../out"]})),
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
        let init = git_tool(&paths, &args(json!({"action": "init"})));
        if !init.ok && init.output.contains("未找到 git 可执行文件") {
            eprintln!("跳过：本机没有 git");
            return;
        }
        assert!(init.ok, "{}", init.output);
        let status = git_tool(&paths, &args(json!({"action": "status"})));
        assert!(status.ok, "{}", status.output);
        assert!(
            status.output.contains("On branch") || status.output.contains("No commits"),
            "{}",
            status.output
        );
        assert!(root.join(".git").is_dir());
    }
}
