//! Agent 临时工作区（对应 `omnicrawl/workspace/temp.py`）：创建分类子目录、按间隔清理、
//! 后台清理线程与启动补清理。
//!
//! 清理边界固定在工作区内的临时目录内部：每个待删条目先解析再校验位置，避免路径穿越、
//! 符号链接或配置错误把删除范围带到工作区之外。
//!
//! 与 Python 的差异（见 crate `README.md`）：
//!
//! - **时间源**：Python 用 naive local `datetime`（`datetime.now()` / `fromtimestamp`），
//!   Rust 用 `chrono::DateTime<Local>`；同一时刻的 ISO 秒级文本一致。跨 DST 切换的差减
//!   两侧可能差一小时（Python 的 naive 差减忽略 DST），Rust 保留真实时长。
//! - **后台线程**：`close` 用 `join` 直接等待（Python 是 `join(timeout=1)` 后放任守护线程）；
//!   调度线程的等待点都会被唤醒，因此退出同样很快。
//! - **路径集合判定**：Python 的 `Path.parts` 会吃掉 `.` 段，Rust 的 `components()` 不会；
//!   这里显式跳过 `CurDir`，因此 `a/./b` 与 `.`/`..`/空串的接受与拒绝和 Python 逐一对应。

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Duration as ChronoDuration, Local, TimeZone};

use omnicrawl_config::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use omnicrawl_config::error::ConfigError;
use omnicrawl_config::toml::{Table, Value};

use crate::paths::{expand_user, resolve_path};

/// 默认临时目录（工作区内相对路径）。
pub const DEFAULT_AGENT_TEMP_DIRECTORY: &str = ".omnicrawl/.agent_tmp";
/// 默认清理间隔（小时）。
pub const DEFAULT_AGENT_TEMP_CLEANUP_INTERVAL_HOURS: i64 = 24;
/// 默认分类子目录。
pub const DEFAULT_AGENT_TEMP_SUBDIRECTORIES: [&str; 6] =
    ["files", "images", "code", "videos", "scripts", "audio"];
/// 上次清理时间的标记文件名。
pub const LAST_CLEANUP_FILENAME: &str = ".last_cleanup";
/// 清理时保留的根级条目。
pub const PRESERVED_ROOT_NAMES: [&str; 3] = [".gitignore", "README.md", LAST_CLEANUP_FILENAME];

/// 临时工作区配置、初始化或清理失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTempWorkspaceError {
    message: String,
}

impl AgentTempWorkspaceError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for AgentTempWorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AgentTempWorkspaceError {}

/// Agent 临时工作区配置。
///
/// `enabled` 控制是否创建并暴露临时目录；`cleanup_enabled` 控制启动补清理和后台清理线程。
/// `directory` 必须是工作区内的相对路径，避免清理任务越界影响用户文件。
/// `cleanup_interval_hours` 是两次清理之间的最小间隔；上次清理时间记录在 `.last_cleanup`
/// 的文件时间里，因此 Agent 下次启动时也能补上错过的清理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTempWorkspaceConfig {
    pub enabled: bool,
    pub directory: String,
    pub cleanup_enabled: bool,
    pub cleanup_interval_hours: i64,
    pub subdirectories: Vec<String>,
}

impl Default for AgentTempWorkspaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: DEFAULT_AGENT_TEMP_DIRECTORY.to_string(),
            cleanup_enabled: true,
            cleanup_interval_hours: DEFAULT_AGENT_TEMP_CLEANUP_INTERVAL_HOURS,
            subdirectories: DEFAULT_AGENT_TEMP_SUBDIRECTORIES
                .iter()
                .map(|item| item.to_string())
                .collect(),
        }
    }
}

/// 一次临时目录清理的结果，便于日志、测试和命令行输出复用。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTempCleanupResult {
    pub root: PathBuf,
    pub deleted_entries: Vec<String>,
    pub failed_entries: Vec<String>,
    pub cleaned_at: DateTime<Local>,
}

