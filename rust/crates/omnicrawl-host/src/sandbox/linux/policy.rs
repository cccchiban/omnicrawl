//! 沙盒策略：把 `[sandbox]` 配置折算成「哪些路径给哪些权限」与「哪些端口放行」。
//!
//! 这里**不发系统调用**，全部是纯逻辑，因此可以完整单测。真正的施加在 [`super::launcher`]。
//!
//! 设计要点（见 `sandbox_linux_design.md` §6.1）：Landlock 是**白名单**语义，
//! 只列允许的路径与权限，未列出的自动拒绝。所以这里不枚举 `~/.ssh` 之类的黑名单。

use std::path::{Path, PathBuf};

use omnicrawl_config::features::sandbox::{NetPolicy, SandboxConfig};

use super::runtimes;

/// 一组路径与它被允许的权限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRule {
    pub path: PathBuf,
    pub access: Access,
}

/// 权限档位。抽象成三档而不是裸位掩码，是为了让「这条路径是什么用途」在测试与诊断里可读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// 只读 + 可执行（解释器、动态库、证书）。
    ReadExecute,
    /// 只读（配置、证书目录）。
    ReadOnly,
    /// 读写（工作区、临时目录）。
    ReadWrite,
}

impl Access {
    /// 这台内核（该 ABI）上这一档对应的 `handled_access_fs` 位。
    ///
    /// `abi` 决定哪些位存在：`REFER` 要 ABI ≥ 2，`TRUNCATE` 要 ABI ≥ 3。
    /// 低位内核上把这些位算进去，`add_rule` 会以 `EINVAL` 失败，于是整条路径失效——
    /// 那正是"看起来配了、实际没生效"的最坏情况，因此必须裁剪。
    pub fn bits(self, abi: u32) -> u64 {
        let mut bits = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
        match self {
            Access::ReadExecute => {}
            Access::ReadOnly => bits &= !FS_EXECUTE,
            Access::ReadWrite => {
                bits |= FS_WRITE_FILE
                    | FS_REMOVE_DIR
                    | FS_REMOVE_FILE
                    | FS_MAKE_DIR
                    | FS_MAKE_REG
                    | FS_MAKE_SOCK
                    | FS_MAKE_FIFO
                    | FS_MAKE_SYM;
                if abi >= 2 {
                    bits |= FS_REFER;
                }
                if abi >= 3 {
                    bits |= FS_TRUNCATE;
                }
            }
        }
        bits
    }
}

/// Landlock 文件系统权限位（`linux/landlock.h`，ABI v1 起的全集）。
pub const FS_EXECUTE: u64 = 1 << 0;
pub const FS_WRITE_FILE: u64 = 1 << 1;
pub const FS_READ_FILE: u64 = 1 << 2;
pub const FS_READ_DIR: u64 = 1 << 3;
pub const FS_REMOVE_DIR: u64 = 1 << 4;
pub const FS_REMOVE_FILE: u64 = 1 << 5;
pub const FS_MAKE_CHAR: u64 = 1 << 6;
pub const FS_MAKE_DIR: u64 = 1 << 7;
pub const FS_MAKE_REG: u64 = 1 << 8;
pub const FS_MAKE_SOCK: u64 = 1 << 9;
pub const FS_MAKE_FIFO: u64 = 1 << 10;
pub const FS_MAKE_BLOCK: u64 = 1 << 11;
pub const FS_MAKE_SYM: u64 = 1 << 12;
/// ABI ≥ 2。
pub const FS_REFER: u64 = 1 << 13;
/// ABI ≥ 3。
pub const FS_TRUNCATE: u64 = 1 << 14;

/// 一个 abi 下要交给内核管的文件权限全集。
///
/// **必须显式列出**：不在 `handled_access_fs` 里的位对这个规则集不生效（即"不管"）。
/// `REFER` 是唯一例外——它即使不列也默认拒绝，但要给出规则仍须显式带上。
pub fn handled_fs(abi: u32) -> u64 {
    let mut bits = FS_EXECUTE
        | FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM;
    if abi >= 2 {
        bits |= FS_REFER;
    }
    if abi >= 3 {
        bits |= FS_TRUNCATE;
    }
    bits
}

