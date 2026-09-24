//! 插件 CLI（对应 `omnicrawl/cli.py`）：参数面、作用域解析、各子命令与退出码映射。
//!
//! 语义基准是 Python 侧：参数名 / 默认值 / 位置参数个数、`_resolve_scope` 的三段判定、
//! 每个子命令打印的文案与退出码、以及 `run_plugin_command` 的异常→退出码阶梯
//! （Node → 用户取消 → manifest / 权限 → registry / 下载 → 冒烟 → 兜底 manifest）全部照搬。
//!
//! 与 Python 的差异（见 `README.md`）：
//!
//! - **参数解析**：手写解析器逐子命令校验选项集，不接受 argparse 的长选项前缀缩写
//!   （`--pro` 这类），报错文案是 `omnicrawl plugin: error: …` 而不是 argparse 的 usage 全文；
//! - **runner 路径**：Python 从 `plugin_protocol.__file__` 推 `node_runner.mjs`，内核侧由
//!   `OMNICRAWL_RUNNER_DIR` 环境变量或可执行文件同级的 `extensions/` 注入（[`runner_dir`]）；
//! - **配置环境**：配置读写经 `omnicrawl-config` 的 `ConfigEnvironment` 注入，不读进程全局。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use omnicrawl_config::core::runtime::{
    get_section, load_config_data, save_config_data, ConfigEnvironment,
};
use omnicrawl_config::error::ConfigError;
use omnicrawl_config::toml::{Table, Value as TomlValue};
use omnicrawl_config::value::toml_to_json_object;
use omnicrawl_extensions::error::{PluginError, PluginInstallError};
use omnicrawl_extensions::install::{
    doctor, install_from_npm, list_plugins, parse_package_spec, register_local_dev_plugin,
    rollback_plugin, set_enabled, uninstall_plugin, ConfirmCallback,
};
use omnicrawl_extensions::models::{parse_plugins_config, PluginsConfig};
use omnicrawl_extensions::protocol::{
    node_runner_path, resolve_runner_path, WorkerLauncher, NODE_RUNNER_FILENAME,
};
use omnicrawl_extensions::registry::{project_registry_path, user_registry_path};
use omnicrawl_workspace::resolve_path;
use serde_json::{Map, Value};

pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NODE: i32 = 3;
pub const EXIT_REGISTRY_NET: i32 = 4;
pub const EXIT_MANIFEST: i32 = 5;
pub const EXIT_USER_CANCEL: i32 = 6;
pub const EXIT_ATOMIC: i32 = 7;
pub const EXIT_SMOKE: i32 = 8;

/// runner 目录的环境变量名（与扩展内核、启动器脚本共用同一约定）。
pub const RUNNER_DIR_ENV: &str = omnicrawl_extensions::protocol::RUNNER_DIR_ENV;

const PLUGIN_COMMAND: &str = "plugin";
const SUBCOMMANDS: [&str; 10] = [
    "system",
    "install",
    "list",
    "info",
    "enable",
    "disable",
    "update",
    "rollback",
    "uninstall",
    "doctor",
];

/// `omnicrawl plugin ...` 的解析结果（对映 argparse 的 namespace）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginArgs {
    /// 顶层子命令（固定 `plugin`）。
    pub command: String,
    pub plugin_command: String,
    /// 顶层 `--resume`（插件路径用不到，保留以对齐参数面）。
    pub resume: String,
    pub project: bool,
    pub user: bool,
    pub enable: bool,
    pub yes: bool,
    pub dev: bool,
    pub all: bool,
    pub json: bool,
    pub purge: bool,
    pub no_activate: bool,
    pub name: String,
    pub package_spec: String,
    pub action: String,
    pub to_version: String,
}

/// CLI 层错误：既要区分退出码阶梯，也要保住各自的原错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// `--help`：打印帮助后以 0 退出。
    Help(String),
    /// 参数错误：打印错误后以 [`EXIT_USAGE`] 退出。
    Usage(String),
    /// `_resolve_scope` 里 `raise SystemExit(EXIT_USAGE)`：静默以 2 退出。
    SilentUsage,
    Install(PluginInstallError),
    Plugin(PluginError),
    Config(ConfigError),
}

