# Linux 进程级轻量沙箱设计方案

> 状态：设计稿（未实现）。目标读者：后续实现者。
> 平台：Linux 5.13+（Landlock 可用）；5.19+ 具备 rename/link 控制；6.7+ 具备 TCP 端口控制。
> 定位：**进程级、无特权、无镜像、无守护进程**。不引入 Docker/Podman，不需要 root，
> 不需要 setuid 帮助程序，不建文件系统镜像。

本文是 `sandbox_windows_design.md` 的 Linux 对位方案，除平台实现外，**威胁模型、开关语义、
配置形状、与审批的关系、失败方向**一律沿用那边已经定下的结论，只在 Linux 必须不同的地方
（身份模型、网络控制、资源限额）另作说明。已有实现见 `omnicrawl-host/src/sandbox/`
（`mod.rs` 策略与判定、`gate.rs` 开关到执行路径、`proxy.rs` 出网通道、`windows.rs` Windows 后端）。

## 1. 要解决什么问题

与 Windows 侧同一个出发点（见 `sandbox_windows_design.md` §1）：审批拦的是"要不要做"，
拦不住"做完之后还能做什么"。Linux 上这套破坏面具体长这样：

- 模型跑 `pip install`，某个 `setup.py` 读走 `~/.ssh/id_ed25519`、`~/.aws/credentials` 并外发。
- 模型写的脚本 `curl | sh`，脚本再起后台进程连内网 `10.0.0.0/8`。审批只审了第一层。
- 一次 `rm -rf` 写错路径，删的是 `~` 而不是工作区。
- 沙盒进程把宿主的 `~/.omnicrawl/config.toml`（明文 API key）读走。

**要给的保证**：即使模型（或它拉起的任意后代）完全失控，破坏面被限制在
「工作区可写 + 显式批准的网络目标」内，且宿主进程自身不受影响；进程无法把限制甩掉。

## 2. 非目标

- **不是容器、不是虚拟机**。不提供独立 PID 视图、独立 rootfs、独立用户数据库。
  `ps aux` 仍能看到宿主进程（看不到内容，见 §3）。
- **不是防 APT**。同内核攻击面（未修复的 LPE）不在防护范围。
- **不沙盒化宿主自身**。只作用于工具拉起的子进程。
- **不需要 root，也不申请任何特权**。凡是需要 root 才能给的保证（mount 命名空间、
  cgroup 写入、nftables），一律不进默认路径，只作为可选加固（§9）。
- **不覆盖 GUI / 桌面类工具**：Linux 侧本就只有 `bash` 与 `git` 两条 spawn 路径（§5.1）。

## 3. 威胁模型与保证

| 攻击/误用 | 沙盒前 | 沙盒后 | 靠什么 |
| --- | --- | --- | --- |
| 读 `~/.ssh`、`~/.aws`、`~/.omnicrawl/config.toml` | 可读 | 读不到 | Landlock 文件规则（白名单外无权限） |
| 读工作区之外的任意项目 | 可读 | 读不到 | 同上 |
| 写 `~`、写 `/etc`、写别的项目 | 可能成功 | 写不进 | Landlock（写权限只给工作区与临时目录） |
| 外连 attacker.com 上传数据 | 直连 | 被拒（或在 allowlist 下只放行白名单端口/经代理） | Landlock 网络规则（6.7+）+ 宿主代理 |
| 连内网 `10.0.0.0/8`、`169.254.169.254` | 直连 | 被拒 | 同上（端口级；域名级由代理判定） |
| `curl \| sh` 起的后代逃逸 | 后代继承一切 | 后代继承同一 Landlock 域，不可撤销 | 内核：域随 fork/exec 继承 |
| 提权后摆脱限制 | — | 提权后仍在同一域内（域只能叠加不能撤销） | Landlock no_new_privs + 不可降级 |
| 把宿主进程杀了 / 干扰宿主 | 可以（同 UID） | 可以（见下） | **明确不给** |
| 吃满 CPU/内存 | 拖垮整机 | 可选 cgroup 限额（默认不启用） | cgroup v2（§9） |
| 宿主崩溃后子进程残留 | 已由进程组回收解决 | 沿用 | `process_control.rs`（`killpg`） |

**明确不给的**（都写下来，避免误以为有）：

1. **同 UID 进程间干扰**：Linux 上同 UID 进程默认可互相 `kill`、可读 `/proc/<pid>/environ`。
   Landlock 的 ptrace 限制只约束**受沙盒进程去 ptrace 别人**，不阻止它向同域外的同 UID
   进程发信号，也不阻止读 `/proc`。要挡住需要 PID 命名空间（需 root 或 userns）。
   诚实结论：**沙盒进程能打死宿主**。这一条与 Windows 侧 AppContainer 不同（那边有
   独立 SID 与作业对象），必须在文档与 `/sandbox` 输出里说清。
2. **已在沙盒进程里打开的 fd**：Landlock 对"沙盒化之前就已打开的 fd"不追溯。
   因此 launcher 在施加规则前必须只保留 0/1/2 三个 fd（§6.3）。
3. **时序与侧信道**、物理访问、内核 LPE。
4. **DNS 内容**：端口级规则只能放行/禁止"连 53 端口"，不检查查询内容；域名判定靠代理。

## 4. 技术选型与可行性核对

### 4.1 候选与结论

| 能力 | 候选 | v1 结论 | 理由 |
| --- | --- | --- | --- |
| 文件系统 | **Landlock** | ✅ 采用，唯一必需件 | 无特权、无镜像、内核级、不可撤销、后代继承 |
| 文件系统（强隔离） | mount ns + pivot_root + overlayfs | ❌ 不进 v1 | 需 userns；Ubuntu 24.04 起 `kernel.apparmor_restrict_unprivileged_userns=1` 默认拦住无 profile 的 userns 创建（bwrap 踩的就是这个坑） |
| 网络 | **Landlock 网络规则**（6.7+） | ✅ 采用（端口级） | 无特权、无需 nftables |
| 网络（域名级审批） | **宿主回环代理** | ✅ 采用，与 Windows 侧同一套 | 复用 `sandbox/proxy.rs`，审批逻辑零改动 |
| 网络（严格离线） | 空网络命名空间 | ⚠️ 可选降级 | 需要 `CLONE_NEWUSER|CLONE_NEWNET`，受 userns 策略影响；`offline` 下用 Landlock 全禁端口即可达到同等效果 |
| 系统调用面 | seccomp-bpf | ⚠️ P3 可选 | 能在 6.12+ 用 Landlock 命名空间规则替代大半（见 §4.4） |
| 资源限额 | **cgroup v2** | ⚠️ 可选，默认关 | 需要写 cgroup 目录权限；拿不到就如实降级 |
| 资源限额（不需要 root 的替代） | `setrlimit`（RLIMIT_NPROC/AS/FSIZE） | ✅ P3 | 无特权可用；粒度粗但方向正确 |

### 4.2 Landlock 的事实（均已核实，不是推测）

- **无特权**：任何进程可自我限制；施加后**不可撤销**，只能继续叠加；域随
  `fork`/`exec` 继承，后代无法逃离。
- **ABI 与内核版本**（内核文档 `userspace-api/landlock.html` 与官方演讲一致）：

  | ABI | 内核 | 新增能力 |
  | --- | --- | --- |
  | 1 | 5.13 | 文件系统基础权限（read/write/execute/目录增删） |
  | 2 | 5.19 | `LANDLOCK_ACCESS_FS_REFER`（rename/link 跨目录） |
  | 3 | 6.2 | `LANDLOCK_ACCESS_FS_TRUNCATE` |
  | 4 | 6.7 | `LANDLOCK_ACCESS_NET_BIND_TCP` / `LANDLOCK_ACCESS_NET_CONNECT_TCP` |
  | 5 | 6.10 | `LANDLOCK_ACCESS_FS_IOCTL_DEV` |
  | 6 | 6.12 | `LANDLOCK_SCOPE_SIGNAL` / `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` |
  | 9 | — | `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` |
  | 10 | — | UDP 端口规则 |
  | 12 | — | 能力规则 / 命名空间规则（`handled_permissions`） |

- **必须 `no_new_privs`**：无特权进程施加规则前需 `prctl(PR_SET_NO_NEW_PRIVS, 1)`；
  ABI ≥ 11 可在 `landlock_restrict_self` 里带 `LANDLOCK_RESTRICT_SELF_NO_NEW_PRIVS` 一并设。
  不设的后果是 setuid / file-capability 二进制会带着高权限继续运行在同一域里（confused deputy）。
- **多线程陷阱**：ABI < 8 时 `landlock_restrict_self` 只约束**调用线程**及其子线程。
  因此规则必须在**刚 fork 出来、尚未多线程化**的子进程里施加；这也是选择
  独立 launcher 进程而不是在宿主里 `pre_exec` 的根本原因（§6.3）。
- **符号来源**：`libc 0.2` 已含 `SYS_landlock_create_ruleset`(444) / `SYS_landlock_add_rule`(445) /
  `SYS_landlock_restrict_self`(446)（x86_64、aarch64、riscv64、musl 各 arch 均已核对），
  **不引入新的 crate 依赖**。`LANDLOCK_*` 常量 libc 未导出，在 `sandbox/linux.rs` 里按内核
  UAPI 头文件自行定义（值稳定，属内核 ABI）。
