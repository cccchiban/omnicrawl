//! 严格使用 TOML 的运行配置仓库，支持 UTF-8 读取与原子写回。
//!
//! 对应 `omnicrawl/config/core/runtime.py`。与 Python 的差别只有两处，都写在 `README.md`：
//! 进程外信息（home、平台、`APPDATA`/`XDG_CONFIG_HOME`、三个路径环境变量）由
//! [`ConfigEnvironment`] 注入；`project_root` 与依赖它的 `_is_development_environment`
//! 不搬（Python 侧已明确它们不参与默认路径解析）。

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::ConfigError;
use crate::toml::{self, Table, Value};

pub const DEFAULT_CONFIG_FILENAME: &str = "config.toml";
pub const DEFAULT_MODELS_FILENAME: &str = "models.toml";
pub const DEFAULT_SUBAGENTS_FILENAME: &str = "subagents.toml";
pub const GLOBAL_AGENTS_FILENAME: &str = "AGENTS.md";
pub const USER_CONFIG_DIRNAME: &str = ".OmniCrawl";
pub const CONFIG_PATH_ENV: &str = "AI_CONFIG_FILE";
pub const MODELS_PATH_ENV: &str = "AI_MODELS_FILE";
pub const SUBAGENTS_PATH_ENV: &str = "AI_SUBAGENTS_FILE";

const TOML_SUFFIX: &str = "toml";
const ATOMIC_REPLACE_MAX_ATTEMPTS: usize = 8;
const ATOMIC_REPLACE_RETRY_SECONDS: f64 = 0.05;

/// 路径解析与配置读取需要知道的进程外信息。
///
/// Python 侧直接读 `os.environ` 与 `Path.home()`；Rust 侧把它们收进一个结构，
/// 便于对照测试与让宿主覆盖（例如把配置根指向临时目录、把工作区切到隔离环境）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEnvironment {
    home: PathBuf,
    platform: String,
    process: bool,
    overrides: BTreeMap<String, String>,
}

impl ConfigEnvironment {
    /// 取进程真实环境：未覆盖的变量回落到真实进程环境。
    pub fn from_process() -> Self {
        Self {
            home: process_home().unwrap_or_default(),
            platform: std::env::consts::OS.to_string(),
            process: true,
            overrides: BTreeMap::new(),
        }
    }

    /// 隔离环境：只认显式注入的取值（与 Python 对照测试里 `os.environ` 被清空同义）。
    pub fn new(home: impl Into<PathBuf>, platform: impl Into<String>) -> Self {
        Self {
            home: home.into(),
            platform: platform.into(),
            process: false,
            overrides: BTreeMap::new(),
        }
    }

