//! Linux 隔离后端。
//!
//! 设计见 `rust/assets/docs/sandbox_linux_design.md`。分三块：
//!
//! - [`probe`] / [`Capability`]：问内核要 Landlock ABI 版本（P0，只读无副作用）；
//! - [`policy`]：把 `[sandbox]` 配置折算成路径/端口规则（纯逻辑，可单测）；
//! - [`launcher`]：真正施加规则并 `execvp` 的子进程（被宿主以 `sandbox-exec` 拉起）。
//!
//! 探测全部是只读系统调用：`landlock_create_ruleset(NULL, 0, VERSION)` 只返回版本号，
//! 不建规则集、不改本进程权限。因此在宿主里探测是安全的，不会把宿主自己关进沙盒。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use omnicrawl_config::features::sandbox::{NetPolicy, SandboxConfig};

pub mod cgroup;
pub mod launcher;
pub mod policy;
pub mod runtimes;

pub use launcher::LAUNCHER_COMMAND;
pub use policy::Policy;

// 内核 UAPI 常量（`linux/landlock.h`）。`libc` 只导出 `SYS_landlock_*` 系统调用号，
// 不导出这些取值，故按内核 ABI 自行定义——这些值一经发布即稳定。
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;

/// `landlock_create_ruleset` 的返回：探测成功或失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// 可用，值为 ABI 版本（≥ 1）。
    Supported(u32),
    /// 已编译进内核但启动时被禁用（`EOPNOTSUPP`）。
    Disabled,
    /// 内核不含 Landlock（`ENOSYS`，通常是 < 5.13）。
    Unsupported,
    /// 其它 errno，原样带出便于排查。
    Failed(i32),
}

/// 一次探测的结果与由此推出的能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub probe: Probe,
}

impl Capability {
    /// 真隔离是否可做。`Disabled`/`Unsupported` 都不行。
    pub fn can_isolate(&self) -> bool {
        matches!(self.probe, Probe::Supported(_))
    }

    pub fn abi(&self) -> Option<u32> {
        match self.probe {
            Probe::Supported(abi) => Some(abi),
            _ => None,
        }
    }

    /// 能不能在**内核层**管网络（ABI ≥ 4，Linux 6.7+）。
    ///
    /// 这条决定 `offline` 策略能否兑现：不能管网络时宣称"沙盒内不能出网"就是假话。
    pub fn controls_network(&self) -> bool {
        self.abi().is_some_and(|abi| abi >= 4)
    }

    /// 能不能管 UDP 端口（ABI ≥ 10）。
    pub fn controls_udp(&self) -> bool {
        self.abi().is_some_and(|abi| abi >= 10)
    }

    /// 这一档能力能不能满足给定网络策略，不能则给出人话原因。
    ///
    /// 规则：`offline` 声称的是**内核级断网**，因此必须有内核网络控制；
    /// 其余三档本来就是应用层代理级（代理不论 ABI 都在），无需网络内核支持。
    pub fn check_network_policy(&self, policy: NetPolicy) -> Result<(), String> {
        match policy {
            NetPolicy::Offline if !self.controls_network() => Err(format!(
                "网络策略 offline 要求内核能管网络（Landlock ABI ≥ 4，Linux 6.7+），\
                 当前{}。改为 ask/allowlist（应用层代理级）或升级内核，否则无法兑现「沙盒内不能出网」。",
                self.describe()
            )),
            _ => Ok(()),
        }
    }

    /// 一行诊断：ABI 与"哪些能力没管"。
    pub fn describe(&self) -> String {
        match self.probe {
            Probe::Supported(abi) => {
                let mut parts = vec![format!("Landlock ABI {abi}")];
                if !self.controls_network() {
                    parts.push("网络端口未受控（需 ABI ≥ 4）".to_string());
                } else if !self.controls_udp() {
                    parts.push("UDP 端口未受控（需 ABI ≥ 10）".to_string());
                }
                parts.join("，")
            }
            Probe::Disabled => "Landlock 已在启动时被禁用（内核参数 lsm= 未含 landlock）".to_string(),
            Probe::Unsupported => "内核不含 Landlock（需 5.13+）".to_string(),
            Probe::Failed(code) => format!("Landlock 探测失败（errno {code}）"),
        }
    }
}