/// 时间源：Python 的 `now_factory` 注入点（缺省是本地当前时间）。
pub type NowFactory = Box<dyn Fn() -> DateTime<Local> + Send + Sync>;

/// 临时目录的核心状态：句柄与后台调度线程共享同一份。
pub struct TempCore {
    pub workspace_root: PathBuf,
    pub config: AgentTempWorkspaceConfig,
    pub root: PathBuf,
    now_factory: Arc<NowFactory>,
}

impl TempCore {
    /// 按配置解析出临时目录；`workspace_root` 与 Python 一样先解析成绝对路径。
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        config: AgentTempWorkspaceConfig,
        now_factory: Arc<NowFactory>,
    ) -> Result<Self, AgentTempWorkspaceError> {
        let workspace_root = resolve_path(&expand_user(&workspace_root.into().to_string_lossy()));
        let root = resolve_agent_temp_dir(&workspace_root, &config.directory)?;
        Ok(Self {
            workspace_root,
            config,
            root,
            now_factory,
        })
    }

    fn now(&self) -> DateTime<Local> {
        (self.now_factory)()
    }

    /// 适合提示词和终端展示的相对路径（分隔符与平台一致，和 Python 的 `str(Path)` 相同）。
    pub fn display_path(&self) -> String {
        match self.root.strip_prefix(&self.workspace_root) {
            Ok(relative) => relative.to_string_lossy().to_string(),
            Err(_) => self.root.to_string_lossy().to_string(),
        }
    }

    /// 创建临时目录、分类子目录和本地说明文件。
    ///
    /// 说明文件放在目录根部并在清理时保留；真正的一次性产物放进分类子目录，这样间隔清理
    /// 可以删除工作内容，同时保留目录用途说明。
    pub fn ensure(&self) -> Result<(), AgentTempWorkspaceError> {
        if !self.config.enabled {
            return Ok(());
        }
        if let Err(error) = std::fs::create_dir_all(&self.root) {
            return Err(self.init_error(error));
        }
        for name in &self.config.subdirectories {
            let child = self.resolve_child(name)?;
            if let Err(error) = std::fs::create_dir_all(&child) {
                return Err(self.init_error(error));
            }
        }
        self.write_marker_files()
    }

    /// 清空临时目录里的临时产物，并重建分类子目录。
    pub fn clean(
        &self,
        now: Option<DateTime<Local>>,
    ) -> Result<AgentTempCleanupResult, AgentTempWorkspaceError> {
        let cleaned_at = now.unwrap_or_else(|| self.now());
        if !self.config.enabled {
            return Ok(AgentTempCleanupResult {
                root: self.root.clone(),
                deleted_entries: Vec::new(),
                failed_entries: Vec::new(),
                cleaned_at,
            });
        }
        self.ensure()?;

        let mut deleted_entries: Vec<String> = Vec::new();
        let mut failed_entries: Vec<String> = Vec::new();
        for entry in self.root_entries()? {
            let name = entry
                .file_name()
                .map(|item| item.to_string_lossy().to_string())
                .unwrap_or_default();
            if PRESERVED_ROOT_NAMES.contains(&name.as_str())
                && (name != LAST_CLEANUP_FILENAME || entry.is_file())
            {
                continue;
            }
            match delete_temp_entry(&self.root, &entry) {
                Ok(()) => deleted_entries.push(name),
                Err(_) => failed_entries.push(name),
            }
        }

        for name in &self.config.subdirectories {
            let child = self.resolve_child(name)?;
            if let Err(error) = std::fs::create_dir_all(&child) {
                return Err(AgentTempWorkspaceError::new(format!(
                    "重建 Agent 临时目录分类子目录失败：{error}"
                )));
            }
        }

        self.touch_cleanup_marker(cleaned_at)?;
        Ok(AgentTempCleanupResult {
            root: self.root.clone(),
            deleted_entries,
            failed_entries,
            cleaned_at,
        })
    }

    /// 如果距离上次清理已达到配置间隔，则立即补清理。
    ///
    /// 这一步解决「程序没有持续运行到清理时间点」的现实情况：上次清理时间写在
    /// `.last_cleanup` 的文件时间里，启动时只要发现它缺失或过期，就同步清理一次。
    pub fn clean_if_due(
        &self,
        now: Option<DateTime<Local>>,
    ) -> Result<Option<AgentTempCleanupResult>, AgentTempWorkspaceError> {
        if !self.config.enabled || !self.config.cleanup_enabled {
            return Ok(None);
        }
        let checked_at = now.unwrap_or_else(|| self.now());
        self.ensure()?;
        if !self.is_cleanup_due(Some(checked_at)) {
            return Ok(None);
        }
        self.clean(Some(checked_at)).map(Some)
    }

    /// 判断当前是否已达到清理间隔。
    pub fn is_cleanup_due(&self, now: Option<DateTime<Local>>) -> bool {
        if !self.config.enabled || !self.config.cleanup_enabled {
            return false;
        }
        let Some(last_cleanup_at) = self.last_cleanup_time() else {
            return true;
        };
        let current = now.unwrap_or_else(|| self.now());
        let interval = ChronoDuration::hours(self.config.cleanup_interval_hours);
        current - last_cleanup_at >= interval
    }

    /// 读取 `.last_cleanup` 的文件修改时间；缺失时表示从未记录过清理。
    pub fn last_cleanup_time(&self) -> Option<DateTime<Local>> {
        let marker = self.root.join(LAST_CLEANUP_FILENAME);
        let metadata = std::fs::metadata(&marker).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata.modified().ok()?;
        system_time_to_local(modified)
    }

    /// 计算距离下一次间隔清理的秒数，单独暴露便于测试边界时间。
    pub fn seconds_until_next_cleanup(&self, now: Option<DateTime<Local>>) -> f64 {
        let current = now.unwrap_or_else(|| self.now());
        let Some(last_cleanup_at) = self.last_cleanup_time() else {
            return 1.0;
        };
        let target = last_cleanup_at + ChronoDuration::hours(self.config.cleanup_interval_hours);
        seconds_between(current, target)
    }

    fn init_error(&self, error: std::io::Error) -> AgentTempWorkspaceError {
        AgentTempWorkspaceError::new(format!(
            "初始化 Agent 临时目录失败：{}，{error}",
            self.root.display()
        ))
    }

    /// 根目录下按 `name.lower()` 排序的条目（对映 Python 的 `sorted(..., key=name.lower)`）。
    fn root_entries(&self) -> Result<Vec<PathBuf>, AgentTempWorkspaceError> {
        let listing = std::fs::read_dir(&self.root).map_err(|error| self.init_error(error))?;
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in listing {
            entries.push(entry.map_err(|error| self.init_error(error))?.path());
        }
        // 仅大小写不同的名字在两侧都可能有任意相对顺序，因此排序键里再带上原名字，
        // 保证同一批输入在两侧得到同一个顺序。
        entries.sort_by_key(|path| {
            let name = path
                .file_name()
                .map(|item| item.to_string_lossy().to_string())
                .unwrap_or_default();
            (name.to_lowercase(), name)
        });
        Ok(entries)
    }

    fn resolve_child(&self, relative_path: &str) -> Result<PathBuf, AgentTempWorkspaceError> {
        resolve_temp_child(&self.root, relative_path)
    }
}

