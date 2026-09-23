//! 连接器子进程的跨进程单例互斥（对应 `omnicrawl/workspace/connector_singleton.py`）。
//!
//! 问题背景：多个 OmniCrawl 进程（多个 TUI、TUI + API、手工启动的连接器）同时启动时，
//! 每个进程都会各自拉起一个飞书 / Telegram 连接器子进程，同一平台出现多个长连接实例
//! （重复建连、消息被重复处理）。本模块为每个连接器平台提供一把「用户级单例锁」。
//!
//! 锁文件放在用户配置目录（`~/.OmniCrawl/`）下，按平台名区分，与工作区无关，因此跨项目、
//! 跨工作区共享同一把锁；用 OS 级文件锁保证并发获取的原子性，锁文件内记录持有者 PID
//! 用于「粘滞接管」。
//!
//! 与 Python 的差异（见 crate `README.md`）：
//!
//! - **锁原语**：复用 `omnicrawl-session` 的 `try_lock_file`（与 Python 的 `ProcessFileLock`
//!   同一套 OS 锁：Windows 字节区间锁、POSIX `flock`），但 Rust 侧持有的是**锁句柄本身**
//!   （`std::fs::File`），而不是 Python 的「实例内部持句柄 + 显式 release」。句柄在手上即
//!   持锁，`Drop` 或 [`ConnectorInstanceLock::release`] 关闭句柄即解锁，语义一致。
//! - **PID 存活探测**：与 `process_control` 共用同一份跨平台实现（Python 侧本模块自带一份，
//!   再导出成 `pid_is_running`）。
//! - **`pid=` 行解析**：`\d` 在 Python 的 `str` 正则里包含 Unicode 十进制数字，Rust 侧只认
//!   ASCII 数字；锁文件由本实现自己写入，这个差异不可达。

use std::fs::File;
use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_session::{try_lock_file, LOCK_OWNER_LINE_PREFIX};

/// 删除粘滞锁文件前最多等待时长（秒）。
///
/// Python 侧定义了这个常量但当前实现并没有真的等待：`try_acquire` 判定粘滞后直接删文件
/// 重试。这里保留同一常量与同一行为，方便调用方按它推算重试窗口。
pub const STALE_RECLAIM_WAIT_SECONDS: f64 = 2.0;
/// 锁文件名前缀。
pub const LOCK_FILENAME_PREFIX: &str = "connector-";
/// 锁文件名后缀。
pub const LOCK_FILENAME_SUFFIX: &str = ".lock";
/// 单例判定不能长时间阻塞启动，因此取锁超时必须很短。
pub const LOCK_TIMEOUT_SECONDS: f64 = 0.5;
/// 取锁轮询间隔。
pub const LOCK_POLL_SECONDS: f64 = 0.05;

/// 平台名 → 安全的文件名片段：非字母数字与 `._-` 之外的一律折成 `-`，再剥掉首尾的 `.`/`-`。
///
/// 锁文件名保留中文等 Unicode 字符（Windows 与 POSIX 均支持），保证「飞书」/「Telegram」
/// 等平台名一一对应，不会被 sanitize 成同一个文件。
pub fn sanitize_platform_name(name: &str) -> String {
    let folded: String = name
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = folded.trim_matches(|ch| ch == '.' || ch == '-').to_string();
    if trimmed.is_empty() {
        "connector".to_string()
    } else {
        trimmed
    }
}

/// 平台连接器的单例锁文件路径（用户配置目录，跨工作区共享）。
pub fn connector_lock_path(env: &ConfigEnvironment, name: &str) -> PathBuf {
    user_config_dir(env).join(format!(
        "{LOCK_FILENAME_PREFIX}{}{LOCK_FILENAME_SUFFIX}",
        sanitize_platform_name(name)
    ))
}

