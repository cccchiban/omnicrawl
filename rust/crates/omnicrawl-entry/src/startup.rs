//! 启动编排：首次配置、配置诊断、连接器子进程与路由前的准备。
//!
//! 对映 Python `omnicrawl/entry.py`：`initialize_user_configuration` → 打印启动报告 →
//! 起连接器（`connectors/autostart.py`）→ 进工作台 → 退出时先回收连接器再收尾。
//! 与 Python 的差别集中在两处端口化：模板资源与渠道向导都由本模块注入（Rust 侧没有包资源
//! 目录，模板既有磁盘目录也有编译期内嵌副本），交互式 API Key 索取用行式向导承担
//! （[`crate::channel_setup`]），不再单独做 getpass 分支。

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use omnicrawl_config::core::bootstrap::{
    format_startup_report, initialize_user_configuration, NodeProbe, PluginRow, StartupPorts,
    StartupSetup,
};
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_connectors::autostart::{
    default_connector_specs, start_configured_connectors, ConnectorProcessManager, ConnectorSpec,
};
use omnicrawl_extensions::install::list_plugins;

use crate::channel_setup::{run_channel_setup, unavailable_outcome};

/// 模板目录覆盖（与 `omnicrawl-host` 的提示词模板定位同一约定）。

const CONFIG_TEMPLATE: &str =
    include_str!("../../../../rust/assets/config-templates/config.example.toml");
const MODELS_TEMPLATE: &str =
    include_str!("../../../../rust/assets/config-templates/models.example.toml");
const SUBAGENTS_TEMPLATE: &str =
    include_str!("../../../../rust/assets/config-templates/subagents.example.toml");

/// 首次启动的编排结果：诊断文本与是否具备启动条件。
pub struct StartupOutcome {
    pub setup: StartupSetup,
    pub lines: Vec<String>,
}

impl StartupOutcome {
    /// 可继续启动（没有配置错误，且已配置凭据）。
    pub fn ready(&self) -> bool {
        self.setup.errors.is_empty() && self.setup.api_key_configured
    }

    /// 进程退出码：配置错误为 1，凭据缺失为 2（与 Python `run_application` 的分支一致）。
    pub fn exit_code(&self) -> i32 {
        if !self.setup.errors.is_empty() {
            return 1;
        }
        if !self.setup.api_key_configured {
            return 2;
        }
        0
    }
}

/// 跑一次首次配置与启动诊断。
///
/// `interactive` 为假时不启动渠道向导（无交互终端），结果里会给出「未完成模型渠道配置」，
/// 由调用方决定退出码——与 Python 的 `_has_interactive_terminal` 判定同语义。
pub fn bootstrap(env: &ConfigEnvironment, interactive: bool) -> Result<StartupOutcome, String> {
    let wizard = |config_path: &Path, models_path: &Path| -> Result<bool, String> {
        let outcome = run_channel_setup(env, config_path, models_path);
        for line in &outcome.lines {
            println!("{line}");
        }
        Ok(outcome.completed)
    };
    let wizard_port: Option<omnicrawl_config::core::bootstrap::ChannelSetupFn<'_>> =
        if interactive { Some(&wizard) } else { None };
    if !interactive {
        for line in unavailable_outcome().lines {
            println!("{line}");
        }
    }

    let read_template = |resource: &str| -> Result<String, String> {
        if let Some(text) = read_disk_template(env, resource) {
            return Ok(text);
        }
        embedded_template(resource)
            .map(str::to_string)
            .ok_or_else(|| format!("找不到模板 {resource}"))
    };
    let node_probe = probe_node;
    let plugin_rows = probe_plugin_rows;

    let ports = StartupPorts {
        read_template: &read_template,
        channel_setup: wizard_port,
        // 交互式 API Key 索取由渠道向导承担；这里不再单独开一路 prompt。
        prompt: None,
        node_probe: &node_probe,
        plugin_rows: &plugin_rows,
    };
    let setup = initialize_user_configuration(env, None, &ports)
        .map_err(|error| error.message().to_string())?;
    let lines = format_startup_report(&setup);
    Ok(StartupOutcome { setup, lines })
}

/// 连接器的子进程命令行：优先宿主二进制（真实工具表与会话），无宿主载荷时退回内核。
///
/// 内核的 `--connector <平台>` 只在进程内自持三个自持工具，其余工具一律 `tool_unavailable`，
/// 会话也不落盘——远程用户会看到「无法访问项目仓库」。因此只要有宿主载荷，就走
/// `omnicrawl-host connector <平台>`（由宿主承载完整 Agent）；只有像 armv7 那样只发内核的
/// 平台才退回旧路径，那条路径的行为与提示保持不变。
pub fn connector_specs(kernel: &Path) -> Vec<ConnectorSpec> {
    let host = host_binary_beside(kernel);
    match host {
        // 有宿主载荷：连接器由宿主承载（`omnicrawl-host connector <平台>`）。
        Some(program) => default_connector_specs(program),
        None => {
            let mut specs = default_connector_specs(kernel);
            for spec in specs.iter_mut() {
                for argument in spec.args.iter_mut() {
                    if argument == "connector" {
                        *argument = "--connector".to_string();
                    }
                }
            }
            specs
        }
    }
}

