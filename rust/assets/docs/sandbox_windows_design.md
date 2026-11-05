# Windows 沙盒模式设计方案

> 状态：设计稿（未实现）。目标读者：后续实现者。
> 平台：Windows 10 1809+ / Windows 11（AppContainer 与 Job Object 全量可用）。
> 依赖结论：`windows-sys 0.52` 已含本方案全部所需符号，**无需新增依赖**（详见「技术可行性」）。

## 1. 要解决什么问题

OmniCrawl 让模型在本机执行命令、读写文件、访问网络。当前只有**审批**（`manual`/`review`/`auto`），
审批拦的是"要不要做"，拦不住"做完之后还能做什么"：

- 模型跑 `npm install`，某依赖的 postinstall 脚本读走 `~/.ssh/id_rsa` 并外发——审批看到的是
  "执行 npm install"，看不到后续动作。
- 模型写一个脚本，脚本里再 fork 子进程、连内网 `192.168.x.x`。审批只审了第一层。
- 一次误操作的毁伤面是全用户：`C:\Users\Administrator` 整个可写。

**沙盒模式要给的保证**：即使模型（或它拉起的任意后代进程）完全失控，破坏面被限制在
「工作区可写 + 显式批准的网络目标」内，且宿主进程自身不受影响。

## 2. 非目标

- **不是**防恶意代码的虚拟机。同用户态内核 API（如 `NtQuerySystemInformation`）仍可被探测；
  沙盒防的是**误伤放大**与**常规提权/横向移动**，不是 APT。
- **不是**替换审批。沙盒与审批是两层：沙盒是硬边界，审批是边界上的门。
- **不**试图沙盒化宿主自身（`omnicrawl-host` 及其 crate）。沙盒只作用于**工具拉起的子进程**。
- **不**覆盖 GUI 工具（`windows_control`/`windows_input`）。它们操作桌面上已存在的进程，
  不属于子进程沙盒范畴——但要在沙盒开启时**拒绝**，见 §9。

## 3. 威胁模型与保证

| 攻击/误用 | 沙盒前 | 沙盒后 |
| --- | --- | --- |
| 脚本读 `~/.ssh`、`~/.OmniCrawl/config.toml` | 可读（明文 API key 就在里面） | 读不到（ACL 不含沙盒 SID） |
| 脚本写 `C:\Windows`、其他项目目录 | 可能成功（取决于 ACL） | 写不进 |
| 脚本外连 attacker.com 上传数据 | 直接出网 | 被拦，需审批 |
| 脚本连内网 `192.168.1.1` | 直接出网 | 被拦（`privateNetworkClientServer` 未授予） |
| 脚本 fork 后代逃逸 | 后代继承一切 | 后代继承同一 AppContainer + 同一 Job，无法脱离 |
| 脚本吃满 CPU/内存 | 拖垮整机 | Job 限额，超限即杀 |
| 宿主崩溃后子进程残留 | Job kill-on-close 已解决 | 沿用同一机制 |

**明确不给的**：内核态提权（AppContainer 无法阻止已提权进程）、侧信道、物理访问。

## 4. 技术选型与可行性核对

### 4.1 已核对的 `windows-sys 0.52` 符号

| 能力 | 符号 | 所在 feature |
| --- | --- | --- |
| 创建容器身份 | `CreateAppContainerProfile`、`DeriveAppContainerSidFromAppContainerName`、`DeleteAppContainerProfile` | `Win32_Security_Isolation` |
| 以容器身份拉起 | `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`、`SECURITY_CAPABILITIES`、`STARTUPINFOEXW`、`InitializeProcThreadAttributeList`、`UpdateProcThreadAttribute` + **普通 `CreateProcessW`** | `Win32_Security`、`Win32_System_Threading` |
| 派生受限 token | `CreateRestrictedToken`、`GetTokenInformation`、`SetTokenInformation` | `Win32_Security` |
| 带 token 启动（**本方案不用**） | `CreateProcessAsUserW`、`CreateProcessWithTokenW` | `Win32_System_Threading` |
| 资源限额 | `CreateJobObjectW`、`AssignProcessToJobObject`、`SetInformationJobObject`、`JOBOBJECT_EXTENDED_LIMIT_INFORMATION` | `Win32_System_JobObjects`（**已在用**） |
| 容器网络能力 | `NetworkIsolationEnumAppContainers`、`NetworkIsolationSetAppContainerConfig` | `Win32_NetworkManagement_WindowsFirewall` |
| 回环豁免 | **无直接 API**，需写 `HKCU\…\AppContainer\Storage\…\Exemptions` 或调 `CheckNetIsolation.exe LoopbackExempt` | — |