/// 读取锁文件里的持有者 PID；无锁文件、读不到或格式异常返回 `None`。
///
/// 读取不需要也不应获取文件锁——被活动实例持有的锁文件在 Windows 上无法被其他进程读取
/// （字节区间锁导致共享违例），返回 `None` 后由 `try_acquire` 按「已有实例」处理；只有
/// 无锁 / 粘滞锁（文件存在但无进程持有）才能被读到内容。
///
/// 行形状与 Python 的 `^pid=(\d+)$`（MULTILINE）一致。行切分按 Python 的文本模式来：
/// `read_text` 走通用换行，`\r\n` 与单独的 `\r` 都会被折成 `\n`，因此这里也把 `\r` 当作
/// 行结束符（否则 CRLF 锁文件在 Python 侧能读到 PID、Rust 侧读不到）。
pub fn locked_pid(lock_path: &Path) -> Option<i64> {
    let raw = std::fs::read(lock_path).ok()?;
    let text = String::from_utf8_lossy(&raw);
    for line in text.split(['\n', '\r']) {
        let Some(digits) = line.strip_prefix(LOCK_OWNER_LINE_PREFIX) else {
            continue;
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if let Ok(pid) = digits.parse::<i64>() {
            return Some(pid);
        }
    }
    None
}

/// 尽力删除粘滞锁文件；失败仅忽略（Python 只记调试日志，不阻塞后续重试）。
fn remove_stale_lock(lock_path: &Path) {
    let _ = std::fs::remove_file(lock_path);
}

/// 一个平台连接器的跨进程单例锁。
///
/// ```text
/// let mut lock = ConnectorInstanceLock::at("飞书", lock_path);
/// if lock.try_acquire() {
///     // 已持有平台单例，可安全拉起连接器子进程
///     lock.release();
/// }
/// ```
///
/// 注意（Windows 行为）：持锁期间锁文件被字节区间锁锁定，其他进程无法读取其内容；这使
/// 「粘滞接管」只能发生在锁文件存在但无人持锁（崩溃残留、锁随进程消失）的场合，恰好构成
/// 安全边界——不会误删活动实例的锁。
pub struct ConnectorInstanceLock {
    pub name: String,
    pub lock_path: PathBuf,
    handle: Option<File>,
    owner: bool,
}

impl ConnectorInstanceLock {
    /// 按用户配置目录推导锁文件路径。
    pub fn new(env: &ConfigEnvironment, name: &str) -> Self {
        Self::at(name, connector_lock_path(env, name))
    }

    /// 指定锁文件路径（测试与宿主自定义布局用）。
    pub fn at(name: &str, lock_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.to_string(),
            lock_path: lock_path.into(),
            handle: None,
            owner: false,
        }
    }

    /// 尝试获取平台单例；被其他活动实例持有则返回 `false`。
    ///
    /// 判定顺序（竞争失败时区分「另一实例持有」与「粘滞残留」）：
    ///
    /// 1. 尝试获取文件锁；
    /// 2. 成功：锁文件已写入 PID，返回 `true`；
    /// 3. 失败：读取锁文件 PID（Windows 上锁被活动实例持有时不可读，读不到就按「已有实例」
    ///    处理）；PID 存在说明有活动实例，返回 `false`；PID 不存在则删除锁文件后重试。
    pub fn try_acquire(&mut self) -> bool {
        loop {
            match try_lock_file(&self.lock_path) {
                Ok(file) => {
                    self.handle = Some(file);
                    self.owner = true;
                    return true;
                }
                Err(error) => {
                    let _ = error;
                }
            }

            // 文件锁竞争失败：尝试判断是否为粘滞残留。
            match locked_pid(&self.lock_path) {
                None => {
                    // 读不到 pid：Windows 上通常是锁被活动实例持有（不可读）；POSIX 上可能
                    // 刚被创建、内容未写完。两种都按「已有实例在运行」处理，避免误删活动锁。
                    return false;
                }
                Some(pid) => {
                    if !crate::process_control::pid_is_running(pid) {
                        // 旧实例已退出（崩溃残留）：删掉锁文件后重试。
                        remove_stale_lock(&self.lock_path);
                        continue;
                    }
                    return false;
                }
            }
        }
    }

    /// 释放单例锁并删除锁文件（仅持有者执行）。
    pub fn release(&mut self) {
        if !self.owner {
            return;
        }
        // 先释放文件锁（关闭句柄），再删除锁文件：Windows 上文件若仍被打开的句柄引用
        // （字节区间锁住期间），unlink 会失败。
        self.handle = None;
        remove_stale_lock(&self.lock_path);
        self.owner = false;
    }

    /// 是否仍持有这把锁。
    pub fn is_owner(&self) -> bool {
        self.owner
    }
}

impl Drop for ConnectorInstanceLock {
    fn drop(&mut self) {
        // Python 侧靠显式 `release()` 或上下文管理器退出；Rust 侧再补一层 Drop，避免
        // 提前返回时把锁留在进程里（句柄关闭本来就会解锁，删除锁文件才是额外的一步）。
        self.release();
    }
}

/// 判断进程是否仍在运行（对映 Python 的 `pid_is_running`）。
///
/// 与 `process_control::pid_is_running` 是同一份实现：Windows 走 `OpenProcess` +
/// `GetExitCodeProcess`，其他平台走 `os.kill(pid, 0)`。
pub use crate::process_control::pid_is_running;