/// 向内核询问 Landlock ABI 版本。
///
/// `attr` 必须为 NULL、`size` 必须为 0，`flags` 取 `LANDLOCK_CREATE_RULESET_VERSION`；
/// 成功时返回值**就是** ABI 版本而不是 fd——这是该 flag 的特殊语义（见 `landlock_create_ruleset(2)`）。
pub fn probe() -> Probe {
    // SAFETY: 传 NULL attr + size 0 + VERSION flag 是内核文档指定的"只查版本"用法：
    // 不读任何用户指针、不建 fd、不改本进程权限。返回值 < 0 时按 errno 归类。
    //
    // 变参必须显式注明宽度：`syscall` 是 variadic，C 的默认参数提升不会发生在 Rust 侧，
    // `usize`/`u32` 直接传入在部分 ABI 上会取错寄存器宽度。
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0 as libc::size_t,
            LANDLOCK_CREATE_RULESET_VERSION as libc::c_uint,
        )
    };
    if result >= 0 {
        return Probe::Supported(result as u32);
    }
    let code = std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default();
    match code {
        libc::ENOSYS => Probe::Unsupported,
        libc::EOPNOTSUPP => Probe::Disabled,
        other => Probe::Failed(other),
    }
}

/// 探测并折算成能力。结果不缓存：调用方（[`super::LinuxCapabilityCache`]）负责缓存，
/// 免得每次命令都撞一次系统调用。
pub fn capability() -> Capability {
    Capability { probe: probe() }
}

/// 一次准备就绪的 Linux 隔离：策略已折算、launcher 路径已确定。
///
/// 与 Windows 的 `Isolation`（容器身份 + ACL）不同，这里**没有持久内核对象**：
/// Landlock 规则只能由被约束的进程自己施加，因此每次命令都要一个新 launcher 进程。
/// 可复用的是"策略怎么算"（便宜），不是"内核里的什么东西"。
#[derive(Debug, Clone)]
pub struct Isolation {
    abi: u32,
    /// 策略文本（已编码，直接喂给 launcher）。
    encoded: String,
    /// 诊断用的一句话。
    summary: String,
    /// launcher 的可执行文件：就是宿主自己（`current_exe()`）。
    exe: PathBuf,
    /// 宿主回环代理地址（`http://127.0.0.1:<port>`）。`offline` 下为 `None`。
    proxy_url: Option<String>,
    /// 沙盒内的 `HOME`：真实家目录按策略不可读，留着真实路径只会让程序拿到打不开的位置。
    home: PathBuf,
    /// 沙盒内的 `TMPDIR`。
    temp: PathBuf,
    /// 沙盒内的依赖缓存根（`CARGO_HOME`/`npm_config_cache`/`PIP_CACHE_DIR` 指这里）。
    cache: PathBuf,
}

impl Isolation {
    /// 按配置准备一次隔离。
    ///
    /// `proxy` 是宿主回环代理端口（`offline` 下传 `None`，表示真断网）。
    ///
    /// 失败一律返回原因：**没有"准备不出来就退回宿主身份"这条路**。
    pub fn prepare(
        config: &SandboxConfig,
        root: &Path,
        sandbox_temp: &Path,
        abi: u32,
        proxy: Option<u16>,
    ) -> Result<Arc<Self>, String> {
        // 目录必须先落地再折算策略：`Policy::existing()` 会丢弃不存在的路径，
        // 而沙盒临时目录通常还不存在——顺序反了就会把"可写临时目录"整条规则悄悄丢掉，
        // 表现为沙盒内任何写临时文件的操作都以 `EACCES` 失败。
        let home = sandbox_temp.join("home");
        std::fs::create_dir_all(&home)
            .map_err(|error| format!("无法创建沙盒家目录 {}：{error}", home.display()))?;
        // 依赖缓存也放在沙盒内：`cargo`/`npm`/`pip` 拿不到可写的缓存目录会在建缓存时失败，
        // 但把 `~/.cargo` 这类**真实**缓存改授可写等于让沙盒污染宿主的依赖树
        // （供应链层面比"构建失败"严重）。重定向到沙盒内既让构建跑通，又不留痕。
        let cache = sandbox_temp.join("cache");
        std::fs::create_dir_all(&cache)
            .map_err(|error| format!("无法创建沙盒缓存目录 {}：{error}", cache.display()))?;

        let env = |name: &str| std::env::var(name).ok();
        let mut policy = Policy::build(
            config,
            root,
            sandbox_temp,
            abi,
            proxy.map(policy::ProxyPort),
            &env,
        );
        // 资源限额（§8）：拿不到就不设，并如实记下原因——绝不因此拒绝执行。
        let limits = cgroup::create(&config.limits);
        match &limits {
            Ok(applied) => policy.cgroup = Some(applied.path.clone()),
            // 建不起来时策略里不带 `cgroup` 行，launcher 因此不迁入任何 cgroup。
            Err(_) => policy.cgroup = None,
        }
        policy.validate(config, root)?;
        let policy = policy.existing();
        let summary = format!(
            "{}；{}",
            policy.describe(abi),
            match &limits {
                Ok(applied) => applied.describe(),
                Err(reason) => format!("资源限额未启用（{reason}）"),
            }
        );
        let encoded = format!("abi {abi}\n{}", launcher::encode_policy(&policy));

        let exe = std::env::current_exe()
            .map_err(|error| format!("无法定位宿主可执行文件，沙盒 launcher 不可用：{error}"))?;

        Ok(Arc::new(Self {
            abi,
            encoded,
            summary,
            exe,
            proxy_url: proxy.map(|port| format!("http://127.0.0.1:{port}")),
            home,
            temp: sandbox_temp.to_path_buf(),
            cache,
        }))
    }