> 核对记录（2026-11-05 实测本机 `windows-sys 0.52.0` 源码与官方语义）：
> `NetworkIsolationSetAppContainerConfig` 的语义是**把容器 SID 列为"公开容器"**（影响
> `EnumAppContainers` 的可见性与防火墙规则作用域），**不是**回环豁免——两者常被混为一谈。
> 回环豁免的真实落点是 `HKCU` 下 AppContainer 的 `Exemptions` 键（`CheckNetIsolation
> LoopbackExempt -a -n <name>` 改的就是它）。**因此回环豁免这一步没有干净的 Win32 API，
> 见 §7.6 的替代方案与风险**。

### 4.2 关键取舍：网络为什么用 AppContainer 能力而不是 WFP

调研时考虑过 WFP（`FwpmFilterAdd0` + `FWP_ACTION_BLOCK`，所需符号齐全），**否决**，理由：

1. **WFP 是静态判定**：过滤器只能"放行/阻止"，无法"挂起等人点确认"。要挂起必须写
   内核 callout（WFP 的 `FWP_ACTION_CALLOUT_TERMINATING` 需要内核驱动），
   而本项目不发驱动、不做驱动签名——这条路直接否掉。
2. **AppContainer 的网络默认是关的**：lowbox token 必须显式持有 `internetClient` /
   `privateNetworkClientServer` / `enterpriseAuthentication` 之一才能出网。这正好是我们要的
   "默认拒绝"语义，且**不需要任何额外组件**。
3. **审批落点自然**：既然网络是否可用取决于"进程创建时 token 带不带能力"，审批就发生在
   **创建进程之前**——这时审批链、TUI 弹窗、`ask_user` 全都能用，不需要反向回调进内核。

**结论**：网络控制用「AppContainer 能力 + 本地代理」组合，不用 WFP。
WFP 仅作为可选的**加固层**（§7.5），不进 v1。

### 4.2.1 为什么不用 `CreateProcessAsUserW`（重要，避免走弯路）

直觉上"以另一个身份启动进程"会想到 `CreateProcessAsUserW`，但它在**普通用户进程**里
会以 `ERROR_PRIVILEGE_NOT_HELD (1314)` 失败——它要求调用方持有 `SE_INCREASE_QUOTA_NAME`，
且当 token 不可分配时还要 `SE_ASSIGNPRIMARYTOKEN_NAME`。OmniCrawl 的宿主就是一个普通
交互式进程，没有这些特权，也不应该为了沙盒去调整本地安全策略。

**正确做法**：官方文档明确写着「`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 被设置时，
新进程将作为 AppContainer 进程创建」。也就是说：

```
InitializeProcThreadAttributeList
UpdateProcThreadAttribute(…, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                          &SECURITY_CAPABILITIES { AppContainerSid, 0 capabilities, null })
CreateProcessW(…, EXTENDED_STARTUPINFO_PRESENT, &si_ex, …)   ← 普通 CreateProcess，零特权
```

代价是 `std::process::Command` 不支持自定义 `STARTUPINFOEX`，所以沙盒路径要**自己调
`CreateProcessW`**（再包成 `Child` 需要的句柄，或用 `PROC_THREAD_ATTRIBUTE_JOB_LIST`
把 Job 一并挂上，省掉单独 `AssignProcessToJobObject`）。这是 P1 的主要工作量。

### 4.3 进程与限额

沿用并扩展已有 `process_control.rs::KillOnCloseJob`（已在 `command.rs`/`monitor.rs` 使用）。
沙盒模式额外设置：进程数上限、Job 内存上限、CPU 速率、子进程禁止脱离
（**不设** `JOB_OBJECT_LIMIT_BREAKAWAY_OK`，即后代无法逃出 Job）。

## 5. 总体架构

```
                       ┌─────────────────────────────────────────┐
                       │ omnicrawl-host （宿主，永在沙盒外）      │
                       │                                         │
  模型调用 bash ──────► │  tools/command.rs                       │
                       │    │                                    │
                       │    ▼                                    │
                       │  sandbox/mod.rs  ──── 策略解析 ───┐     │
                       │    │                             │     │
                       │    │  ① 审批（沿用现有决策层）     │     │
                       │    │  ② 建/复用容器身份            │     │
                       │    │  ③ 派生 Job + 设限额          │     │
                       │    ▼                             │     │
                       │  沙盒子进程（AppContainer 身份）   │     │
                       │    ├ 文件系统：仅工作区 + 临时目录 │     │
                       │    ├ 网络：无能力 → 只能连本地代理 │     │
                       │    └ 后代：继承同一容器与 Job      │     │
                       └───────────┬─────────────────────────────┘
                                   │ 出网请求
                                   ▼
                       ┌───────────────────────────┐
                       │ 宿主内的审批代理（回环）   │
                       │  规则命中 → 放行           │
                       │  未命中   → 审批 → 放行/拒 │
                       └───────────────────────────┘