impl CliError {
    pub fn message(&self) -> String {
        match self {
            Self::Help(text) | Self::Usage(text) => text.clone(),
            Self::SilentUsage => String::new(),
            Self::Install(error) => error.message().to_string(),
            Self::Plugin(error) => error.message().to_string(),
            Self::Config(error) => error.message().to_string(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for CliError {}

/// `omnicrawl plugin ...` 的退出码；`argv` 不以 `plugin` 开头时返回 `None`。
///
/// `argv` 是剥掉程序名后的完整参数表（与 `env::args().skip(1)` 一致），因此顶层
/// `--resume` 也在这里解析——与 `cli.py` 的 `build_parser` 保持同一形状。
pub fn run_plugin_cli(
    argv: &[String],
    env: &ConfigEnvironment,
    workspace_root: Option<&Path>,
) -> Option<i32> {
    if argv.first().map(String::as_str) != Some(PLUGIN_COMMAND) {
        return None;
    }
    let args = match parse_plugin_args(argv) {
        Ok(args) => args,
        Err(CliError::Help(text)) => {
            println!("{text}");
            return Some(EXIT_OK);
        }
        Err(error) => {
            let message = error.message();
            if !message.is_empty() {
                eprintln!("omnicrawl plugin: error: {message}");
            }
            return Some(EXIT_USAGE);
        }
    };
    Some(run_plugin_command(&args, env, workspace_root))
}

/// 分发一条插件命令，并把错误折算成退出码（对映 `run_plugin_command`）。
pub fn run_plugin_command(
    args: &PluginArgs,
    env: &ConfigEnvironment,
    workspace_root: Option<&Path>,
) -> i32 {
    let root = match workspace_root {
        Some(path) => resolve_path(path),
        None => resolve_path(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    };
    let result = match args.plugin_command.as_str() {
        "system" => cmd_system(args, env),
        "install" => cmd_install(args, env, &root),
        "list" => cmd_list(args, &root),
        "info" => cmd_info(args, &root),
        "enable" => cmd_enable(args, env, &root, true),
        "disable" => cmd_enable(args, env, &root, false),
        "update" => cmd_update(args, env, &root),
        "rollback" => cmd_rollback(args, &root),
        "uninstall" => cmd_uninstall(args, &root),
        "doctor" => cmd_doctor(args, &root),
        other => {
            eprintln!("未知 plugin 命令：{other}");
            return EXIT_USAGE;
        }
    };
    match result {
        Ok(code) => code,
        Err(CliError::Install(error)) => {
            let message = error.message().to_string();
            eprintln!("{message}");
            exit_code_for_install_error(&message)
        }
        Err(CliError::Plugin(error)) => {
            eprintln!("{}", error.message());
            EXIT_MANIFEST
        }
        Err(CliError::Config(error)) => {
            eprintln!("{}", error.message());
            EXIT_ATOMIC
        }
        Err(CliError::Usage(message)) => {
            eprintln!("omnicrawl plugin: error: {message}");
            EXIT_USAGE
        }
        Err(CliError::Help(text)) => {
            println!("{text}");
            EXIT_OK
        }
        Err(CliError::SilentUsage) => EXIT_USAGE,
    }
}

/// `PluginInstallError` 文案 → 退出码阶梯（逐条对映 `run_plugin_command` 的 except 分支）。
pub fn exit_code_for_install_error(message: &str) -> i32 {
    if message.contains("Node") || message.contains("npm") {
        return EXIT_NODE;
    }
    if message.contains("用户") && (message.contains("取消") || message.contains("拒绝")) {
        return EXIT_USER_CANCEL;
    }
    let lowered = message.to_lowercase();
    if message.contains("integrity") || lowered.contains("manifest") || message.contains("权限") {
        return EXIT_MANIFEST;
    }
    if lowered.contains("registry") || message.contains("下载") || message.contains("NPM") {
        return EXIT_REGISTRY_NET;
    }
    if message.contains("冒烟") || message.contains("握手") {
        return EXIT_SMOKE;
    }
    EXIT_MANIFEST
}

/// 作用域解析（对映 `_resolve_scope`）：`--project` / `--user` 同时给出时静默以 2 退出；
/// 否则工作区内有 `AGENTS.md` / `package.json` 时默认 project，其余默认 user。
pub fn resolve_scope(args: &PluginArgs, workspace_root: &Path) -> Result<String, CliError> {
    if args.project && args.user {
        return Err(CliError::SilentUsage);
    }
    if args.project {
        return Ok("project".to_string());
    }
    if args.user {
        return Ok("user".to_string());
    }
    if workspace_root.join("AGENTS.md").exists() || workspace_root.join("package.json").exists() {
        return Ok("project".to_string());
    }
    Ok("user".to_string())
}

/// `node_runner.mjs` 所在目录。
///
/// 解析顺序：`OMNICRAWL_RUNNER_DIR` → 扩展内核的搜索链（可执行文件及其祖先下的
/// `extensions/`、`rust/assets/extensions/`（仓库检出）与 `omnicrawl/extensions/`、
/// 进程工作目录）→ 可执行文件同级的
/// `extensions/`。宿主运行期的 [`worker_launcher`] 用的是同一条链，
/// 因此「CLI 装得上」与「宿主起得来」不会分裂。
pub fn runner_dir() -> PathBuf {
    if let Some(value) = std::env::var_os(RUNNER_DIR_ENV) {
        let text = value.to_string_lossy().trim().to_string();
        if !text.is_empty() {
            return PathBuf::from(text);
        }
    }
    if let Some(found) = resolve_runner_path() {
        if let Some(parent) = found.parent() {
            return parent.to_path_buf();
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("extensions")
}

/// runner 路径（`<runner_dir>/node_runner.mjs`）。
pub fn runner_path() -> PathBuf {
    node_runner_path(&runner_dir())
}

/// Worker 启动路径（Node 可执行文件 + `node_runner.mjs`）：解析失败时带回可展示的搜索链。
pub fn worker_launcher() -> Result<WorkerLauncher, PluginError> {
    WorkerLauncher::resolve().map_err(|error| PluginError::new(error.to_string()))
}

/// `confirm` 回调：stderr 提示 + stdin 读一行；EOF / 非 y/yes 一律视为取消。
fn confirm(message: &str) -> bool {
    eprintln!("{message}");
    print!("继续？[y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    if answer.is_empty() {
        // EOF：Python 的 `input()` 抛 EOFError → False。
        return false;
    }
    let normalized = answer.trim().to_lowercase();
    normalized == "y" || normalized == "yes"
}

fn confirm_callback() -> ConfirmCallback {
    Arc::new(confirm)
}

/// 读取并校验 `plugins` 段（对映 `parse_plugins_config(get_section(load_config_data(), "plugins"))`）。
pub fn load_plugins_config(env: &ConfigEnvironment) -> Result<PluginsConfig, CliError> {
    let data: Table = load_config_data(env, None).map_err(CliError::Config)?;
    let section = get_section(&data, "plugins").map_err(CliError::Config)?;
    let json = toml_to_json_object(&section);
    parse_plugins_config(Some(&json)).map_err(CliError::Plugin)
}

/// `json.dumps(..., ensure_ascii=False, indent=2)`。
fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// Python 的 `bool` 打印形状（`True` / `False`），CLI 文案要与 Python 逐字对齐。
fn python_bool_text(flag: bool) -> &'static str {
    if flag {
        "True"
    } else {
        "False"
    }
}

/// Python 的 `f"{value}"`：字符串原样，`None` 打 `None`，其余按 JSON 形状。
fn python_display(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => python_bool_text(*flag).to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn json_array(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn cmd_system(args: &PluginArgs, env: &ConfigEnvironment) -> Result<i32, CliError> {
    let mut data = load_config_data(env, None).map_err(CliError::Config)?;
    let mut plugins = get_section(&data, "plugins").map_err(CliError::Config)?;
    let enabled = args.action == "enable";
    plugins.insert("enabled".to_string(), TomlValue::Boolean(enabled));
    data.insert("plugins".to_string(), TomlValue::Table(plugins));
    let path = save_config_data(env, &data, None).map_err(CliError::Config)?;
    let state = if enabled { "启用" } else { "禁用" };
    println!("已{state}全局插件系统：{}", path.display());
    Ok(EXIT_OK)
}

fn cmd_install(args: &PluginArgs, env: &ConfigEnvironment, root: &Path) -> Result<i32, CliError> {
    let scope = resolve_scope(args, root)?;
    let registry = if scope == "project" {
        project_registry_path(root)
    } else {
        user_registry_path()
    };
    eprintln!("目标注册表：{}", registry.display());

    let config = load_plugins_config(env)?;
    let package_spec = args.package_spec.clone();
    let mut is_dev = args.dev
        || package_spec.starts_with('.')
        || package_spec.starts_with('/')
        || package_spec.contains('\\');
    // Windows 盘符路径。
    if package_spec.len() >= 2 && package_spec.as_bytes()[1] == b':' {
        is_dev = true;
    }

    let result = if is_dev {
        register_local_dev_plugin(
            Path::new(&package_spec),
            &runner_path(),
            &scope,
            Some(root),
            // 本地开发插件默认启用，便于联调；可用后续 disable 关掉。
            true,
        )
        .map_err(CliError::Install)?
    } else {
        // 预解析帮助给出更好错误。
        parse_package_spec(&package_spec).map_err(CliError::Install)?;
        install_from_npm(
            &package_spec,
            &runner_path(),
            &scope,
            Some(root),
            args.enable,
            args.yes,
            Some(confirm_callback()),
            config.allow_network_install,
        )
        .map_err(CliError::Install)?
    };
    println!(
        "已安装 {}@{} scope={} enabled={}",
        result.name, result.version, result.scope, result.enabled
    );
    for item in &result.diagnostics {
        eprintln!("- {item}");
    }
    if !config.enabled {
        eprintln!("提示：全局 plugins.enabled=false。需要时执行：ocl plugin system enable");
    }
    Ok(EXIT_OK)
}

fn cmd_list(args: &PluginArgs, root: &Path) -> Result<i32, CliError> {
    let scope = if args.project {
        "project"
    } else if args.user {
        "user"
    } else {
        "all"
    };
    let rows = list_plugins(scope, Some(root));
    if args.json {
        print_json(&Value::Array(
            rows.iter().cloned().map(Value::Object).collect(),
        ));
        return Ok(EXIT_OK);
    }
    if rows.is_empty() {
        println!("未安装插件。");
        return Ok(EXIT_OK);
    }
    for row in &rows {
        if let Some(error) = row.get("error").and_then(Value::as_str) {
            let scope_text = row.get("scope").and_then(Value::as_str).unwrap_or_default();
            println!("[{scope_text}] ERROR {error}");
            continue;
        }
        let mut version = row
            .get("active")
            .and_then(Value::as_object)
            .and_then(|active| active.get("version"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if version.is_empty() && row.get("devMode").and_then(Value::as_bool).unwrap_or(false) {
            version = "dev".to_string();
        }
        let shown = if version.is_empty() {
            "-".to_string()
        } else {
            version
        };
        println!(
            "{}\tscope={}\tenabled={}\tversion={shown}",
            row.get("name").and_then(Value::as_str).unwrap_or_default(),
            row.get("scope").and_then(Value::as_str).unwrap_or_default(),
            python_bool_text(row.get("enabled").and_then(Value::as_bool).unwrap_or(false))
        );
    }
    Ok(EXIT_OK)
}

fn cmd_info(args: &PluginArgs, root: &Path) -> Result<i32, CliError> {
    let rows: Vec<Map<String, Value>> = list_plugins("all", Some(root))
        .into_iter()
        .filter(|row| row.get("name").and_then(Value::as_str) == Some(args.name.as_str()))
        .collect();
    if rows.is_empty() {
        eprintln!("未找到插件：{}", args.name);
        return Ok(EXIT_MANIFEST);
    }
    if args.json {
        print_json(&Value::Array(
            rows.iter().cloned().map(Value::Object).collect(),
        ));
    } else {
        for row in rows {
            print_json(&Value::Object(row));
        }
    }
    Ok(EXIT_OK)
}

fn cmd_enable(
    args: &PluginArgs,
    env: &ConfigEnvironment,
    root: &Path,
    enabled: bool,
) -> Result<i32, CliError> {
    let scope = resolve_scope(args, root)?;
    let registry = if scope == "project" {
        project_registry_path(root)
    } else {
        user_registry_path()
    };
    eprintln!("目标注册表：{}", registry.display());
    set_enabled(&args.name, enabled, &scope, Some(root)).map_err(CliError::Install)?;
    let state = if enabled { "启用" } else { "禁用" };
    println!("已{state} {} ({scope})", args.name);
    let config = load_plugins_config(env)?;
    if enabled && !config.enabled {
        eprintln!(
            "提示：插件已启用，但全局 plugins.enabled=false。请执行 ocl plugin system enable。"
        );
    }
    Ok(EXIT_OK)
}

fn cmd_update(args: &PluginArgs, env: &ConfigEnvironment, root: &Path) -> Result<i32, CliError> {
    let scope = resolve_scope(args, root)?;
    let version = args.to_version.trim();
    let spec = if version.is_empty() {
        args.name.clone()
    } else {
        format!("{}@{}", args.name, version)
    };
    let config = load_plugins_config(env)?;
    // 已启用插件默认 activate；--no-activate 时只写 candidate（enable=False 且保留 enabled）。
    let result = install_from_npm(
        &spec,
        &runner_path(),
        &scope,
        Some(root),
        !args.no_activate,
        args.yes,
        Some(confirm_callback()),
        config.allow_network_install,
    )
    .map_err(CliError::Install)?;
    println!("已更新 {}@{}", result.name, result.version);
    Ok(EXIT_OK)
}

fn cmd_rollback(args: &PluginArgs, root: &Path) -> Result<i32, CliError> {
    let scope = resolve_scope(args, root)?;
    let reference = rollback_plugin(&args.name, &runner_path(), &scope, Some(root))
        .map_err(CliError::Install)?;
    println!("已回滚 {} -> {}", args.name, reference.version);
    Ok(EXIT_OK)
}

fn cmd_uninstall(args: &PluginArgs, root: &Path) -> Result<i32, CliError> {
    let scope = resolve_scope(args, root)?;
    if !args.yes {
        let message = format!("确认卸载插件 {}（scope={scope}）？", args.name);
        if !confirm(&message) {
            eprintln!("已取消。");
            return Ok(EXIT_USER_CANCEL);
        }
    }
    uninstall_plugin(&args.name, &scope, Some(root), args.purge).map_err(CliError::Install)?;
    println!("已卸载 {}", args.name);
    Ok(EXIT_OK)
}

fn cmd_doctor(args: &PluginArgs, root: &Path) -> Result<i32, CliError> {
    let name = if args.name.is_empty() {
        None
    } else {
        Some(args.name.as_str())
    };
    let report = doctor(name, Some(root));
    if args.json {
        print_json(&Value::Object(report.clone()));
    } else {
        println!("node: {}", python_display(report.get("node")));
        println!("npm: {}", python_display(report.get("npm")));
        for issue in json_array(report.get("issues")) {
            println!("ISSUE: {}", issue.as_str().unwrap_or_default());
        }
        for plugin in json_array(report.get("plugins")) {
            let issues = json_array(plugin.get("issues"));
            let mark = if issues.is_empty() { "OK" } else { "WARN" };
            println!(
                "[{mark}] {} scope={}",
                python_display(plugin.get("name")),
                plugin
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            );
            for issue in issues {
                println!("  - {}", issue.as_str().unwrap_or_default());
            }
        }
    }
    let ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);
    Ok(if ok { EXIT_OK } else { EXIT_MANIFEST })
}

/// 参数解析：`omnicrawl plugin <子命令> [选项]`。
pub fn parse_plugin_args(argv: &[String]) -> Result<PluginArgs, CliError> {
    let mut args = PluginArgs::default();
    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].as_str();
        match token {
            "-h" | "--help" => return Err(CliError::Help(plugin_help(""))),
            PLUGIN_COMMAND => {
                args.command = PLUGIN_COMMAND.to_string();
                index += 1;
                break;
            }
            "--resume" => {
                index += 1;
                let Some(value) = argv.get(index) else {
                    return Err(CliError::Usage(
                        "argument --resume: expected one argument".to_string(),
                    ));
                };
                args.resume = value.clone();
                index += 1;
            }
            other if other.starts_with("--resume=") => {
                args.resume = other["--resume=".len()..].to_string();
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(CliError::Usage(format!("unrecognized arguments: {other}")));
            }
            other => {
                args.command = other.to_string();
                index += 1;
                break;
            }
        }
    }
    if args.command != PLUGIN_COMMAND {
        return Err(CliError::Usage(
            "the following arguments are required: command".to_string(),
        ));
    }
    let Some(subcommand) = argv.get(index) else {
        return Err(CliError::Usage(
            "the following arguments are required: plugin_command".to_string(),
        ));
    };
    if subcommand == "-h" || subcommand == "--help" {
        return Err(CliError::Help(plugin_help("")));
    }
    if subcommand.starts_with('-') {
        return Err(CliError::Usage(
            "the following arguments are required: plugin_command".to_string(),
        ));
    }
    if !SUBCOMMANDS.contains(&subcommand.as_str()) {
        return Err(CliError::Usage(format!(
            "argument plugin_command: invalid choice: '{subcommand}'"
        )));
    }
    args.plugin_command = subcommand.clone();
    index += 1;
    parse_subcommand(&mut args, &argv[index..])?;
    Ok(args)
}

/// 每个子命令声明的选项集（逐个对映 argparse 的 `add_argument`，多给选项按未识别处理）。
fn allowed_flags(subcommand: &str) -> &'static [&'static str] {
    match subcommand {
        "install" => &["project", "user", "enable", "yes", "dev"],
        "list" => &["project", "user", "all", "json"],
        "info" => &["json"],
        "enable" | "disable" | "rollback" => &["project", "user"],
        "update" => &["to", "project", "user", "yes", "no-activate"],
        "uninstall" => &["project", "user", "purge", "yes"],
        "doctor" => &["json"],
        _ => &[],
    }
}

fn parse_subcommand(args: &mut PluginArgs, rest: &[String]) -> Result<(), CliError> {
    let subcommand = args.plugin_command.clone();
    let allowed = allowed_flags(&subcommand);
    let mut positionals: Vec<String> = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let token = rest[index].clone();
        if token == "-h" || token == "--help" {
            return Err(CliError::Help(plugin_help(&subcommand)));
        }
        if let Some(name) = token.strip_prefix("--") {
            let (flag, inline_value) = match name.split_once('=') {
                Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
                None => (name.to_string(), None),
            };
            if !allowed.contains(&flag.as_str()) {
                return Err(CliError::Usage(format!("unrecognized arguments: --{flag}")));
            }
            if flag == "to" {
                let value = match inline_value {
                    Some(value) => value,
                    None => {
                        index += 1;
                        match rest.get(index) {
                            Some(value) => value.clone(),
                            None => {
                                return Err(CliError::Usage(
                                    "argument --to: expected one argument".to_string(),
                                ))
                            }
                        }
                    }
                };
                args.to_version = value;
            } else {
                if inline_value.is_some() {
                    return Err(CliError::Usage(format!(
                        "argument --{flag}: ignored explicit argument"
                    )));
                }
                match flag.as_str() {
                    "project" => args.project = true,
                    "user" => args.user = true,
                    "enable" => args.enable = true,
                    "yes" => args.yes = true,
                    "dev" => args.dev = true,
                    "all" => args.all = true,
                    "json" => args.json = true,
                    "purge" => args.purge = true,
                    "no-activate" => args.no_activate = true,
                    _ => unreachable!("allowed_flags 与解析分支必须同步"),
                }
            }
        } else if token.starts_with('-') {
            return Err(CliError::Usage(format!("unrecognized arguments: {token}")));
        } else {
            positionals.push(token);
        }
        index += 1;
    }

    let take_positional = |positionals: &[String], name: &str| -> Result<String, CliError> {
        match positionals.first() {
            Some(value) => Ok(value.clone()),
            None => Err(CliError::Usage(format!(
                "the following arguments are required: {name}"
            ))),
        }
    };
    match subcommand.as_str() {
        "system" => {
            let action = take_positional(&positionals, "action")?;
            if action != "enable" && action != "disable" {
                return Err(CliError::Usage(format!(
                    "argument action: invalid choice: '{action}'"
                )));
            }
            args.action = action;
        }
        "install" => args.package_spec = take_positional(&positionals, "package_spec")?,
        // `list` 无位置参数；argparse 会把多余词当作未识别参数。
        "list" => {}
        "info" | "enable" | "disable" | "update" | "rollback" | "uninstall" => {
            args.name = take_positional(&positionals, "name")?;
        }
        // `doctor` 的 name 是可选的（`nargs="?"`）。
        "doctor" => args.name = positionals.first().cloned().unwrap_or_default(),
        _ => {}
    }
    let consumed = if subcommand == "list" { 0 } else { 1 };
    if positionals.len() > consumed {
        return Err(CliError::Usage(format!(
            "unrecognized arguments: {}",
            positionals[consumed..].join(" ")
        )));
    }
    Ok(())
}

fn plugin_help(subcommand: &str) -> String {
    let header = if subcommand.is_empty() {
        "用法：omnicrawl plugin <子命令> [选项]".to_string()
    } else {
        format!("用法：omnicrawl plugin {subcommand} [选项]")
    };
    format!(
        "{header}\n\n\
         子命令：system / install / list / info / enable / disable / update / rollback / \
         uninstall / doctor\n\
         选项：--project / --user / --enable / --yes / --dev / --all / --json / --purge / \
         --no-activate / --to <version>\n\n\
         {NODE_RUNNER_FILENAME} 所在目录由环境变量 {RUNNER_DIR_ENV} 注入。"
    )
}