    /// 按环境变量名注入取值。
    pub fn with_env_value(mut self, name: &str, value: &str) -> Self {
        self.overrides.insert(name.to_string(), value.to_string());
        self
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// 读环境变量；未注入且未走进程环境时返回 `None`（对应 Python 的 `env.get(name)`）。
    pub fn get(&self, name: &str) -> Option<String> {
        if let Some(value) = self.overrides.get(name) {
            return Some(value.clone());
        }
        if self.process {
            std::env::var(name).ok()
        } else {
            None
        }
    }

    /// `os.getenv(name, "").strip()` 的同义写法。
    pub fn get_trimmed(&self, name: &str) -> String {
        self.get(name).unwrap_or_default().trim().to_string()
    }

    fn is_windows(&self) -> bool {
        self.platform.starts_with("win")
    }
}

/// 统一用户配置目录：`~/.OmniCrawl`，不依赖当前工作目录或安装位置。
pub fn user_config_dir(env: &ConfigEnvironment) -> PathBuf {
    env.home.join(USER_CONFIG_DIRNAME)
}

/// 升级前的用户配置目录，按兼容顺序排列。
pub fn legacy_user_config_dirs(env: &ConfigEnvironment) -> Vec<PathBuf> {
    if env.is_windows() {
        let appdata = env.get_trimmed("APPDATA");
        let base = if appdata.is_empty() {
            env.home.join("AppData").join("Roaming")
        } else {
            PathBuf::from(normalize_path_text(&appdata))
        };
        vec![base.join("OmniCrawl"), env.home.join(".omnicrawl")]
    } else {
        let config_home = env.get_trimmed("XDG_CONFIG_HOME");
        let base = if config_home.is_empty() {
            env.home.join(".config")
        } else {
            PathBuf::from(normalize_path_text(&config_home))
        };
        vec![base.join("omnicrawl"), env.home.join(".omnicrawl")]
    }
}

/// 用户级全局 `AGENTS.md` 路径。
pub fn global_agents_path(env: &ConfigEnvironment) -> PathBuf {
    user_config_dir(env).join(GLOBAL_AGENTS_FILENAME)
}

/// 把旧用户目录迁移到 `~/.OmniCrawl` 并删除旧目录。
///
/// 目标目录中已有的文件不会被覆盖：同名冲突时旧文件落到 `<name>.migrated.bak`
/// （必要时追加序号）。任何一步失败都中止并保留旧目录，避免升级过程丢配置。
pub fn migrate_legacy_user_config(
    env: &ConfigEnvironment,
    legacy_dirs: Option<&[PathBuf]>,
) -> Result<PathBuf, ConfigError> {
    let target = user_config_dir(env);
    fs::create_dir_all(&target).map_err(|error| {
        ConfigError::new(format!("创建配置目录失败：{}，{error}", target.display()))
    })?;
    let candidates: Vec<PathBuf> = match legacy_dirs {
        Some(dirs) => dirs.to_vec(),
        None => legacy_user_config_dirs(env),
    };
    for legacy in candidates {
        let legacy = expand_user(env, &legacy.to_string_lossy());
        if !legacy.is_dir() || same_path(&legacy, &target) {
            continue;
        }
        migrate_directory(&legacy, &target)?;
    }
    Ok(target)
}

fn migrate_directory(legacy: &Path, target: &Path) -> Result<(), ConfigError> {
    let entries = fs::read_dir(legacy).map_err(|error| {
        ConfigError::new(format!("检查旧配置目录失败：{}，{error}", legacy.display()))
    })?;
    let names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    let run = || -> io::Result<()> {
        for name in &names {
            let source = legacy.join(name);
            let destination = if target.join(name).exists() {
                migration_backup_path(target, name)
            } else {
                target.join(name)
            };
            fs::rename(&source, &destination)?;
        }
        fs::remove_dir(legacy)
    };
    run().map_err(|error| {
        ConfigError::new(format!(
            "迁移旧配置目录失败：{} -> {}，{error}。旧目录已保留，请修复权限后重试。",
            legacy.display(),
            target.display()
        ))
    })
}

/// 为迁移冲突生成不覆盖既有文件的备份路径。
fn migration_backup_path(target: &Path, name: &str) -> PathBuf {
    let mut candidate = target.join(format!("{name}.migrated.bak"));
    let mut index = 1;
    while candidate.exists() {
        candidate = target.join(format!("{name}.migrated.{index}.bak"));
        index += 1;
    }
    candidate
}

/// 用户默认运行配置路径。
pub fn default_config_path(env: &ConfigEnvironment) -> PathBuf {
    user_config_dir(env).join(DEFAULT_CONFIG_FILENAME)
}

/// 兼容既有调用名；默认配置本身就是 TOML。
pub fn default_toml_config_path(env: &ConfigEnvironment) -> PathBuf {
    default_config_path(env)
}

/// 用户默认模型目录配置路径。
pub fn default_models_path(env: &ConfigEnvironment) -> PathBuf {
    user_config_dir(env).join(DEFAULT_MODELS_FILENAME)
}

/// 用户默认子代理设置配置路径。
pub fn default_subagents_path(env: &ConfigEnvironment) -> PathBuf {
    user_config_dir(env).join(DEFAULT_SUBAGENTS_FILENAME)
}

/// 显式路径或对应环境变量优先；否则始终使用用户目录配置。
pub fn resolve_config_path(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_path(
        env,
        config_path,
        CONFIG_PATH_ENV,
        DEFAULT_CONFIG_FILENAME,
        "运行配置",
    )
}

/// 显式路径或对应环境变量优先；否则始终使用用户目录配置。
pub fn resolve_models_path(
    env: &ConfigEnvironment,
    models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_path(
        env,
        models_path,
        MODELS_PATH_ENV,
        DEFAULT_MODELS_FILENAME,
        "模型配置",
    )
}

/// 显式路径或对应环境变量优先；否则始终使用用户目录配置。
pub fn resolve_subagents_path(
    env: &ConfigEnvironment,
    subagents_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_path(
        env,
        subagents_path,
        SUBAGENTS_PATH_ENV,
        DEFAULT_SUBAGENTS_FILENAME,
        "子代理设置",
    )
}

/// 解析配置写入路径；未显式指定时始终写入用户目录。
///
/// 与读路径同源（Python 侧两者也只是分工不同的包装），保留独立名字以便调用点自述意图。
pub fn resolve_config_write_path(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_config_path(env, config_path)
}

/// 解析模型配置写入路径；未显式指定时始终写入用户目录。
pub fn resolve_models_write_path(
    env: &ConfigEnvironment,
    models_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_models_path(env, models_path)
}

/// 解析子代理设置写入路径；未显式指定时始终写入用户目录。
pub fn resolve_subagents_write_path(
    env: &ConfigEnvironment,
    subagents_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    resolve_subagents_path(env, subagents_path)
}

fn resolve_path(
    env: &ConfigEnvironment,
    explicit: Option<&Path>,
    env_name: &str,
    filename: &str,
    source: &str,
) -> Result<PathBuf, ConfigError> {
    let path = match explicit {
        Some(path) => expand_user(env, &path.to_string_lossy()),
        None => {
            let raw = env.get_trimmed(env_name);
            if raw.is_empty() {
                user_config_dir(env).join(filename)
            } else {
                expand_user(env, &raw)
            }
        }
    };
    validate_toml_path(&path, source)?;
    Ok(path)
}

/// 读取 TOML 运行配置；文件不存在时返回空表。
pub fn load_config_data(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<Table, ConfigError> {
    let path = resolve_config_path(env, config_path)?;
    if path.exists() {
        return load_mapping_file(&path);
    }
    // 只检测默认位置的遗留文件以给出可操作错误：不解析、不迁移，也不当作配置源。
    // 显式路径与 AI_CONFIG_FILE 已在扩展名校验阶段拒绝 JSON。
    if config_path.is_none() && env.get_trimmed(CONFIG_PATH_ENV).is_empty() {
        let legacy_path = path.with_extension("json");
        if legacy_path.exists() {
            return Err(ConfigError::new(
                "检测到不再支持的 config.json，且 config.toml 不存在。\
                 请根据 config.example.toml 手工创建 config.toml；程序不会读取或自动迁移 JSON。",
            ));
        }
    }
    Ok(Table::new())
}

/// 把运行配置以 TOML 原子写回。
pub fn save_config_data(
    env: &ConfigEnvironment,
    data: &Table,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let path = resolve_config_write_path(env, config_path)?;
    atomic_write_text(&path, &toml::dump_document(data))?;
    Ok(path)
}

/// 安全读取配置子对象。
pub fn get_section(data: &Table, key: &str) -> Result<Table, ConfigError> {
    match data.get(key) {
        None => Ok(Table::new()),
        Some(Value::String(text)) if text.is_empty() => Ok(Table::new()),
        Some(Value::Table(section)) => Ok(section.clone()),
        Some(_) => Err(ConfigError::new(format!("配置项 {key} 必须是对象。"))),
    }
}

/// 底层加载，供模型 store 等配置模块复用。
pub fn load_raw_file(path: &Path) -> Result<Table, ConfigError> {
    load_mapping_file(path)
}

/// 原子写文本（同目录临时文件 + 替换）。
pub fn atomic_write_text(path: &Path, text: &str) -> Result<(), ConfigError> {
    write_atomic(path, text)
        .map_err(|error| ConfigError::new(format!("写入配置文件失败：{}，{error}", path.display())))
}

/// 兼容既有调用名。
pub fn dump_toml_text(data: &Table) -> String {
    toml::dump_document(data)
}

/// 兼容既有调用名；实际输出 TOML 文本。
pub fn dump_yaml_text(data: &Table) -> String {
    toml::dump_document(data)
}

fn validate_toml_path(path: &Path, source: &str) -> Result<(), ConfigError> {
    let suffix = path
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if suffix != TOML_SUFFIX {
        return Err(ConfigError::new(format!(
            "{source}仅支持 .toml 文件：{}。JSON 配置已停止支持。",
            path.display()
        )));
    }
    Ok(())
}

fn load_mapping_file(path: &Path) -> Result<Table, ConfigError> {
    validate_toml_path(path, "配置文件")?;
    let bytes = fs::read(path).map_err(|error| {
        ConfigError::new(format!("读取配置文件失败：{}，{error}", path.display()))
    })?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let text = String::from_utf8(bytes.to_vec()).map_err(|error| {
        ConfigError::new(format!("读取配置文件失败：{}，{error}", path.display()))
    })?;
    if text.trim().is_empty() {
        return Ok(Table::new());
    }
    toml::parse_document(&text).map_err(|error| {
        ConfigError::new(format!(
            "配置文件 TOML 解析失败：{}，{error}",
            path.display()
        ))
    })
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let (mut handle, temp_path) = create_temp_file(path)?;
    let mut outcome = handle
        .write_all(text.as_bytes())
        .and_then(|()| handle.flush())
        .and_then(|()| handle.sync_all());
    drop(handle);
    if outcome.is_ok() {
        outcome = replace_with_retry(&temp_path, path);
    }
    if outcome.is_err() && temp_path.exists() {
        let _ = fs::remove_file(&temp_path);
    }
    outcome
}

fn create_temp_file(path: &Path) -> io::Result<(File, PathBuf)> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).rotate_left(17);
    for attempt in 0u64..64 {
        let candidate = parent.join(format!(".{name}.{:08x}.tmp", (seed + attempt) as u32));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(handle) => return Ok((handle, candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "无法创建临时配置文件",
    ))
}

/// 原子替换；Windows 上短暂的 Access Denied 时短退避重试（与 Python 侧同参数）。
fn replace_with_retry(temp_path: &Path, path: &Path) -> io::Result<()> {
    let attempts = if cfg!(windows) {
        ATOMIC_REPLACE_MAX_ATTEMPTS
    } else {
        1
    };
    for attempt in 1..=attempts {
        match fs::rename(temp_path, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if attempt >= attempts || !is_transient_windows_access_denied(&error) {
                    return Err(error);
                }
                thread::sleep(Duration::from_secs_f64(
                    ATOMIC_REPLACE_RETRY_SECONDS * attempt as f64,
                ));
            }
        }
    }
    Ok(())
}

