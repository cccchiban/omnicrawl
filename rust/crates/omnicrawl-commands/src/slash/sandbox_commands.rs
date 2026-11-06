//! 沙盒模式查询与开关命令。
//!
//! P0 只有查询与**切换开关/模式**（写回 `config.toml`），不做隔离。
//! dry-run 的判定记录在宿主侧 [`omnicrawl_host::sandbox::Sandbox`] 里，随 `bash`/`powershell`
//! 的输出一并回显（见 `[sandbox dry-run]` 段），所以这里不必单独取。

use omnicrawl_config::core::runtime::{load_config_data, save_config_data};
use omnicrawl_config::features::sandbox::{
    load_sandbox_config, NetPolicy, SandboxMode, SANDBOX_MODES, SANDBOX_NET_POLICIES,
};
use omnicrawl_config::toml::Value;

use super::*;

/// 查询当前沙盒配置。
pub fn handle_sandbox_command(ctx: &CommandContext<'_>) -> CommandResult {
    let environment = ctx.agent.config_environment();
    let config = match load_sandbox_config(&environment, None) {
        Ok(config) => config,
        Err(error) => {
            return CommandResult::message(format!(
                "❌ 沙盒配置无法解析：{}\n修好之前沙盒按关闭处理。",
                error.message()
            ))
        }
    };

    let mut lines = vec![config.status_line()];
    if config.enabled {
        lines.push(format!(
            "可写：工作区 + `.agent_tmp`{}",
            if config.writable.is_empty() {
                String::new()
            } else {
                format!(" + {}", config.writable.join("、"))
            }
        ));
        if !config.readable.is_empty() {
            lines.push(format!("额外可读：{}", config.readable.join("、")));
        }
        if config.net_policy == NetPolicy::Allowlist && !config.net_allow.is_empty() {
            lines.push(format!("网络白名单：{}", config.net_allow.join("、")));
        }
        if !config.net_deny.is_empty() {
            lines.push(format!("网络黑名单：{}", config.net_deny.join("、")));
        }
        if config.is_dry_run() {
            lines.push(
                "dry-run 的判定随命令输出回显（`[sandbox dry-run]` 段），可先跑一周看会拦什么。"
                    .to_string(),
            );
        }
        if config.isolates() {
            // 配置说的是"用户想要什么"，这里是"本机真能做到什么"——两者分开显示，
            // 否则 `/sandbox` 会在"声称已隔离"和"其实做不到"之间说谎。
            let diagnostic = ctx.agent.sandbox_diagnostic();
            if !diagnostic.is_empty() {
                lines.push(format!("实际能力：{diagnostic}"));
            }
            let network = match config.mode {
                SandboxMode::Landlock => {
                    "网络：Linux 侧端口层由 Landlock 管（内核级，只放行回环代理端口；\
                     offline 则连代理端口也不放行、含 UDP），域名判定在宿主回环代理（应用层）。\
                     注意端口规则不含地址，offline 下本地 dev server 同样连不上；\
                     且 ask 尚无网络审批面板，当前等同全拒。\
                     `git` 在沙盒内可用（普通可执行文件，经 launcher 施加规则）。"
                }
                _ => {
                    if config.net_policy == NetPolicy::Offline {
                        "网络：offline，沙盒内完全不能出网。"
                    } else {
                        "网络：经宿主的回环代理出网（HTTP_PROXY 注入）。这是应用层代理级，不是内核级：\
                         尊重代理变量的客户端（curl/pip/npm/git/cargo）受 `[sandbox.net]` 判定约束，\
                         直接读 WinINET 设置的 .NET 客户端可能绕开判定。裸 socket 仍然失败。"
                    }
                }
            };
            lines.push(network.to_string());
            // Windows 上 `git`/`bash` 是 MSYS 程序，在 AppContainer 内打不开 `NUL`，
            // 属于"用不了"而不是"被拦"。提前说清，免得模型反复重试。
            #[cfg(windows)]
            lines.push(
                "`git` 与 `bash` 在 Windows 沙盒内不可用（MSYS 依赖 `NUL` 设备，容器无访问权，\
                 授权目录解决不了）。改用 `powershell` 或 `read`/`write_file`/`Edit_file`/`grep`/`find`。"
                    .to_string(),
            );
        }
    }
    lines.push(format!(
        "用法：/sandbox [on|off|dry-run|appcontainer|landlock] [net <{SANDBOX_NET_POLICIES}>]"
    ));
    CommandResult::message(lines.join("\n"))
}