/// 删除临时目录里的一个条目（对映 Python `AgentTempWorkspace._delete_entry`）。
///
/// 符号链接只删链接本身（不跟随目标），因此边界始终是「链接文件位于临时目录内」；其余条目
/// 先 `resolve` 再校验是否仍在临时目录内，避免路径穿越把删除范围带到工作区之外。
///
/// 公开出来是为了让对照测试能直接重放 Python 的用例（Python 侧同名的 `_delete_entry` 也
/// 是私有方法，但它是清理边界的一部分）。
pub fn delete_temp_entry(root: &Path, entry: &Path) -> Result<(), AgentTempWorkspaceError> {
    if is_symlink(entry) {
        // 符号链接的目标可能指向项目外，但删除链接本身仍只受「链接文件位于临时目录内」
        // 约束；不能跟随目标去扩大清理边界。
        let parent = resolve_path(entry.parent().unwrap_or(entry));
        if parent != *root && !crate::is_relative_to(&parent, root) {
            return Err(AgentTempWorkspaceError::new(format!(
                "拒绝清理 Agent 临时目录外路径：{}",
                entry.display()
            )));
        }
        return std::fs::remove_file(entry).map_err(|error| {
            AgentTempWorkspaceError::new(format!(
                "删除 Agent 临时目录条目失败：{}，{error}",
                entry.display()
            ))
        });
    }

    let resolved = resolve_path(entry);
    if resolved == *root || !crate::is_relative_to(&resolved, root) {
        return Err(AgentTempWorkspaceError::new(format!(
            "拒绝清理 Agent 临时目录外路径：{}",
            entry.display()
        )));
    }
    let outcome = if entry.is_dir() {
        std::fs::remove_dir_all(entry)
    } else {
        std::fs::remove_file(entry)
    };
    outcome.map_err(|error| {
        AgentTempWorkspaceError::new(format!(
            "删除 Agent 临时目录条目失败：{}，{error}",
            entry.display()
        ))
    })
}