- **`handled_access_*` 是"拒绝并集"**：需要在 ruleset 创建时显式列出要管的权限，未列出的
  权限对该 ruleset 不生效。因此必须按运行内核的 ABI 裁剪（官方推荐的 best-effort 写法：
  `abi < 4` 去掉网络位、`abi < 3` 去掉 TRUNCATE 位……），否则老内核上 `create_ruleset` 直接 `EINVAL`。
- **`FS_REFER` 特殊**：即使不在 `handled_access_fs` 里也默认拒绝，要放行 rename/link 必须显式带上。

### 4.3 为什么不在宿主里直接 `pre_exec` 施加规则

`std::process::Command::pre_exec` 在**父进程 fork 出的子进程**里执行，看似正是"刚 fork、
尚未多线程"的时机，但有两个问题：

1. `pre_exec` 的闭包必须 `async-signal-safe`：`open(O_PATH)`、多次 `syscall()`、错误字符串
   格式化都踩线；一旦出错，唯一的报错通道是 `write(2)` 到 fd 2。
2. 判定失败必须**让命令不执行**。`pre_exec` 里返回 `Err` 确实会让 `spawn` 失败，但拿不到
   可读原因（只有 `io::Error`），与 Windows 侧 `SANDBOX_UNAVAILABLE` 的"可读中文错误"要求
   冲突。

**做法**：用宿主自己的可执行文件做 launcher（`omnicrawl-host sandbox-exec -- <cmd…>`，
与 Windows 侧"自己调 `CreateProcessW`"是同一位置的对位实现），参数经**管道**传递而不是
命令行（避免 `ps` 泄露策略、避免 `argv` 长度限制）。launcher 全程单线程、只在最后
`execvp` 一次，任何准备失败都以退出码 + stderr 明确说明。

### 4.4 seccomp 的位置

seccomp-bpf 能挡很多"读内核信息"的路径（如 `unshare`、`mount`、`keyctl`），但：

- 它**不做路径判定**，挡不住"读 `~/.ssh`"这类同权限文件访问——那正是 Landlock 的活。
- 它能挡 `ptrace`/`process_vm_readv` 逃逸面，但 Landlock 的 ptrace 限制已覆盖同域内情形。
- 6.12+ 的 Landlock 命名空间规则（`LANDLOCK_PERMISSION_NAMESPACE_USE` + 能力规则）能用
  声明方式表达"禁止新建 userns/netns/mountns"，比手写 BPF 白名单更稳、更可读。

**结论**：v1 不做 seccomp。P3 只在两处考虑：(a) 内核 < 6.12 时用一处极窄的
`unshare`/`clone`(CLONE_NEW*) 黑名单补位；(b) `offline` 策略下额外挡裸 socket 系统调用
（但 Landlock 端口规则在 6.7+ 已足够，故 (b) 实际不做）。

## 5. 总体架构

```
                       ┌──────────────────────────────────────────────┐
                       │ omnicrawl-host（宿主，永在沙盒外）            │
                       │                                              │
  模型调用 bash ──────► │ tools/command.rs  tools/monitor.rs  tools/git │
                       │        │                                     │
                       │        ▼                                     │
                       │ sandbox/mod.rs  ── 策略（SandboxConfig）      │
                       │        │                                     │
                       │        ▼                                     │
                       │ sandbox/gate.rs   Plan::Host | Plan::Isolated │
                       │        │              （Linux 分支新增）      │
                       │        ▼                                     │
                       │ 拉起自己：omnicrawl-host sandbox-exec …       │
                       │   policy ──[管道]──► launcher                │
                       └───────────────┬──────────────────────────────┘
                                       │ fork/exec
                                       ▼
                       ┌──────────────────────────────────────────────┐
                       │ sandbox-exec（单线程，一次性）                │
                       │  ① 读策略                                     │
                       │  ② 关掉 3 以上的所有 fd（保留策略管道）        │
                       │  ③ prctl(PR_SET_NO_NEW_PRIVS)                │
                       │  ④ landlock_create_ruleset（按 ABI 裁剪）     │
                       │  ⑤ add_rule：工作区/临时目录/系统只读路径      │
                       │  ⑥ add_rule：网络端口（按策略）               │
                       │  ⑦ landlock_restrict_self                    │
                       │  ⑧ 若可写 -> 无；若 cgroup 可用 -> 自挂       │
                       │  ⑨ execvp(argv)   ← 失败即退出，不降级        │
                       └───────────────┬──────────────────────────────┘
                                       │ 出网（HTTP(S)/其他 TCP）
                                       ▼
                       ┌──────────────────────────────────────────────┐
                       │ 宿主回环代理（复用 sandbox/proxy.rs）         │
                       │  命中规则 → 放行 / 未命中 → 审批 → 放/拒      │
                       └──────────────────────────────────────────────┘
```

三层边界，逐层收紧（与 Windows 侧一一对位）：

1. **文件边界**（Landlock）：以「白名单路径 + 白名单权限」表达，白名单外一律 `EACCES`。
2. **网络边界**（Landlock 端口规则 + 宿主代理）：非回环目标在端口层先受限，
   通过的再经代理做**域名级**判定与审批。
3. **生命周期与资源**（进程组 / 可选 cgroup）：沿用 `process_control.rs` 的
   `configure_process_group` + `killpg`，宿主退出不留孤儿；cgroup 为可选加固。

### 5.1 受沙盒约束的工具有哪些（Linux 侧）

按实际 spawn 点判定（不能拿 `agent_tools.json` 的 `run_in_subprocess` 当依据）：

| 工具 | 现状 | Linux 侧目标 | 说明 |
| --- | --- | --- | --- |
| `bash`（`command.rs`） | 已接（Windows 分支） | ✅ 必须 | 主执行入口 |
| `monitor`（`monitor.rs` 后台命令） | 已接（Windows 分支） | ✅ 必须 | 同上，长跑 |
| `git`（`git.rs:119` 直接 `Command::new("git")`） | **未接沙盒**（两平台都未接） | ✅ 必须（P4） | 会写 `.git`、会读 `/etc/gitconfig` 与凭据助手，漏接是当前的实际缺口 |
| 内部子进程（TTS、node 扩展、决策服务等） | — | ❌ 不进沙盒 | 属宿主自身组件，不是模型命令 |

> 说明：`git` 这一行是审计现状得出的结论，不是设计期望。Windows 侧同样没接，属既有缺口，
> Linux 实现时一并补上；在补齐前，"沙盒内破坏面仅限工作区"对 `git` 并不成立。

Windows 侧真隔离下会把 `bash` 移出工具表、把 `powershell` 补进来（AppContainer 里 Git Bash
起不来）。**Linux 侧不需要这套开关**：`bash` 就是唯一解释器，launcher 也不影响它。
`registry.rs::sandbox_tool_switches` 必须保持 `#[cfg(windows)]` 语义，不能被 Linux 复用。

## 6. 文件系统隔离

### 6.1 规则形状（白名单，不是黑名单）

Landlock 是**拒绝优先**的：`handled_access_fs` 里列出的权限，在没有任何 allow 规则命中的
路径上一律拒绝。因此不需要（也不应该）枚举 `~/.ssh`、`~` 之类的黑名单——**只列允许的**：

| 路径 | 读写 | 为什么 |
| --- | --- | --- |
| 工作区根 | READ_FILE, READ_DIR, WRITE_FILE, TRUNCATE, MAKE_*, REMOVE_*, REFER, EXECUTE | 产物落这里；`REFER` 让 `git`/包管理器能在工作区内 rename |
| `.omnicrawl/.agent_tmp` | 同上 | 临时脚本与中间产物 |
| `<沙盒专用 TMPDIR>` | 同上 | 见 §6.2 |
| `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc/ld.so.cache` | READ_FILE, READ_DIR, EXECUTE | 解释器、动态库、工具链 |
| `/etc`（**除** `ld.so.cache` 与 `ssl` 之外） | 视需要（P1 默认**不给**） | 给了就能读全机配置；开局收紧，报错再放开 |
| `/etc/ssl/certs`, `/usr/lib/ssl` | READ_FILE, READ_DIR | 否则 https 全废（证书校验失败） |
| `/proc`, `/sys`, `/dev/null`, `/dev/zero`, `/dev/tty`, `/dev/urandom` | READ_FILE（`/dev/null` 另需写） | 缺了 `bash`/`node`/`python` 会以极晦涩的方式失败 |

**明确不给**：`~` 本体、`~/.ssh`、`~/.gnupg`、`~/.aws`、`~/.config`（除显式列入）、
`~/.omnicrawl`、其它项目目录、`/` 递归。

要点：**优先给叶子目录**（内核文档 "Good practices"）。给 `~` 只读再指望 `~/.ssh` 挡住是
做不到的——Landlock 只做减法，无法在"已放行的祖先"里再挖洞。

### 6.2 环境变量重定向（Linux 侧的坑与 Windows 同源）

