//! `omnicrawl-mcp-server`：本地 stdio MCP Server 入口。
//!
//! 对应 `python -m omnicrawl.mcp.server`：工作区取 `MCP_WORKSPACE_ROOT`（支持 `~`），
//! 未设置时用当前目录。

use std::path::PathBuf;
use std::process::ExitCode;

use omnicrawl_mcp::server::LocalMcpServer;

fn main() -> ExitCode {
    let raw = std::env::var("MCP_WORKSPACE_ROOT").unwrap_or_default();
    let raw = raw.trim();
    let workspace = if raw.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        expand_user(raw)
    };
    match LocalMcpServer::new(workspace).run_stdio() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[mcp-server] 读取请求失败：{error}");
            ExitCode::FAILURE
        }
    }
}

/// `Path("~/x").expanduser()` 的可用子集：用 `HOME`／`USERPROFILE` 展开 `~`。
fn expand_user(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    if !rest.is_empty() && !rest.starts_with(['/', '\\']) {
        return PathBuf::from(path);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if home.is_empty() {
        return PathBuf::from(path);
    }
    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        PathBuf::from(home)
    } else {
        PathBuf::from(home).join(rest)
    }
}
