//! 沙盒配置（`[sandbox]` 段）。
//!
//! 设计见 `rust/assets/docs/sandbox_windows_design.md`。本模块只负责**读配置与校验**，
//! 以及把配置折算成 [`SandboxPolicy`]。真正的隔离在 `omnicrawl-host` 的 `sandbox` 模块里。
//!
//! 三档开关（缺一档都会让用户踩坑，见设计文档 §9.2）：
//!
//! - `enabled = false`：与没有沙盒时完全一致，零额外开销；
//! - `enabled = true, mode = "off"`：**dry-run**，只判定"本会拦什么"并记录，不真隔离；
//! - `enabled = true, mode = "appcontainer"`：真隔离。

use std::path::Path;

use crate::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::Value;

/// `[sandbox] mode` 允许取值。
pub const SANDBOX_MODES: &str = "off, appcontainer";

/// `[sandbox.net] policy` 允许取值。
pub const SANDBOX_NET_POLICIES: &str = "offline, ask, allowlist, online";

/// 隔离强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// 只判定并记录，不真隔离（dry-run）。
    Off,
    /// AppContainer + Job Object 真隔离。
    AppContainer,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::AppContainer => "appcontainer",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.trim().to_lowercase().as_str() {
            "off" | "dry_run" | "dry-run" => Ok(Self::Off),
            "appcontainer" | "container" => Ok(Self::AppContainer),
            other => Err(ConfigError::new(format!(
                "sandbox.mode 仅支持 {SANDBOX_MODES}，当前值：{other}。"
            ))),
        }
    }
}

/// 网络策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetPolicy {
    /// 全拒（回环除外）。
    Offline,
    /// 每个新目标审批一次，会话内记住（默认）。
    Ask,
    /// 仅白名单放行，其余拒绝。
    Allowlist,
    /// 全放行，仅记录。
    Online,
}

impl NetPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Ask => "ask",
            Self::Allowlist => "allowlist",
            Self::Online => "online",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.trim().to_lowercase().as_str() {
            "offline" | "none" | "deny" => Ok(Self::Offline),
            "ask" | "confirm" | "approval" => Ok(Self::Ask),
            "allowlist" | "allow_list" | "whitelist" => Ok(Self::Allowlist),
            "online" | "allow" | "all" => Ok(Self::Online),
            other => Err(ConfigError::new(format!(
                "sandbox.net.policy 仅支持 {SANDBOX_NET_POLICIES}，当前值：{other}。"
            ))),
        }
    }
}

/// 资源限额。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    pub max_processes: u32,
    pub max_memory_mb: u64,
    /// 0 表示不设 CPU 速率上限（避免误伤正常编译）。
    pub cpu_rate_percent: u32,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_processes: 64,
            max_memory_mb: 4096,
            cpu_rate_percent: 0,
        }
    }
}

/// `[sandbox]` 段的完整取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub mode: SandboxMode,
    /// 额外可写路径（相对路径按工作区根）。
    pub writable: Vec<String>,
    /// 额外可读路径。
    pub readable: Vec<String>,
    pub net_policy: NetPolicy,
    /// `allowlist` 用的域名；支持 `*.suffix` 后缀匹配。
    pub net_allow: Vec<String>,
    /// 永远拒绝，优先级高于放行。
    pub net_deny: Vec<String>,
    pub net_approval_timeout_seconds: u64,
    pub net_log_all: bool,
    pub limits: SandboxLimits,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SandboxMode::Off,
            writable: Vec::new(),
            readable: Vec::new(),
            net_policy: NetPolicy::Ask,
            net_allow: Vec::new(),
            net_deny: Vec::new(),
            net_approval_timeout_seconds: 60,
            net_log_all: true,
            limits: SandboxLimits::default(),
        }
    }
}

impl SandboxConfig {
    /// 是否真的执行隔离（`enabled` 且 `mode = appcontainer`）。
    pub fn isolates(&self) -> bool {
        self.enabled && self.mode == SandboxMode::AppContainer
    }

    /// 是否 dry-run：开关打开但只判定不隔离。
    pub fn is_dry_run(&self) -> bool {
        self.enabled && self.mode == SandboxMode::Off
    }

