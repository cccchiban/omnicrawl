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
            let network = if config.net_policy == NetPolicy::Offline {
                "网络：offline，沙盒内完全不能出网。"
            } else {
                "网络：经宿主的回环代理出网（HTTP_PROXY 注入）。这是应用层代理级，不是内核级：\
                 尊重代理变量的客户端（curl/pip/npm/git/cargo）受 `[sandbox.net]` 判定约束，\
                 直接读 WinINET 设置的 .NET 客户端可能绕开判定。裸 socket 仍然失败。"
            };
            lines.push(network.to_string());
        }
    }
    lines.push(format!(
        "用法：/sandbox [on|off|dry-run|appcontainer] [net <{SANDBOX_NET_POLICIES}>]"
    ));
    CommandResult::message(lines.join("\n"))
}

/// 切换沙盒开关 / 模式 / 网络策略（写回 `config.toml`）。
pub fn handle_sandbox_set_command(ctx: &CommandContext<'_>) -> CommandResult {
    let spec = ctx.args.trim().to_lowercase();
    if spec.is_empty() {
        return CommandResult::message(format!(
            "用法：/sandbox:set <on|off|dry-run|appcontainer> 或 `/sandbox:set net <策略>`\
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
                    "沙盒已设为 appcontainer：命令以 AppContainer 身份执行，工作区可读写、\
                     宿主用户目录不可读，出网经宿主回环代理。"
                        .to_string()
                }
                Err(error) => return CommandResult::message(error.message().to_string()),
            },
            other => {
                return CommandResult::message(format!(
                    "❌ 无法识别的取值：{other}。只支持 on / off / dry-run / appcontainer，\
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