沙盒进程的 `TMPDIR`/`TMP`/`HOME`/`XDG_*` 必须改指向沙盒内可写目录，否则：

- 往 `$TMPDIR` 写临时文件被拒 → 各种工具以离奇方式失败（`gcc`、`npm`、`pip` 全都用临时目录）；
- `HOME` 指向真实家目录 → 读不到也写不进，报错信息误导模型。

做法与 Windows 侧 `windows.rs::debug_environment` 同构：**`env_clear()` 后显式重建最小环境**
（`PATH`、`TMPDIR`、`HOME`、`LANG`、`LC_ALL`、`TERM`、`SSL_CERT_FILE`、代理变量…），
`HOME` 指向 `<沙盒 TMPDIR>/home`，否则等于把 `~/.gitconfig`、`~/.npmrc` 一并关在门外
（多数时候这正是想要的；需要时通过 `[sandbox.fs] readable` 显式放开）。

同时这不泄漏宿主令牌类环境变量（`OMNICRAWL_*`、`OPENAI_API_KEY` 等）给沙盒子进程。

### 6.3 施加规则的顺序（每一步都有理由，不可调换）

```
1. 读策略（管道）            // 此时还没有任何限制，错了能报清楚
2. 打开所有要授予的目录 fd   // O_PATH | O_CLOEXEC，桌面相对路径按工作区根解析
3. 关掉 3..=所有 fd          // 策略管道读完即关；Landlock 不追溯已打开的 fd
4. prctl(PR_SET_NO_NEW_PRIVS, 1)
5. landlock_create_ruleset   // 按 ABI 裁剪 handled_access_*
6. landlock_add_rule × N     // 文件规则 + 网络规则，mask 到 handled 位
7. landlock_restrict_self    // ★ 此后子进程的一切都受限，且不可撤销
8. （可选）cgroup 自挂
9. execvp(argv)              // 失败 → 写 stderr + 退出，绝不"不带规则继续跑"
```

第 2 步先开 fd、第 3 步才关：`O_PATH` fd 本身可以授予权限，必须先于限制打开。
第 7 步之后**任何失败都不能退回宿主身份**——这是与 Windows 侧
`IsolationGate::plan`「准备失败不放行」一致的安全方向：跑不了 > 没隔离地跑。

## 7. 网络

### 7.1 两种能力，各管一段

| 层 | 机制 | 粒度 | 覆盖不到的 |
| --- | --- | --- | --- |
| 内核 | Landlock `NET_BIND_TCP` / `NET_CONNECT_TCP`（6.7+） | **端口**（不是地址） | 同端口上的不同主机无法区分 |
| 应用 | 宿主回环 HTTP 代理（复用 `proxy.rs`） | **域名**（可审批） | 不尊重 `HTTP(S)_PROXY` 的客户端 |

两者叠加后的语义（与 Windows 侧"能力 + 代理"完全同构）：

- 沙盒内裸 `socket()` 连接非白名单端口 → **内核直接 `EPERM`**，不是静默放行。
- 出网唯一实用通路是 `127.0.0.1:<proxy>`，于是域名判定落在代理里，策略改动不需重启。
- `CONNECT_TCP` 放行代理端口 + `BIND_TCP` 放行随机本地端口，否则本地 dev server 与
  代理通路双双失效。**`offline` 是例外**：它连代理端口也不放行（§7.2）。
- **`BIND_TCP 0` 是必须的**：客户端连代理时内核要为 socket 自动 bind 一个临时端口，
  不放行 `bind 0` 则连"连代理"这一步都做不到。

### 7.2 四种策略如何落到端

沿用 `NetPolicy` 四个取值（`offline` / `ask` / `allowlist` / `online`），Linux 侧的落法：

| 策略 | Landlock 端口规则 | 代理 | 严格性 |
| --- | --- | --- | --- |
| `offline` | **不放行任何端口**；ABI ≥ 10 时连 UDP 一起管（只放行 `bind 0`） | 不需要（不起） | **严格**：内核级，连回环 TCP 也一起拒 |
| `ask` | 只放行回环代理端口 | 域名级审批，会话内记住 | 端口层只剩代理一门；域名判定在代理（应用层） |
| `allowlist` | 只放行回环代理端口 | 仅白名单域名放行 | 同 `ask` |
| `online` | 全放行（`handled_access_net = 0`，即不管网络） | 仅记录 | 不设边界 |

**端口层为什么只放行代理端口**：`curl`/`pip`/`npm`/`git`/`cargo` 在拿到 `HTTP(S)_PROXY` 后
只连代理端口，把目标主机名交给代理去解析；而"忽视代理变量、直连 443"的客户端会被
`connect()` 直接 `EPERM`。这比"放行 443 再靠应用层自律"严格一整个层级，且不需要额外机制。
代理拉不起来时端口集合自然为空 —— 失败方向是关闭。

**`offline` 必须说清的一件事实**：Landlock 的端口规则**不含地址**，所以"不放行任何端口"
意味着连 `127.0.0.1:8080` 也连不上。早期文案写成"只允许回环"是错的，已在
`Policy::describe` 与测试里改正。

**结论要如实写进文档与 `/sandbox`**：`offline` 是**内核级**严格；`ask`/`allowlist` 的
端口层也是内核级，但**域名判定在代理里**，属应用层。需要严格不出网就用 `offline`。

**`ask` 的实现状态（P5 已接通）**：宿主侧有网络审批站（`sandbox/net_approval.rs`），
代理线程在 `ask` 下把目标交给它，由界面弹面板问一句。三条失败方向都是关闭：
用户拒绝、超时（`[sandbox.net] approval_timeout_seconds`）、**没有界面接管**。

没有界面时**立即拒绝**而不是空等超时：无人值守入口（本地 API、触发接口、子代理）
里没人能回答，空等只会让每条命令被拖住一整个超时，表现为"沙盒一开就卡死"。
交互开关是 `RegistryOptions::net_approval_interactive`，只有 TUI 传 `true`——
它必须随工具表重建一起传下去，因为改沙盒设置会换掉整个 `Sandbox`。
细节见 §22.2。

### 7.3 DNS

`ask`/`allowlist` 下**不管 UDP**（`control_udp = false`）：端口规则不含地址，管住会把
`127.0.0.53` 这类本地 DNS 桩一并打断，导致所有域名解析失败。因此这两档下 UDP 完全不受控，
`describe` 里如实标注"UDP 未受控"。

`offline` 在 ABI ≥ 10 时才管 UDP（`BIND_UDP`/`CONNECT_SEND_UDP`）：放行 `bind 0`（否则
socket 都建不起来），目标端口一个不放。ABI < 10 时如实标注"UDP 未受控（需 ABI ≥ 10）"。

### 7.4 环境注入（不做这一步，前三档全废）

沙盒进程的环境必须**清掉宿主代理变量再注入本机代理**：

```text
env_remove: HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY (及小写形式)
env:        HTTP_PROXY=HTTPS_PROXY=ALL_PROXY=http://127.0.0.1:<proxy>
            NO_PROXY=127.0.0.1,localhost,::1
            HOME=<sandbox_temp>/home   TMPDIR=<sandbox_temp>
```

两条都是硬需求：

1. **不清理**：宿主自己配了 `HTTP_PROXY` 时会被继承，沙盒内客户端直奔用户代理，
   我们的判定被整段跳过 —— `offline` 看起来"断网"实则畅通。
2. **不注入**：`ask`/`allowlist` 没有任何出网通路，等于断网。

`HOME` 必须指向沙盒内目录：真实家目录按策略不可读，留着只会让 `git`/`npm` 拿到一个
打不开的位置后行为诡异，甚至写配置时 `EACCES`。

## 8. 进程与资源约束

| 项 | v1 默认 | 手段 | 备注 |
| --- | --- | --- | --- |
| 进程组回收 | 始终 | `configure_process_group(0)` + `killpg` | 已有，`process_control.rs` |
| 进程数 | 64 | cgroup v2 `pids.max` | P5 接入；拿不到 cgroup 时不设（见下） |
| 内存 | 4096 MB | cgroup v2 `memory.max` | P5 接入；同上 |
| 文件大小 | 不设 | `RLIMIT_FSIZE` | 无特权可用 |
| CPU 时间/速率 | 不设 | `cpu.max` / `RLIMIT_CPU` | 沿用"避免误伤正常编译"的取舍（`cpu_rate_percent = 0`） |
| 墙钟超时 | 沿用现有 | `command.rs` 的 deadline | 已有 |

`RLIMIT_NPROC` **不用**：它按**用户**计数的，同 UID 的宿主进程会互相干扰。

cgroup v2 挂载点与写权限因发行版而异（`/sys/fs/cgroup` 在 systemd 下通常需要
delegation），因此 cgroup 是**可选加固**：拿不到就降级为"不设限额"并在诊断里写明，
**绝不因为拿不到限额就拒绝执行**（限额是可用性手段，不是安全边界；安全边界在 Landlock）。
落地细节与四个实现要点见 §22.1。

## 9. 可选加固（不进 v1 默认路径）