```

三层边界，逐层收紧：

1. **身份边界**（AppContainer）：进程以独立 SID 运行，ACL 决定它看得见什么。
2. **资源边界**（Job Object）：进程组的上限与生命周期。
3. **网络边界**（能力 + 代理）：默认无网，"要网"这件事变成一次显式审批。

## 6. 文件系统隔离

### 6.1 原理

AppContainer 进程的 token 里有一个 **package SID**（`S-1-15-2-…`）。访问任何对象时，
内核同时检查常规 ACL 与「低完整性/容器」标签。默认情况下：

- **用户目录读不到**：`C:\Users\X` 的 ACL 里没有 `ALL APPLICATION PACKAGES`（`S-1-15-2-1`）
  条目，所以容器**读不到**用户目录（这正是我们要的）。`~/.ssh`、`~/.OmniCrawl/config.toml`
  （明文 API key）都在里面，一并挡住。
- **系统目录可读不可写**：`C:\Windows`、`C:\Program Files` 的默认 ACL 里**带有**
  `ALL APPLICATION PACKAGES: 读取和执行` 条目，因此容器能读、不能写。
- **容器自身是低完整性**：即使某处给了写权限，低 IL 也写不进标了中/高 IL 的对象。

### 6.2 需要授予的路径（`icacls` 语义，实现时用 `SetNamedSecurityInfoW` 而非起进程）

| 路径 | 权限 | 为什么 |
| --- | --- | --- |
| 工作区根（`[workspace] root` 或启动目录） | 读写执行 | Agent 的产物都落这里 |
| `.omnicrawl/.agent_tmp` | 读写执行 | 临时脚本、中间产物 |
| `%TEMP%\omnicrawl-sandbox-<id>` | 读写执行 | **专用**临时目录，避免污染全局 TEMP |
| 运行时目录（node/python 等解释器所在路径） | 读执行 | 否则 `node`/`python` 起不来 |

**不授予**：`C:\Users\X` 本体、`~/.OmniCrawl`、`~/.ssh`、`~/.gitconfig`、其他项目目录。

### 6.3 关键实现细节：环境变量重定向

沙盒进程的 `TEMP`/`TMP`/`HOME`/`USERPROFILE` 必须**改指向**专用目录。否则：

- 子进程往 `%TEMP%` 写临时文件 → ACL 拒绝 → 大量工具诡异失败（真实踩坑点）；
- 某些工具读 `USERPROFILE` 找配置 → 拿到路径但读不到 → 报错信息误导。

所以 `command.rs` 里 `.env_clear()` 后要**显式**重建一份最小环境（PATH、TEMP、
TMP、HOME、USERPROFILE、SYSTEMROOT、COMSPEC、LANG…），而不是继承宿主环境。
这同时避免把 `OMNICRAWL_*` 令牌类环境变量泄漏给沙盒进程。

### 6.4 写入判定（"能不能写"）

现有审批层已有"写文件要走确认"的判定。沙盒模式下增加一条**前置**规则：

```
目标路径 ∈ 可写集合  → 直接放行（沙盒 ACL 会兜底，即使判定漏了也写不进）
目标路径 ∉ 可写集合  → 审批（与现有 manual/review 语义一致）
```

即：**沙盒 ACL 是硬约束，审批是体验层**。这点很重要——审批判定有 bug 时，
最坏结果是"写失败"，不是"写到不该写的地方"。

## 7. 网络请求审批

### 7.1 为什么需要"代理"这一层

AppContainer 不给网络能力时，**连本地回环都被禁**（这是 Windows 的默认，UWP 那套）。
所以不能简单地"没能力=全禁"——那样沙盒内连 `localhost:3000` 的 dev server、
`pip download` 走公司代理、`git` 走 SSH，全部瘫痪，沙盒会没人愿意开。

因此策略是：

- 沙盒进程**不持有** `internetClient`；
- 但必须允许它**连回环**（否则代理本身也连不上）；
- 所有出网必须经过**宿主内的回环代理**，由代理决定放行还是审批。

回环能不能连，取决于容器有没有 loopback 豁免（见 §7.6）。

### 7.2 代理的两种形态（v1 选 A）

**A. HTTP 代理（`HTTP_PROXY`/`HTTPS_PROXY` 注入）—— v1 采纳**

- 宿主起一个回环 HTTP 代理（复用 `web_transport.rs` 已有的传输栈）。
- 沙盒进程的环境里注入 `HTTP_PROXY=http://127.0.0.1:<port>`、`HTTPS_PROXY` 同理。
- 代理解析 `CONNECT host:port`（HTTPS）与绝对 URI（HTTP），得到**目标域名/端口**。
- 覆盖率：`curl`、`pip`、`npm`、`git http(s)`、`cargo`、`go mod`、大多数语言运行时的
  HTTP 客户端——**都能覆盖**，因为它们都尊重 `HTTP_PROXY`。