impl TempCore {
    fn write_marker_files(&self) -> Result<(), AgentTempWorkspaceError> {
        let readme_path = self.root.join("README.md");
        if !readme_path.exists() {
            std::fs::write(&readme_path, temp_workspace_readme().as_bytes())
                .map_err(|error| self.init_error(error))?;
        }
        let gitignore_path = self.root.join(".gitignore");
        if !gitignore_path.exists() {
            std::fs::write(&gitignore_path, b"*\n!.gitignore\n!README.md\n")
                .map_err(|error| self.init_error(error))?;
        }
        Ok(())
    }

    fn touch_cleanup_marker(
        &self,
        cleaned_at: DateTime<Local>,
    ) -> Result<(), AgentTempWorkspaceError> {
        let marker_path = self.root.join(LAST_CLEANUP_FILENAME);
        let text = format!("last_cleanup={}\n", format_seconds(cleaned_at));
        let failure = |error: std::io::Error| {
            AgentTempWorkspaceError::new(format!(
                "更新 Agent 临时目录清理标记失败：{}，{error}",
                marker_path.display()
            ))
        };
        let mut file = std::fs::File::create(&marker_path).map_err(failure)?;
        file.write_all(text.as_bytes()).map_err(failure)?;
        file.flush().map_err(failure)?;
        file.set_modified(local_to_system_time(cleaned_at))
            .map_err(failure)
    }
}

/// 管理 Agent 专用临时目录的句柄：核心状态 + 后台清理线程。
pub struct AgentTempWorkspace {
    core: Arc<TempCore>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AgentTempWorkspace {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        config: Option<AgentTempWorkspaceConfig>,
    ) -> Result<Self, AgentTempWorkspaceError> {
        let core = Arc::new(TempCore::new(
            workspace_root,
            config.unwrap_or_default(),
            Arc::new(Box::new(Local::now)),
        )?);
        Ok(Self {
            core,
            stop: Arc::new((Mutex::new(false), Condvar::new())),
            thread: None,
        })
    }

    /// 替换时间源（Python 的 `now_factory` 参数）。
    ///
    /// 必须在 `start_scheduler` 之前调用：调度线程持有的是当时的核心状态。
    pub fn with_now_factory(mut self, now_factory: NowFactory) -> Self {
        self.core = Arc::new(TempCore {
            workspace_root: self.core.workspace_root.clone(),
            config: self.core.config.clone(),
            root: self.core.root.clone(),
            now_factory: Arc::new(now_factory),
        });
        self
    }