fn is_transient_windows_access_denied(error: &io::Error) -> bool {
    if !cfg!(windows) {
        return false;
    }
    error.kind() == io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(5) | Some(13))
}

/// `~` / `~/x` 展开；`~user` 依赖平台账号库，Python 在 Windows 上也直接报错，故原样保留。
pub(crate) fn expand_user(env: &ConfigEnvironment, raw: &str) -> PathBuf {
    if raw == "~" {
        return env.home.clone();
    }
    for prefix in ["~/", "~\\"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let rest = normalize_path_text(rest);
            if rest == "." {
                return env.home.clone();
            }
            return env.home.join(rest);
        }
    }
    PathBuf::from(normalize_path_text(raw))
}

/// 相对路径按当前目录绝对化，并规范化 `.` / `..` 段（对齐 `Path.resolve()` 的路径观感，不解析符号链接）。
pub(crate) fn absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => result.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            Component::Normal(part) => result.push(part),
        }
    }
    result
}

/// 与 `pathlib` 同形的字符串化：统一分隔符、去掉 `.` 段、合并重复分隔符、去掉尾随分隔符。
///
/// `..` 段按 `pathlib` 一样保留；空路径归一成 `.`。这些形状会进错文案，所以必须对齐。
fn normalize_path_text(raw: &str) -> String {
    let windows = cfg!(windows);
    let separator = if windows { '\\' } else { '/' };
    let unified = if windows {
        raw.replace('/', "\\")
    } else {
        raw.to_string()
    };
    let mut prefix = String::new();
    let mut rest = unified.as_str();
    if windows {
        if let Some(stripped) = rest.strip_prefix("\\\\") {
            prefix.push_str("\\\\");
            rest = stripped;
        } else if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
            prefix.push_str(&rest[..2]);
            rest = &rest[2..];
            if let Some(stripped) = rest.strip_prefix('\\') {
                prefix.push('\\');
                rest = stripped;
            }
        } else if let Some(stripped) = rest.strip_prefix('\\') {
            prefix.push('\\');
            rest = stripped;
        }
    } else if let Some(stripped) = rest.strip_prefix('/') {
        prefix.push('/');
        rest = stripped;
    }
    let segments: Vec<&str> = rest
        .split(separator)
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect();
    if segments.is_empty() {
        return if prefix.is_empty() {
            ".".to_string()
        } else {
            prefix
        };
    }
    format!("{prefix}{}", segments.join(&separator.to_string()))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn process_home() -> Option<PathBuf> {
    if cfg!(windows) {
        if let Some(profile) = non_empty_env("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        let drive = env::var("HOMEDRIVE").unwrap_or_default();
        let path = env::var("HOMEPATH").unwrap_or_default();
        if !drive.is_empty() || !path.is_empty() {
            return Some(PathBuf::from(format!("{drive}{path}")));
        }
        return non_empty_env("HOME").map(PathBuf::from);
    }
    non_empty_env("HOME")
        .or_else(|| non_empty_env("USERPROFILE"))
        .map(PathBuf::from)
}

fn non_empty_env(name: &str) -> Option<String> {
    let value = env::var(name).unwrap_or_default();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}
