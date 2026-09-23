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
pub const TEMPLATES_DIR_ENV: &str = "OMNICRAWL_TEMPLATES_DIR";

const CONFIG_TEMPLATE: &str =
    include_str!("../../../../omnicrawl/config/templates/config.example.toml");
const MODELS_TEMPLATE: &str =
    include_str!("../../../../omnicrawl/config/templates/models.example.toml");
const SUBAGENTS_TEMPLATE: &str =
    include_str!("../../../../omnicrawl/config/templates/subagents.example.toml");

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

/// 连接器的子进程命令行：内核二进制的 `--connector <平台>`（与 `omnicrawl-cli` 的入口一致）。
pub fn connector_specs(kernel: &Path) -> Vec<ConnectorSpec> {
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

/// 磁盘上的模板目录：环境变量优先，其次可执行文件祖先里的 `omnicrawl/config/templates`。
pub fn locate_templates_dir(env: &ConfigEnvironment) -> Option<PathBuf> {
    let configured = env.get_trimmed(TEMPLATES_DIR_ENV);
    if !configured.trim().is_empty() {
        let candidate = PathBuf::from(configured);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    let mut current = std::env::current_exe().ok();
    while let Some(path) = current {
        current = path.parent().map(PathBuf::from);
        let Some(directory) = current.as_ref() else {
            break;
        };
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

    #[test]
    fn connector_specs_use_the_kernel_flag() {
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
            ]
        );
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
