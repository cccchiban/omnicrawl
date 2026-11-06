//! `sandbox-exec` launcher：给一条命令施加 Landlock 规则后 `execvp` 它。
//!
//! 为什么是一个独立进程而不是宿主的 `pre_exec`（设计文档 §4.3）：
//!
//! 1. `pre_exec` 要求 async-signal-safe —— `open(O_PATH)`、多次 `syscall()`、错误格式化都踩线；
//! 2. `pre_exec` 里失败只能得到一个 `io::Error`，无法把"哪条规则不行"如实说给模型；
//! 3. Landlock ABI < 8 时 `landlock_restrict_self` 只约束**调用线程**，必须在尚未多线程化
//!    的子进程里施加（launcher 全程单线程）。
//!
//! 策略经**管道**（stdin）传入而不是命令行：`ps` 看不到、不受 `ARG_MAX` 限制。
//!
//! 施加顺序见设计文档 §6.3，每一步都有理由，不可调换。第 7 步之后**任何失败都直接退出**，
//! 绝不"不带规则继续跑"——跑不了 > 没隔离地跑。

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::policy::{Access, Policy};

/// launcher 的子命令名。宿主把它放在 argv[1]，后面接 `--` 与真正的命令。
pub const LAUNCHER_COMMAND: &str = "sandbox-exec";

/// 失败时写进 stderr 的前缀：宿主据此把退出码翻成 `SANDBOX_*` 错误码。
pub const FAILURE_PREFIX: &str = "SANDBOX_";

// Landlock 规则类型（`linux/landlock.h`）。
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const LANDLOCK_RULE_NET_PORT: u32 = 2;
/// `landlock_ruleset_attr` 的有效字段长度。ABI < 4 的内核只认 8 字节
/// （`handled_access_fs` 一个 u64），传更长长度会 `E2BIG`。
const RULESET_ATTR_FS_ONLY: usize = 8;
const RULESET_ATTR_WITH_NET: usize = 16;

/// `LANDLOCK_ACCESS_NET_BIND_TCP`（ABI ≥ 4）。
const NET_BIND_TCP: u64 = 1 << 0;
/// `LANDLOCK_ACCESS_NET_CONNECT_TCP`（ABI ≥ 4）。
const NET_CONNECT_TCP: u64 = 1 << 1;
/// `LANDLOCK_ACCESS_NET_BIND_UDP`（ABI ≥ 10）。
const NET_BIND_UDP: u64 = 1 << 2;
/// `LANDLOCK_ACCESS_NET_CONNECT_SEND_UDP`（ABI ≥ 10）。
const NET_CONNECT_SEND_UDP: u64 = 1 << 3;

/// `handled_access_net` 的取值：按 ABI 裁剪，只列**本内核认识**的位。
///
/// 内核文档要求 `handled_access_net` 只能是本 ABI 支持的位；把 ABI 10 的 UDP 位传给
/// ABI 4 的内核会让 `create_ruleset` 以 `EINVAL` 失败——先是规则集建不出来，
/// 随后整条命令都跑不了。
fn handled_net(abi: u32, control_udp: bool) -> u64 {
    if abi < 4 {
        return 0;
    }
    let mut bits = NET_BIND_TCP | NET_CONNECT_TCP;
    if control_udp && abi >= 10 {
        bits |= NET_BIND_UDP | NET_CONNECT_SEND_UDP;
    }
    bits
}

/// 与内核 ABI 对齐的 `struct landlock_ruleset_attr`。
#[repr(C)]
#[derive(Default)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

/// `struct landlock_path_beneath_attr`。
#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// `struct landlock_net_port_attr`。
#[repr(C)]
struct NetPortAttr {
    allowed_access: u64,
    port: u64,
}