    pub fn core(&self) -> &Arc<TempCore> {
        &self.core
    }

    pub fn workspace_root(&self) -> &Path {
        &self.core.workspace_root
    }

    pub fn config(&self) -> &AgentTempWorkspaceConfig {
        &self.core.config
    }

    pub fn root(&self) -> &Path {
        &self.core.root
    }

    /// 适合提示词和终端展示的相对路径。
    pub fn display_path(&self) -> String {
        self.core.display_path()
    }

    pub fn ensure(&self) -> Result<(), AgentTempWorkspaceError> {
        self.core.ensure()
    }

    pub fn clean(
        &self,
        now: Option<DateTime<Local>>,
    ) -> Result<AgentTempCleanupResult, AgentTempWorkspaceError> {
        self.core.clean(now)
    }

    pub fn clean_if_due(
        &self,
        now: Option<DateTime<Local>>,
    ) -> Result<Option<AgentTempCleanupResult>, AgentTempWorkspaceError> {
        self.core.clean_if_due(now)
    }

    pub fn is_cleanup_due(&self, now: Option<DateTime<Local>>) -> bool {
        self.core.is_cleanup_due(now)
    }

    pub fn last_cleanup_time(&self) -> Option<DateTime<Local>> {
        self.core.last_cleanup_time()
    }

    pub fn seconds_until_next_cleanup(&self, now: Option<DateTime<Local>>) -> f64 {
        self.core.seconds_until_next_cleanup(now)
    }

    /// 启动后台清理线程；线程只在 Agent 进程存活期间工作。
    pub fn start_scheduler(&mut self) {
        if !self.core.config.enabled || !self.core.config.cleanup_enabled {
            return;
        }
        if self
            .thread
            .as_ref()
            .map(|thread| !thread.is_finished())
            .unwrap_or(false)
        {
            return;
        }
        {
            let (lock, _) = &*self.stop;
            *lock.lock().unwrap_or_else(|item| item.into_inner()) = false;
        }
        let core = Arc::clone(&self.core);
        let stop = Arc::clone(&self.stop);
        self.thread = Some(
            std::thread::Builder::new()
                .name("agent-temp-cleanup".to_string())
                .spawn(move || run_scheduler(&core, &stop))
                .unwrap_or_else(|_| std::thread::spawn(|| {})),
        );
    }