    /// `/sandbox` 里的一行状态摘要。
    pub fn status_line(&self) -> String {
        if !self.enabled {
            return "沙盒：关闭（`[sandbox] enabled = false`）。子进程以宿主身份运行，".to_string()
                + "仅受审批约束。";
        }
        if self.is_dry_run() {
            return format!(
                "沙盒：dry-run（`mode = off`）。只判定并记录「本会拦什么」，不真隔离；网络策略 {}。",
                self.net_policy.as_str()
            );
        }
        format!(
            "沙盒：appcontainer（真隔离）。网络策略 {}，进程上限 {}，内存上限 {}MB。",
            self.net_policy.as_str(),
            self.limits.max_processes,
            self.limits.max_memory_mb
        )
    }
}

/// 读取 `[sandbox]` 段；缺项回落到默认值（即"关闭"）。
///
/// 任何类型错误都返回可读中文错误，而不是静默用默认值——沙盒配置写错却按"关闭"跑，
/// 是最危险的一种静默失败。
pub fn load_sandbox_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<SandboxConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, "sandbox")?;
    let mut config = SandboxConfig::default();
    if section.is_empty() {
        return Ok(config);
    }

    config.enabled = flag(&section, "enabled", "sandbox.enabled", false)?;
    if let Some(value) = text(&section, "mode", "sandbox.mode")? {
        config.mode = SandboxMode::parse(&value)?;
    }

    // `[sandbox.fs]` 在解析后是 `sandbox` 表里的 `fs` 子表，不是顶层 `"sandbox.fs"` 键。
    let fs = nested(&section, "fs")?;
    if let Some(list) = string_list(&fs, "writable", "sandbox.fs.writable")? {
        config.writable = list;
    }
    if let Some(list) = string_list(&fs, "readable", "sandbox.fs.readable")? {
        config.readable = list;
    }

    let net = nested(&section, "net")?;
    if let Some(value) = text(&net, "policy", "sandbox.net.policy")? {
        config.net_policy = NetPolicy::parse(&value)?;
    }
    if let Some(list) = string_list(&net, "allow", "sandbox.net.allow")? {
        config.net_allow = list;
    }
    if let Some(list) = string_list(&net, "deny", "sandbox.net.deny")? {
        config.net_deny = list;
    }
    if let Some(value) = integer(
        &net,
        "approval_timeout_seconds",
        "sandbox.net.approval_timeout_seconds",
    )? {
        config.net_approval_timeout_seconds = value.max(1) as u64;
    }
    config.net_log_all = flag(&net, "log_all", "sandbox.net.log_all", true)?;

    let limits = nested(&section, "limits")?;
    if let Some(value) = integer(&limits, "max_processes", "sandbox.limits.max_processes")? {
        config.limits.max_processes = value.clamp(1, 4096) as u32;
    }
    if let Some(value) = integer(&limits, "max_memory_mb", "sandbox.limits.max_memory_mb")? {
        config.limits.max_memory_mb = value.clamp(64, 1024 * 1024) as u64;
    }
    if let Some(value) = integer(
        &limits,
        "cpu_rate_percent",
        "sandbox.limits.cpu_rate_percent",
    )? {
        config.limits.cpu_rate_percent = value.clamp(0, 100) as u32;
    }

    validate(&config)?;
    Ok(config)
}

/// 语义校验：跨字段的约束放在这里，单字段的类型错误在上面的读取里就报掉了。
pub fn validate(config: &SandboxConfig) -> Result<(), ConfigError> {
    if config.net_policy == NetPolicy::Allowlist && config.net_allow.is_empty() {
        return Err(ConfigError::new(
            "sandbox.net.policy = \"allowlist\" 但 sandbox.net.allow 为空：\
             这会让沙盒完全无法联网，请补白名单或改用 offline。",
        ));
    }
    if config.isolates() && config.limits.max_processes < 2 {
        return Err(ConfigError::new(
            "sandbox.limits.max_processes 至少为 2：命令本身就要占一个进程，\
             上限 1 会让所有命令立刻被杀。",
        ));
    }
    Ok(())
}