- 覆盖不到：裸 TCP/UDP、SSH、非 HTTP 协议、绕过代理的客户端。

**B. 全部网络流量的透明拦截 —— 不作为 v1**

需要 WFP 内核 callout 或 NDIS 过滤驱动，代价与风险都远超收益（见 §4.2）。

**取舍要写在文档里让用户知道**：v1 的网络沙盒是"应用层代理级"，不是"内核级"。
裸 `socket()` 调用会直接失败（因为没有 `internetClient`），**不是**悄悄放行——
这点是安全的：失败而非泄漏。用户如果确实需要非 HTTP 协议（如 ssh），走 §7.4 的
"显式授予能力"路径，那是**整段会话级**的授权，粒度更粗但可控。

### 7.3 审批决策顺序（代理内）

```
请求 (host, port, 协议)
  │
  ├─ 1. 命中「本地」规则？  127.0.0.1 / ::1 / localhost → 直接放行（含 dev server）
  ├─ 2. 命中「已批准」缓存？ (host, port) 在本次会话批准表 → 放行
  ├─ 3. 命中「明确拒绝」表？ → 拒绝（403 + 原因）
  ├─ 4. 沙盒策略 = offline → 拒绝
  ├─ 5. 沙盒策略 = ask    → 弹审批（超时默认拒绝）
  └─ 6. 沙盒策略 = online → 放行
```

**默认值**：`ask`。超时（复用 `[approval]` 的 `timeout_seconds`）一律**拒绝**——
网络审批的失败方向必须是关闭，否则沙盒等于没开。

### 7.4 四种网络策略

| 策略 | 行为 | 适用 |
| --- | --- | --- |
| `offline` | 全拒（除回环） | 纯离线数据处理 |
| `ask`（默认） | 每个新目标审批一次，会话内记住 | 日常 |
| `allowlist` | 仅域名白名单放行，其余拒绝 | 无人值守/CI |
| `online` | 全放行（仅记录） | 明确信任的任务 |

`allowlist` 的匹配规则：精确域名 > 后缀匹配（`*.npmjs.org`，**不含** `npmjs.org` 本身，
避免 `evil-npmjs.org` 之类的后缀混淆），大小写不敏感，端口需显式给出（默认 443/80）。

### 7.5 v1 明确不做

- WFP 加固层（可选，将来加：给容器 SID 加一条 block 过滤器，防止代理被绕过）。
- 非 HTTP 协议的细粒度审批。
- DNS 层控制（域名解析仍走宿主，`allowlist` 在代理层按 SNI/Host 判定；
  **因此 DNS 预解析泄漏目标存在，但连接仍会被拦**——写进文档）。

### 7.6 回环豁免的落地方式（v1 的已知薄弱点）

**问题**：AppContainer 默认禁止回环，而我们的代理就在回环上。没有豁免，代理形同虚设。

**没有干净的 Win32 API**：`NetworkIsolationSetAppContainerConfig` 是"公开容器"语义，
不是豁免（见 §4.1 核对记录）。官方给的途径只有两条，都不理想：

| 途径 | 做法 | 问题 |
| --- | --- | --- |
| A. 起 `CheckNetIsolation.exe LoopbackExempt -a -n <容器名>` | 调官方工具 | 要起子进程；写 HKCU 全局状态；需撤销 |
| B. 直接写 `HKCU\…\AppContainer\…\Exemptions` 注册表 | 自己写 R/SID | 未文档化；格式随版本变；写错会让所有容器的豁免表损坏（社区有实例） |

**v1 决策：用 A，且严格遵守三条纪律**

1. **只在沙盒首次拉起时加一次**，容器名固定为 `omnicrawl-sandbox-<用户SID短哈希>`，
   避免与其他应用的容器名冲突。
2. **退出时必须撤销**（`-d -n <name>`）；宿主崩溃导致没撤销时，下次启动先清再设
   （幂等）。
3. **失败即关闭网络**：加不上豁免 → 网络策略强制 `offline`，并在 `/sandbox` 里显示
   「回环豁免不可用，网络已禁」。**绝不**因为豁免失败就放开 `internetClient`。

**风险要正视**：这条路依赖一个外部 exe 和未文档化的注册表状态，是 v1 里最脆的一环。
所以 §13 把它单独列为 P3，并允许 P3 先不做——P1/P2（文件隔离 + 资源限额）本身已经
有独立价值，且不依赖回环豁免。