/// 宿主二进制：优先与内核同目录，其次就是当前正在运行的宿主自身。
///
/// 内核路径可能是裸文件名（解析不到文件时的回退形态），此时 `parent()` 是空目录，同级探测
/// 必然失败。那种情况下若判成「没有宿主载荷」，连接器会退回只能对话的内核路径——因此补一条
/// 兜底：本进程就是 `omnicrawl-host`，它自己就是可用的宿主载荷。
fn host_binary_beside(kernel: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "omnicrawl-host.exe"
    } else {
        "omnicrawl-host"
    };
    let sibling = kernel
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(name));
    // 兜底只认真正的宿主进程：链接了本 crate 的其他二进制不能冒充宿主载荷。
    let itself = std::env::current_exe().ok().filter(|current| {
        current
            .file_name()
            .map(|file| file == std::ffi::OsStr::new(name))
            .unwrap_or(false)
    });
    sibling
        .into_iter()
        .chain(itself)
        .find(|candidate| candidate.is_file())
}

/// 拉起已配置的连接器；未配置任何平台时监督器不启动任何子进程。
pub fn start_connectors(
    env: &ConfigEnvironment,
    workspace_root: &Path,
    kernel: &Path,
) -> ConnectorProcessManager {
    let options = omnicrawl_connectors::autostart::ConnectorManagerOptions::new(env.clone())
        .with_specs(connector_specs(kernel));
    start_configured_connectors(options, workspace_root.to_path_buf())
}

/// 当前终端是否可交互（stdin 与 stdout 都是 TTY）。
pub fn interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// 磁盘上的模板目录：可执行文件祖先里的 `rust/assets/config-templates`（仓库检出）
/// 与 `omnicrawl/config/templates`（已发布载荷）。
pub fn locate_templates_dir(_env: &ConfigEnvironment) -> Option<PathBuf> {
    let mut current = std::env::current_exe().ok();
    while let Some(path) = current {
        current = path.parent().map(PathBuf::from);
        let Some(directory) = current.as_ref() else {
            break;
        };
        // 仓库检出里的新家优先（脱离 Python 包树后的单一来源），再落到已发布载荷的旧布局。
        let candidate = directory
            .join("rust")
            .join("assets")
            .join("config-templates");
        if candidate.join("config.example.toml").is_file() {
            return Some(candidate);
        }
        let candidate = directory.join("omnicrawl").join("config").join("templates");
        if candidate.join("config.example.toml").is_file() {
            return Some(candidate);
        }
    }
    None
}

fn read_disk_template(env: &ConfigEnvironment, resource: &str) -> Option<String> {
    let directory = locate_templates_dir(env)?;
    std::fs::read_to_string(directory.join(resource)).ok()
}

fn embedded_template(resource: &str) -> Option<&'static str> {
    match resource {
        "config.example.toml" => Some(CONFIG_TEMPLATE),
        "models.example.toml" => Some(MODELS_TEMPLATE),
        "subagents.example.toml" => Some(SUBAGENTS_TEMPLATE),
        _ => None,
    }
}

/// Node 探测：`node --version` 与 `npm` 是否可用（超时 5 秒，与 Python 一致）。
fn probe_node() -> NodeProbe {
    let Some(node) = which("node") else {
        return NodeProbe::Missing;
    };
    let output = std::process::Command::new(&node).arg("--version").output();
    let npm_available = which("npm").is_some();
    match output {
        Ok(output) => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let version = if version.is_empty() {
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            } else {
                version
            };
            NodeProbe::Version {
                version,
                npm_available,
            }
        }
        Err(error) => NodeProbe::Failed {
            error: error.to_string(),
        },
    }
}

/// 插件注册表投影（user + project 两份，读取失败按错误行上报）。
fn probe_plugin_rows() -> Result<Vec<PluginRow>, String> {
    let workspace = std::env::current_dir().ok();
    let rows = list_plugins("all", workspace.as_deref());
    Ok(rows
        .into_iter()
        .map(|row| PluginRow {
            error: row
                .get("error")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            enabled: row
                .get("enabled")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        })
        .collect())
}