| 加固 | 前提 | 收益 | 何时做 |
| --- | --- | --- | --- |
| 空网络命名空间 | userns 可用 | 内核级断网（含 UDP、非 TCP 协议） | 有用户报告 `ask` 被绕时 |
| mount 命名空间 + overlayfs | userns 可用 | 沙盒内看不到工作区之外的挂载 | 同上 |
| cgroup v2 全限额 | delegation 可用 | 内存/进程数硬上限 | P2 之后按需 |
| seccomp 窄黑名单 | 无 | 挡 `unshare`/`mount`/`keyctl` | 内核 < 6.12 时补位 |

**userns 为什么会失效**（必须在文档里说清，否则实现者会踩）：Ubuntu 24.04+ 默认
`kernel.apparmor_restrict_unprivileged_userns=1`，无 AppArmor profile 的程序创建 userns 会被拒
（`bwrap`/`codex` 的经典报错）。因此**任何依赖 userns 的设计都不能作为默认路径**；
Landlock 不依赖 userns，这正是选它做主干的第二层理由。

## 10. 配置形状

沿用 `[sandbox]`，不新增段、不新增键（Linux 侧没有新的用户决策项）：

```toml
[sandbox]
enabled = false
# off | appcontainer | landlock
#   - "appcontainer"：仅 Windows；在 Linux 上会被拒绝（见 §10.1）
#   - "landlock"：Linux 真隔离；在 Windows 上会被拒绝
mode = "off"

[sandbox.fs]
writable = []          # 额外可写；相对路径按工作区根
readable = []          # 额外可读（解释器、SDK、证书目录）

[sandbox.net]
policy = "offline"     # offline | ask | allowlist | online
allow = []             # allowlist 时必填（已在 config 校验里强制）
deny = []
approval_timeout_seconds = 60
log_all = true

[sandbox.limits]
max_processes = 64     # Linux 侧只有 cgroup 可用时才生效
max_memory_mb = 4096
cpu_rate_percent = 0
```

### 10.1 `mode` 取值与平台绑定（P0 已落地，实际实现以此为准）

改动（最小）：

1. `SandboxMode` 增加 `Landlock` 变体，`as_str() -> "landlock"`，`parse` 接受
   `"landlock" | "bwrap" | "linux"`（后两个是便利别名，落到同一变体）；
   新增 `requires_isolation()`，`isolates()` 改为 `enabled && requires_isolation()`。
   **平台是否支持**不在配置层判定（配置层不知道运行平台，写了 `landlock` 却在 Windows 上跑，
   应由 gate 报错而不是配置报错）。
2. `gate.rs` 拆成三个 `platform_plan`（`cfg(windows)` / `cfg(target_os="linux")` / 其它）：
   模式与平台不符一律**报错**（`mismatch_message` 指出该平台该用哪一档）；
   Linux 分支先探测能力，再校验 `offline` 是否被内核网络控制支撑。
   P0 阶段 Linux 的可执行路径为**空**：探测通过后仍返回"launcher 属 P1，命令未执行"，
   而不是退回宿主身份——否则用户会以为命令在沙盒里跑过。
3. `gate.rs` 新增 `isolates()`（平台感知），并把 AppContainer 特有的取舍改用
   `Sandbox::appcontainer_active()`：`registry.rs::sandbox_tool_switches` 与
   `registry.rs`/`monitor.rs` 的 bash 拒绝点原本用平台无关的 `Sandbox::isolates()`，
   Linux 上配了 `appcontainer` 会把唯一可用的解释器踢出工具表。
4. `SANDBOX_MODES` 常量与三处展示文案（`/sandbox`、`/sandbox:set`、TUI 设置项
   `form.rs` 的 `SANDBOX_MODE_OPTIONS`）同步补上 `landlock`；
   `status_line()` 按模式分支（原实现硬编码 AppContainer 语义）。
5. `config.example.toml` 的 `[sandbox]` 注释补 Linux 段落，并按平台说明已落地范围。
6. `omnicrawl-host/Cargo.toml` 增加 `[target.'cfg(target_os = "linux")'.dependencies] libc`。

### 10.2 开关语义（三档，与 Windows 侧一致）

| `enabled` | `mode` | 行为 |
| --- | --- | --- |
| `false` | — | 与今天完全一致，零额外开销（`Sandbox::disabled` 路径不变） |
| `true` | `off` | dry-run：只判定并记录"本会拦什么" |
| `true` | `landlock` / `appcontainer` | 真隔离（按平台） |

dry-run 的判定逻辑 `Sandbox::check_read/check_write/check_target` 已经平台无关，
Linux 侧**直接复用**，`/sandbox` 的"本会拦什么"因此天然一致。

## 11. 一次 `bash` 调用的完整路径（Linux）

1. `command.rs::run_invocation` / `monitor.rs` → `gate.plan()`。
2. `gate` 在 `mode = landlock` 下取能力（`Sandbox::capability()`，惰性缓存）、
   按策略拉起宿主回环代理（`offline` 不起），调 `linux::Isolation::prepare` 折算策略。
3. 宿主把 argv 包装成 `[current_exe, "sandbox-exec", "--", <真实 argv…>]`。
4. `spawn` 走**宿主身份**的常规 `Command`（不需要特殊 API），并在
   `configure_process_group` 里自成一组；stdin 改为管道，spawn 后立刻写入策略并关闭。
5. launcher（`intercept` → `launcher::run`）按 §6.3 施加规则后 `execvp` 真正的命令。
   **进程是同一个**，因此 `child.id()` 就是最终命令的 pid，超时/取消/killpg 全部照旧工作。
6. stdout/stderr 由宿主照常读取、解码、按 `finish()` 组织；输出头部多一行
   `[sandbox:landlock] 本命令以隔离身份执行`（标记由 `gate.label()` 给出，随平台/模式变化）。
7. launcher 以非零退出码退出、stderr 带 `SANDBOX_UNAVAILABLE` / `SANDBOX_EXEC_FAILED` 前缀时，
   宿主把它组织成可读错误，让模型看到"沙盒没起来，命令未执行"，而不是一个含义不明的退出码。

与 Windows 侧的差别只在第 3–5 步（那边要自己调 `CreateProcessW` 并持有容器身份），
第 6–7 步的组织方式保持同构，否则两平台的行为会分叉。

## 12. 失败与降级

**唯一允许的降级是"拒绝执行"**：

| 失败点 | 行为 | 面向模型的文案 |
| --- | --- | --- |
| Landlock 不可用（`ENOSYS`/`EOPNOTSUPP`，如内核未启用或容器内含 `lsm=`） | 真隔离请求直接失败，命令不执行 | "内核不支持 Landlock（或已被禁用），沙盒无法启用；命令未执行。可改 `mode = off` 用 dry-run 观测。" |
| ABI 太低（< 4，无网络控制） | 仍执行，但**诊断里明确标注网络未受控**；若 `policy = offline` 则需 ABI ≥ 4，否则**拒绝**（不能宣称断网却没断） | 按情况二选一 |
| 规则里的路径不存在（如 `readable` 写错） | 视为配置错误，**拒绝执行**并指出是哪一条 | "sandbox.fs.readable 的 `xxx` 不存在" |
| 代理拉不起来 | 同 Windows 侧：不注入代理变量、按端口规则执行（失败方向为关闭） | 诊断里 `NetStatus::Failed` |
| launcher 自身 exec 失败 | 命令不执行，带可读原因 | `SANDBOX_EXEC_FAILED` |

第 3 行反例值得强调：**"路径写错就跳过"会让用户以为边界生效了，实际比不写更危险**。

## 13. 可观测性

- `/sandbox` 状态行按平台分支：Linux 打 `landlock(ABI N)` + 受管/未受管权限清单。
- dry-run 记录（`Sandbox::decisions`）继续工作，文案与 Windows 一致。
- launcher 失败一律 stderr + 退出码，且**不在 stdout 里混入**（会污染命令输出）。
- 一次执行的诊断摘要（模型可见）：ABI、是否 `no_new_privs`、网络策略的实际严格级别
  （内核级/应用级）、cgroup 是否生效。

