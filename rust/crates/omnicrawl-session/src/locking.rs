//! 跨进程写锁与耐久写辅助：对齐 Python `omnicrawl/state/session_locking.py`。
//!
//! 同一个会话目录允许多个进程（TUI、API、内核）同时访问，写路径靠 OS 级文件锁互斥：
//! Windows 走字节区间锁（与 Python `msvcrt.locking` 同一个机制），POSIX 走 `flock`。
//! 两个实现锁的是同一个文件、同一个区间，所以能互相排斥。进程异常退出时由 OS 释放锁，
//! 因此不存在需要抢占的「残留锁」。
//!
//! 与 Python 的两点差异（`README.md` 有记录）：本类型不可重入（内部步骤走 `store.rs` 的
//! `*_locked` 方法），且超时按每次调用传入而不是粘在实例上。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::error::SessionStoreError;

pub const DEFAULT_LOCK_TIMEOUT_SECONDS: f64 = 30.0;
pub const DEFAULT_LOCK_POLL_SECONDS: f64 = 0.05;
pub const LOCK_FILE_NAME: &str = ".session_store.lock";
/// 持有者信息行：`pid=<pid>`（连接器单例锁的粘滞接管也依赖这个形状）。
pub const LOCK_OWNER_LINE_PREFIX: &str = "pid=";
/// Windows 上目标文件被只读打开/杀软扫描时，替换可能短暂返回拒绝访问。
const ATOMIC_REPLACE_MAX_ATTEMPTS: u32 = 8;
const ATOMIC_REPLACE_RETRY_SECONDS: f64 = 0.05;

/// 持久化写策略：fsync 默认开启——会话转录与索引是恢复真相源，宁可多一次磁盘同步。
#[derive(Debug, Clone, PartialEq)]
pub struct DurableWritePolicy {
    pub fsync: bool,
    pub lock_timeout_seconds: f64,
    pub lock_poll_seconds: f64,
}

impl Default for DurableWritePolicy {
    fn default() -> Self {
        Self {
            fsync: true,
            lock_timeout_seconds: DEFAULT_LOCK_TIMEOUT_SECONDS,
            lock_poll_seconds: DEFAULT_LOCK_POLL_SECONDS,
        }
    }
}

/// 基于锁文件的跨进程互斥锁（同进程内另有一层互斥，线程之间不去争 OS 锁）。
pub struct ProcessFileLock {
    path: PathBuf,
    in_process: Mutex<()>,
}

impl ProcessFileLock {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            in_process: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 取锁；超出时限返回会话错误（文案与 Python 一致）。
    pub fn acquire(
        &self,
        timeout_seconds: f64,
        poll_seconds: f64,
    ) -> Result<ProcessLockGuard<'_>, SessionStoreError> {
        let timeout_seconds = timeout_seconds.max(0.1);
        let poll_seconds = poll_seconds.max(0.01);
        let in_process = self
            .in_process
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                SessionStoreError::new(format!("创建锁文件目录失败：{}，{error}", parent.display()))
            })?;
        }

        let deadline = Instant::now() + Duration::from_secs_f64(timeout_seconds);
        loop {
            match self.try_acquire() {
                Ok(file) => {
                    return Ok(ProcessLockGuard {
                        file: Some(file),
                        _in_process: in_process,
                    })
                }
                Err(error) => {
                    if Instant::now() >= deadline {
                        return Err(self.timeout_error(timeout_seconds, error));
                    }
                    std::thread::sleep(Duration::from_secs_f64(poll_seconds));
                }
            }
        }
    }

    /// 超时文案与 Python 一致：`获取会话存储写锁超时（30.0s）：<路径>：<最后一次错误>`。
    fn timeout_error(&self, timeout_seconds: f64, error: std::io::Error) -> SessionStoreError {
        SessionStoreError::new(format!(
            "获取会话存储写锁超时（{timeout_seconds:.1}s）：{}：{error}",
            self.path.display()
        ))
    }

    fn try_acquire(&self) -> std::io::Result<File> {
        try_lock_file(&self.path)
    }
}