/// launcher 的一次运行。成功时**不返回**（进程已被 `execvp` 替换）。
///
/// 返回 `Err` 表示命令没有执行；调用方（`main`）负责把原因写到 stderr 并以非零码退出。
pub fn run(args: &[String], policy_text: &str, abi: u32) -> Result<(), String> {
    if args.is_empty() {
        return Err(format!(
            "SANDBOX_EXEC_FAILED 用法：{LAUNCHER_COMMAND} <命令> [参数…]（策略经 stdin 传入）"
        ));
    }
    let policy = decode_policy(policy_text)?;

    // ① 先关掉 3 以上的所有继承 fd：Landlock 不追溯"沙盒化之前就已打开的 fd"，
    //    而策略此时已从 stdin 读完，不再需要任何继承来的句柄。
    //
    //    这一步必须在 open_rule_fds 之前：反过来会把刚打开的规则 fd 一并关掉，
    //    那些 fd 正是下一步 add_rule 要用的。
    close_inherited_fds();

    // ② 打开所有要授予的路径：`O_PATH` fd 必须在施加限制之前拿到。
    //    这一步也顺带验证路径存在，失败能说清是哪一个。
    let opened = open_rule_fds(&policy)?;

    // ②' 关掉 core dump。崩溃时内核会把进程内存整块写到磁盘（`/proc/sys/kernel/core_pattern`
    //     可能指向工作区或可写目录），沙盒读过的内容会随之外泄。`RLIMIT_CORE` 无特权可用，
    //     且它只是把"崩溃取证"降级为"没有 core"，不影响任何正常命令。
    disable_core_dumps()?;

    // ③ 无特权进程施加规则的前提。
    set_no_new_privs()?;

    // ③' 迁入 cgroup：限额必须**在命令跑起来之前**生效，否则第一秒的写入已经越界。
    //     位置重要：它只写 `cgroup.procs`，不依赖 Landlock，也不需要在此之前建立规则集；
    //     放在施加规则之后则会失败（Landlock 不限制 cgroupfs，但迁移的时机已过）。
    //     失败只降级为"没有限额"，绝不因此拒绝执行（§8：限额是可用性手段）。
    let limits_note = match &policy.cgroup {
        Some(path) => match super::cgroup::join(path) {
            Ok(()) => None,
            Err(reason) => Some(reason),
        },
        None => None,
    };

    // ④ 建规则集：handled 位按 ABI 裁剪，结构体长度同样按 ABI 裁剪。
    let ruleset_fd = create_ruleset(&policy, abi)?;

    // ⑤ 文件规则。
    for (rule, fd) in policy.fs_rules.iter().zip(opened.iter()) {
        add_path_rule(ruleset_fd, *fd, rule.access.bits(abi))?;
    }

    // ⑥ 网络规则（ABI ≥ 4 才存在）。
    //
    // `allowed_access` 必须与 `handled_access_net` 相容：这里加的每条规则都只带
    // `handled_net` 里确实列出的位，因此不会在低 ABI 上因"授予了不受管的权限"而失败。
    if abi >= 4 && policy.control_network {
        for port in &policy.connect_ports {
            add_net_rule(ruleset_fd, *port, NET_CONNECT_TCP)?;
        }
        for port in &policy.bind_ports {
            add_net_rule(ruleset_fd, *port, NET_BIND_TCP)?;
        }
        if policy.control_udp && abi >= 10 {
            for port in &policy.connect_udp_ports {
                add_net_rule(ruleset_fd, *port, NET_CONNECT_SEND_UDP)?;
            }
            for port in &policy.bind_udp_ports {
                add_net_rule(ruleset_fd, *port, NET_BIND_UDP)?;
            }
        }
    }

    for fd in &opened {
        close_fd(*fd);
    }

    // ⑦ 施加。此后不可撤销。
    restrict_self(ruleset_fd)?;
    close_fd(ruleset_fd);

    // ⑧ execvp：失败即退出，不降级。
    //    进这一行之前先把限额降级的原因告出去（stderr，不污染命令的 stdout）。
    if let Some(reason) = limits_note {
        eprintln!("SANDBOX_LIMITS_SKIPPED {reason}");
    }
    let error = Command::new(&args[0]).args(&args[1..]).exec();
    Err(format!(
        "SANDBOX_EXEC_FAILED 无法执行 `{}`：{error}",
        args[0]
    ))
}

fn decode_policy(text: &str) -> Result<Policy, String> {
    decode_policy_lines(text)
}

