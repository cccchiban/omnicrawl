//! 原始工具调用的落盘：把整轮调用（请求 + 结果）写进工作区临时目录。
//!
//! 压缩只把工具调用压成一段概述，原文并没有丢：它被写进 `.omnicrawl/.agent_tmp/files/`
//! 下的一个文本文件，概述里带上该路径，模型需要逐字核对时可以自己去读。

use omnicrawl_session::utc_now;
use std::path::{Path, PathBuf};

/// 一次原始工具调用：请求 + 配对结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawToolCall {
    pub tool: String,
    pub arguments: String,
    pub ok: bool,
    pub output: String,
}

impl RawToolCall {
    fn char_count(&self) -> usize {
        self.arguments.chars().count() + self.output.chars().count()
    }
}

/// 工作区内的临时目录：与宿主、连接器的落点约定一致。
pub const AGENT_TEMP_DIR: &str = ".omnicrawl/.agent_tmp";
const ARCHIVE_SUBDIR: &str = "files";

/// 原始内容的字符总数（落盘说明里报给模型）。
pub fn raw_chars(calls: &[RawToolCall]) -> usize {
    calls.iter().map(RawToolCall::char_count).sum()
}

/// 原始工具调用的落盘正文：逐条给出工具、参数、状态与完整输出。
pub fn render_raw_archive(calls: &[RawToolCall]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(calls.len() * 6);
    for (index, call) in calls.iter().enumerate() {
        lines.push(format!("===== 调用 {} =====", index + 1));
        lines.push(format!("工具：{}", call.tool));
        lines.push(format!(
            "参数：{}",
            if call.arguments.trim().is_empty() {
                "（无参数）"
            } else {
                call.arguments.trim()
            }
        ));
        lines.push(format!("状态：{}", if call.ok { "成功" } else { "失败" }));
        lines.push("输出：".to_string());
        lines.push(call.output.clone());
        lines.push(String::new());
    }
    lines.join("\n")
}

/// 把原始调用写进工作区临时目录，返回相对工作区（拿不到时给绝对路径）的可读路径。
pub fn write_raw_archive(
    workspace_root: &str,
    session_id: &str,
    calls: &[RawToolCall],
) -> Result<String, String> {
    let root = archive_root(workspace_root);
    std::fs::create_dir_all(&root).map_err(|error| format!("建目录 {root:?} 失败：{error}"))?;
    let stamp = utc_now().format("%Y%m%d-%H%M%S").to_string();
    let name = sanitize(&format!(
        "tool_calls_{}_{}.txt",
        session_id,
        stamp.replace([':', '+'], "-")
    ));
    let path = root.join(name);
    std::fs::write(&path, render_raw_archive(calls))
        .map_err(|error| format!("写文件 {path:?} 失败：{error}"))?;
    Ok(display_path(&path, workspace_root))
}

/// 临时目录：工作区为空时退回当前进程目录。
fn archive_root(workspace_root: &str) -> PathBuf {
    let root = workspace_root.trim();
    let base = if root.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(root)
    };
    base.join(AGENT_TEMP_DIR).join(ARCHIVE_SUBDIR)
}

/// 文件名只留下安全字符。
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// 相对工作区的展示路径：工作区为空或路径不在工作区内时给出绝对路径。
fn display_path(path: &Path, workspace_root: &str) -> String {
    let root = workspace_root.trim();
    if !root.is_empty() {
        if let Ok(relative) = path.strip_prefix(root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    path.to_string_lossy().replace('\\', "/")
}
