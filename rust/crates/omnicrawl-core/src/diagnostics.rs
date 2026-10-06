//! 进程级诊断汇：库代码不直接往 stderr 写诊断，交给宿主进程决定去处。
//!
//! 起因（用户反馈）：TUI 是全屏画布，库里的 `eprintln!` 会写在终端光标处；而画布只在
//! 「该区块内容变了」时才重画对应区域 —— 报错就**残留**在画面上、盖住会话区与输入区。
//! 这里给出统一出口：宿主进程装接收端（TUI 装成「进会话流 + 落盘」），没装的进程
//! （本地 API、内核、测试）仍按原样落 stderr，行为与改造前完全一致。
//!
//! 约定：文案由调用方写全（含 `[host]` / `[tui]` 前缀），本模块**不加任何前缀**，
//! 这样从 stderr 切到会话流时用户看到的字完全一样，排障口径不变。

use std::sync::{Arc, Mutex, OnceLock};

/// 诊断级别。宿主可据此决定去处；TUI 目前一视同仁，原样进会话流。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 常规进度信息。
    Info,
    /// 降级但仍继续：某项能力不可用，已回退到备用路径。
    Warning,
    /// 确实出错的路径。
    Error,
}

/// 诊断接收端：级别 + 文案。
pub type Sink = Arc<dyn Fn(Level, &str) + Send + Sync + 'static>;

/// 本进程唯一的接收端。装了之后所有 [`report`] 都走它。
static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Sink>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// 装上接收端（重复装以后一次为准）。
pub fn install(sink: Sink) {
    if let Ok(mut current) = slot().lock() {
        *current = Some(sink);
    }
}

/// 摘掉接收端：之后的 [`report`] 回落 stderr。
///
/// 宿主退出（或测试收尾）时用，避免诊断写进已经没人读的通道。
pub fn uninstall() {
    if let Ok(mut current) = slot().lock() {
        *current = None;
    }
}

/// 上报一条诊断：有接收端就交给它，没有就按原样打 stderr。
///
/// 接收端可能在任何线程被调用（内核读取线程、工具执行线程…），因此文案里的换行
/// 由接收端自己处理；空文案直接丢弃——它只是噪声。
pub fn report(level: Level, message: impl AsRef<str>) {
    let message = message.as_ref();
    if message.trim().is_empty() {
        return;
    }
    // 先把句柄克隆出来再调用：接收端自身可能又要上报（重入），
    // 持锁调用会把自己锁死。
    let sink = slot().lock().ok().and_then(|current| current.clone());
    match sink {
        Some(sink) => sink(level, message),
        None => eprintln!("{message}"),
    }
}

/// 常规进度信息。
pub fn info(message: impl AsRef<str>) {
    report(Level::Info, message);
}

/// 降级但仍继续（能力不可用、已回退备用路径）。
pub fn warn(message: impl AsRef<str>) {
    report(Level::Warning, message);
}

/// 出错的路径。
pub fn error(message: impl AsRef<str>) {
    report(Level::Error, message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// 接收端是进程全局的：两个用例若并行装/摘会互相顶掉，这里串行化。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 装了接收端就走它、摘掉后回落（回落分支只验证不 panic，stderr 内容不在断言范围）。
    #[test]
    fn sink_receives_levels_and_uninstall_restores_fallback() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let (tx, rx) = channel();
        install(Arc::new(move |level, message| {
            let _ = tx.send((level, message.to_string()));
        }));
        warn("[host] 能力不可用，已回退。");
        info("[tui] 常规进度。");
        uninstall();
        assert_eq!(
            rx.recv().expect("警告应当进接收端"),
            (Level::Warning, "[host] 能力不可用，已回退。".to_string())
        );
        assert_eq!(
            rx.recv().expect("信息应当进接收端"),
            (Level::Info, "[tui] 常规进度。".to_string())
        );
    }

    /// 空文案是噪声，任何一级都不上报。
    #[test]
    fn blank_messages_are_dropped() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let (tx, rx) = channel();
        install(Arc::new(move |level, message| {
            let _ = tx.send((level, message.to_string()));
        }));
        report(Level::Error, "   ");
        report(Level::Error, "");
        uninstall();
        assert!(rx.try_recv().is_err(), "空文案不该进接收端");
    }
}
