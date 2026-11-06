//! 沙盒开关到执行路径的桥：惰性准备隔离环境，并区分「关 / dry-run / 真隔离」。
//!
//! 设计（`sandbox_windows_design.md` §13、`sandbox_linux_design.md` §10.1）要求
//! 「沙盒关闭时，全部既有测试零变化」，因此 [`IsolationGate`] 在关闭状态下不做任何事。
//!
//! 平台与模式必须对得上：`mode = appcontainer` 只在 Windows 有意义，`mode = landlock`
//! 只在 Linux。配错时**报错而不是退化成不隔离**——能跑但没隔离，比跑不了危险得多。

use std::sync::Arc;

use omnicrawl_config::features::sandbox::SandboxMode;

use super::Sandbox;

/// 一条命令该怎么跑。
pub enum Plan {
    /// 以宿主身份直接跑（沙盒关闭）。
    Host,
    /// 真隔离：拿一个准备好的容器身份（Windows AppContainer）。
    #[cfg(windows)]
    Isolated(Arc<super::windows::Isolation>),
    /// Linux 侧真隔离：策略已折算，命令经 `sandbox-exec` launcher 施加规则后执行。
    #[cfg(target_os = "linux")]
    Landlock(Arc<super::linux::Isolation>),
}

impl Plan {
    /// 是否走真隔离。
    pub fn is_isolated(&self) -> bool {
        match self {
            Self::Host => false,
            #[cfg(windows)]
            Self::Isolated(_) => true,
            #[cfg(target_os = "linux")]
            Self::Landlock(_) => true,
        }
    }
}

pub struct IsolationGate {
    sandbox: Arc<Sandbox>,
}