/// 系统只读路径：解释器与动态库的所在地。缺了它们连 `bash` 都起不来。
///
/// 只列真实存在的目录（调用方负责过滤），且**只给读+执行**。
pub fn system_read_paths() -> Vec<PathBuf> {
    [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/pki",
        "/usr/local",
        "/opt",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// 运行时必需的最小设备与内核视图。
///
/// 不给 `/proc` 会让 `bash`/`ps`/部分语言运行时以极晦涩的方式失败；给了就意味着沙盒进程
/// 能看到宿主的进程列表（读不到别人 environ，但看得到 pid/命令行）。这是设计文档 §15
/// 已记录的取舍：**接受信息泄漏，换取工具可用**。
pub fn runtime_read_paths() -> Vec<PathBuf> {
    [
        "/proc",
        "/sys",
        "/dev/zero",
        "/dev/urandom",
        "/dev/random",
        "/dev/tty",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// 必须**可写**的设备。
///
/// `/dev/null` 单独列出来而不是并进只读表：`> /dev/null`、`2>/dev/null` 是脚本里最常见的
/// 写法，只读会让它们以 `EACCES` 失败。Windows 侧已经踩过这个坑——Git Bash 因为
/// AppContainer 拿不到 `NUL` 设备权限而整体起不来，Linux 这侧不能重犯。
pub fn runtime_write_paths() -> Vec<PathBuf> {
    ["/dev/null"].into_iter().map(PathBuf::from).collect()
}

/// 一个完整的沙盒策略：文件规则 + 网络端口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub fs_rules: Vec<PathRule>,
    /// 放行的 TCP 连接端口（`CONNECT_TCP`）。空表示一个都不放行。
    pub connect_ports: Vec<u16>,
    /// 放行的 TCP 绑定端口（`BIND_TCP`）。
    pub bind_ports: Vec<u16>,
    /// 放行的 UDP 发送目标端口（`CONNECT_SEND_UDP`）。空表示一个都不放行。
    pub connect_udp_ports: Vec<u16>,
    /// 放行的 UDP 绑定端口（`BIND_UDP`）。
    pub bind_udp_ports: Vec<u16>,
    /// 是否管网络。`false` 时网络不受本沙盒约束（`online` 档）。
    pub control_network: bool,
    /// 是否管 UDP。只有 `offline` 才管：其余档是应用层代理级，UDP 本就绕不过代理，
    /// 管住反而会打断回环 UDP（`Control UDP` 无法区分地址）。
    pub control_udp: bool,
    /// 发现的用户解释器根数量（只用于诊断：`node` 能不能跑是用户最先撞上的问题）。
    pub install_roots: usize,
    /// 要迁入的 cgroup（宿主已建好并写好限额）。`None` 表示不设限额。
    pub cgroup: Option<std::path::PathBuf>,
}

/// 宿主的回环代理端口：沙盒出网的唯一通路，必须放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyPort(pub u16);

impl Policy {
    /// 按配置与运行环境折算策略。
    ///
    /// `root` 是工作区根，`temp` 是沙盒专用临时目录（两者可写）。
    /// `abi` 决定权限位裁剪；`proxy` 是已就绪的回环代理端口（`offline` 下为 `None`）。
    pub fn build(
        config: &SandboxConfig,
        root: &Path,
        temp: &Path,
        abi: u32,
        proxy: Option<ProxyPort>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let mut fs_rules = Vec::new();

        // 工作区与临时目录：读写。`REFER` 让 git/包管理器能在工作区内 rename。
        fs_rules.push(PathRule {
            path: root.to_path_buf(),
            access: Access::ReadWrite,
        });
        fs_rules.push(PathRule {
            path: temp.to_path_buf(),
            access: Access::ReadWrite,
        });

        // 系统路径与运行时视图：只读 + 执行。
        for path in system_read_paths().into_iter().chain(runtime_read_paths()) {
            fs_rules.push(PathRule {
                path,
                access: Access::ReadExecute,
            });
        }
        // 用户装的解释器（nvm/rustup/pyenv）：`system_read_paths()` 覆盖不到 `$HOME` 下的安装，
        // 不补这一段则沙盒里 `node`/`cargo` 表现为"不存在"。这里授的是精确到安装根的路径，
        // 含凭据的根已被拆成安全子目录（见 `runtimes` 模块）。
        let installs = runtimes::discover(env);
        let install_roots: usize = installs.iter().map(|install| install.roots.len()).sum();
        for install in installs {
            for path in install.roots {
                fs_rules.push(PathRule {
                    path,
                    access: Access::ReadExecute,
                });
            }
        }
        // `HOME` 被改到沙盒内后，`git` 读不到真实的 `~/.gitconfig`：`commit` 会报
        // "Please tell me who you are"。这里只授**配置本身**（只读），不授 `$HOME`：
        // 凭据在 `~/.git-credentials` 与凭据助手里，与本文件是两回事。
        if let Some(home) = env("HOME") {
            let home = PathBuf::from(home);
            for relative in [".gitconfig", ".config/git"] {
                let path = home.join(relative);
                if path.exists() {
                    fs_rules.push(PathRule {
                        path,
                        access: Access::ReadOnly,
                    });
                }
            }
        }
        // `/dev/null` 这类设备必须可写，否则 `> /dev/null` 直接失败。
        for path in runtime_write_paths() {
            fs_rules.push(PathRule {
                path,
                access: Access::ReadWrite,
            });
        }

        // `readable` / `writable` 是用户显式扩大范围的唯一手段，相对路径按工作区根解析。
        for entry in &config.writable {
            fs_rules.push(PathRule {
                path: resolve(root, entry),
                access: Access::ReadWrite,
            });
        }
        for entry in &config.readable {
            fs_rules.push(PathRule {
                path: resolve(root, entry),
                access: Access::ReadExecute,
            });
        }

        // 回环永远放行（代理本身与本地 dev server 都在回环上），因此端口 0 之外的
        // 随机本地端口要能 bind，代理端口要能 connect。
        //
        // 端口放行的口径（设计文档 §7.2）：**非 online 档一律只放行代理端口**。
        // 不放行 80/443 是有意的——尊重 `HTTP(S)_PROXY` 的客户端（curl/pip/npm/git/cargo）
        // 只连代理端口，把目标主机名交给代理去解析与判定；忽视代理变量、想直连 443 的客户端
        // 则在 `connect()` 处直接 `EPERM`。这样"端口层只有一扇门"与"域名判定在代理"同时成立，
        // 比放行 443 再靠应用层自律严格得多。
        //
        // 于是代理拉不起来时端口集合自然退化成一个都不放行：失败方向是关闭。
        let mut connect_ports: Vec<u16> = Vec::new();
        let mut bind_ports: Vec<u16> = vec![0];
        let control_network = config.net_policy != NetPolicy::Online;

        if control_network {
            if let Some(port) = proxy {
                connect_ports.push(port.0);
            }
        }
        // `offline` 是唯一承诺"内核级断网"的档：ABI ≥ 10 时连 UDP 一起交给内核管，
        // 只放行回环 UDP bind（不 bind 的话本地 socket 都建不起来），目标端口不放行。
        // 其余三档**不管 UDP**：Landlock 的端口规则不含地址，管住会把 127.0.0.53 这类
        // 本地 DNS 桩一并打断，于是所有本地解析失败。这一点必须如实写进诊断。
        let control_udp = control_network && config.net_policy == NetPolicy::Offline && abi >= 10;
        let mut connect_udp_ports: Vec<u16> = Vec::new();
        let mut bind_udp_ports: Vec<u16> = Vec::new();
        if control_udp {
            bind_udp_ports.push(0);
        }

        connect_ports.sort_unstable();
        connect_ports.dedup();
        bind_ports.sort_unstable();
        bind_ports.dedup();

        Self {
            fs_rules,
            connect_ports,
            bind_ports,
            connect_udp_ports,
            bind_udp_ports,
            control_network,
            control_udp,
            install_roots,
            // cgroup 由 launcher 在施加 Landlock 之前迁入；策略只负责带路。
            cgroup: None,
        }
    }

    /// 去掉不存在的路径，并合并重复项。
    ///
    /// 不存在的路径**不能静默跳过**（设计文档 §12 第 3 条）：用户以为边界生效了、
    /// 实际什么都没限，比配错更危险。因此这里只过滤"系统路径里不存在的那些"——
    /// 它们是环境差异，不是用户输入；用户显式给的 `readable`/`writable` 若不存在，
    /// 由 [`Self::validate`] 单独报错。
    pub fn existing(mut self) -> Self {
        self.fs_rules.retain(|rule| rule.path.exists());
        self
    }

    /// 校验用户显式给出的路径是否都存在。
    ///
    /// 返回第一条缺失的路径：一次说完没有意义，改完再看下一条更快。
    pub fn validate(&self, config: &SandboxConfig, root: &Path) -> Result<(), String> {
        for (key, entries) in [
            ("sandbox.fs.writable", &config.writable),
            ("sandbox.fs.readable", &config.readable),
        ] {
            for entry in entries {
                let path = resolve(root, entry);
                if !path.exists() {
                    return Err(format!(
                        "{key} 里的 `{entry}`（解析为 {}）不存在：沙盒规则无法施加，命令未执行。",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// 给诊断用的一行摘要。
    ///
    /// 必须如实，有两处容易说错：
    ///
    /// - **端口规则不含地址**。放行 443 意味着放行任何地址的 443，不放行任何端口则
    ///   连 `127.0.0.1:8080` 也连不上。把 `offline` 说成"只允许回环"是错的。
    /// - **`ask`/`allowlist` 的域名判定在代理里**，是应用层边界。只有 `offline`
    ///   是内核级断网。
    pub fn describe(&self, abi: u32) -> String {
        let network = if !self.control_network {
            "网络不受约束（online）".to_string()
        } else if self.connect_ports.is_empty() {
            format!(
                "出站 TCP 端口全拒（含回环：端口规则不含地址）{}",
                self.udp_note(abi)
            )
        } else {
            format!(
                "出站 TCP 只放行端口 {:?}（端口规则不含地址，等于该端口对所有地址开放），\
                 域名判定在代理（应用层）{}",
                self.connect_ports,
                self.udp_note(abi)
            )
        };
        format!(
            "ABI {abi}，{} 条文件规则（含 {} 个用户解释器根），{network}",
            self.fs_rules.len(),
            self.install_roots,
        )
    }

    fn udp_note(&self, abi: u32) -> String {
        if self.control_udp {
            "；UDP 受控：只放行 bind 随机端口，目标端口全拒（同样不含地址）".to_string()
        } else if abi >= 10 {
            "；UDP 未受控".to_string()
        } else {
            "；UDP 未受控（需 ABI ≥ 10）".to_string()
        }
    }
}

/// 相对路径按工作区根解析；绝对路径原样返回。
fn resolve(root: &Path, entry: &str) -> PathBuf {
    let path = Path::new(entry);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SandboxConfig {
        SandboxConfig {
            enabled: true,
            ..SandboxConfig::default()
        }
    }

    /// 测试用的空环境：不发现任何用户装的解释器，保证断言只反映固定白名单。
    fn no_env() -> impl Fn(&str) -> Option<String> {
        |_| None
    }

    /// 建一对真实存在的目录当工作区与临时目录。
    ///
    /// 不能用 `/work` 这类虚构路径：`build` 末尾会调 [`Policy::existing`] 去掉不存在的路径
    /// （产品语义：不存在的路径不施加规则），虚构路径会被一并滤掉，测出来的就成"规则缺失"。
    fn scratch() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "omnicrawl-policy-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let work = base.join("work");
        let temp = work.join(".omnicrawl").join(".agent_tmp").join("sandbox");
        std::fs::create_dir_all(&temp).expect("应能建临时目录");
        (work, temp)
    }

    fn build(policy_config: &SandboxConfig, abi: u32, proxy: Option<u16>) -> Policy {
        Policy::build(
            policy_config,
            Path::new("/work"),
            Path::new("/work/.omnicrawl/.agent_tmp/sandbox"),
            abi,
            proxy.map(ProxyPort),
            &no_env(),
        )
        .existing()
    }

    #[test]
    fn workspace_and_temp_are_read_write() {
        let (work, temp) = scratch();
        let policy = Policy::build(
            &config(),
            &work,
            &temp,
            5,
            None,
            &no_env(),
        )
        .existing();
        let work_rule = policy
            .fs_rules
            .iter()
            .find(|rule| rule.path == work)
            .expect("工作区必须在规则里");
        assert_eq!(work_rule.access, Access::ReadWrite);
        let temp_rule = policy
            .fs_rules
            .iter()
            .find(|rule| rule.path == temp)
            .expect("临时目录必须在规则里");
        assert_eq!(temp_rule.access, Access::ReadWrite);
    }

    #[test]
    fn a_missing_temp_directory_drops_its_rule_instead_of_failing() {
        // 与上一条互补：临时目录不存在时**不报错**（它是宿主自己建的，环境问题），
        // 只是那条规则不施加上去。用户显式给的路径才由 `validate` 报错。
        let (work, temp) = scratch();
        std::fs::remove_dir_all(&work).expect("清理临时目录");
        let policy = Policy::build(&config(), &work, &temp, 5, None, &no_env()).existing();
        assert!(
            !policy.fs_rules.iter().any(|rule| rule.path == work),
            "不存在的路径不该留下规则"
        );
    }

    /// 用户目录绝不能默认出现在规则里——这是整套沙盒的核心保证。
    #[test]
    fn home_is_never_granted_by_default() {
        let policy = build(&config(), 5, None);
        for rule in &policy.fs_rules {
            let text = rule.path.to_string_lossy();
            assert!(
                !text.starts_with("/home/") && !text.starts_with("/root"),
                "默认规则不该包含用户目录：{text}"
            );
        }
    }

    #[test]
    fn refer_and_truncate_are_cropped_by_abi() {
        assert_eq!(Access::ReadWrite.bits(1) & FS_REFER, 0, "ABI 1 无 REFER");
        assert_eq!(Access::ReadWrite.bits(1) & FS_TRUNCATE, 0, "ABI 1 无 TRUNCATE");
        assert_ne!(Access::ReadWrite.bits(2) & FS_REFER, 0, "ABI 2 起有 REFER");
        assert_eq!(Access::ReadWrite.bits(2) & FS_TRUNCATE, 0, "ABI 2 无 TRUNCATE");
        assert_ne!(Access::ReadWrite.bits(3) & FS_TRUNCATE, 0, "ABI 3 起有 TRUNCATE");
        // 只读档不该带执行位以外的写位。
        assert_eq!(Access::ReadOnly.bits(5) & FS_WRITE_FILE, 0);
        assert_eq!(Access::ReadOnly.bits(5) & FS_EXECUTE, 0);
        assert_ne!(Access::ReadExecute.bits(5) & FS_EXECUTE, 0);
    }

    #[test]
    fn handled_bits_are_cropped_to_abi() {
        assert_eq!(handled_fs(1) & FS_REFER, 0);
        assert_ne!(handled_fs(2) & FS_REFER, 0);
        assert_eq!(handled_fs(2) & FS_TRUNCATE, 0);
        assert_ne!(handled_fs(3) & FS_TRUNCATE, 0);
        // 基础位在任何 ABI 上都要在：否则对应权限根本不受管。
        for abi in 1..=6 {
            assert_ne!(handled_fs(abi) & FS_READ_FILE, 0, "abi {abi}");
            assert_ne!(handled_fs(abi) & FS_WRITE_FILE, 0, "abi {abi}");
            assert_ne!(handled_fs(abi) & FS_MAKE_REG, 0, "abi {abi}");
        }
    }

    #[test]
    fn offline_allows_only_the_proxy_port() {
        let mut policy_config = config();
        policy_config.net_policy = NetPolicy::Offline;
        let with_proxy = build(&policy_config, 5, Some(41234));
        assert_eq!(with_proxy.connect_ports, vec![41234]);

        let without_proxy = build(&policy_config, 5, None);
        assert!(
            without_proxy.connect_ports.is_empty(),
            "没有代理时 offline 必须一个端口都不放行"
        );
        assert!(without_proxy.control_network);
    }

    /// 非 `online` 档在端口层只有一扇门：代理端口。
    ///
    /// 放行 80/443/53 会让"忽视代理变量、直连 443"的客户端绕过域名判定——那正是
    /// 设计文档 §7.2 说的"应用层边界"，能往内核层收就收。
    #[test]
    fn ask_and_allowlist_keep_only_the_proxy_port() {
        for policy in [NetPolicy::Ask, NetPolicy::Allowlist] {
            let mut policy_config = config();
            policy_config.net_policy = policy;
            let built = build(&policy_config, 5, Some(41234));
            assert_eq!(
                built.connect_ports,
                vec![41234],
                "{policy:?} 不该放行代理之外的端口"
            );
            assert!(built.control_network, "{policy:?}");
        }
    }

    /// 代理拉不起来时，端口层必须退化成"一个都不放行"（失败方向为关闭）。
    #[test]
    fn missing_proxy_closes_every_port() {
        for policy in [NetPolicy::Offline, NetPolicy::Ask, NetPolicy::Allowlist] {
            let mut policy_config = config();
            policy_config.net_policy = policy;
            let built = build(&policy_config, 10, None);
            assert!(
                built.connect_ports.is_empty(),
                "{policy:?} 在没有代理时必须拒绝一切出站"
            );
        }
    }

    #[test]
    fn online_does_not_control_network() {
        let mut policy_config = config();
        policy_config.net_policy = NetPolicy::Online;
        let built = build(&policy_config, 5, Some(41234));
        assert!(!built.control_network);
        assert!(!built.control_udp);
        assert!(built.connect_ports.is_empty(), "online 不该列端口");
    }

    /// UDP 只由 `offline` 管，且需要 ABI ≥ 10。
    ///
    /// 其余档管 UDP 会打断回环 DNS 桩（端口规则不含地址），代价大于收益。
    #[test]
    fn udp_is_controlled_only_when_offline_and_abi_ten() {
        let mut policy_config = config();
        policy_config.net_policy = NetPolicy::Offline;
        assert!(!build(&policy_config, 9, None).control_udp, "ABI 9 无 UDP");
        let controlled = build(&policy_config, 10, None);
        assert!(controlled.control_udp);
        // 只管 bind(0)，目标端口一个不放：断网档不该留任何 UDP 出口。
        assert_eq!(controlled.bind_udp_ports, vec![0]);
        assert!(controlled.connect_udp_ports.is_empty());

        for policy in [NetPolicy::Ask, NetPolicy::Allowlist] {
            let mut policy_config = config();
            policy_config.net_policy = policy;
            assert!(
                !build(&policy_config, 10, Some(41234)).control_udp,
                "{policy:?} 不该管 UDP"
            );
        }
    }

    /// 诊断文案必须说出**实际**严格级别，不能把应用层边界说成断网。
    #[test]
    fn describe_reports_the_actual_strictness() {
        let mut offline = config();
        offline.net_policy = NetPolicy::Offline;
        let text = Policy::build(&offline, Path::new("/work"), Path::new("/work/tmp"), 10, None, &no_env())
            .describe(10);
        // 端口规则不含地址，说"只允许回环"是错的：这里必须说清连回环也一起拒。
        assert!(text.contains("出站 TCP 端口全拒"), "{text}");
        assert!(text.contains("含回环"), "{text}");
        assert!(text.contains("UDP 受控"), "{text}");

        let mut ask = config();
        ask.net_policy = NetPolicy::Ask;
        let text = Policy::build(
            &ask,
            Path::new("/work"),
            Path::new("/work/tmp"),
            5,
            Some(ProxyPort(41234)),
            &no_env(),
        )
        .describe(5);
        assert!(text.contains("只放行端口"), "{text}");
        assert!(text.contains("不含地址"), "端口规则不含地址必须写明：{text}");
        assert!(text.contains("应用层"), "必须说清这是应用层判定：{text}");
        assert!(text.contains("UDP 未受控"), "{text}");
        assert!(!text.contains("断网"), "应用层边界不能说成断网：{text}");
        assert!(!text.contains("只允许回环"), "端口规则不含地址：{text}");

        let mut online = config();
        online.net_policy = NetPolicy::Online;
        let text =
            Policy::build(&online, Path::new("/work"), Path::new("/work/tmp"), 5, None, &no_env())
                .describe(5);
        assert!(text.contains("网络不受约束"), "{text}");
    }

    #[test]
    fn bind_always_allows_ephemeral_ports() {
        // 本地 dev server 与代理客户端都要能 bind 随机端口。
        assert_eq!(build(&config(), 5, None).bind_ports, vec![0]);
    }

    #[test]
    fn missing_user_path_is_an_error_not_a_silent_skip() {
        let mut policy_config = config();
        policy_config.readable = vec!["definitely-not-here-zzz".to_string()];
        let error = Policy::build(
            &policy_config,
            Path::new("/work"),
            Path::new("/work/tmp"),
            5,
            None,
            &no_env(),
        )
        .validate(&policy_config, Path::new("/work"))
        .expect_err("缺失路径必须报错");
        assert!(error.contains("definitely-not-here-zzz"), "{error}");
        assert!(error.contains("沙盒"), "{error}");
    }

    #[test]
    fn relative_user_paths_resolve_against_workspace_root() {
        let mut policy_config = config();
        policy_config.writable = vec!["out".to_string()];
        let policy = Policy::build(
            &policy_config,
            Path::new("/work"),
            Path::new("/work/tmp"),
            5,
            None,
            &no_env(),
        );
        assert!(
            policy
                .fs_rules
                .iter()
                .any(|rule| rule.path == Path::new("/work/out")),
            "相对路径要按工作区根解析"
        );
    }

    /// 发现到的解释器根必须真的进策略：这是 P3 的核心断言。
    ///
    /// 不进策略的话，沙盒里 `node`/`cargo` 表现为"不存在"，而用户看到的是
    /// "命令找不到"——完全不知道是沙盒挡的。
    #[test]
    fn discovered_interpreters_reach_the_policy() {
        let base = std::env::temp_dir().join(format!(
            "omnicrawl-policy-install-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let version = home.join(".nvm/versions/node/v20.11.0");
        let bin = version.join("bin");
        std::fs::create_dir_all(&bin).expect("应能建");
        let node = bin.join("node");
        std::fs::write(&node, "#!/bin/sh\n").expect("应能写");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&node).expect("元数据").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&node, permissions).expect("应能设权限");

        let path = bin.display().to_string();
        let env = move |name: &str| match name {
            "HOME" => Some(home.display().to_string()),
            "PATH" => Some(path.clone()),
            _ => None,
        };
        let policy = Policy::build(
            &config(),
            Path::new("/work"),
            Path::new("/work/tmp"),
            5,
            None,
            &env,
        );

        assert_eq!(policy.install_roots, 1, "应发现 1 个解释器根");
        let rule = policy
            .fs_rules
            .iter()
            .find(|rule| rule.path == version)
            .expect("nvm 版本目录必须进规则");
        assert_eq!(
            rule.access,
            Access::ReadExecute,
            "解释器只需要读+执行，不该给写"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `HOME` 指进沙盒后，`~/.gitconfig` 必须单独只读授权。
    ///
    /// 不授它就报 "Please tell me who you are"，`git commit` 完全不可用——而 `$HOME` 本身
    /// 绝不能授（里面有 `~/.ssh`、`~/.git-credentials`）。
    #[test]
    fn gitconfig_is_granted_readonly_without_opening_home() {
        let base = std::env::temp_dir().join(format!(
            "omnicrawl-policy-gitconfig-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        std::fs::create_dir_all(&home).expect("应能建");
        let gitconfig = home.join(".gitconfig");
        std::fs::write(&gitconfig, "[user]\n\tname = x\n").expect("应能写");

        let home_text = home.display().to_string();
        let env = move |name: &str| (name == "HOME").then(|| home_text.clone());
        let policy = Policy::build(
            &config(),
            Path::new("/work"),
            Path::new("/work/tmp"),
            5,
            None,
            &env,
        );

        let rule = policy
            .fs_rules
            .iter()
            .find(|rule| rule.path == gitconfig)
            .expect("~/.gitconfig 必须进规则");
        assert_eq!(rule.access, Access::ReadOnly, "配置只需要读");
        assert!(
            !policy.fs_rules.iter().any(|rule| rule.path == home),
            "$HOME 本身绝不能被授权"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dev_null_is_writable() {
        // `> /dev/null` 是脚本里最常见的写法，只读会让它 EACCES。
        let policy = build(&config(), 5, None);
        let dev_null = policy
            .fs_rules
            .iter()
            .find(|rule| rule.path == Path::new("/dev/null"))
            .expect("/dev/null 必须在规则里");
        assert_eq!(dev_null.access, Access::ReadWrite);
    }

    #[test]
    fn describe_mentions_uncontrolled_udp() {
        let mut policy_config = config();
        policy_config.net_policy = NetPolicy::Ask;
        // 带代理，走在"只放行端口"那条分支上；`ask` 不管 UDP，必须如实标注。
        let text = build(&policy_config, 5, Some(41234)).describe(5);
        assert!(text.contains("ABI 5"), "{text}");
        assert!(text.contains("UDP 未受控"), "{text}");
    }
}