## 14. 实现切分

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| **P0 ✅** | `SandboxMode::Landlock` + 平台分支报错 + 文案/示例同步；`sandbox/linux.rs` 的 ABI 探测与能力折算；`Sandbox::capability()` 缓存 | 现有 Windows 行为零变化；Linux 上 `mode=landlock` 能报出 ABI 或明确不支持（§17） |
| **P1 ✅** | `sandbox/linux/{policy,launcher}.rs`：策略折算、ABI 裁剪、fd 收口、no_new_privs、文件规则、`execvp`；`gate`→`Plan::Landlock`；`command.rs`/`monitor.rs` 经 stdin 传策略；TUI/API `main` 分流 `sandbox-exec` | 命令在沙盒内可跑；`cat ~/.ssh/id_rsa` 以 `EACCES` 失败（§18） |
| **P2 ✅** | 网络：Landlock TCP 端口规则（ABI ≥ 4）+ UDP 规则（ABI ≥ 10）；`handled_access_net` 按 ABI 裁剪；端口层只留代理一门；宿主代理变量的清理与本机代理注入（同步 + 后台两条路径） | `curl https://example.com` 在 `offline` 下 `EPERM`；在 `ask` 下经代理（无审批通道时按拒绝） |
| **P3 ✅** | `sandbox/linux/runtimes.rs`：从 `PATH` 反推安装根（nvm/rustup/pyenv），两道闸拒绝授凭据与 `$HOME`；`~/.rustup` 一并补授；依赖缓存重定向到沙盒内；launcher 加 `RLIMIT_CORE=0`；诊断补解释器根数量 | 沙盒内 `node -v`/`cargo --version` 能跑；`cat ~/.cargo/credentials.toml` 仍 `EACCES`（§20） |
| **P4 ✅** | `git` 接入沙盒：Linux 经 launcher 施加规则 + 代理注入 + 真超时（整组回收）；Windows **明确拒绝**（MSYS 依赖 `NUL`，实测不可用，绝不退回宿主）；`/sandbox` 新增"实际能力"（ABI/严格级别）与平台限制说明；`Policy` 补授只读 `~/.gitconfig` | Linux：`git status` 在沙盒内可跑、`git clone` 受限；Windows：沙盒模式下 `git` 报明确原因而非静默放行（§21） |
| **P5 ✅** | `sandbox/linux/cgroup.rs`：cgroup v2 的 `pids.max`/`memory.max`/`cpu.max`，launcher 在施加 Landlock 前迁入；拿不到即降级并在诊断里写明。`sandbox/net_approval.rs`：`ask` 档网络审批站（阻塞等待 + 界面队列 + 超时），接进 TUI 面板与按键 | `ask` 下沙盒命令出网时弹面板，`y` 放行后请求成功、`n`/超时/无界面一律拒绝；`/sandbox` 如实报告限额设了几项（§22） |

**顺带的实现纪律**（与 Windows 侧同源）：沙盒关闭时所有既有测试必须零变化，
`Sandbox::disabled` 路径不得多出任何系统调用。

## 15. 风险与待确认

1. **同 UID 可互杀**（§3 第 1 条）：这是 Linux 轻量方案的硬限制。要堵住需 userns
   （受发行版策略影响）或 root。需在 UI 文案里明说，避免用户以为"沙盒 = 完全隔离"。
2. **动态链接器的搜索路径**：`/etc/ld.so.cache` 与 `LD_LIBRARY_PATH` 的处理极易漏，
   漏了表现为"所有命令都是 command not found"。P1 的验收里必须有这一项。
3. **`/proc` 的粒度**：给只读会让沙盒进程看到宿主全部进程信息；不给则 `bash`/`ps`/部分
   运行时异常。倾向给只读并接受信息泄漏（见 §3 第 1 条），但要在文档里写明。
4. **musl 静态内核 + glibc 宿主载荷**：发布包同时存在两种 libc（见 `publish-npm.yml`）。
   launcher 属宿主载荷（gnu），`SYS_*` 常量在两种 libc 的 `libc` crate 里都已核对，
   但需在 CI 的 `ubuntu-latest` 上跑一次真实用例（当前 CI 已覆盖 Linux 测试矩阵）。
5. **ABI 差异下的测试矩阵**：CI runner 内核版本会漂移，测试必须"按探测到的 ABI 分支断言"，
   不能硬编码"网络规则一定生效"。

## 16. 一句话总结

用 **Landlock 做唯一必需的硬边界**（文件白名单 + 网络端口），用**宿主回环代理**补域名级审批，
用 **launcher 进程**换取"刚 fork、单线程、失败可读"的施加时机，其余（userns、cgroup、
seccomp）全部降级为可选加固——不引入新依赖、不需要 root、不需要镜像，代价是同 UID 进程间
干扰挡不住，这一点必须写在明面上。

## 17. P0 实施记录（2026-11-06）

### 落地内容

| 位置 | 改动 |
| --- | --- |
| `omnicrawl-config/src/features/sandbox.rs` | `SandboxMode` 加 `Landlock`（别名 `bwrap`/`linux`）+ `requires_isolation()`；`isolates()` 不再绑死 AppContainer；`SANDBOX_MODES` 补 `landlock`；`status_line()` 按模式分支，landlock 档如实写出同 UID 限制 |
| `omnicrawl-host/src/sandbox/linux.rs`（新） | Landlock ABI 探测（`syscall(SYS_landlock_create_ruleset, NULL, 0, VERSION)`）与 `Capability` 折算：`can_isolate` / `controls_network`(ABI≥4) / `controls_udp`(ABI≥10) / `check_network_policy` / `describe` |
| `omnicrawl-host/src/sandbox/mod.rs` | `mod linux`；`LinuxCapabilityCache`（探测一次、会话内复用）；`Sandbox::capability()`；`Sandbox::appcontainer_active()`（平台感知） |
| `omnicrawl-host/src/sandbox/gate.rs` | 三个 `platform_plan`（Windows/Linux/其它）+ `mismatch_message`；新增平台感知的 `IsolationGate::isolates()`；`Plan::Landlock` 变体**未加**（P0 无可执行路径） |
| `omnicrawl-host/src/tools/registry.rs`、`tools/monitor.rs` | bash 拒绝点与 `sandbox_tool_switches` 改用 `appcontainer_active()` |
| `omnicrawl-commands/src/slash/{sandbox_commands,registry}.rs` | `/sandbox`、`/sandbox:set` 文案与取值补 `landlock`，网络说明按模式分支 |
| `omnicrawl-tui/src/ui/settings/form.rs` | `SANDBOX_MODE_OPTIONS` 补 landlock 一项 |
| `omnicrawl-host/Cargo.toml` | 新增 `[target.'cfg(target_os = "linux")'.dependencies] libc = "0.2"` |
| `rust/assets/config-templates/config.example.toml` | `[sandbox]` 注释补 Linux 段落与平台绑定说明 |

### P0 的行为边界（已用测试钉住）

1. 沙盒关闭 / dry-run：`plan()` 恒返回 `Plan::Host`，无标记、无系统调用。
2. 配置要求隔离但**平台不对**（Linux 上写 `appcontainer`、Windows 上写 `landlock`）：
   `plan()` 返回可读错误，指出该平台应改用哪一档；`isolates()`（gate）为 `false`。
3. Linux 上 `mode = landlock`：
   - Landlock 不可用 → 报 `ENOSYS`/`EOPNOTSUPP` 归类原因；
   - `policy = offline` 但 ABI < 4 → 拒绝（不能宣称内核级断网）；
   - 能力足够 → 仍报"launcher 属 P1，命令未执行"。**任何情况下都不退回宿主身份。**
4. `appcontainer_active()` 在 Linux 上恒为 `false`：`bash` 不会被误踢出工具表。

### 验证

按项目约定**未跑编译类命令**。静态自查覆盖：`SandboxMode` 全部 match 点、`Plan` 各 cfg 组合的
穷尽性、`libc::syscall` 变参宽度、`appcontainer_active` 的全部调用点。

### 已知限制（P0 的本分）

- Linux 上 `mode = landlock` 必然拒绝执行（无 launcher），这是刻意的失败方向。
- `Sandbox::capability()` 只有 Linux 目标存在，Windows 目标不编译该字段。
- launcher、文件规则、网络端口规则均属 P1/P2，本阶段未实现。

## 18. P1 实施记录（2026-11-06）

### 落地内容

| 位置 | 改动 |
| --- | --- |
| `sandbox/linux/policy.rs`（新） | 策略模型：`Access` 三档、`PathRule`、`handled_fs(abi)` 裁剪、`Policy::build/validate/existing`、`system_read_paths`/`runtime_read_paths`/`runtime_write_paths` |
| `sandbox/linux/launcher.rs`（新） | `run()`：读策略 → 关继承 fd → 开规则 fd → `PR_SET_NO_NEW_PRIVS` → `create_ruleset` → `add_rule` × N → `restrict_self` → `execvp`；`encode_policy`/`read_policy` |
| `sandbox/linux/mod.rs` | `Isolation::prepare/wrap/command/policy_text`、`intercept()`（argv 分流）、P0 探测保留 |
| `sandbox/gate.rs` | Linux 分支真正返回 `Plan::Landlock`；`Plan` 增加该变体 |
| `tools/command.rs` | Linux 包装 argv + 经 stdin 写策略；`finish` 标记改用 `gate.label()` |
| `tools/monitor.rs` | 同上的后台版本；`spawn_background_with_policy` |
| `crates/omnicrawl-tui/src/main.rs`、`crates/omnicrawl-api/src/main.rs` | 最先分流 `sandbox-exec` |

### 三个必须记住的实现陷阱（都已在代码注释里）

1. **fd 关闭顺序**：`close_inherited_fds()` 必须在 `open_rule_fds()` **之前**。反过来会把刚打开的
   规则 fd 一并关掉，`add_rule` 拿到无效 fd → 整条路径静默失效。
2. **`format!` 相邻花括号**：`"{CONST}EXEC_FAILED"` 不会展开常量，会原样输出字面量。
   前缀一律直接写字面量（`"SANDBOX_EXEC_FAILED …"`）。