/// 策略的行格式（极简，避免引入 serde 依赖到这条最关键路径上）：
///
/// ```text
/// fs <rw|rx|ro> <路径>
/// connect <端口>
/// bind <端口>
/// connect-udp <端口>
/// bind-udp <端口>
/// network <0|1>
/// udp <0|1>
/// ```
fn decode_policy_lines(text: &str) -> Result<Policy, String> {
    let mut policy = Policy {
        fs_rules: Vec::new(),
        connect_ports: Vec::new(),
        bind_ports: Vec::new(),
        connect_udp_ports: Vec::new(),
        bind_udp_ports: Vec::new(),
        control_network: false,
        control_udp: false,
        install_roots: 0,
        cgroup: None,
    };
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let kind = parts.next().unwrap_or_default();
        let result = match kind {
            "fs" => {
                let access = match parts.next() {
                    Some("rw") => Access::ReadWrite,
                    Some("rx") => Access::ReadExecute,
                    Some("ro") => Access::ReadOnly,
                    other => {
                        return Err(format!(
                            "SANDBOX_EXEC_FAILED 策略第 {} 行的权限档无效：{other:?}",
                            index + 1
                        ))
                    }
                };
                let path = parts.next().unwrap_or_default();
                if path.is_empty() {
                    return Err(format!(
                        "SANDBOX_EXEC_FAILED 策略第 {} 行缺少路径",
                        index + 1
                    ));
                }
                policy.fs_rules.push(super::policy::PathRule {
                    path: std::path::PathBuf::from(path),
                    access,
                });
                Ok(())
            }
            "connect" | "bind" | "connect-udp" | "bind-udp" => {
                let port: u16 = parts
                    .next()
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| {
                        format!(
                            "SANDBOX_EXEC_FAILED 策略第 {} 行的端口无效",
                            index + 1
                        )
                    })?;
                match kind {
                    "connect" => policy.connect_ports.push(port),
                    "bind" => policy.bind_ports.push(port),
                    "connect-udp" => policy.connect_udp_ports.push(port),
                    _ => policy.bind_udp_ports.push(port),
                }
                Ok(())
            }
            "network" => {
                policy.control_network = parts.next() == Some("1");
                Ok(())
            }
            "udp" => {
                policy.control_udp = parts.next() == Some("1");
                Ok(())
            }
            // 沙盒进程要迁入的 cgroup（宿主已建好并写好限额）。缺省表示不设限额。
            "cgroup" => {
                let path = parts.next().unwrap_or_default();
                if !path.is_empty() {
                    policy.cgroup = Some(std::path::PathBuf::from(path));
                }
                Ok(())
            }
            other => Err(format!(
                "SANDBOX_EXEC_FAILED 策略第 {} 行无法识别：{other}",
                index + 1
            )),
        };
        result?;
    }
    Ok(policy)
}

/// 把策略编码成 launcher 读得懂的行格式。
pub fn encode_policy(policy: &Policy) -> String {
    let mut out = String::new();
    for rule in &policy.fs_rules {
        let access = match rule.access {
            Access::ReadWrite => "rw",
            Access::ReadExecute => "rx",
            Access::ReadOnly => "ro",
        };
        out.push_str(&format!("fs {access} {}\n", rule.path.display()));
    }
    for port in &policy.connect_ports {
        out.push_str(&format!("connect {port}\n"));
    }
    for port in &policy.bind_ports {
        out.push_str(&format!("bind {port}\n"));
    }
    for port in &policy.connect_udp_ports {
        out.push_str(&format!("connect-udp {port}\n"));
    }
    for port in &policy.bind_udp_ports {
        out.push_str(&format!("bind-udp {port}\n"));
    }
    out.push_str(&format!(
        "network {}\n",
        u8::from(policy.control_network)
    ));
    out.push_str(&format!("udp {}\n", u8::from(policy.control_udp)));
    if let Some(path) = &policy.cgroup {
        out.push_str(&format!("cgroup {}\n", path.display()));
    }
    out
}