/// `which`：在 PATH 上找可执行文件（带平台后缀），不做 shell 展开。
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cfg!(windows) {
            for suffix in [".exe", ".cmd", ".bat"] {
                let candidate = directory.join(format!("{name}{suffix}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_templates_cover_the_three_files() {
        for resource in [
            "config.example.toml",
            "models.example.toml",
            "subagents.example.toml",
        ] {
            let text = embedded_template(resource).expect("内嵌模板");
            assert!(!text.trim().is_empty(), "{resource} 不应为空");
        }
        assert!(embedded_template("system_prompt.md").is_none());
    }

    /// 仓库检出里配置模板目录的优先级：`rust/assets/config-templates` 必须先于旧的
    /// `omnicrawl/config/templates`（后者只剩「已发布载荷」的兼容角色）。
    #[test]
    fn config_templates_dir_prefers_rust_assets() {
        let env = ConfigEnvironment::new("bundle", "test");
        let found = locate_templates_dir(&env).expect("仓库检出里应能找到配置模板目录");
        assert!(found.join("config.example.toml").is_file(), "{found:?}");
        assert!(
            found
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("rust/assets/config-templates"),
            "应优先命中 rust/assets/config-templates：{found:?}"
        );
    }

    /// 无宿主载荷（armv7 等）时退回内核：那条路径保留 `--connector <平台>` 的旧写法。
    #[test]
    fn connector_specs_use_the_kernel_flag_without_a_host_binary() {
        let specs = connector_specs(Path::new("C:/bin/omnicrawl.exe"));
        let lines: Vec<Vec<String>> = specs.iter().map(|spec| spec.command_line()).collect();
        assert_eq!(
            lines,
            vec![
                vec![
                    "C:/bin/omnicrawl.exe".to_string(),
                    "--connector".to_string(),
                    "telegram".to_string()
                ],
                vec![
                    "C:/bin/omnicrawl.exe".to_string(),
                    "--connector".to_string(),
                    "feishu".to_string()
                ],
                vec![
                    "C:/bin/omnicrawl.exe".to_string(),
                    "--connector".to_string(),
                    "qqbot".to_string()
                ],
            ]
        );
    }

    /// 有宿主载荷时连接器由宿主承载：内核那条路径只能对话（工具全不可用、会话不落盘），
    /// 远程用户会看到「无法访问项目」。
    #[test]
    fn connector_specs_prefer_the_host_binary() {
        let root = std::env::temp_dir().join(format!("oc-entry-connector-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建立临时目录");
        let kernel = root.join(if cfg!(windows) {
            "omnicrawl.exe"
        } else {
            "omnicrawl"
        });
        std::fs::write(&kernel, b"").expect("写内核占位");
        let host = root.join(if cfg!(windows) {
            "omnicrawl-host.exe"
        } else {
            "omnicrawl-host"
        });
        std::fs::write(&host, b"").expect("写宿主占位");

        let lines: Vec<Vec<String>> = connector_specs(&kernel)
            .iter()
            .map(|spec| spec.command_line())
            .collect();
        assert_eq!(
            lines,
            vec![
                vec![
                    host.to_string_lossy().to_string(),
                    "connector".to_string(),
                    "telegram".to_string()
                ],
                vec![
                    host.to_string_lossy().to_string(),
                    "connector".to_string(),
                    "feishu".to_string()
                ],
                vec![
                    host.to_string_lossy().to_string(),
                    "connector".to_string(),
                    "qqbot".to_string()
                ],
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_without_terminal_creates_templates_and_reports() {
        let root = std::env::temp_dir().join(format!("oc-entry-boot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建立临时配置目录");
        let env = ConfigEnvironment::new(root.clone(), "windows");

        let outcome = bootstrap(&env, false).expect("首次配置");
        assert!(outcome.setup.config_created);
        assert!(outcome.setup.models_created);
        assert!(!outcome.setup.api_key_configured);
        assert_eq!(outcome.exit_code(), 2, "凭据缺失应给退出码 2");
        // 模板写在用户配置目录（`<home>/.OmniCrawl/`），与 Python 的 `user_config_dir` 同址。
        let config_dir = omnicrawl_config::core::runtime::user_config_dir(&env);
        assert!(config_dir.join("config.toml").is_file());
        assert!(config_dir.join("models.toml").is_file());
        assert!(config_dir.join("subagents.toml").is_file());
        assert!(
            outcome
                .lines
                .iter()
                .any(|line| line.contains("未完成模型渠道配置")),
            "报告应说明未完成渠道配置：{:?}",
            outcome.lines
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exit_code_reports_config_errors_first() {
        let setup = StartupSetup {
            config_dir: PathBuf::new(),
            config_path: PathBuf::new(),
            models_path: PathBuf::new(),
            subagents_path: PathBuf::new(),
            config_created: false,
            models_created: false,
            subagents_created: false,
            api_key_prompted: false,
            api_key_configured: false,
            checks: Vec::new(),
            errors: vec!["配置坏了".to_string()],
        };
        let outcome = StartupOutcome {
            setup,
            lines: Vec::new(),
        };
        assert_eq!(outcome.exit_code(), 1);
        assert!(!outcome.ready());
    }
}