3. **变参宽度**：`libc::syscall`/`libc::prctl` 是 variadic，Rust 不做 C 的默认参数提升，
   实参必须显式 `as libc::size_t` / `as libc::c_uint` / `as libc::c_ulong`。

### 两处设计上的取舍

- **没有给 `Plan` 加"退回宿主"的降级分支**：launcher 任何一步失败都只是退出码 + stderr，
  宿主把它组织成可读错误。方向永远是"跑不了"而不是"没隔离地跑"。
- **`/dev/null` 必须可写**（`runtime_write_paths`）。Windows 侧已经因为 AppContainer 拿不到
  `NUL` 设备权限而整体起不来；Linux 这侧 `> /dev/null` 同样是最常见写法，只读会让大量脚本
  以 `EACCES` 失败。

### 验证说明

按项目约定未跑编译类命令（`cargo check` 亦未跑）。做的是静态审查：全部 `Plan` 匹配点的
cfg 穷尽性、`Arc<Isolation>` 借用与解引用、`Access`/`Policy` 导入、`close_range`
与 `getrlimit` 的 libc 类型差异、`strip_prefix` 切片用法（已用独立 `rustc` 验证）。

## 19. P2 实施记录（2026-11-06）

### 落地内容

| 位置 | 改动 |
| --- | --- |
| `sandbox/linux/policy.rs` | `Policy` 增加 `connect_udp_ports`/`bind_udp_ports`；**所有非 `online` 档的 TCP 端口集合收紧为「只有代理端口」**（原来 `ask`/`allowlist` 还放行 80/443/53）；`control_udp` 只在 `offline` + ABI ≥ 10 时成立；`describe` 重写，如实报出"端口规则不含地址""域名判定在应用层""UDP 受控/未受控" |
| `sandbox/linux/launcher.rs` | `handled_net(abi, control_udp)` 按 ABI 裁剪网络位；`run()` 的第 ⑥ 步落 `CONNECT_SEND_UDP`/`BIND_UDP` 规则；策略行格式新增 `connect-udp`/`bind-udp` |
| `sandbox/linux/mod.rs` | `Isolation` 增加 `proxy_url`/`home`/`temp`；新增 `apply_environment()`：`env_remove` 八个代理变量 + 注入本机代理（大小写各一套）+ 改写 `HOME`/`TMPDIR` |
| `sandbox/gate.rs` | Linux 分支注释改准；测试改用真实可写临时根（准备阶段会建 `HOME`） |
| `tools/command.rs` | 同步路径 `isolation.apply_environment(&mut command)`；**顺手修掉一处 P1 遗留的 cfg 缺陷**（见下） |
| `tools/monitor.rs` | `spawn_background_with_policy` 增加 `configure` 闭包参数；后台路径同样注入环境 |

### 三个必须记住的实现陷阱

1. **`handled_access_net` 必须按 ABI 裁剪**。把 ABI 10 的 `BIND_UDP`/`CONNECT_SEND_UDP`
   传给 ABI 4 的内核，`create_ruleset` 直接 `EINVAL` —— 整条命令都跑不了，比"UDP 未受控"
   严重得多。这与 `handled_fs` 裁剪 `REFER`(ABI 2)/`TRUNCATE`(ABI 3) 是同一类纪律。
2. **目录必须在折算策略之前创建**。`Policy::existing()` 会丢弃不存在的路径，而沙盒临时
   目录通常还不存在 —— 顺序反了会把"可写临时目录"整条规则**悄悄丢掉**，表现为沙盒内
   写临时文件全部 `EACCES`。`Isolation::prepare` 现在先建 `HOME` 再折算。
3. **`Plan::Landlock` 只在 Linux 目标存在**。`command.rs` 里原来用
   `#[cfg(not(windows))]` 匹配它，在 macOS 等目标上会引用不存在的变体 → 编译失败。
   已改为 `#[cfg(target_os = "linux")]` + `#[cfg(not(any(windows, target_os = "linux")))]`
   两套。

### 两处刻意的语义收紧（比原设计更严）

- **`ask`/`allowlist` 不再放行 80/443/53**。这两个档的全部价值在"域名判定"，而放行 443
  等于给直连开了后门；只留代理端口后，"忽视代理变量"的客户端在内核层就被拒，域名判定
  不再是唯一防线。
- **`offline` 在 ABI ≥ 10 时连 UDP 一起管**，放行 `bind 0` 让 socket 能建立，目标端口全拒。

### 如实记录的两条限制

- **`ask` 目前等同全拒**：`ensure_network` 的两处调用都传 `None`，代理的既有语义是
  "没有审批通道按拒绝处理"。这是安全的失败方向，但不是 P2 承诺的"域名级审批"；
  接线到 TUI/API 审批流程属后续工作。
- **`offline` 连回环 TCP 也拒**：Landlock 端口规则不含地址，无法只放行 `127.0.0.1`。
  需要本地 dev server 的场景只能用 `ask`/`allowlist`/`online`。

### 验证说明

按项目约定未跑编译类命令。静态核对：`Policy` 的 5 处结构体字面量是否都补齐新字段、
`handled_net` 在各 ABI 的取值、`plan` 的 cfg 覆盖（Windows / Linux / 其它）、
`get_envs` 在 `env_remove` 后返回 `Some(None)` 的语义（已按此写断言）。

内核常量取自 `include/uapi/linux/landlock.h`：`BIND_TCP`=bit0、`CONNECT_TCP`=bit1（ABI 4）、
`BIND_UDP`=bit2、`CONNECT_SEND_UDP`=bit3（ABI 10）。

## 20. P3 实施记录（2026-11-06）

### 为什么必须做

`system_read_paths()` 只列 `/usr`、`/usr/local`、`/opt`。而现代工具链基本都装在 `$HOME` 下：

| 安装方式 | 可执行文件 | `system_read_paths()` 覆盖？ |
| --- | --- | --- |
| nvm | `~/.nvm/versions/node/v20/bin/node` | ❌ |
| rustup | `~/.cargo/bin/cargo`（shim） | ❌ |
| pyenv | `~/.pyenv/versions/3.12.0/bin/python3` | ❌ |

Landlock 是白名单语义，没列出等于拒绝。因此 P2 之后，沙盒里 `node`/`cargo`/`python`
表现为**不存在**——用户看到的是"命令找不到"，完全不知道是沙盒挡的。这比"被拒绝"更糟：
没有可诊断的信号。

### 但绝不能因此授 `$HOME`

`~/.ssh`、`~/.npmrc`（含 auth token）、`~/.git-credentials`、`~/.cargo/credentials.toml`
（crates.io 发布令牌）全在下面。Windows 侧 `runtime.rs` 已确立同一原则：
"授它等于把隔离拆了"。

### 落法：`sandbox/linux/runtimes.rs`

从 `PATH` 里找可执行文件 → 反推安装根 → 经两道闸决定授不授：

```text
可执行文件           →  安装根（仅当父目录名为 bin/）
~/.nvm/.../v20/bin/node  →  ~/.nvm/.../v20
~/.cargo/bin/cargo       →  ~/.cargo
```

**闸 1：安装根必须严格在 `$HOME` 之下，且不能等于 `$HOME`。**
`~/bin/node` 的根会是 `$HOME` 本身 —— 这种情况直接放弃授权。爬过头的代价是
把整个家目录（含私钥）放出去，比"node 跑不起来"严重得多。

**闸 2：安装根若直接含凭据文件，就不授它本身，改授安全子目录。**

| 安装根 | 直接子项有凭据？ | 结果 |
| --- | --- | --- |
| `~/.nvm/.../v20` | 否 | 整目录只读执行 |
| `~/.pyenv/versions/3.12.0` | 否 | 整目录只读执行 |
| `~/.cargo` | **是**（`credentials.toml`） | 只授 `bin`、`registry`、`git` |

敏感名单只在**直接子项**里查一层，不递归：凭据总在固定位置，递归扫描既慢又无收益。
读不到目录内容时按"有凭据"处理（宁可少授权，也不要在看不见时放行）。

### 两个让"安全"反噬可用性的坑（都已修）

1. **`~/.rustup` 必须一并只读授权**。`~/.cargo/bin/cargo` 是 rustup 的 shim，它靠
   `RUSTUP_HOME`（默认 `~/.rustup`）找 `settings.toml` 与 toolchain。只授 `~/.cargo` 的
   子目录会让 cargo 报 "no toolchain installed" —— 为了安全把 Rust 整个弄坏。
   `~/.rustup` 不含凭据（`settings.toml` 只是配置），按只读补上。
2. **`RUSTUP_HOME` 不能重定向**。最初我把它和 `CARGO_HOME` 一起指向沙盒内缓存，
   同样是 "no toolchain installed"。`CARGO_HOME` 只管数据目录（registry/git 缓存），
   重定向它是安全收益；`RUSTUP_HOME` 决定 toolchain 位置，必须保持宿主值。

### 依赖缓存重定向（不授宿主缓存可写）

`cargo build`/`npm install`/`pip install` 都需要**可写**的缓存位置，否则在建缓存时 `EACCES`。
但把 `~/.cargo` 这类真实缓存改授可写，等于允许沙盒污染宿主的依赖树（供应链风险，
比"构建失败"严重）。因此把它们重定向到沙盒内目录：