**若 P3 受阻的备选**：不做代理，直接 `capability = 0`（完全断网），
把"需要联网"的任务留给用户在沙盒外执行。这是更严格但更可用的降级——
**"沙盒内不能联网"是清晰的，而"沙盒内能联网但只能连代理"若代理不通就是诡异故障。**

## 8. 进程与资源约束

沿用 `KillOnCloseJob`，沙盒模式下追加限额：

| 限额 | 值（默认） | 配置键 |
| --- | --- | --- |
| 进程数 | 64 | `max_processes` |
| Job 总内存 | 4 GiB | `max_memory_mb` |
| 单进程内存 | 2 GiB | （同时设） |
| CPU 速率 | 不设（避免误伤正常编译） | `cpu_rate_percent`（可选，0=不设） |
| 单命令墙钟超时 | 沿用 `bash`/`monitor` 现有超时 | — |
| 是否允许脱离 Job | **否**（不设 `BREAKAWAY_OK`） | 固定 |

超过内存上限：Windows 直接终止 Job 内进程，返回可读错误
（`沙盒内存超限（>4096MB），进程已终止`），而不是让模型猜"为什么被杀"。

**注意**：Job 的 `KILL_ON_JOB_CLOSE` 已保证宿主崩溃时回收，沙盒沿用即可，不重复实现。

## 9. 沙盒模式开关与工具语义

### 9.1 配置形状（`config.toml`）

```toml
[sandbox]
# 总开关：关闭时行为与今天完全一致（零代码路径变化）
enabled = false
# 隔离强度：off | appcontainer（v1 唯一实现）
mode = "appcontainer"

[sandbox.fs]
# 额外可写路径（除工作区与 .agent_tmp 外）；相对路径按工作区根
writable = []
# 额外可读路径
readable = []

[sandbox.net]
# offline | ask | allowlist | online
policy = "ask"
# allowlist 策略下的域名；支持 *.example.com 后缀匹配
allow = ["registry.npmjs.org", "pypi.org", "*.githubusercontent.com"]
# 永远拒绝（优先级高于 allow）
deny = []
# 审批超时（秒），超时即拒
approval_timeout_seconds = 60
# 是否记录全部出网请求（含放行）
log_all = true

[sandbox.limits]
max_processes = 64
max_memory_mb = 4096
cpu_rate_percent = 0
```

### 9.2 开关语义（三档，逐级明确）

| `enabled` | `mode` | 行为 |
| --- | --- | --- |
| `false` | — | 与今天**完全一致**。不建容器、不查 ACL、不设限额，零额外开销 |
| `true` | `off` | 显式"我要开关但先不隔离"：只记录"本会做什么"（dry-run），用于试跑 |
| `true` | `appcontainer` | 真隔离 |

`mode = "off"` 不是多余档位：它让用户可以先在真实工作流里看一周 dry-run 日志，
确认"我的编译/依赖安装会不会被拦"，再真切。**这能显著降低沙盒一开就干不了活的挫败感。**

### 9.3 哪些工具受沙盒约束

> 核对记录：`agent_tools.json` 里的 `run_in_subprocess` 全是 `true`，它是"工具整体在宿主
> 子进程里执行"的元数据，**不表示"该工具内部会拉起子进程"**。因此下表按**实际实现**分类，
> 实现者需要在 `command.rs`、`monitor.rs`、`git.rs` 三个真正的 spawn 点挂沙盒。

| 工具 | 沙盒下行为 | 挂载点 |
| --- | --- | --- |
| `bash` / `powershell` | **进沙盒**（主要目标） | `tools/command.rs::run_shell` |
| `monitor`（后台命令） | **进沙盒**，同一容器身份 + 各自 Job | `tools/monitor.rs` 两处 spawn |
| `git` | **进沙盒**（它本身就是子进程） | `tools/git.rs` |
| `read` / `write` / `edit` / `list` / `find` / `grep` | **不进沙盒**（宿主内进程执行），但沿用"写路径判定"（§6.4） | — |
| `fetcher` / `web_search` / `web_transport` | **不进沙盒**（宿主网络栈），沙盒不影响它们 | — |
| `windows_window` / `windows_control` / `windows_input` / `windows_clipboard` / `windows_screenshot` | 沙盒开启时**拒绝**，返回"沙盒模式下不提供桌面控制" | `tools/registry.rs` 执行前 |
| `computer_use`（子代理） | 与 GUI 工具同理，**拒绝** | 同上 |
| `subagent` | 继承父进程的沙盒策略（子代理不能放宽） | 子代理启动时透传 |

**为什么 GUI 工具要拒**：桌面控制能在**宿主身份**下操作任意窗口、读任意可见内容、
往任意窗口输入——这直接绕过文件与网络隔离。沙盒开启还允许它，等于前门锁了后门开着。
同理，`windows_clipboard` 能读写全局剪贴板（常有密码），必须一并拒。