/// 切换沙盒开关 / 模式 / 网络策略（写回 `config.toml`）。
pub fn handle_sandbox_set_command(ctx: &CommandContext<'_>) -> CommandResult {
    let spec = ctx.args.trim().to_lowercase();
    if spec.is_empty() {
        return CommandResult::message(format!(
            "用法：/sandbox:set <on|off|dry-run|appcontainer|landlock> 或 `/sandbox:set net <策略>`\
             （可选策略 {SANDBOX_NET_POLICIES}）"
        ));
    }

    let environment = ctx.agent.config_environment();
    let mut data = match load_config_data(&environment, None) {
        Ok(data) => data,
        Err(error) => {
            return CommandResult::message(format!("读取 config.toml 失败：{}", error.message()))
        }
    };
    let mut section = match data.get("sandbox") {
        Some(Value::Table(table)) => table.clone(),
        None => omnicrawl_config::toml::Table::new(),
        Some(_) => {
            return CommandResult::message("❌ `sandbox` 必须是表（`[sandbox]`）。".to_string())
        }
    };

    let feedback = if let Some(policy) = spec.strip_prefix("net ") {
        let policy = match NetPolicy::parse(policy.trim()) {
            Ok(policy) => policy,
            Err(error) => return CommandResult::message(error.message().to_string()),
        };
        let mut net = match section.get("net") {
            Some(Value::Table(table)) => table.clone(),
            None => omnicrawl_config::toml::Table::new(),
            Some(_) => {
                return CommandResult::message("❌ `sandbox.net` 必须是表。".to_string());
            }
        };
        net.insert(
            "policy".to_string(),
            Value::String(policy.as_str().to_string()),
        );
        section.insert("net".to_string(), Value::Table(net));
        format!("沙盒网络策略已设为 {}。", policy.as_str())
    } else {
        match spec.as_str() {
            "off" => {
                section.insert("enabled".to_string(), Value::Boolean(false));
                "沙盒已关闭（`enabled = false`）。".to_string()
            }
            "on" | "dry-run" | "dry_run" => {
                section.insert("enabled".to_string(), Value::Boolean(true));
                section.insert(
                    "mode".to_string(),
                    Value::String(SandboxMode::Off.as_str().to_string()),
                );
                "沙盒已开启为 dry-run：只判定并记录「本会拦什么」，不真隔离。".to_string()
            }
            "appcontainer" => match SandboxMode::parse("appcontainer") {
                Ok(mode) => {
                    section.insert("enabled".to_string(), Value::Boolean(true));
                    section.insert("mode".to_string(), Value::String(mode.as_str().to_string()));
                    "沙盒已设为 appcontainer（仅 Windows）：命令以 AppContainer 身份执行，\
                     工作区可读写、宿主用户目录不可读，出网经宿主回环代理。"
                        .to_string()
                }
                Err(error) => return CommandResult::message(error.message().to_string()),
            },
            "landlock" | "linux" => match SandboxMode::parse("landlock") {
                Ok(mode) => {
                    section.insert("enabled".to_string(), Value::Boolean(true));
                    section.insert("mode".to_string(), Value::String(mode.as_str().to_string()));
                    "沙盒已设为 landlock（仅 Linux）：命令经 `sandbox-exec` 施加文件与网络规则，\
                     工作区可读写、宿主用户目录不可读。注意同 UID 的沙盒进程仍可向宿主发信号。"
                        .to_string()
                }
                Err(error) => return CommandResult::message(error.message().to_string()),
            },
            other => {
                return CommandResult::message(format!(
                    "❌ 无法识别的取值：{other}。只支持 on / off / dry-run / appcontainer / landlock，\
                     或 `net <{SANDBOX_NET_POLICIES}>`（mode 可选 {SANDBOX_MODES}）。"
                ))
            }
        }
    };

    data.insert("sandbox".to_string(), Value::Table(section));
    match save_config_data(&environment, &data, None) {
        Ok(path) => CommandResult::message(format!("{feedback}\n已写入 {}", path.display()))
            .with_refresh_context(),
        Err(error) => {
            CommandResult::message(format!("❌ 未能写入 config.toml：{}", error.message()))
        }
    }
}