```text
CARGO_HOME=     <sandbox_temp>/cache
npm_config_cache=<sandbox_temp>/cache
PIP_CACHE_DIR=  <sandbox_temp>/cache
XDG_CACHE_HOME= <sandbox_temp>/cache
```

### 资源约束：只加 `RLIMIT_CORE=0`

launcher 在施加规则前调用 `setrlimit(RLIMIT_CORE, 0)`：崩溃时内核不再把进程内存写盘。
`core_pattern` 可能指向工作区或其它可写目录，沙盒读过的源码会整块落进 core 文件。
无特权可用，且只把"崩溃取证"降级为"没有 core"，不影响任何正常命令。

**其余限额（CPU/内存/进程数）仍未接入，如实说明**：`cgroup v2` 在 systemd 下的 delegation
因发行版而异，`RLIMIT_NPROC` 按**用户**计数（同 UID 的宿主进程会互相干扰）。
按 §8 的原则，限额是可用性手段不是安全边界，拿不到就不设，**绝不因此拒绝执行**。
诊断里不宣称已设限额。

### 配置面变化

无。`[sandbox]` 不新增键：解释器发现是自动的，用户仍可用 `[sandbox.fs] readable`
显式补漏（例如装在 `$HOME` 之外的自定义 SDK）。

### 验证说明

按项目约定未跑编译类命令。静态核对：`Policy` 的 5 处结构体字面量补齐 `install_roots`、
`Policy::build` 的 7 处调用补齐 `env` 参数、`runtimes` 模块的无用导入与 `Path` 用法。
新增单测覆盖：nvm/pyenv 授权、`~/.cargo` 拆分、`~/.rustup` 补授、
安装根不得爬到 `$HOME`、系统根跳过、重复 `PATH` 折叠、解释器根进入 `fs_rules`。

## 21. P4 实施记录（2026-11-06，`git` 接入与 `/sandbox` 如实展示）

### 目标与结论

P1–P3 只有 `bash`/`powershell`/`monitor` 三条执行路径接了沙盒，`git` 工具一直在宿主身份下跑
（`git.rs` 的 spawn 点没有 gate）。P4 把它接上，并让 `/sandbox` 说出**实际**能力。

| 平台 | 做法 | 理由 |
| --- | --- | --- |
| Linux | 真正进沙盒：经 `sandbox-exec` 施加文件与网络规则，注入代理变量 | `git` 是普通 ELF 可执行文件，Landlock 对它没区别 |
| Windows | **明确拒绝**并给出原因 | MSYS 程序在 AppContainer 内打不开 `NUL`，`sandbox_windows_design.md` §18 已实测 |

**Windows 这条最容易做错**：想让"沙盒模式下 git 也能用"，就会退化成"在宿主身份下执行"。
那等于让最该被约束的操作（`push`、`config`、`submodule`）完全不受约束——比"跑不了"危险得多。
因此拒绝路径带专门的测试断言。

### 落地内容

| 位置 | 改动 |
| --- | --- |
| `tools/git.rs` | `git_tool` 增加 `gate: Option<&IsolationGate>`；拆出 `run_git` 的 Linux / Windows 两个实现与共享的 `finish_git` |
| `tools/registry.rs` | `"git"` 分支传入 `IsolationGate` |
| `sandbox/linux/policy.rs` | 补授只读 `~/.gitconfig` 与 `~/.config/git` |
| `commands/agent.rs` | 新增 `CommandAgent::sandbox_diagnostic()`（默认空） |
| `tui/app/host_commands.rs` | 新增 `sandbox_capability_note()`，取宿主 `Sandbox::capability()` |
| `commands/slash/sandbox_commands.rs` | 展示"实际能力"；Linux 说明 `git` 可用；Windows 说明 `git`/`bash` 不可用 |
| `tui/tests/search_tools_parity.rs` | 调用点补 `None` |

### 三个必须记住的坑

1. **`HOME` 指进沙盒后 `git` 缺身份**。`commit` 会报 "Please tell me who you are"，因为
   `~/.gitconfig` 不可读。解法是**单独只读授权配置文件本身**，绝不授 `$HOME`
   （里面有 `~/.ssh`、`~/.git-credentials`）。
2. **`Command::spawn` 不会自动接管 stdout/stderr**。原代码用 `command.output()` 才隐式 piped；
   改成手动 `spawn` + 读线程后必须显式 `.stdout(piped).stderr(piped)`，否则 `child.stdout`
   为 `None`，读线程什么都收不到——表现为"git 执行成功但没有任何输出"。
3. **读管道必须先起线程再等退出**。管道缓冲区写满时 `git` 会阻塞在 `write` 上，先 `wait`
   就是死锁。这与 `command.rs` 的做法一致。

### 顺带修掉的既有缺陷

`git` 原来完全**没有超时**：`let _ = Duration::from_secs(GIT_COMMAND_TIMEOUT_SECONDS);` 只是
把常量读出来扔掉，`command.output()` 会无限等待。`git push` 卡在网络上会一直占着这条工具
调用。P4 让它真正生效：`configure_process_group` 让 git 自成一组，超时后
`kill_process_tree_by_pid` 整组回收（钩子、凭据助手一并终止）。两个平台语义一致。

### `/sandbox` 的"意图 vs 实际"

配置表达用户想要什么，`CommandAgent::sandbox_diagnostic()` 表达本机内核/平台真能做到什么。
两者分开显示，否则 `/sandbox` 会在"声称已隔离"和"其实做不到"之间说谎。命令层拿不到
内核能力（也不该在命令层发 `landlock` syscall），因此这个通道由宿主实现，`commands` crate
不新增对 `omnicrawl-host` 的依赖。

### 验证

按项目约定未跑编译。静态核对：`git_tool` 全部 16 处调用点补齐第三个参数、6 个改动文件
括号配平、`Policy::build` 的调用点全部带 `env`。

新增单测：沙盒关闭时 git 照常执行且参数校验仍在沙盒判定之前；`~/.gitconfig` 只读授权且
`$HOME` 未被授权；Windows 沙盒模式下 git 明确拒绝而非退回宿主。

## 22. P5 实施记录（2026-11-06，cgroup 资源限额 + `ask` 网络审批）

两项一起做，因为它们回答的是同一类问题：**沙盒说了要做到、但实际没做到的事**。
限额「静默不生效」与 `ask`「静默全拒」都不会报错，只会让沙盒的说明与实际不符——
这正是这个项目的核心失败模式。

### 22.1 cgroup v2 资源限额（§8 的落地）

`SandboxLimits` 的三个字段（`max_processes` / `max_memory_mb` / `cpu_rate_percent`）
在 P0 就已经解析进配置，但从未有任何代码读取它们——配置里写了、状态行也照着念，
实际一个限额都没设。本轮接上，落法是 `sandbox/linux/cgroup.rs`：

| 项 | cgroup 文件 | 说明 |
| --- | --- | --- |
| 进程数 | `pids.max` | 取 `limits.max_processes` |
| 内存 | `memory.max` | 取 `max_memory_mb` × 1 MiB |
| CPU 速率 | `cpu.max` | `<百分比×1000> 100000`，0 表示不设 |

四个设计要点，每条都有具体理由（**前两条是在真机上推翻初版后重写的**）：

1. **不能建在本进程所在的 `session-N.scope` 里，要用它的父级**（通常是
   `user.slice/user-0.slice`）。真机（Debian 13 / 内核 7.0 / systemd）实测对照：

   | 位置 | 建子目录 | 写 `pids.max` | 写 `memory.max` | 迁入 |
   | --- | --- | --- | --- | --- |
   | `session-N.scope`（本进程所在） | 可以 | **EPERM** | **EPERM** | 可以 |
   | `user-0.slice`（scope 的父级） | 可以 | 可以 | 可以 | 可以 |

   原因：`session-N.scope` 是 systemd 的 leaf cgroup，其 `cgroup.subtree_control`
   不可写，于是子目录永远长不出限额文件。初版把 cgroup 建在 scope 下，`pids.max`
   写"成功"了（实际没有这个文件）、20 个进程照样起得来——**一次自我感觉良好的假成功**。
   因此资格判定不能只看"能否建目录"，必须**真去写一个限额文件**再回读（`can_write_limits`）。
2. **由 launcher 自己写 `cgroup.procs`**。cgroup 归属由写入那一刻决定；宿主在 fork 后
   再写就晚了——子进程可能已经 `execvp` 并 fork 出后代，那些后代落在别的 cgroup 里，
   限额形同虚设。因此策略文本多一行 `cgroup <路径>`，launcher 在施加 Landlock **之前**
   迁入（§6.3 的第 ③' 步）。真机验证：迁入后 fork 到第 8 个（`pids.max=8`）回
   `EAGAIN`；不迁入则 20 个全过。写入 `cgroup.procs` 后还要回读 `/proc/self/cgroup`
   确认，不能只信写入返回值。