**为什么 `read`/`write`/`grep` 等不进沙盒**：它们是宿主进程内的 Rust 实现，不 fork 子进程，
没有可隔离的边界。对它们的保护靠 §6.4 的写路径判定（这是**软**约束）。
若要让它们也硬隔离，唯一办法是把这些工具塞进沙盒子进程里跑——那会把简单读文件变成
跨进程调用，代价与收益不成比例。**结论：文件读写的硬边界只覆盖"子进程能看到什么"，
宿主自身的读写靠审批与路径判定。这一点必须写进用户文档，否则会产生错误的安全预期。**

### 9.4 与既有配置的关系

- **`[tools]`**（`powershell = true` / `windows_control = true` …）是"这个工具**存在**吗"。
  沙盒是"它**怎么执行**"。两者正交：`powershell = false` 时根本不声明 `powershell` 工具，
  沙盒无从谈起；`powershell = true` + `sandbox.enabled = true` 时，工具存在且进沙盒。
- **`[approval]`**：见 §9.5。
- **`[workspace] root`**：沙盒的可写范围以它为根。**注意本机当前配置是
  `[workspace] root = "D:\\other"`**，与启动目录不同——沙盒必须用**实际工作区根**，
  不能假设是进程 CWD（这点与上一轮 QQ 富媒体的 `[FILE:]` 路径解析是同一类坑）。
- **`[agent_workspace]` / `[agent_temp]`**：`.agent_tmp` 的挂载点，沙盒要给读写。
- **不改现有任何段的语义**：沙盒只读它们，所有新增配置都在 `[sandbox]` 下。

### 9.5 与既有审批的关系

沙盒**不替代**审批，两条正交的线同时生效：

```
工具调用 → 审批判定（该不该做）→ 沙盒执行（做完能到哪）→ 网络代理审批（能不能出网）
```

因此 `[approval] mode = "auto"` + `[sandbox] enabled = true` 是**推荐组合**：
审批全放行（不烦人），但爆炸半径被沙盒兜住。这比今天"要么全问、要么全放"好得多。

## 10. 关键流程：一次 `bash` 调用在沙盒内的完整路径

```
1. 模型请求 bash("npm install")
2. registry 查工具 → 审批层判定（auto 模式下直接过）
3. sandbox::prepare(command)
   a. 取/建容器身份（AppContainer profile 名 = "omnicrawl-sandbox"，进程级复用）
   b. 给工作区/.agent_tmp/专用 TEMP 授予容器 SID 读写（幂等；用 SetNamedSecurityInfoW）
   c. 建 Job：KILL_ON_JOB_CLOSE + 进程数/内存上限
   d. 组 STARTUPINFOEX：
        SECURITY_CAPABILITIES = { AppContainerSid, CapabilityCount = 0 }   ← 无网络能力
        PROC_THREAD_ATTRIBUTE_JOB_LIST = [job]                             ← 一并挂 Job
        (回环豁免在首次拉起时用 CheckNetIsolation 配好，见 §7.6)
   e. 环境变量重建：PATH/TEMP/TMP/HOME/USERPROFILE/SYSTEMROOT/COMSPEC +
        HTTP_PROXY/HTTPS_PROXY=http://127.0.0.1:<proxy_port> + 沙盒标记
4. **普通 `CreateProcessW`**（`EXTENDED_STARTUPINFO_PRESENT`）拉起 cmd/powershell
   —— 不是 `CreateProcessAsUserW`：零特权下它会以 1314 失败（§4.2.1）
5. Job 已由 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 在创建时挂上，无需再 Assign
6. 子进程任何出网 → 打到宿主代理 → §7.3 决策 → 放行/审批/拒
7. 命令结束或超时 → Job 回收整棵树（已有逻辑）
8. 返回 stdout/stderr，附「沙盒执行」标记
```

## 11. 失败与降级

| 情形 | 处理 |
| --- | --- |
| `CreateAppContainerProfile` 失败 | **拒绝执行**，返回原因（不要静默降级成无沙盒！） |
| 授予 ACL 失败 | 拒绝执行（说明是哪个路径） |
| 代理起不来 | 网络策略强制 `offline`，并在消息里说明；不静默放行 |
| `CreateProcessW`（带 SECURITY_CAPABILITIES）失败 | 返回错误（含 Win32 码与中文含义），让用户决定是否关沙盒。**绝不**回退到无沙盒的 `Command::spawn` |
| AppContainer 不可用（老系统） | 启动时探测，`enabled = true` 时报错并给出关闭方法 |
| Job 限额不生效（已在别的不可嵌套 Job 里） | 拒绝执行并说明——没有限额的沙盒不是沙盒 |