    pub fn abi(&self) -> u32 {
        self.abi
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// 把真实命令包装成 launcher 调用：`[exe, "sandbox-exec", "--", argv…]`。
    ///
    /// `--` 之后的内容原样传给 `execvp`，因此命令里带 `--sandbox-exec` 之类的字样
    /// 不会被误认成宿主子命令。
    pub fn wrap(&self, argv: &[String]) -> Vec<String> {
        let mut wrapped = Vec::with_capacity(argv.len() + 3);
        wrapped.push(self.exe.to_string_lossy().to_string());
        wrapped.push(LAUNCHER_COMMAND.to_string());
        wrapped.push("--".to_string());
        wrapped.extend(argv.iter().cloned());
        wrapped
    }

    /// 拉起 launcher，并把策略经 stdin 交过去。
    ///
    /// 返回已搭好的 [`Command`]（stdin 已接管），由调用方决定 stdout/stderr 与环境。
    /// 这样两条宿主路径（`command.rs` 同步等待、`monitor.rs` 后台流式）能共用同一份
    /// 包装逻辑，只有 IO 组织不同。
    pub fn command(&self, argv: &[String]) -> Result<Command, String> {
        let wrapped = self.wrap(argv);
        let mut command = Command::new(&wrapped[0]);
        command
            .args(&wrapped[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    /// 策略正文，供调用方写进 stdin。
    pub fn policy_text(&self) -> &str {
        &self.encoded
    }

    /// 把沙盒环境装到 launcher 进程上。
    ///
    /// 两件事，缺一不可（设计文档 §7.4）：
    ///
    /// 1. **清空宿主代理变量**。宿主若自身配了 `HTTP_PROXY`，不清理就会被继承，
    ///    于是沙盒内客户端直奔用户代理，我们的域名判定被整段跳过——`offline`
    ///    看起来"断网"实则畅通。
    /// 2. **注入宿主回环代理**。这是 `ask`/`allowlist` 的唯一通路；不注入就等于断网。
    ///
    /// 同时改写 `HOME`/`TMPDIR`：真实家目录按策略不可读，留着只会让程序拿到
    /// 一个打不开的位置后行为诡异（Windows 侧 `whoami` 因此崩溃过）。
    pub fn apply_environment(&self, command: &mut Command) {
        for name in PROXY_VARS {
            command.env_remove(name);
        }
        // 注入必须连大小写一起给：`curl`/`git`/`pip` 只认小写形式。
        if let Some(url) = &self.proxy_url {
            for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
                command.env(name, url);
            }
            for name in ["http_proxy", "https_proxy", "all_proxy"] {
                command.env(name, url);
            }
            // 回环自身不能走代理：否则访问本地 dev server 会绕一圈再被拒。
            for name in ["NO_PROXY", "no_proxy"] {
                command.env(name, "127.0.0.1,localhost,::1");
            }
        }
        command.env("HOME", &self.home);
        command.env("TMPDIR", &self.temp);
        // 依赖缓存重定向：指向沙盒内的目录，**不是**宿主的 `~/.cargo`。
        // `cargo`/`npm`/`pip` 都需要可写的缓存位置，指错会让构建以 `EACCES` 失败；
        // 指回宿主缓存则让沙盒能往宿主的依赖树里塞东西（供应链风险）。
        //
        // `CARGO_HOME` 只管**数据**目录（registry/git 缓存、`.crates.toml`），不影响
        // 可执行文件位置：`~/.cargo/bin/cargo` 由 `PATH` 找到，已按只读授权。
        //
        // `RUSTUP_HOME` **不能**重定向：`cargo` 是 rustup 的 shim，要靠它找到 toolchain；
        // 指到空目录会直接报"no toolchain installed"，于是"为了安全"反而让 cargo 不可用。
        command.env("CARGO_HOME", &self.cache);
        command.env("npm_config_cache", &self.cache);
        command.env("PIP_CACHE_DIR", &self.cache);
        command.env("XDG_CACHE_HOME", &self.cache);
    }
}

/// 必须在沙盒进程里清掉的代理变量。
///
/// 大小写都列：环境变量在 Unix 上区分大小写，而不同工具认不同形式。
const PROXY_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// 一次落地的 `sandbox-exec` 调用：把策略写进 stdin 后返回。
///
/// 抽出来是因为"写策略"这一步在两个宿主路径里完全一样，而 stdin 一旦被 `take()`
/// 就必须在 spawn 之后立刻写，否则命令会阻塞在读策略上。
pub fn spawn_with_policy(
    isolation: &Isolation,
    argv: &[String],
) -> Result<std::process::Child, String> {
    use std::io::Write;

    let mut command = isolation.command(argv)?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("沙盒 launcher 启动失败：{error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // 写失败说明 launcher 已经退出（例如参数错误），此时不吞掉原因：
        // 让调用方在 wait 之后从 stderr 读到真正的错误。
        let _ = stdin.write_all(isolation.policy_text().as_bytes());
        let _ = stdin.flush();
    }
    Ok(child)
}

/// 进程启动时是否该走 launcher 分支；是的话就地执行并返回退出码。
///
/// 必须在**任何参数解析之前**调用：宿主（TUI / API）的参数解析不认识 `sandbox-exec`，
/// 而 launcher 拿到的是用户的原始命令（可能恰好长得很像宿主参数）。
///
/// 判定只看 argv[1] 是否等于 [`LAUNCHER_COMMAND`]，且要求 argv[2] 是 `--`——
/// 这样用户真的想跑一条名为 `sandbox-exec` 的命令（`bash -c 'sandbox-exec ...'`）
/// 不会误入：那条命令的 argv 由 shell 解释，根本不经过这里。
pub fn intercept(program_args: &[String]) -> Option<std::process::ExitCode> {
    if program_args.first().map(String::as_str) != Some(LAUNCHER_COMMAND) {
        return None;
    }
    let rest = &program_args[1..];
    let rest = rest.strip_prefix(&["--".to_string()][..]).unwrap_or(rest);

    // 策略从 stdin 读；读不到就报错退出——绝不"没有策略照常跑"。
    let (policy_text, abi) = match launcher::read_policy(&mut std::io::stdin().lock()) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return Some(std::process::ExitCode::FAILURE);
        }
    };
    match launcher::run(rest, &policy_text, abi) {
        // 成功时进程已被 execvp 替换，走不到这里。
        Ok(()) => Some(std::process::ExitCode::SUCCESS),
        Err(error) => {
            eprintln!("{error}");
            Some(std::process::ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实探测：任何 Linux 上都必须给出三种确定结论之一，且不能 panic。
    #[test]
    fn probe_always_answers() {
        let capability = capability();
        match capability.probe {
            Probe::Supported(abi) => assert!(abi >= 1, "ABI 至少为 1"),
            Probe::Disabled | Probe::Unsupported => {
                assert!(!capability.can_isolate());
                assert!(!capability.controls_network());
            }
            Probe::Failed(code) => assert_ne!(code, 0),
        }
    }

    #[test]
    fn abi_gates_network_and_udp() {
        let at = |abi| Capability {
            probe: Probe::Supported(abi),
        };
        assert!(!at(3).controls_network(), "ABI 3 无网络控制");
        assert!(at(4).controls_network());
        assert!(!at(4).controls_udp());
        assert!(at(10).controls_udp());
        assert!(at(1).can_isolate(), "ABI 1 也能做文件隔离");
    }

    #[test]
    fn offline_requires_kernel_network_control() {
        let at = |abi| Capability {
            probe: Probe::Supported(abi),
        };
        let error = at(3)
            .check_network_policy(NetPolicy::Offline)
            .expect_err("ABI 3 无法兑现内核级断网");
        assert!(error.contains("ABI"), "{error}");
        assert!(at(4).check_network_policy(NetPolicy::Offline).is_ok());
        // 其余三档是应用层语义，不要求内核网络支持。
        for policy in [NetPolicy::Ask, NetPolicy::Allowlist, NetPolicy::Online] {
            assert!(at(1).check_network_policy(policy).is_ok(), "{policy:?}");
        }
        // 压根没有 Landlock 时，offline 一样不成立。
        let none = Capability {
            probe: Probe::Unsupported,
        };
        assert!(none.check_network_policy(NetPolicy::Offline).is_err());
    }

    #[test]
    fn describe_mentions_uncontrolled_capabilities() {
        let old = Capability {
            probe: Probe::Supported(3),
        };
        let text = old.describe();
        assert!(text.contains("ABI 3"), "{text}");
        assert!(text.contains("网络端口未受控"), "{text}");

        let with_net = Capability {
            probe: Probe::Supported(5),
        };
        let text = with_net.describe();
        assert!(text.contains("UDP 端口未受控"), "{text}");
        assert!(!text.contains("网络端口未受控"), "{text}");

        assert!(Capability {
            probe: Probe::Unsupported
        }
        .describe()
        .contains("5.13"));
    }

    /// `intercept` 只看 argv 头：不是 launcher 调用就必须原样放行给宿主参数解析。
    ///
    /// 只测"不该接管"的判定——真接管会去读 stdin 并 exec，不能在测试进程里跑。
    /// 注意 `["sandbox-exec"]` 单独出现**会**被接管（后续由 `launcher::run` 报用法错误），
    /// 这里刻意不覆盖它。
    #[test]
    fn intercept_ignores_ordinary_arguments() {
        for args in [
            vec![],
            vec!["--help".to_string()],
            vec!["sandbox".to_string()],
            vec!["sandbox-execx".to_string()],
            vec!["api".to_string(), "sandbox-exec".to_string()],
            vec!["kernel".to_string()],
        ] {
            assert!(intercept(&args).is_none(), "不该接管：{args:?}");
        }
    }

    /// 测试用的临时工作区：绝不写 `/work` 这类系统路径。
    fn scratch(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "omnicrawl-sandbox-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let temp = base.join(".omnicrawl/.agent_tmp/sandbox");
        std::fs::create_dir_all(&temp).expect("应能建临时目录");
        (base, temp)
    }

    /// 准备隔离：策略能被算出来，且 wrap 出来的 argv 形状正确。
    #[test]
    fn prepare_encodes_policy_and_wraps_argv() {
        let Some(abi) = capability().abi() else {
            eprintln!("内核不支持 Landlock，跳过");
            return;
        };
        let (root, temp) = scratch("prepare");
        let mut config = omnicrawl_config::features::sandbox::SandboxConfig::default();
        config.enabled = true;
        config.mode = omnicrawl_config::features::sandbox::SandboxMode::Landlock;
        config.net_policy = omnicrawl_config::features::sandbox::NetPolicy::Offline;

        let isolation = Isolation::prepare(&config, &root, &temp, abi, None).expect("应可准备");

        assert_eq!(isolation.abi(), abi);
        let text = isolation.policy_text();
        assert!(text.starts_with(&format!("abi {abi}\n")), "{text}");
        assert!(
            text.contains(&format!("fs rw {}", root.display())),
            "{text}"
        );
        // 临时目录规则不能丢：丢了之后沙盒里写临时文件全部 EACCES。
        assert!(
            text.contains(&format!("fs rw {}", temp.display())),
            "{text}"
        );

        let wrapped = isolation.wrap(&["bash".to_string(), "-lc".to_string(), "ls".to_string()]);
        assert_eq!(wrapped[1], LAUNCHER_COMMAND);
        assert_eq!(wrapped[2], "--");
        assert_eq!(&wrapped[3..], &["bash", "-lc", "ls"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用户给的路径不存在时必须**报错**，不能算出一份更宽松的策略。
    #[test]
    fn prepare_refuses_missing_user_path() {
        let Some(abi) = capability().abi() else {
            eprintln!("内核不支持 Landlock，跳过");
            return;
        };
        let (root, temp) = scratch("missing");
        let mut config = omnicrawl_config::features::sandbox::SandboxConfig::default();
        config.enabled = true;
        config.mode = omnicrawl_config::features::sandbox::SandboxMode::Landlock;
        config.readable = vec!["definitely-not-here-zzz".to_string()];

        let error =
            Isolation::prepare(&config, &root, &temp, abi, None).expect_err("缺失路径必须报错");
        assert!(error.contains("definitely-not-here-zzz"), "{error}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 环境注入：宿主代理变量必须清掉，本机代理必须注入。
    ///
    /// 不清理宿主代理，沙盒内的客户端会直奔用户代理，`offline`/`ask` 的判定整段被跳过。
    #[test]
    fn environment_drops_host_proxy_and_injects_ours() {
        let Some(abi) = capability().abi() else {
            eprintln!("内核不支持 Landlock，跳过");
            return;
        };
        let (root, temp) = scratch("env");
        let mut config = omnicrawl_config::features::sandbox::SandboxConfig::default();
        config.enabled = true;
        config.mode = omnicrawl_config::features::sandbox::SandboxMode::Landlock;
        config.net_policy = omnicrawl_config::features::sandbox::NetPolicy::Ask;

        // `env_remove` 会留下显式的"已移除"记录（`get_envs` 里值位为 `None`），
        // 因此不必真去改宿主的进程环境——测试并行时改全局环境是数据竞争。
        let env_of = |isolation: &Isolation| -> Vec<(String, Option<String>)> {
            let mut command = std::process::Command::new("/bin/true");
            isolation.apply_environment(&mut command);
            command
                .get_envs()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().to_string(),
                        value.map(|value| value.to_string_lossy().to_string()),
                    )
                })
                .collect()
        };
        let find = |env: &[(String, Option<String>)], name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };

        let isolation = Isolation::prepare(&config, &root, &temp, abi, Some(43210)).expect("应可准备");
        let env = env_of(&isolation);
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            assert_eq!(
                find(&env, name),
                Some(Some("http://127.0.0.1:43210".to_string())),
                "{name} 必须指向宿主代理"
            );
        }
        // `HOME` 必须在沙盒内，否则真实家目录（不可读）会被指过去。
        assert!(
            find(&env, "HOME")
                .is_some_and(|value| value.as_deref().is_some_and(|text| text.contains("sandbox"))),
            "HOME 必须落在沙盒目录里"
        );
        // 依赖缓存必须重定向到沙盒内：指回宿主缓存等于允许沙盒污染宿主依赖树。
        for name in ["CARGO_HOME", "npm_config_cache", "PIP_CACHE_DIR", "XDG_CACHE_HOME"] {
            let value = find(&env, name).flatten().unwrap_or_default();
            assert!(
                value.contains("sandbox") && value.contains("cache"),
                "{name} 必须指向沙盒内缓存，实际：{value}"
            );
        }
        // `RUSTUP_HOME` 不能重定向：cargo 是 rustup shim，指到空目录就报 no toolchain。
        assert_eq!(
            find(&env, "RUSTUP_HOME"),
            None,
            "RUSTUP_HOME 不该被改写"
        );

        // `offline` 不注入代理：端口层不放行任何端口，注入只会让客户端白等。
        let mut offline = config.clone();
        offline.net_policy = omnicrawl_config::features::sandbox::NetPolicy::Offline;
        let forced = Isolation::prepare(&offline, &root, &temp, abi, None).expect("应可准备");
        let env = env_of(&forced);
        assert_eq!(
            find(&env, "HTTP_PROXY"),
            Some(None),
            "offline 必须显式清掉 HTTP_PROXY"
        );
        assert_eq!(find(&env, "https_proxy"), Some(None), "小写形式同样要清");

        let _ = std::fs::remove_dir_all(&root);
    }
}