/// 读策略文本：宿主经 stdin 写一行头（含 ABI）再把策略正文跟上。
pub fn read_policy(reader: &mut impl Read) -> Result<(String, u32), String> {
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|error| format!("SANDBOX_EXEC_FAILED 读取策略失败：{error}"))?;
    let mut lines = text.splitn(2, '\n');
    let header = lines.next().unwrap_or_default();
    let abi: u32 = header
        .strip_prefix("abi ")
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| {
            format!("SANDBOX_EXEC_FAILED 策略头无效（应为 `abi <N>`）：{header:?}")
        })?;
    Ok((lines.next().unwrap_or_default().to_string(), abi))
}

fn open_rule_fds(policy: &Policy) -> Result<Vec<i32>, String> {
    let mut fds = Vec::with_capacity(policy.fs_rules.len());
    for rule in &policy.fs_rules {
        let c_path = std::ffi::CString::new(rule.path.to_string_lossy().as_bytes()).map_err(
            |_| {
                format!(
                    "SANDBOX_UNAVAILABLE 路径含 NUL 字节：{}",
                    rule.path.display()
                )
            },
        )?;
        // SAFETY: O_PATH 只取"指向该 inode 的句柄"，不读内容、不要求读权限；
        // 返回的 fd 由本函数负责关闭（调用方在施加规则后统一 close）。
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return Err(format!(
                "SANDBOX_UNAVAILABLE 无法打开沙盒规则路径 {}：{error}",
                rule.path.display()
            ));
        }
        fds.push(fd);
    }
    Ok(fds)
}

/// 关掉所有继承来的 fd（0/1/2 除外）。
fn close_inherited_fds() {
    // 优先用 `close_range`（Linux 5.9+）：一次系统调用搞定，且不受 fd 表大小影响。
    // SAFETY: close_range 只关闭本进程 fd 表里的项；0..=2 被显式排除（起点是 3）。
    let result = unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) };
    if result == 0 {
        return;
    }
    // 退化路径（老内核没有 close_range）：逐个数。
    // 上界取 65536 而不是 `getrlimit`：后者在 glibc/musl 上参数类型不同
    // （`__rlimit_resource_t` vs `c_int`），为这点收益引入平台分叉不值得；
    // 关闭无效 fd 只返回 EBADF，没有副作用。
    for fd in 3..65_536 {
        // SAFETY: close 对无效 fd 只返回 EBADF，无副作用。
        unsafe {
            libc::close(fd);
        }
    }
}

fn set_no_new_privs() -> Result<(), String> {
    // SAFETY: PR_SET_NO_NEW_PRIVS 只把本进程的 no_new_privs 置 1，不可逆但对宿主没有影响
    // （launcher 是独立子进程，马上被 execvp 替换）。
    let result = unsafe {
        libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!("SANDBOX_UNAVAILABLE 无法设置 no_new_privs：{error}"));
    }
    Ok(())
}

/// 关掉 core dump：崩溃时内核不再把进程内存写到磁盘。
///
/// 失败不致命——core dump 是取证/调试手段，不是安全边界；但**必须**尝试，
/// 因为沙盒读过的内容（含工作区源码）会整块落进 core 文件，而 `core_pattern`
/// 可能指向一个可写目录。
fn disable_core_dumps() -> Result<(), String> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: 传本进程栈上的有效 `rlimit` 结构体地址，`RLIMIT_CORE` 是合法资源项。
    // 降限额对无特权进程永远允许（不可逆但只影响这个即将被 execvp 替换的进程）。
    let result = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!("SANDBOX_UNAVAILABLE 无法关闭 core dump：{error}"));
    }
    Ok(())
}

fn create_ruleset(policy: &Policy, abi: u32) -> Result<i32, String> {
    let handled_fs = super::policy::handled_fs(abi);
    let (attr, size) = if abi >= 4 && policy.control_network {
        (
            RulesetAttr {
                handled_access_fs: handled_fs,
                handled_access_net: handled_net(abi, policy.control_udp),
            },
            RULESET_ATTR_WITH_NET,
        )
    } else {
        // ABI < 4 的内核不认识 handled_access_net，长度必须只报 8，否则 E2BIG。
        (
            RulesetAttr {
                handled_access_fs: handled_fs,
                handled_access_net: 0,
            },
            RULESET_ATTR_FS_ONLY,
        )
    };
    // SAFETY: attr 是有效的本进程栈上结构体，size 与其有效字段长度一致。
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            size as libc::size_t,
            0 as libc::c_uint,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "SANDBOX_UNAVAILABLE 无法创建 Landlock 规则集（ABI {abi}）：{error}"
        ));
    }
    Ok(fd as i32)
}