**铁律**：沙盒的任何组件失败都必须**失败关闭（fail-closed）**。
"沙盒没起来所以就直接跑"是所有安全设计里最常见的致命错误。

## 12. 可观测性

- 每条沙盒命令的回显带标记（TUI 与日志）：`[sandbox:appcontainer]`。
- 网络请求一条一行日志：`[sandbox:net] allow GET api.github.com:443（规则：allowlist *.github.com）`。
- `/sandbox` 斜杠命令：显示当前策略、容器名、代理端口、本次会话已批准目标、拒绝计数。
- dry-run（`mode = "off"`）输出「本会拦截」清单，用于上线前评估。

## 13. 实现切分（建议顺序）

> **进度（2026-11-05）**：P0 已完成。见 §16「P0 实施记录」。

| 阶段 | 内容 | 可独立验证 |
| --- | --- | --- |
| P0 | `[sandbox]` 配置解析 + `/sandbox` 命令 + dry-run 判定（不真隔离） | 配置改动立即生效，日志能看到"本会拦" |
| P1 | 新建 `sandbox/win.rs`：容器身份 + `STARTUPINFOEX` + 裸 `CreateProcessW` 封装；Job 限额；环境重建。先只接 `powershell` 一个工具 | 沙盒内 `whoami /groups` 能看到 package SID；读 `~/.ssh` 失败；`echo` 与管道能用 |
| P2 | 文件 ACL 授予（工作区/`.agent_tmp`/专用 TEMP）+ 解释器最小可读集合 | 沙盒内 `npm install`、`python -c` 能跑 |
| P3 | 回环代理 + `HTTP_PROXY` 注入 + 审批桥接（含 §7.6 的豁免三纪律） | 沙盒内 `curl https://example.com` 触发审批；拒绝时明确报错 |
| P4 | `bash`/`monitor`/`git` 全部接上；GUI 工具与 `computer_use` 拒绝 | 回归既有编排测试 |
| P5 | allowlist/deny 规则、会话批准缓存、日志与 `/sandbox` 全量信息 | |

### P1 的真实成本（不要低估）

`tools/command.rs::run_invocation` 现在是 `Command::builder().spawn()` + 手工
`child.stdout.take()` / `spawn_reader` / `try_wait` 轮询。`std::process::Child` 的字段是
私有的，**无法从裸 `CreateProcessW` 的 `PROCESS_INFORMATION` 直接构造**。因此 P1 有两条路：

- **A（推荐）**：在 `sandbox/win.rs` 里实现一个 `SandboxChild`，自己持有
  `hProcess`/`hThread`/管道句柄，提供与现有循环同形的 `try_wait`/`wait`/`kill`/`read` 方法，
  再把 `run_invocation` 里的轮询循环抽成对二者的共用逻辑（泛型或小 trait）。
- **B（不推荐）**：先 `Command::spawn()` 再"改造"成沙盒进程——做不到，进程身份在创建瞬间
  就定了。

**A 的代价**：`spawn_reader`、`KillOnCloseJob::assign`、`child.id()` 三处都要改成工作
`SandboxChild`。这是 P1 工作量的主体，比"加个 flag"大得多，需预留时间。

每阶段结束都应满足：**沙盒关闭时，全部既有测试零变化**（沙盒开关默认 `false`，
代码路径与今天完全一致）。

## 14. 风险与待确认

1. **解释器可读性（本方案最大的可用性风险）**：`node`/`python`/`npm` 常装在同用户目录
   （`C:\Users\X\AppData\Local\Programs\...`、`...\Roaming\npm`），而**用户目录默认对容器
   完全不可读**（§6.1）。这意味着 P2 阶段很可能出现"`python` 命令都找不到"。
   必须实测确定"最小可读集合"，例如只给 `AppData\Local\Programs\nodejs` 开读权，
   而**不给** `AppData\Roaming`（那里有大量凭据）。
   **这会局部削弱隔离**，需在文档里逐条列出开了什么、为什么。
   若某解释器只能装在用户目录且无法单独开权，那就是该环境下沙盒不适用——如实告知，不要硬塞。
2. **回环豁免是全局用户级状态**：它写在 `HKCU`、按容器名生效，不是按进程。同名的多个
   沙盒进程共享同一豁免；宿主崩溃未撤销时，豁免会残留到下次启动。§7.6 的三条纪律
   （只加一次、退出撤销、失败即断网）就是为此。**这是 v1 最脆的一环。**
3. **`npm`/`pip` 的缓存目录**：默认在用户目录，沙盒内会被拒。需要在环境重建时把
   `npm_config_cache`、`PIP_CACHE_DIR` 指向工作区内目录，否则每次都要重下。