/// `[sandbox.fs]` 这类子表：`get_section` 只认表名，嵌套表要在这里取。
fn nested(section: &crate::toml::Table, key: &str) -> Result<crate::toml::Table, ConfigError> {
    match section.get(key) {
        None => Ok(crate::toml::Table::new()),
        Some(Value::Table(table)) => Ok(table.clone()),
        Some(_) => Err(ConfigError::new(format!(
            "配置项 sandbox.{key} 必须是对象。"
        ))),
    }
}

fn flag(
    section: &crate::toml::Table,
    key: &str,
    path: &str,
    default: bool,
) -> Result<bool, ConfigError> {
    match section.get(key) {
        None => Ok(default),
        Some(Value::Boolean(value)) => Ok(*value),
        Some(_) => Err(ConfigError::new(format!("配置项 {path} 必须是布尔值。"))),
    }
}

fn text(
    section: &crate::toml::Table,
    key: &str,
    path: &str,
) -> Result<Option<String>, ConfigError> {
    match section.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim().to_string())),
        Some(_) => Err(ConfigError::new(format!("配置项 {path} 必须是字符串。"))),
    }
}

fn integer(
    section: &crate::toml::Table,
    key: &str,
    path: &str,
) -> Result<Option<i64>, ConfigError> {
    match section.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(ConfigError::new(format!("配置项 {path} 必须是整数。"))),
    }
}