fn add_path_rule(ruleset_fd: i32, parent_fd: i32, allowed: u64) -> Result<(), String> {
    if allowed == 0 {
        // 该 ABI 上这条路径一档权限都不支持：跳过而不是传 0（内核会 EINVAL）。
        return Ok(());
    }
    let attr = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd,
    };
    // SAFETY: attr 指向有效的栈上结构体，parent_fd 是上面 open(O_PATH) 得到的有效 fd。
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH as libc::c_uint,
            &attr as *const PathBeneathAttr,
            0 as libc::c_uint,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "SANDBOX_UNAVAILABLE 无法施加文件规则（fd {parent_fd}）：{error}"
        ));
    }
    Ok(())
}

fn add_net_rule(ruleset_fd: i32, port: u16, allowed: u64) -> Result<(), String> {
    let attr = NetPortAttr {
        allowed_access: allowed,
        port: u64::from(port),
    };
    // SAFETY: 同 add_path_rule；ABI ≥ 4 才走到这里（调用方已判）。
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset_fd,
            LANDLOCK_RULE_NET_PORT as libc::c_uint,
            &attr as *const NetPortAttr,
            0 as libc::c_uint,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "SANDBOX_UNAVAILABLE 无法施加网络规则（端口 {port}）：{error}"
        ));
    }
    Ok(())
}

fn restrict_self(ruleset_fd: i32) -> Result<(), String> {
    // SAFETY: ruleset_fd 是上面创建的有效 fd；flags 为 0。此调用不可逆，
    // 但对本进程（马上 execvp）正是我们要的。
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_restrict_self,
            ruleset_fd,
            0 as libc::c_uint,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "SANDBOX_UNAVAILABLE 无法施加 Landlock 规则：{error}"
        ));
    }
    Ok(())
}