3. **名字取宿主 pid，同名复用**。一个会话最多一个沙盒 cgroup。这同时消掉了两类缺陷：
   并发命令各建一个（额度被摊薄）与"忘了清理"（长会话攒一堆空 cgroup）。
   代价是同会话的多条沙盒命令共用一份额度——这正是"整体限额"的语义。
4. **拿不到就降级，绝不让命令失败**（§8 原则）。三种降级都会如实写进诊断：
   控制器未委派、父目录不可写、`/sys/fs/cgroup` 不是 v2。

launcher 迁入失败时在 stderr 打 `SANDBOX_LIMITS_SKIPPED`，**不改变退出码**：
限额是可用性手段，不是安全边界，因为它失败而让命令跑不了是本末倒置。

### 22.2 `ask` 档的网络审批（§7.2 的落地）

P2 时 `ensure_network` 两处调用都传 `None`，而 `proxy.rs` 的既有语义是"没有审批通道
按拒绝处理"，于是 `ask` 与 `offline` 在行为上完全等价——配了 `ask` 的用户会发现
所有出网都被拒。本轮把审批通道接上，新增 `sandbox/net_approval.rs`。

**为什么不是复用工具审批面板**（这是本轮最关键的判断）：

| | 工具审批（`Waiting::Approval`） | 网络审批 |
| --- | --- | --- |
| 谁发问 | 模型，经工具批次 | 代理线程，由沙盒内某条命令的连接触发 |
| 生命周期 | 挂在批次上，随批次推进出现、回答后消失 | 与批次无关，可能在回合任何时刻出现，一条命令可多次触发 |
| 能否延后 | 可以（批次停在原地等） | 不能（客户端正卡在 `CONNECT` 上等 `200`/`403`） |

因此它有自己的面板、自己的按键通路（只认 `y`/`n`/`Esc`，其余键放行给输入框——
用户可能正在打字，网络审批不该像工具审批那样吞掉全部输入）。

**线程模型**：代理线程在 `ask_with_timeout` 里用 `Condvar` 阻塞（最多
`[sandbox.net] approval_timeout_seconds`），UI 线程每帧 `tick_net_approvals` 取队列、
回答时写回编号；阻塞方不碰任何 UI 状态，UI 也从不等待代理。

**三条失败方向都是关闭**：用户拒绝、超时、没有界面接管。

**没有界面时立即拒绝，而不是空等超时**——这条是刻意的。无人值守入口（本地 API、
触发接口、`RegistryOptions::default()`）里没人能回答问题，空等只会让每条命令
被拖住一整个超时，表现为"沙盒一开就卡死"。交互开关因此是
`RegistryOptions::net_approval_interactive`，**不是**在 Sandbox 上一次性设置：
设置面板里改沙盒会重建整个工具表与 `Sandbox`，设在实例上的开关会在下次重建后丢失，
症状是"改完设置 ask 又变回全拒"。只有 TUI 传 `true`。

### 22.3 两个既有缺陷（本轮才发现）

这两个都在 `#[cfg(target_os = "linux")]` 的测试代码里，此前**从未在 Linux 目标上
编译过**，因此一直没人发现：

1. `runtimes.rs` 的 `env_of(pairs: Vec<(&str, String)>)` 返回 `impl Fn`，闭包捕获了
   匿名生命周期而返回值没有约束（`E0700`）。改成 `Vec<(&'static str, String)>`。
2. `launcher.rs` 的测试用 `crate::sandbox::linux::launcher::CHILD_POLICY_ENV` 引用
   同模块常量，多写了一层（`E0425`）。改为直接引用。

另外 `gate.rs` 一处测试用了 `Plan::expect_err`，而 `Plan` 没有 `Debug`；改为
`match` 取出错误，比为此给执行计划加 `Debug` 更合适。

### 22.4 验证

本轮**实际编译并运行了测试**，且找到了真正能校验 Linux 专有代码的办法：

- Rust 的 `x86_64-unknown-linux-musl` 标准库已装，`cargo check` 不需要链接器，
  但 `ring` 的 build script 需要 Linux C 编译器。用 **zig 作交叉 CC** 解决
  （`zig cc -target x86_64-linux-musl`，包一层 `.cmd` 因为 Windows 无法把 shell 脚本
  当 CC 执行）。此前 `ring` 还因 cc 传入了 Rust 三元组而报
  `UnknownOperatingSystem`，包装脚本里把它滤掉即可。
- 结果：`cargo check -p omnicrawl-host --target x86_64-unknown-linux-musl --tests`
  **通过**——这是本项目第一次真正类型检查 Linux 侧代码与它的 `#[cfg(test)]`。
- 运行侧：`cargo test -p omnicrawl-host --lib sandbox::` 73 passed；`-p omnicrawl-tui --lib`
  534 passed；新增单测覆盖审批站的五种情形（无界面立即拒绝、回答唤醒阻塞方、超时按拒绝、
  撤销释放等待方、未知编号无操作）、cgroup 摘要如实性、`cgroup` 策略行往返、
  TUI 面板高度与共存。

**如实说明验证边界**：交叉编译只做到"类型检查通过"，Linux 测试二进制在 Windows 上
跑不了（链接阶段缺 musl 的 crt 对象）。因此上面那次验证**不足以**判定 cgroup 真的能用；
后续已在真机上补做，见 §22.5——并在那里推翻了本节的两条结论。

### 22.5 真机验证（Debian 13 / 内核 7.0.0 / systemd）

交叉编译之后在一台 Debian 虚机上实跑，**结果推翻了 §22.1 初版的两条设计**，
并暴露一个与沙盒无关的测试缺陷。这些是交叉编译永远查不出来的东西。

**环境**：内核对 Landlock 上报 **ABI 8**（`landlock_create_ruleset(NULL, 0, VERSION)`），
`landlock` 在 `lsm` 列表里；`apparmor_restrict_unprivileged_userns=1`（§9 那条 userns
隐患果然存在）；cgroup v2 统一挂载，根启用 `cpu memory pids`。

**1. cgroup 建在 scope 下是错的**（推翻 §22.1 第 1 条）

`session-N.scope` 能建子目录、能迁入，但 `pids.max` 写入 `EPERM`。初版据此报告
"已设 pids.max=8"，而实际那个文件根本不存在，20 个 `sleep` 全部起来。
改到父级 `user.slice/user-0.slice` 后立即正常。

**2. 判定资格必须真写一次限额文件**

`can_create_child`（只建目录再删）在 leaf scope 上返回 `true`，于是初版"资格检查通过"。
改为 `can_write_limits`：建临时子 cgroup，真写一个限额文件再读回。

**3. 限额真的生效（真机实测）**

| | 裸跑 | 沙盒内（`pids.max=8`） |
| --- | --- | --- |
| fork 20 个 | 20 个全成功 | **7 个成功，第 8 个 `EAGAIN`** |

`memory.max` 也确认写入 `268435456`。端到端还验了 launcher 路径：宿主把 `cgroup <路径>`
写进策略、launcher 在施加 Landlock 前迁入，子进程从第 8 个起 `EAGAIN`。

**4. `ask` 审批四种情形全部符合预期**

真实代理 + 真实 TCP 客户端（不是 mock）：

| 情形 | 结果 |
| --- | --- |
| 没有界面接管 | `403`，耗时 **276µs**（远小于 3 秒超时，即"立即拒绝"） |
| 有界面、批准 | `200`，请求真的出去了 |
| 有界面、拒绝 | `403` |
| 有界面、没人回答 | `403`，耗时 **3.001s**（正好超时） |

**5. Landlock 的文件边界（真机实测）**

区外目录的读、写、建、删、改名全部 `EACCES`；`/root/.bashrc` 读失败；工作区内读写正常。
**一条真实边界**：`chmod` 不受限制——内核头里 `LANDLOCK_ACCESS_FS_*` 没有对应位
（最新只到 `IOCTL_DEV`）。这是 Landlock 的设计边界，不是缺陷，但文档里得写明。

**6. 一个既有的测试缺陷：在测试进程里调 `run()`**

`missing_rule_path_fails_before_touching_kernel_state` 直接在测试二进制内调用
`launcher::run()`，而 `run()` 第一件事是 `close_inherited_fds()`——把本进程 fd 3 以上
全关掉，包括 libtest 的捕获管道。症状：**整套测试跑完后**（610 个里 609 个已通过）以
`fatal runtime error: IO Safety violation: owned file descriptor already closed` 中止。
并行必现，`--nocapture` 与 `--test-threads=1` 不复现——这也是为什么它藏了这么久。
改为拉起子进程执行，与相邻的 `applies_rules_and_executes_a_real_command` 同构。

**最终结果**：真机上 `cargo test -p omnicrawl-host --lib` **610 passed / 0 failed**
（并行连跑三次稳定），`sandbox::` 99 passed。

**验证环境已清理**：虚机上的源码副本、构建产物、rustup 工具链（装在 `/tmp/oc`）、
42 个 `/tmp/oc-review-watch-*`、`/root/.OmniCrawl`、`/root/.omnicrawl`、以及全部
测试 cgroup 残留都已删除并逐项复核（`/tmp` 回到 4.0K，磁盘 36%，cgroup 残留 0）。

