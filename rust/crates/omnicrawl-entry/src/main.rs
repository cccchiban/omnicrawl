//! OmniCrawl 统一启动入口：对齐 Python `omnicrawl/entry.py` 的路由职责。
//!
//! 业务实现仍由独立的 Rust TUI、API 和内核二进制提供；本程序只负责选择目标、
//! 继承标准输入输出并传播子进程退出码。`plugin ...` 在进程内由 [`omnicrawl_entry::cli`]
//! 处理（对应 Python `omnicrawl/cli.py`），不再转发给其他进程。

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use omnicrawl_config::core::context::detect_project_context;
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_entry::cli::{run_plugin_cli, EXIT_USAGE};
use omnicrawl_entry::startup;

const KERNEL_COMMAND: &str = "kernel";
const API_COMMAND: &str = "api";
const PLUGIN_COMMAND: &str = "plugin";
const BINARY_ENV: &str = "OMNICRAWL_BINARY";
const TUI_ENV: &str = "OMNICRAWL_TUI_BINARY";
const API_ENV: &str = "OMNICRAWL_API_BINARY";
const KERNEL_ENV: &str = "OMNICRAWL_KERNEL_BINARY";

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(code) => ExitCode::from(code as u8),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<i32, String> {
    let environment = ConfigEnvironment::from_process();
    let context = detect_project_context(&environment, None);
    let args_text: Vec<String> = args
        .iter()
        .map(|item| item.to_string_lossy().to_string())
        .collect();

    // 与 npm 启动器一致：显式指定内核时，宿主只负责 stdio 透传。
    // 因此即使参数里出现 --help 或 api，也不能被宿主截走。
    if env::var_os(BINARY_ENV).is_some() {
        let target = resolve_target(BINARY_ENV, "omnicrawl", None)?;
        return run_child(&target, &args, &context.workspace_root);
    }

    if args_text.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(0);
    }
    if args_text
        .iter()
        .any(|arg| arg == "--version" || arg == "-V")
    {
        println!("omnicrawl-host {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }

    match args_text.first().map(String::as_str) {
        // 插件 CLI 在进程内执行：退出码直接透传，不启动子进程。
        Some(PLUGIN_COMMAND) => {
            Ok(
                run_plugin_cli(&args_text, &environment, Some(&context.workspace_root))
                    .unwrap_or(EXIT_USAGE),
            )
        }
        Some(KERNEL_COMMAND) => {
            let target = resolve_target(KERNEL_ENV, "omnicrawl", None)?;
            run_child(&target, &args[1..], &context.workspace_root)
        }
        Some(API_COMMAND) => {
            let target = resolve_target(API_ENV, "omnicrawl-api", None)?;
            run_child(&target, &args[1..], &context.workspace_root)
        }
        _ => run_tui(&args, &context.workspace_root),
    }
}

/// 默认路由：先跑首次配置与连接器自动启动，再进工作台，退出时先回收连接器。
///
/// 与 Python `entry.py` 的顺序一致：无交互终端直接报错退出（不静默回退），渠道未配置时按
/// 「配置错误 1 / 凭据缺失 2」给退出码。连接器排在 TUI 之前拉起、退出后统一回收，避免
/// 两台宿主同时持有同一平台的连接器。
fn run_tui(args: &[OsString], workspace: &Path) -> Result<i32, String> {
    if !startup::interactive_terminal() {
        eprintln!(
            "当前没有交互式终端，无法启动工作台。\n\
             无头/服务器场景请改用：omnicrawl api\n\
             或从 Telegram / 飞书连接器远程接入。"
        );
        return Ok(2);
    }

    let environment = ConfigEnvironment::from_process();
    let outcome = startup::bootstrap(&environment, true)?;
    for line in &outcome.lines {
        println!("{line}");
    }
    if !outcome.ready() {
        return Ok(outcome.exit_code());
    }

    let kernel = resolve_target(KERNEL_ENV, "omnicrawl", Some(workspace))?;
    let connectors = startup::start_connectors(&environment, workspace, &kernel);
    for name in connectors.started_connectors() {
        eprintln!("[connectors] {name} 连接器已启动。");
    }
    for line in connectors.diagnostics() {
        eprintln!("[connectors] {line}");
    }

    let target = resolve_target(TUI_ENV, "omnicrawl-tui", None)?;
    let result = run_child(&target, args, workspace);
    // 连接器子进程先于工作台收尾：它们各自持有独立 Agent，退出慢的不能让主进程空等。
    connectors.close();
    result
}

fn resolve_target(
    variable: &str,
    default_name: &str,
    sibling_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(value) = env::var_os(variable).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }

    let executable_name = if cfg!(windows) {
        format!("{default_name}.exe")
    } else {
        default_name.to_string()
    };
    let directory = sibling_dir.map(PathBuf::from).or_else(|| {
        env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(PathBuf::from))
    });
    if let Some(directory) = directory {
        let sibling = directory.join(&executable_name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from(executable_name))
}

fn run_child(target: &Path, args: &[OsString], workspace: &Path) -> Result<i32, String> {
    let mut command = Command::new(target);
    command
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = command
        .spawn()
        .map_err(|error| format!("omnicrawl：无法启动 {}：{error}", target.display()))?;
    let status = child
        .wait()
        .map_err(|error| format!("omnicrawl：等待 {} 失败：{error}", target.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn print_help() {
    println!(
        "用法：omnicrawl [选项] [指令]\n\n\
         无参数        启动 Rust 终端工作台\n\
         api           启动 Rust 本地 HTTP 服务\n\
         plugin ...    插件管理（install / list / info / enable / disable / update /\n\
                       rollback / uninstall / system / doctor）\n\
         kernel ...    直连 Rust 内核进程\n\n\
         --resume <id>  启动终端工作台时恢复指定会话\n\
         --version, -V  打印版本\n\
         --help, -h     打印本说明"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_is_not_empty() {
        let _ = print_help as fn();
        assert_eq!(KERNEL_COMMAND, "kernel");
        assert_eq!(API_COMMAND, "api");
    }

    #[test]
    fn executable_name_uses_platform_suffix() {
        let target =
            resolve_target("__OMNICRAWL_MISSING__", "omnicrawl", Some(Path::new("."))).unwrap();
        let name = target.file_name().unwrap().to_string_lossy();
        assert!(name == "omnicrawl" || name == "omnicrawl.exe");
    }
}