fn string_list(
    section: &crate::toml::Table,
    key: &str,
    path: &str,
) -> Result<Option<Vec<String>>, ConfigError> {
    match section.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(text) => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_string());
                        }
                    }
                    _ => {
                        return Err(ConfigError::new(format!(
                            "配置项 {path} 的每一项都必须是字符串。"
                        )));
                    }
                }
            }
            Ok(Some(out))
        }
        // 单个字符串也接受：`allow = "example.com"` 不必写成数组。
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            Ok(Some(if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_string()]
            }))
        }
        Some(_) => Err(ConfigError::new(format!(
            "配置项 {path} 必须是字符串数组。"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_from(text: &str) -> Result<SandboxConfig, ConfigError> {
        // 目录名必须每次唯一：按进程 ID 命名会让并发跑的用例写同一个文件、互相踩踏。
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "omnicrawl-sandbox-config-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("config.toml");
        std::fs::write(&path, text).expect("写配置");
        let env = ConfigEnvironment::new(dir.clone(), std::env::consts::OS);
        let result = load_sandbox_config(&env, Some(&path));
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    #[test]
    fn missing_section_means_disabled() {
        let config = load_from("").expect("空配置可用");
        assert!(!config.enabled);
        assert!(!config.isolates());
        assert!(!config.is_dry_run());
    }

    #[test]
    fn enabled_with_mode_off_is_dry_run() {
        let config = load_from("[sandbox]\nenabled = true\nmode = \"off\"\n").expect("应可读");
        assert!(config.enabled);
        assert!(config.is_dry_run());
        assert!(!config.isolates());
        assert!(config.status_line().contains("dry-run"));
    }

    #[test]
    fn enabled_with_appcontainer_isolates() {
        let config =
            load_from("[sandbox]\nenabled = true\nmode = \"appcontainer\"\n").expect("应可读");
        assert!(config.isolates());
        assert!(!config.is_dry_run());
    }

    #[test]
    fn disabled_overrides_mode() {
        // enabled=false 时即使写了 appcontainer 也不隔离：开关优先。
        let config =
            load_from("[sandbox]\nenabled = false\nmode = \"appcontainer\"\n").expect("应可读");
        assert!(!config.isolates());
        assert!(!config.is_dry_run());
        assert!(config.status_line().contains("关闭"));
    }

    #[test]
    fn nested_tables_are_read() {
        let config = load_from(
            r#"
[sandbox]
enabled = true
mode = "appcontainer"

[sandbox.fs]
writable = ["输出", "D:/data"]
readable = ["C:/tools"]

[sandbox.net]
policy = "allowlist"
allow = ["registry.npmjs.org", "*.githubusercontent.com"]
deny = ["evil.example.com"]
approval_timeout_seconds = 30
log_all = false

[sandbox.limits]
max_processes = 16
max_memory_mb = 2048
cpu_rate_percent = 50
"#,
        )
        .expect("应可读");
        assert_eq!(config.writable, vec!["输出", "D:/data"]);
        assert_eq!(config.readable, vec!["C:/tools"]);
        assert_eq!(config.net_policy, NetPolicy::Allowlist);
        assert_eq!(config.net_allow.len(), 2);
        assert_eq!(config.net_deny, vec!["evil.example.com"]);
        assert_eq!(config.net_approval_timeout_seconds, 30);
        assert!(!config.net_log_all);
        assert_eq!(config.limits.max_processes, 16);
        assert_eq!(config.limits.max_memory_mb, 2048);
        assert_eq!(config.limits.cpu_rate_percent, 50);
    }

    #[test]
    fn inline_subtables_are_read() {
        // TOML 允许写 `net = { policy = "offline" }`，不应被当成错误。
        let config = load_from(
            "[sandbox]\nenabled = true\nmode = \"off\"\nnet = { policy = \"offline\" }\n",
        )
        .expect("内联子表应可读");
        assert_eq!(config.net_policy, NetPolicy::Offline);
    }

    #[test]
    fn a_single_string_is_accepted_for_lists() {
        let config = load_from("[sandbox.net]\npolicy = \"allowlist\"\nallow = \"example.com\"\n")
            .expect("单字符串也接受");
        assert_eq!(config.net_allow, vec!["example.com"]);
    }

    #[test]
    fn allowlist_without_entries_is_rejected() {
        let error = load_from("[sandbox.net]\npolicy = \"allowlist\"\n").expect_err("应报错");
        assert!(error.message().contains("allow"), "{}", error.message());
    }

    #[test]
    fn wrong_types_are_reported_not_ignored() {
        // 关键：配置写错不能静默按"关闭"跑。
        let error = load_from("[sandbox]\nenabled = \"yes\"\n").expect_err("应报错");
        assert!(error.message().contains("布尔"), "{}", error.message());

        let error = load_from("[sandbox]\nmode = \"vm\"\n").expect_err("应报错");
        assert!(
            error.message().contains("appcontainer"),
            "{}",
            error.message()
        );

        let error = load_from("[sandbox.net]\npolicy = \"maybe\"\n").expect_err("应报错");
        assert!(error.message().contains("offline"), "{}", error.message());
    }

    #[test]
    fn limits_are_clamped_into_a_sane_range() {
        let config = load_from(
            "[sandbox]\nenabled = true\nmode=\"appcontainer\"\n[sandbox.limits]\nmax_processes = 99999\nmax_memory_mb = 1\n",
        )
        .expect("应可读");
        assert_eq!(config.limits.max_processes, 4096);
        assert_eq!(config.limits.max_memory_mb, 64);
    }

    #[test]
    fn mode_and_policy_aliases_resolve() {
        assert_eq!(
            SandboxMode::parse("dry_run").expect("别名"),
            SandboxMode::Off
        );
        assert_eq!(
            SandboxMode::parse("AppContainer").expect("大小写不敏感"),
            SandboxMode::AppContainer
        );
        assert_eq!(
            NetPolicy::parse("whitelist").expect("别名"),
            NetPolicy::Allowlist
        );
        assert_eq!(NetPolicy::parse("ALL").expect("别名"), NetPolicy::Online);
        assert_eq!(NetPolicy::parse("deny").expect("别名"), NetPolicy::Offline);
    }

    #[test]
    fn status_line_matches_the_three_states() {
        let mut config = SandboxConfig::default();
        assert!(config.status_line().contains("关闭"));
        config.enabled = true;
        assert!(config.status_line().contains("dry-run"));
        config.mode = SandboxMode::AppContainer;
        let line = config.status_line();
        assert!(line.contains("appcontainer"), "{line}");
        assert!(line.contains("4096MB"), "{line}");
    }

    #[test]
    fn a_single_process_limit_is_rejected_when_isolating() {
        let error = load_from(
            "[sandbox]\nenabled=true\nmode=\"appcontainer\"\n[sandbox.limits]\nmax_processes=1\n",
        )
        .expect_err("应报错");
        assert!(error.message().contains("至少为 2"), "{}", error.message());
        // 不隔离时同样的配置不报错：只有真隔离才需要这个约束。
        assert!(load_from(
            "[sandbox]\nenabled=true\nmode=\"off\"\n[sandbox.limits]\nmax_processes=1\n"
        )
        .is_ok());
    }
}