4. **性能**：AppContainer 进程创建比普通进程慢（要建 token、查 ACL），预期 +20~50ms/次。
   对长命令无感，对大量短命令（`grep` 循环）可能有感。需实测。
5. **调试体验**：沙盒内失败往往表现为"权限被拒"而非明确提示。需要主动把
   `ERROR_ACCESS_DENIED` 翻译成"该路径不在沙盒可读范围，如需请加 `[sandbox.fs] readable`"。

## 15. 一句话总结

用 **AppContainer 拿身份与默认断网**，用 **Job Object 拿资源与生命周期**，
用 **回环代理把"要不要出网"变成一次正常审批**；三者在 `command.rs` 里合流，
失败一律关闭，且沙盒关闭时对现有行为零影响。

## 16. P0 实施记录（2026-11-05）

### 落地内容

| 文件 | 内容 |
| --- | --- |
| `omnicrawl-config/src/features/sandbox.rs`（新，约 480 行） | `[sandbox]` 段完整解析（含 `[sandbox.fs]`/`[sandbox.net]`/`[sandbox.limits]` 嵌套表）、`SandboxMode`/`NetPolicy` 枚举与别名、跨字段校验、`status_line()` |
| `omnicrawl-host/src/sandbox.rs`（新，约 850 行） | 运行期策略 [`Sandbox`]：`check_read`/`check_write`/`check_target` 判定、dry-run 记录（上限 512 条）、命令文本启发式抓路径与网络目标、输出提示 |
| `omnicrawl-host/src/tools/registry.rs` | `RegistryOptions.sandbox` 注入点；未注入时按 `[sandbox]` 段构造（配置坏了按**关闭**处理并告警，不阻塞 Agent 启动）；`run_command` 挂上 dry-run 提示 |
| `omnicrawl-commands/src/slash/sandbox_commands.rs`（新） | `/sandbox` 查询、`/sandbox:set <on\|off\|dry-run\|appcontainer\|net 策略>` |
| `omnicrawl-commands/src/agent.rs` | `FakeAgent.environment`：让命令测试能注入隔离的 `config.toml` |
| `assets/config-templates/config.example.toml` | `[sandbox]` 段示例与说明 |

### 判定语义（已用测试钉住的安全边界）

- **路径归一后再比较**：`D:\work\..\Windows` 不会被当成"在工作区内"；`..` 一律先折掉。
- **前缀混淆不误放行**：`D:\workshop` 不属于 `D:\work`。
- **大小写不敏感**（Windows 语义）。
- **相对路径按工作区根解析**，不用进程当前目录（与 QQ 富媒体 `[FILE:]` 那次是同一类坑）。
- **读与写分别判定**：命令里的每个路径都同时判读与写。真实核对时发现只判"写"会漏掉
  `copy ~/.ssh/id_rsa ...` 这种**外泄**路径——那恰恰最要紧，已修。
- **回环永远放行**：`127.x`、`localhost`、`::1`、`0.0.0.0`，本地 dev server 不受影响。
- **`deny` 优先于 `allow`**；`*.suffix` 不匹配裸域（避免 `evil-example.com` 混过）。
- **关闭时不记录、不判定**：`enabled = false` 时对性能与行为零影响。

### 验证

- `cargo test -p omnicrawl-config --lib sandbox`：13 通过。
- `cargo test -p omnicrawl-host --lib sandbox`：33 通过。
- `cargo test -p omnicrawl-commands`：69 通过（含 `/sandbox` 的 5 个用例，走真实分派路径）。
- registry 级端到端：真实 `bash` 调用输出里出现 `[sandbox dry-run]` 段；关闭时输出无任何痕迹。
- 真机核对（真实 `config.toml` → 真实判定）：`copy C:\Users\...\.ssh\id_rsa .\x.txt`
  报出读+写两条拦截、`python .\build.py` 无提示、`curl https://api.example.com` 报 `ask` 待审批、
  `curl http://127.0.0.1:8080` 无提示。
- 既有失败（与本改动无关，已用 `git stash` 复核基线）：`tools::windows::*` 4 个用例因
  "复制屏幕像素失败：拒绝访问（错误码 5）"——无交互桌面会话的环境限制。

### 已知限制（P0 阶段的本分）

1. **`mode = "appcontainer"` 目前等同 dry-run**：执行路径未实现，`/sandbox` 会显式说明这一点，
   避免让人以为隔离已生效。
2. **命令文本解析是启发式**：不解析 shell 语法，漏报/多报都可能。P1 之后真正的边界由
   ACL 保证，不依赖它；dry-run 只用来提前暴露"大概会拦什么"。
3. **dry-run 记录只在宿主进程内存里**，不落盘，也不跨会话。`/sandbox` 的实时统计为此。