fn close_fd(fd: i32) {
    if fd >= 0 {
        // SAFETY: fd 由本模块创建，只关一次；重复关闭返回 EBADF 无副作用。
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::linux::policy::PathRule;

    fn sample_policy() -> Policy {
        Policy {
            fs_rules: vec![
                PathRule {
                    path: std::path::PathBuf::from("/work"),
                    access: Access::ReadWrite,
                },
                PathRule {
                    path: std::path::PathBuf::from("/usr"),
                    access: Access::ReadExecute,
                },
            ],
            connect_ports: vec![443, 41234],
            bind_ports: vec![0],
            connect_udp_ports: vec![53],
            bind_udp_ports: vec![0],
            control_network: true,
            control_udp: true,
            install_roots: 0,
            cgroup: None,
        }
    }

    /// `handled_access_net` 必须随 ABI 裁剪：把 UDP 位传给 ABI 4 的内核会让
    /// `create_ruleset` 直接 `EINVAL`，那比"UDP 未受控"严重得多——整条命令都跑不了。
    #[test]
    fn handled_net_is_cropped_to_abi() {
        assert_eq!(handled_net(3, true), 0, "ABI 3 不认识网络位");
        assert_eq!(handled_net(4, true), NET_BIND_TCP | NET_CONNECT_TCP);
        assert_eq!(handled_net(9, true), NET_BIND_TCP | NET_CONNECT_TCP);
        let at_ten = handled_net(10, true);
        assert_ne!(at_ten & NET_BIND_UDP, 0);
        assert_ne!(at_ten & NET_CONNECT_SEND_UDP, 0);
        // 不管 UDP 时即使 ABI 够也不该把位交出去：交出去就等于承诺"已受控"。
        assert_eq!(handled_net(10, false), NET_BIND_TCP | NET_CONNECT_TCP);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let policy = sample_policy();
        let text = encode_policy(&policy);
        let decoded = decode_policy_lines(&text).expect("应可解回");
        assert_eq!(decoded, policy);
    }

    #[test]
    fn a_cgroup_line_survives_the_roundtrip() {
        // 限额全靠这一行传给 launcher：编码漏了它，沙盒进程就永远不会迁进 cgroup，
        // 而且**不会报错**——表现为"限额静默不生效"，最难查的那一类。
        let mut policy = sample_policy();
        policy.cgroup = Some(std::path::PathBuf::from(
            "/sys/fs/cgroup/user.slice/omnicrawl-sandbox-4321",
        ));
        let text = encode_policy(&policy);
        assert!(text.contains("cgroup "), "{text}");
        let decoded = decode_policy_lines(&text).expect("应可解回");
        assert_eq!(decoded.cgroup, policy.cgroup);
        // 不带这一行时解出来必须是 `None`：不能凭空给一个路径。
        policy.cgroup = None;
        assert_eq!(decode_policy_lines(&encode_policy(&policy)).expect("应可解回").cgroup, None);
    }

    #[test]
    fn decode_rejects_garbage() {
        let error = decode_policy_lines("bogus line\n").expect_err("应报错");
        assert!(error.starts_with(FAILURE_PREFIX), "{error}");
        let error = decode_policy_lines("fs zz /work\n").expect_err("权限档无效");
        assert!(error.contains("权限档"), "{error}");
        let error = decode_policy_lines("fs rw\n").expect_err("缺路径");
        assert!(error.contains("缺少路径"), "{error}");
        let error = decode_policy_lines("connect nope\n").expect_err("端口无效");
        assert!(error.contains("端口"), "{error}");
    }

    #[test]
    fn decode_tolerates_blank_lines() {
        let decoded = decode_policy_lines("\n\nnetwork 1\n\n").expect("空行不算错");
        assert!(decoded.control_network);
        assert!(decoded.fs_rules.is_empty());
    }

    #[test]
    fn read_policy_requires_abi_header() {
        let mut input = "abi 5\nfs rw /work\n".as_bytes();
        let (text, abi) = read_policy(&mut input).expect("应可读");
        assert_eq!(abi, 5);
        assert!(text.contains("/work"));

        let mut bad = "fs rw /work\n".as_bytes();
        let error = read_policy(&mut bad).expect_err("缺头必须报错");
        assert!(error.contains("策略头"), "{error}");
    }

    #[test]
    fn empty_args_is_rejected() {
        let error = run(&[], "abi 5\n", 5).expect_err("没有命令必须报错");
        assert!(error.starts_with(FAILURE_PREFIX), "{error}");
    }

    #[test]
    fn missing_rule_path_fails_before_touching_kernel_state() {
        // **必须在子进程里跑**：`run()` 第一件事是 `close_inherited_fds()`，会把本进程
        // fd 3 以上全部关掉。在测试二进制自身里直接调用，会关掉 libtest 的捕获管道与
        // 并发测试正在用的 fd，整套测试跑完后以
        // `fatal runtime error: IO Safety violation: owned file descriptor already closed`
        // 中止（真机实测：并行必现，`--nocapture` 与单线程不复现）。
        let policy = Policy {
            fs_rules: vec![PathRule {
                path: std::path::PathBuf::from("/definitely-not-here-zzz"),
                access: Access::ReadWrite,
            }],
            connect_ports: Vec::new(),
            bind_ports: Vec::new(),
            connect_udp_ports: Vec::new(),
            bind_udp_ports: Vec::new(),
            control_network: false,
            control_udp: false,
            install_roots: 0,
            cgroup: None,
        };
        // `run` 收到的是**已剥离头部**的策略正文（头由 `read_policy` 在 `main` 里拆走），
        // 这里必须照此构造，否则测的是"run 能否识别 `abi` 行"——它本就不该识别。
        let text = encode_policy(&policy);
        let output = std::process::Command::new(std::env::current_exe().expect("测试二进制"))
            .args([
                "--exact",
                "sandbox::linux::launcher::tests::missing_rule_path_child_probe",
                "--nocapture",
                "--ignored",
            ])
            .env(CHILD_POLICY_ENV, text)
            .output()
            .expect("拉起子进程");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "路径缺失必须让 launcher 失败，stderr={stderr}"
        );
        assert!(stderr.contains("无法打开沙盒规则路径"), "{stderr}");
        assert!(stderr.contains("definitely-not-here-zzz"), "{stderr}");
    }

    /// `missing_rule_path_fails_before_touching_kernel_state` 的被测体。
    #[test]
    #[ignore = "由 missing_rule_path_fails_before_touching_kernel_state 作为子进程拉起"]
    fn missing_rule_path_child_probe() {
        let Some(text) = std::env::var_os(CHILD_POLICY_ENV) else {
            return;
        };
        let text = text.to_string_lossy().to_string();
        let error = run(&["/bin/true".to_string()], &text, 5).expect_err("路径缺失必须报错");
        eprintln!("{error}");
        std::process::exit(1);
    }

    /// 真正施加一次规则并 exec 一个安全命令。
    ///
    /// 只在 Linux 上跑；内核不支持时跳过（CI runner 版本会漂移，不能硬性要求）。
    #[test]
    fn applies_rules_and_executes_a_real_command() {
        use crate::sandbox::linux::capability;
        let capability = capability();
        let Some(abi) = capability.abi() else {
            eprintln!("内核不支持 Landlock，跳过");
            return;
        };
        // 子进程里跑，避免把测试进程自己关进沙盒。
        let policy = Policy {
            fs_rules: vec![PathRule {
                path: std::path::PathBuf::from("/usr"),
                access: Access::ReadExecute,
            }],
            connect_ports: Vec::new(),
            bind_ports: Vec::new(),
            connect_udp_ports: Vec::new(),
            bind_udp_ports: Vec::new(),
            control_network: false,
            control_udp: false,
            install_roots: 0,
            cgroup: None,
        };
        let text = format!("abi {abi}\n{}", encode_policy(&policy));
        let mut child = std::process::Command::new(std::env::current_exe().expect("测试二进制"))
            .args([
                "--exact",
                "sandbox::linux::launcher::tests::launcher_child_probe",
                "--nocapture",
                "--ignored",
            ])
            .env(CHILD_POLICY_ENV, text)
            .spawn()
            .expect("拉起子进程");
        let status = child.wait().expect("等待子进程");
        assert!(status.success(), "launcher 子进程应成功：{status:?}");
    }

    /// 上面的用例拉起的子进程：在这里施加规则并 exec `/bin/true`。
    ///
    /// **必须在独立进程里跑**（`applies_rules_and_executes_a_real_command` 已这么做）：
    /// `run()` 第一件事是 `close_inherited_fds()`，会把本进程 fd 3 以上全部关掉。
    /// 若在测试二进制自身里调用，就会关掉 libtest 的捕获管道与并发测试在用的 fd——
    /// 症状是整套测试跑完后 `fatal runtime error: IO Safety violation:
    /// owned file descriptor already closed`（真机实测：并行必现，`--nocapture` 不复现，
    /// 单线程不复现）。这也是为什么本函数带 `#[ignore]`，只在子进程里被显式拉起。
    const CHILD_POLICY_ENV: &str = "OMNICRAWL_SANDBOX_PROBE_POLICY";

    #[test]
    #[ignore = "由 applies_rules_and_executes_a_real_command 作为子进程拉起"]
    fn launcher_child_probe() {
        let Some(text) = std::env::var_os(CHILD_POLICY_ENV) else {
            return;
        };
        let text = text.to_string_lossy().to_string();
        let mut lines = text.splitn(2, '\n');
        let abi: u32 = lines
            .next()
            .and_then(|header| header.strip_prefix("abi "))
            .and_then(|value| value.parse().ok())
            .expect("策略头");
        let body = lines.next().unwrap_or_default().to_string();
        // 成功时进程被 execvp 替换，这个 run() 不会返回。
        let error = run(&["/bin/true".to_string()], &body, abi).expect_err("exec 不该返回");
        panic!("launcher 失败：{error}");
    }
}