/// 打开锁文件并取 OS 级排他锁，返回**持锁句柄**：句柄在手上就是持锁，句柄关闭
/// （含进程退出）时由 OS 自动释放。锁文件的形状与 [`ProcessFileLock`] 一致：
/// 拿到锁后清空并把持有者 PID 写成 `pid=<pid>\n`。
///
/// 同一个锁文件被两个句柄排他锁定时，第二个句柄必然失败（Windows 的字节区间锁与
/// POSIX 的 `flock` 都是按打开实例排斥），因此这个入口可以当作「跨句柄单例锁」使用；
/// 需要「同进程内也互斥」的调用方仍应走 [`ProcessFileLock`]（它额外持有进程内互斥）。
pub fn try_lock_file(path: &Path) -> std::io::Result<File> {
    // 必须是读写打开：Windows 的字节区间锁要求句柄带 GENERIC_WRITE，
    // 只开 append 拿到的是 FILE_APPEND_DATA，LockFileEx 会回「拒绝访问」。
    // 打开时不能截断——截断必须发生在拿到锁之后，否则会把别人的持有者信息清掉。
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // 字节区间锁要求区域内至少有一个字节：空文件先补占位（锁住后会截掉）。
    if file.metadata()?.len() == 0 {
        file.write_all(b"\0")?;
        file.flush()?;
    }
    lock_file(&file)?;
    file.set_len(0)?;
    // 截断后游标停在占位字节之后，写持有者信息前先回到文件头。
    file.seek(SeekFrom::Start(0))?;
    file.write_all(format!("pid={}\n", std::process::id()).as_bytes())?;
    file.flush()?;
    Ok(file)
}

/// 持锁凭据：销毁即解锁并关闭句柄（解锁失败按已失效处理）。
pub struct ProcessLockGuard<'a> {
    file: Option<File>,
    _in_process: MutexGuard<'a, ()>,
}

impl Drop for ProcessLockGuard<'_> {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            unlock_file(&file);
        }
    }
}

static ROOT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<ProcessFileLock>>>> = OnceLock::new();

/// 同一会话根目录共享一份锁实例：线程之间靠它串行，跨进程靠同一个锁文件。
pub fn process_lock_for_root(root: &Path) -> Arc<ProcessFileLock> {
    let registry = ROOT_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // 只复用仍被持有的实例；会话根目录会随会话累积，所以顺手清掉已释放的条目。
    locks.retain(|_, lock| lock.strong_count() > 0);
    let key = root.to_path_buf();
    if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(ProcessFileLock::new(root.join(LOCK_FILE_NAME)));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// 向 JSONL 追加一行并可选 fsync；行尾统一补换行。
pub fn append_text_line(path: &Path, line: &str, fsync: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    if !line.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.flush()?;
    if fsync {
        file.sync_all()?;
    }
    Ok(())
}

/// 同目录临时文件写入后原子替换，可选 fsync 文件与目录。
pub fn atomic_write_text(path: &Path, text: &str, fsync: bool) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = temporary_path(path);

    let write_result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
        if fsync {
            file.sync_all()?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    if let Err(error) = replace_with_retry(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if fsync {
        fsync_directory(parent);
    }
    Ok(())
}

/// 临时文件名沿用 Python 的形状：`<词干>.<随机>.tmp`。
fn temporary_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".to_string());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.{}.tmp", crate::naming::random_suffix()))
}

/// 原子替换；Windows 上目标被短暂占用时按线性退避重试。
fn replace_with_retry(temporary: &Path, target: &Path) -> std::io::Result<()> {
    let attempts = if cfg!(windows) {
        ATOMIC_REPLACE_MAX_ATTEMPTS
    } else {
        1
    };
    let mut last_error: Option<std::io::Error> = None;
    for attempt in 1..=attempts {
        match std::fs::rename(temporary, target) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable =
                    cfg!(windows) && error.kind() == std::io::ErrorKind::PermissionDenied;
                last_error = Some(error);
                if attempt >= attempts || !retryable {
                    break;
                }
                std::thread::sleep(Duration::from_secs_f64(
                    ATOMIC_REPLACE_RETRY_SECONDS * f64::from(attempt),
                ));
            }
        }
    }
    Err(last_error.expect("循环内必然记录过错误"))
}

/// 尽量把目录项刷盘；Windows 目录句柄通常不支持，忽略失败。
fn fsync_directory(directory: &Path) {
    if let Ok(file) = File::open(directory) {
        let _ = file.sync_all();
    }
}

#[cfg(windows)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    // 全零即所需状态：锁定区间从偏移 0 开始、不使用事件句柄。
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle() as isize,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn unlock_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    // 全零即所需状态：锁定区间从偏移 0 开始、不使用事件句柄。
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    unsafe {
        UnlockFileEx(file.as_raw_handle() as isize, 0, 1, 0, &mut overlapped);
    }
}

#[cfg(unix)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn unlock_file(file: &File) {
    use std::os::unix::io::AsRawFd;

    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}
