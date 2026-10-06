//! 运行期诊断的落地：TUI 进程内的库诊断统一走这里。
//!
//! 为什么需要：TUI 是全屏画布，而库代码里的 `eprintln!` 写的是**终端光标处**；画布只在
//! 「该区块内容变了」时才重画对应区域，于是报错会残留在画面上、盖住会话区与输入区
//! （用户反馈的「错误信息覆盖在面板上」）。现在这些调用点统一改成
//! `omnicrawl_core::diagnostics` 上报，由这里决定去处：
//!
//! * **会话流**：每条诊断进 `Record::Notice`（`· ` 暗灰提示行），与内核报错同一种样式、
//!   同样可滚动回看，且**不占用**画布任何区域；
//! * **`logs/tui.log`**：会话流只在内存里，进程没了就什么都没了，另留一份可 grep 的现场
//!   （与连接器日志同目录、同目录约定）。
//!
//! 只搬 TUI 进程内的写点（`omnicrawl-host` 与 `omnicrawl-tui` 自身）；子进程那条线本来
//! 就隔离好了（内核 stderr 走管道 + 过滤进会话流，连接器/服务/插件/MCP 各走文件或管道）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use chrono::{Local, SecondsFormat};

use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_core::diagnostics::{self, Level};

/// 诊断日志文件名：与 Python 宿主、连接器日志同目录（`~/.omnicrawl/logs/`）。
const DIAGNOSTIC_LOG: &str = "tui.log";

/// 装上诊断接收端，返回接收端供 `App::attach_diagnostics` 每帧 drain。
///
/// 接收端可能在任何线程被调用（内核读取线程、工具执行线程、设置页后台任务），
/// 因此这里只做两件不会阻塞上报方的事：追加一行日志、投进通道。
pub fn install(environment: &ConfigEnvironment) -> Receiver<(Level, String)> {
    let (sender, receiver) = mpsc::channel();
    let path = log_path(environment);
    diagnostics::install(Arc::new(move |level: Level, message: &str| {
        append(&path, level, message);
        // 通道断开说明 TUI 已经收尾：丢弃即可，绝不反过来阻断上报方。
        let _ = sender.send((level, message.to_string()));
    }));
    receiver
}

/// 摘掉接收端：之后的诊断回落 stderr（退出收尾时用，避免写进没人读的通道）。
pub fn uninstall() {
    diagnostics::uninstall();
}

/// 装 panic hook：panic 文案先落盘、再上报、最后交给默认 hook 打到终端。
///
/// release 档是 `panic = "abort"`：进程随即消失，会话流可能来不及重画这一帧，
/// 所以**先落盘**；那一刻画布无论如何都保不住了，把死因留在终端与日志里比保住画面重要。
pub fn install_panic_hook(environment: &ConfigEnvironment) {
    let path = log_path(environment);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = format!("[tui] panic：{info}");
        append(&path, Level::Error, &message);
        diagnostics::report(Level::Error, &message);
        previous(info);
    }));
}

/// 诊断日志落点（`~/.omnicrawl/logs/tui.log`）。
fn log_path(environment: &ConfigEnvironment) -> PathBuf {
    user_config_dir(environment)
        .join("logs")
        .join(DIAGNOSTIC_LOG)
}

/// 追加一行诊断。
///
/// 任何失败都吞掉：诊断记录本身不该反过来影响主流程（磁盘满、目录只读时界面照常能用）。
fn append(path: &Path, level: Level, message: &str) {
    let stamp = Local::now().to_rfc3339_opts(SecondsFormat::Secs, false);
    let line = format!("{stamp} [{}] {message}\n", level_label(level));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn level_label(level: Level) -> &'static str {
    match level {
        Level::Info => "INFO",
        Level::Warning => "WARN",
        Level::Error => "ERROR",
    }
}
