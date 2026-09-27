//! `omnicrawl` 二进制：内核进程，在 stdin/stdout 上提供协议 v1。
//!
//! 协议规格见 `rust/docs/protocol-v1.md`；宿主（启动器、TUI、过渡期的 Python 宿主）启动本进程，
//! 用 NDJSON JSON-RPC 2.0 帧驱动它。

mod approval_audit;
mod compaction;
mod compression;
mod connector;
mod dual_compaction;
mod output_budget;
mod session;
mod settings;
mod subagent;
mod tool_events;
mod turn_summary;
mod undo;
mod vision_proxy;
mod worktree;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Some(index) = args.iter().position(|arg| arg == "--connector") {
        let Some(name) = args.get(index + 1) else {
            eprintln!("用法：omnicrawl --connector <telegram>");
            return ExitCode::FAILURE;
        };
        return match connector::run(name) {
            Ok(()) => ExitCode::SUCCESS,
            Err(detail) => {
                eprintln!("[kernel] 连接器异常结束：{detail}");
                ExitCode::FAILURE
            }
        };
    }

    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("omnicrawl {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "用法：omnicrawl [--version] [--connector <name>]\n\n\
             在 stdin/stdout 上用 NDJSON 协议 v1 与宿主通信。协议见 rust/docs/protocol-v1.md。\n\
             --connector <name>：在进程内启动消息平台连接器（当前支持 telegram），不再走 stdio 协议。"
        );
        return ExitCode::SUCCESS;
    }

    match session::run_stdio() {
        Ok(()) => ExitCode::SUCCESS,
        Err(detail) => {
            eprintln!("[kernel] 会话异常结束：{detail}");
            ExitCode::FAILURE
        }
    }
}