impl IsolationGate {
    pub fn new(sandbox: Arc<Sandbox>) -> Self {
        Self { sandbox }
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// 是否真隔离，且**当前平台真能做到**。
    ///
    /// 与 `Sandbox::isolates()` 的区别：后者只看配置（"要求隔离"），这里还要求平台与模式
    /// 匹配。工具表开关、`bash` 拒绝这类"这个入口在沙盒里有没有意义"的判断只能用这个，
    /// 否则 Linux 上写了 `mode = appcontainer` 就会把 `bash` 误踢出工具表。
    pub fn isolates(&self) -> bool {
        if !self.sandbox.isolates() {
            return false;
        }
        match self.sandbox.config().mode {
            #[cfg(windows)]
            SandboxMode::AppContainer => true,
            #[cfg(target_os = "linux")]
            SandboxMode::Landlock => true,
            _ => false,
        }
    }

    /// 决定执行方式。
    ///
    /// 真隔离模式下**任何准备失败都不回退到宿主身份**：沙盒的价值就在于失败方向是
    /// 「跑不了」，而不是「悄悄不隔离」。错误会带着具体原因返回给模型。
    pub fn plan(&self) -> Result<Plan, String> {
        if !self.sandbox.isolates() {
            return Ok(Plan::Host);
        }
        let mode = self.sandbox.config().mode;
        platform_plan(self, mode)
    }

    /// 输出里的一行标记，让模型与用户都能看出这条命令在沙盒里跑。
    pub fn label(&self) -> String {
        if !self.isolates() {
            return String::new();
        }
        format!("[sandbox:{}]", self.sandbox.config().mode.as_str())
    }

    /// 诊断里的能力说明；不隔离时为空。
    #[cfg(target_os = "linux")]
    pub fn capability_note(&self) -> Option<String> {
        self.sandbox
            .isolates()
            .then(|| self.sandbox.capability().describe())
    }
}

#[cfg(windows)]
fn platform_plan(gate: &IsolationGate, mode: SandboxMode) -> Result<Plan, String> {
    if mode != SandboxMode::AppContainer {
        return Err(mismatch_message(mode, "Windows", "appcontainer"));
    }
    // 网络通道与隔离身份同属「真隔离」这一档，必须一起就绪：容器没有网络能力，
    // 出网只能经宿主回环代理，没有代理就是彻底断网（不是回避隔离）。
    //
    // `ensure_network` 自身幂等且**不返回错误**：豁免或代理拉不起来时它把形态降级为
    // `NetStatus::Failed` 且不注入代理变量，于是这条命令照样在容器里跑，只是出不去。
    // 因此这里不必把它的结果变成命令失败——失败方向仍然是关闭。
    gate.sandbox.ensure_network(None);
    gate.sandbox.isolation().map(Plan::Isolated)
}

/// Linux：探测能力、折算策略，交给 `sandbox-exec` launcher 施加。
#[cfg(target_os = "linux")]
fn platform_plan(gate: &IsolationGate, mode: SandboxMode) -> Result<Plan, String> {
    if mode != SandboxMode::Landlock {
        return Err(mismatch_message(mode, "Linux", "landlock"));
    }
    let capability = gate.sandbox.capability();
    if !capability.can_isolate() {
        return Err(format!(
            "Landlock 不可用，沙盒无法启用：{}。\
             可把 `[sandbox] mode` 设为 off 用 dry-run 观测，或升级/启用内核 Landlock。",
            capability.describe()
        ));
    }
    let abi = capability
        .abi()
        .ok_or_else(|| "Landlock 探测未给出 ABI 版本，沙盒无法启用。".to_string())?;
    // `offline` 承诺的是内核级断网；内核管不了网络时必须拒绝，不能口头保证。
    capability.check_network_policy(gate.sandbox.config().net_policy)?;

    // Linux 侧在网络上的分工（设计文档 §7.2）：端口规则只留代理这一扇门，
    // 域名判定全在代理里。因此 `offline` 不必起代理（它连代理端口都不放行），
    // 其余三档必须有代理——没有代理就等于断网，这正是失败方向为关闭。
    let proxy = match gate.sandbox.config().net_policy {
        omnicrawl_config::features::sandbox::NetPolicy::Offline => None,
        _ => match gate.sandbox.ensure_network(None) {
            super::NetStatus::Proxy(port) => Some(port),
            // 拉不起来就当作断网：策略里不放行任何端口，命令照跑但出不去。
            _ => None,
        },
    };

    let isolation = super::linux::Isolation::prepare(
        gate.sandbox.config(),
        gate.sandbox.root(),
        &gate.sandbox.sandbox_temp(),
        abi,
        proxy,
    )?;
    // 把本次折算出的限额摘要留给 `/sandbox`：它不该为看状态专门建一个 cgroup，
    // 因此只缓存已经得到的结果。
    gate.sandbox.record_limits(isolation.summary().to_string());
    Ok(Plan::Landlock(isolation))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn platform_plan(_gate: &IsolationGate, mode: SandboxMode) -> Result<Plan, String> {
    Err(format!(
        "沙盒真隔离目前只支持 Windows 与 Linux；当前平台（{}）请把 `[sandbox] mode` 设为 off，\
         或把 enabled 设为 false。当前 mode = \"{}\"。",
        std::env::consts::OS,
        mode.as_str()
    ))
}

/// 模式与平台对不上时的话术：说清是哪一档、该改什么。
///
/// `expected` 是该平台唯一可用的真隔离档位，由调用方按平台给出，避免这里再写一遍 cfg。
///
/// 在 Windows 与 Linux 之外的目标上两个 `platform_plan` 都不存在，本函数因此没有调用方。
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn mismatch_message(mode: SandboxMode, platform: &str, expected: &str) -> String {
    let native = match mode {
        SandboxMode::AppContainer => "Windows",
        SandboxMode::Landlock => "Linux",
        // `plan` 在 mode = off 时已提前返回 Host，走不到这里。
        SandboxMode::Off => "关闭档",
    };
    format!(
        "`[sandbox] mode = \"{}\"` 在 {platform} 上不可用：该模式只支持 {native}。\
         当前平台请改用 mode = \"{expected}\"。",
        mode.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::sandbox::{NetPolicy, SandboxConfig};

    fn sandbox_with(mode: SandboxMode, policy: NetPolicy) -> Arc<Sandbox> {
        let mut config = SandboxConfig::default();
        config.enabled = true;
        config.mode = mode;
        config.net_policy = policy;
        Arc::new(Sandbox::new(config, "/work", "/work/.omnicrawl/.agent_tmp"))
    }

    /// Linux 侧的准备会在临时目录下建 `HOME`，所以测试必须给一个真能写的根，
    /// 否则会试图写 `/work` 这种不存在的系统路径。
    #[cfg(target_os = "linux")]
    fn sandbox_in_scratch(mode: SandboxMode, policy: NetPolicy) -> Arc<Sandbox> {
        let root = std::env::temp_dir().join(format!(
            "omnicrawl-gate-{}-{}",
            std::process::id(),
            mode.as_str()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".omnicrawl/.agent_tmp")).expect("应能建临时目录");
        let mut config = SandboxConfig::default();
        config.enabled = true;
        config.mode = mode;
        config.net_policy = policy;
        Arc::new(Sandbox::new(
            config,
            &root,
            &root.join(".omnicrawl/.agent_tmp"),
        ))
    }

    #[test]
    fn disabled_sandbox_always_plans_host() {
        let sandbox = Arc::new(Sandbox::disabled("/work", "/work/.omnicrawl/.agent_tmp"));
        let gate = IsolationGate::new(sandbox);
        assert!(!gate.isolates());
        assert!(matches!(gate.plan(), Ok(Plan::Host)));
        assert!(gate.label().is_empty());
    }

    #[test]
    fn dry_run_plans_host_without_label() {
        let gate = IsolationGate::new(sandbox_with(SandboxMode::Off, NetPolicy::Ask));
        assert!(!gate.isolates());
        assert!(matches!(gate.plan(), Ok(Plan::Host)));
        assert!(
            gate.label().is_empty(),
            "dry-run 没真隔离，不该打沙盒标记"
        );
    }

    /// 配置要求隔离但平台不支持时，必须**报错**而不是退回宿主身份。
    #[test]
    fn mismatched_mode_is_refused_not_downgraded() {
        #[cfg(target_os = "linux")]
        let (wrong, right) = (SandboxMode::AppContainer, SandboxMode::Landlock);
        #[cfg(windows)]
        let (wrong, right) = (SandboxMode::Landlock, SandboxMode::AppContainer);
        #[cfg(not(any(windows, target_os = "linux")))]
        return;

        let gate = IsolationGate::new(sandbox_with(wrong, NetPolicy::Ask));
        assert!(gate.sandbox().isolates(), "配置层仍认为要求隔离");
        assert!(!gate.isolates(), "但当前平台做不到");
        let error = match gate.plan() {
            Ok(_) => panic!("必须报错"),
            Err(error) => error,
        };
        assert!(error.contains(wrong.as_str()), "{error}");
        assert!(error.contains(right.as_str()), "应指出正确的档位：{error}");
    }

    #[test]
    fn label_names_the_active_mode() {
        // 用当前平台真正支持的那一档构造，避免依赖具体平台的分支。
        #[cfg(windows)]
        let mode = SandboxMode::AppContainer;
        #[cfg(target_os = "linux")]
        let mode = SandboxMode::Landlock;
        #[cfg(not(any(windows, target_os = "linux")))]
        return;

        let gate = IsolationGate::new(sandbox_with(mode, NetPolicy::Ask));
        assert!(gate.isolates());
        assert_eq!(gate.label(), format!("[sandbox:{}]", mode.as_str()));
    }

    /// Linux：真隔离档必须给出可执行计划（launcher 已实现），且带出实际内核能力。
    ///
    /// 内核不支持 Landlock 时只能报错——这条分支也要覆盖，所以两种结果都接受，
    /// 但**绝不能**悄悄退回宿主。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_landlock_plans_or_reports_why_not() {
        let sandbox = sandbox_in_scratch(SandboxMode::Landlock, NetPolicy::Offline);
        let root = sandbox.root().to_path_buf();
        let gate = IsolationGate::new(sandbox);
        match gate.plan() {
            Ok(Plan::Host) => panic!("landlock 档绝不能悄悄退回宿主"),
            Ok(Plan::Landlock(isolation)) => {
                assert!(isolation.abi() >= 1, "ABI 至少为 1");
                assert!(isolation.summary().contains("ABI"), "{}", isolation.summary());
                // 相对路径会按工作区根解析；这里只要求策略能被 wrap 出一层 launcher。
                let wrapped = isolation.wrap(&["/bin/true".to_string()]);
                assert_eq!(wrapped[1], crate::sandbox::linux::LAUNCHER_COMMAND);
                assert_eq!(wrapped[2], "--");
            }
            Err(error) => {
                // 内核不支持 Landlock 时必须说清原因，而不是含糊失败。
                assert!(
                    error.contains("Landlock"),
                    "拒绝执行应说明是 Landlock 不可用：{error}"
                );
            }
        }
        // 能力说明不看是否隔离：隔离档下必须能取到诊断。
        assert!(gate.capability_note().is_some());
        let _ = std::fs::remove_dir_all(&root);
    }
}