    /// 停止后台清理线程，避免程序退出时留下悬挂工作。
    pub fn close(&mut self) {
        {
            let (lock, condvar) = &*self.stop;
            *lock.lock().unwrap_or_else(|item| item.into_inner()) = true;
            condvar.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 后台线程的调度循环：按间隔醒来并补清理。
fn run_scheduler(core: &TempCore, stop: &(Mutex<bool>, Condvar)) {
    let (lock, condvar) = stop;
    loop {
        let wait_seconds = core.seconds_until_next_cleanup(None).max(0.05);
        let guard = lock.lock().unwrap_or_else(|item| item.into_inner());
        if *guard {
            return;
        }
        let (guard, _) = condvar
            .wait_timeout(guard, Duration::from_secs_f64(wait_seconds))
            .unwrap_or_else(|item| item.into_inner());
        let stopped = *guard;
        drop(guard);
        if stopped {
            return;
        }
        let _ = core.clean_if_due(None);
    }
}

/// 校验并解析临时目录的子路径（对映 Python `AgentTempWorkspace._resolve_child`）。
///
/// 公开出来是为了让对照测试能直接重放 Python 的用例：`_resolve_child` 在 Python 侧虽是
/// 私有方法，但它的判定（不安全 / 越界两档报错）是清理边界的一部分。
pub fn resolve_temp_child(
    root: &Path,
    relative_path: &str,
) -> Result<PathBuf, AgentTempWorkspaceError> {
    if !is_safe_relative(relative_path, false) {
        return Err(AgentTempWorkspaceError::new(format!(
            "Agent 临时目录子路径不安全：{relative_path}"
        )));
    }
    let resolved = resolve_path(&root.join(relative_path));
    if resolved == *root || !crate::is_relative_to(&resolved, root) {
        return Err(AgentTempWorkspaceError::new(format!(
            "Agent 临时目录子路径越界：{relative_path}"
        )));
    }
    Ok(resolved)
}

/// `Path` 段校验，对映 Python 的
/// `child.is_absolute() or not child.parts or any(part in {"", ".", ".."})`：
///
/// - 绝对路径直接拒绝（Python 先查 `is_absolute()`）；
/// - `Path` 会吃掉单独的 `.` 段，因此 `CurDir` 不算不安全段；`..`（`ParentDir`）拒绝；
/// - 根 / 盘符段**不**在这里拒绝：Python 侧 `/x` 在 Windows 上会被拆成 `"\\"` 与 `"x"`
///   两段，既不是空段也不是 `.`/`..`，因此能走到后续「是否落在临时目录内」的判定并给出
///   越界报错。这里保持同一顺序，两侧报错才一致。
/// - `allow_empty` 对映「`Path.parts` 为空是否可接受」：`.`/空串没有普通段。
fn is_safe_relative(text: &str, allow_empty: bool) -> bool {
    let path = Path::new(text);
    if path.is_absolute() {
        return false;
    }
    let mut normal_parts = 0usize;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => normal_parts += 1,
            Component::ParentDir => return false,
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    allow_empty || normal_parts > 0
}

/// 从 `config.toml` 的 `agent_temp` 段读取临时工作区配置。
pub fn load_agent_temp_workspace_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<AgentTempWorkspaceConfig, AgentTempWorkspaceError> {
    let data = load_config_data(env, config_path).map_err(config_error)?;
    let section = get_section(&data, "agent_temp").map_err(config_error)?;
    Ok(AgentTempWorkspaceConfig {
        enabled: read_bool_config(&section, "enabled", true)?,
        directory: read_text_config(&section, "directory", DEFAULT_AGENT_TEMP_DIRECTORY)?,
        cleanup_enabled: read_bool_config(&section, "cleanup_enabled", true)?,
        cleanup_interval_hours: read_positive_int_config(
            &section,
            "cleanup_interval_hours",
            DEFAULT_AGENT_TEMP_CLEANUP_INTERVAL_HOURS,
        )?,
        subdirectories: DEFAULT_AGENT_TEMP_SUBDIRECTORIES
            .iter()
            .map(|item| item.to_string())
            .collect(),
    })
}

fn config_error(error: ConfigError) -> AgentTempWorkspaceError {
    AgentTempWorkspaceError::new(error.message().to_string())
}

/// 把临时目录配置解析为工作区内的绝对路径。
pub fn resolve_agent_temp_dir(
    workspace_root: &Path,
    directory: &str,
) -> Result<PathBuf, AgentTempWorkspaceError> {
    let raw = directory.trim();
    if raw.is_empty() {
        return Err(AgentTempWorkspaceError::new(
            "配置项 agent_temp.directory 必须是非空字符串。".to_string(),
        ));
    }
    // Python 侧 `.` 会被 `Path.parts` 吃掉，因此这里也允许空段：`"."` 落到下一条「指向
    // 工作区根目录」的报错上，与 Python 一致。
    if !is_safe_relative(raw, true) {
        return Err(AgentTempWorkspaceError::new(
            "配置项 agent_temp.directory 必须是工作区内的普通相对路径。".to_string(),
        ));
    }
    let workspace = resolve_path(workspace_root);
    let resolved = resolve_path(&workspace.join(raw));
    if resolved == workspace || !crate::is_relative_to(&resolved, &workspace) {
        return Err(AgentTempWorkspaceError::new(
            "配置项 agent_temp.directory 不能指向工作区根目录或工作区外。".to_string(),
        ));
    }
    Ok(resolved)
}

/// 启动面板里使用的简短状态文本。
pub fn agent_temp_status_label(config: &AgentTempWorkspaceConfig) -> String {
    if !config.enabled {
        return "关闭".to_string();
    }
    if !config.cleanup_enabled {
        return format!("{}，自动清理关闭", config.directory);
    }
    format!(
        "{}，每 {} 小时自动清理",
        config.directory, config.cleanup_interval_hours
    )
}

/// 临时目录里的 `README.md` 正文（清理时保留）。
pub fn temp_workspace_readme() -> String {
    let mut text = String::new();
    text.push_str("# Agent 临时目录\n\n");
    text.push_str("这个目录用于存放 Agent 工作时产生的一次性文件、图片、代码、视频和脚本。\n\n");
    text.push_str("- `files/`：普通临时文件和中间结果。\n");
    text.push_str("- `images/`：截图、生成图片和图像处理中间文件。\n");
    text.push_str("- `code/`：一次性验证代码、草稿代码和临时样例。\n");
    text.push_str("- `videos/`：临时视频、录屏和转码中间文件。\n");
    text.push_str("- `scripts/`：只为当前任务服务的临时脚本。\n");
    text.push_str("- `audio/`：临时音频、语音和转码中间文件。\n\n");
    text.push_str("长期需要保留的交付物不要放在这里。Agent 会通过 `.last_cleanup` 的时间戳");
    text.push_str("按约 24 小时间隔清理临时内容，并在清理后重建上述分类子目录。\n");
    text
}

/// 清理时保留的根级条目集合。
pub fn preserved_root_names() -> BTreeSet<String> {
    PRESERVED_ROOT_NAMES
        .iter()
        .map(|item| item.to_string())
        .collect()
}

fn read_bool_config(
    section: &Table,
    key: &str,
    default: bool,
) -> Result<bool, AgentTempWorkspaceError> {
    match section.get(key) {
        None => Ok(default),
        Some(Value::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(AgentTempWorkspaceError::new(format!(
            "配置项 agent_temp.{key} 必须是布尔值 true 或 false。"
        ))),
    }
}

fn read_text_config(
    section: &Table,
    key: &str,
    default: &str,
) -> Result<String, AgentTempWorkspaceError> {
    match section.get(key) {
        None => Ok(default.to_string()),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(default.to_string())
            } else {
                Ok(trimmed.to_string())
            }
        }
        Some(_) => Err(AgentTempWorkspaceError::new(format!(
            "配置项 agent_temp.{key} 必须是字符串。"
        ))),
    }
}

fn read_positive_int_config(
    section: &Table,
    key: &str,
    default: i64,
) -> Result<i64, AgentTempWorkspaceError> {
    match section.get(key) {
        None => Ok(default),
        Some(Value::Integer(value)) if *value > 0 => Ok(*value),
        Some(_) => Err(AgentTempWorkspaceError::new(format!(
            "配置项 agent_temp.{key} 必须是大于 0 的整数。"
        ))),
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// `datetime.isoformat(timespec="seconds")`：本地时间、无时区后缀。
pub fn format_seconds(moment: DateTime<Local>) -> String {
    moment.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// 本地时间 → `SystemTime`（`os.utime` 的等价物）。
pub fn local_to_system_time(moment: DateTime<Local>) -> SystemTime {
    let seconds = moment.timestamp();
    if seconds >= 0 {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    }
}

/// `SystemTime` → 本地时间（`datetime.fromtimestamp` 的等价物）。
pub fn system_time_to_local(time: SystemTime) -> Option<DateTime<Local>> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    Local
        .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
        .single()
}

fn seconds_between(current: DateTime<Local>, target: DateTime<Local>) -> f64 {
    let delta = target - current;
    (delta.num_milliseconds() as f64 / 1000.0).max(1.0)
}
