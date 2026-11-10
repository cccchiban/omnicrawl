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

实测补充（§20.4）：还有一类客户端不是"不走代理"，而是**走错了代理**——.NET 的
`GetSystemWebProxy()` 读宿主用户级 WinINET 设置，而回环豁免按容器粒度、无法限定端口，
于是它会绕过我们的判定直连用户自己的代理。这是本方案的已知边界，已决定接受（§20.1）。

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

> **进度（2026-11-05）**：P0、P1、P2、P3 已完成，P3 以方案 B 上线（保留代理、接受应用层
> 绕过）。见 §16–§20 的实施记录。

| 阶段 | 内容 | 可独立验证 |
| --- | --- | --- |
| P0 | `[sandbox]` 配置解析 + `/sandbox` 命令 + dry-run 判定（不真隔离） | 配置改动立即生效，日志能看到"本会拦" |
| P1 | 新建 `sandbox/win.rs`：容器身份 + `STARTUPINFOEX` + 裸 `CreateProcessW` 封装；Job 限额；环境重建。先只接 `powershell` 一个工具 | 沙盒内 `whoami /groups` 能看到 package SID；读 `~/.ssh` 失败；`echo` 与管道能用 |
| P2 | ~~文件 ACL 授予~~（P1 已做）+ 解释器发现与 PATH 重建 + 如实报告不可达目录 | 沙盒内 `node`/`python`/`npm run` 能跑（需显式 `readable`）；`bash`/`git` 因 NUL 设备不可用 |
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

> 以下三条是 P0 当时的实况。第 1 条已在 P1 落地（真隔离执行路径）；P3 之后网络形态
> 见 §20.7，`/sandbox` 的输出也已同步更正。

1. **`mode = "appcontainer"` 目前等同 dry-run**：执行路径未实现，`/sandbox` 会显式说明这一点，
   避免让人以为隔离已生效。
2. **命令文本解析是启发式**：不解析 shell 语法，漏报/多报都可能。P1 之后真正的边界由
   ACL 保证，不依赖它；dry-run 只用来提前暴露"大概会拦什么"。
3. **dry-run 记录只在宿主进程内存里**，不落盘，也不跨会话。`/sandbox` 的实时统计为此。

## 17. P1 实施记录（2026-11-05）

### 落地内容

| 文件 | 内容 |
| --- | --- |
| `omnicrawl-host/src/sandbox/windows.rs`（新，约 1000 行） | AppContainer 容器与 SID、`STARTUPINFOEXW` + 裸 `CreateProcessW`、`SandboxChild`（自持 `hProcess`/Job，提供 `try_wait`/`kill`）、匿名管道与后台读取、Job 限额、最小环境重建、ACL 授予 |
| `omnicrawl-host/src/sandbox/gate.rs`（新） | `IsolationGate`：关 / dry-run / 真隔离三态；真隔离准备失败时**拒绝执行**而不是回退宿主身份 |
| `omnicrawl-host/src/sandbox.rs` → `sandbox/mod.rs` | 原 P0 判定逻辑不变，加上 `IsolationCache`：容器与 ACL 授予在同进程内只做一次 |
| `omnicrawl-host/src/tools/command.rs` | 抽出 `run_isolated` 与共用的 `finish`；沙盒内解输出按 UTF-8 → GBK 顺序；沙盒内 PowerShell 选 5.1 |
| `omnicrawl-host/src/tools/registry.rs` | 沙盒在 `CommandRunner` 之前构造并注入；新增端到端隔离用例 |
| `omnicrawl-host/Cargo.toml` | `windows-sys` 补 `Win32_Security_Isolation`、`Win32_Security_Authorization`、`Win32_System_Pipes`、`Win32_System_IO` |

### 真机验收（每一条都实测，不是推断）

| 验收项 | 结果 |
| --- | --- |
| 容器内读 `C:\Windows\win.ini` | 成功 |
| 容器内写工作区 | 成功 |
| 容器内读 `C:\Users\Administrator` 目录内容 | `拒绝访问` |
| 容器内读 `~/.ssh/id_rsa` | `拒绝访问` |
| 容器内写宿主用户目录 | `拒绝访问` |
| 容器内列 `C:\Windows\System32` 目录 | `拒绝访问`（能读具体文件，不能枚举目录） |
| `echo` / 管道 / `dir` | 正常 |
| 沙盒关闭时 | 输出与今天完全一致，无任何沙盒痕迹 |

### 六个踩过的坑（都留了注释，避免下次重踩）

1. **`STARTUPINFOEXW.StartupInfo.cb` 必须是 `sizeof(STARTUPINFOEXW)`（112），不是 `sizeof(STARTUPINFOW)`（104）**。
   写成 104 时系统认为扩展区不可读，`CreateProcessW` 直接报 `ERROR_INVALID_PARAMETER`(87)。
   反过来，`lpAttributeList` 必须是**整个 `STARTUPINFOEXW`** 的地址传进去——只传首字段
   `StartupInfo` 会让属性列表整份失效，进程照样以宿主身份起来（最危险的静默失败）。
2. **环境里必须有 `LOCALAPPDATA`**。AppContainer profile 落在 `%LOCALAPPDATA%\Packages`，
   `CreateProcessW` 从调用方环境解析它；缺了就报 `ERROR_ENVVAR_NOT_FOUND`(203)。
   这个错误码完全不指向真实原因，社区里有多个项目为此误判成"环境块构造错误"。
3. **`PROC_THREAD_ATTRIBUTE_JOB_LIST` 在本机不可用**：`UpdateProcThreadAttribute` 返回成功，
   但随后的 `CreateProcessW` 报 `ERROR_INVALID_HANDLE`(6)。改为创建后 `AssignProcessToJobObject`，
   风险窗口是创建到 Assign 之间的几微秒（此时进程已受容器约束，只是还没受 Job 限额）。
4. **ACL 授予必须先查再写**。给 `D:\` 这类卷根写一条可继承 ACE 会触发全卷继承传播，
   实测一次 **108 秒**。加上 `GetEffectiveRightsFromAclW` 预检后，`prepare` 从 108 秒降到 **27 毫秒**。
   判据只用 `FILE_TRAVERSE`(0x20) 与 `FILE_READ_ATTRIBUTES`(0x80)，`SYNCHRONIZE`(0x100000)
   常一并返回但不是遍历的必要条件。
5. **PowerShell 7 曾整体不可用，根因已在 P4 查明并修掉**：当时它能启动、能报版本号，但
   加载不出任何 cmdlet，连内置的 `Write-Output` 都报"无法识别"，同一容器里 5.1 完全正常。
   真正原因是**系统盘根 `C:\` 在容器内不可遍历**（工作区在别的盘时 `grant_ancestors`
   覆盖不到它），pwsh 因此建不起 `C:` 驱动器、自己的内置模块全部导入失败；5.1 不依赖
   这一步所以没事。修法与完整证据见 §22。沙盒内现在优先用 7.*，探测不通才回退 5.1。
6. **输出字节不能提前按 UTF-8 有损转换**。沙盒进程的 stdout 是管道，程序按控制台代码页
   （中文系统 936）写字节；先 `from_utf8_lossy` 会把本来能还原的 GBK 字节变成替换字符。
   现在管道保留原始字节，读取时先严格 UTF-8、失败再 GBK。

### 性能

| 场景 | 耗时 |
| --- | --- |
| 容器与 ACL 准备（首次，含祖先链预检） | 27 ms |
| 之后每次 `cmd.exe` 命令 | 40 ms（其中进程创建 3 ms） |
| 之后每次 `powershell.exe` 命令 | 1.2–2.0 s（其中 5.1 冷启动约 1 s） |

ACL 授予是**幂等**的：首次之后 `can_traverse` 直接放行，不再改 DACL。

### 验证

- `cargo test -p omnicrawl-host --lib`：**579 通过 / 1 失败**。失败项
  `tools::windows::control::tests::arguments_are_validated_before_calling_powershell`
  是既有基线失败（无效窗口句柄 42），与沙盒无关，单独复跑可复现。
- 新增 `tools::registry::tests::appcontainer_isolates_the_powershell_tool`：端到端走
  `ToolRegistry::execute("powershell")`，验隔离标记、工作区可写、宿主用户目录不可枚举，3.7 秒。
- `cargo test -p omnicrawl-commands`：69 通过。`cargo test -p omnicrawl-config --lib`：52 通过。
- `cargo clippy -p omnicrawl-host --lib`：沙盒相关代码零告警；三个新文件 `rustfmt --check` 通过。

### 已知限制（P1 的本分）

1. **网络能力为 0**：沙盒内完全不能出网，回环也不行。回环代理是 P3，在它之前
   "沙盒内不能联网"是清晰且安全的降级。
2. **`bash` 工具暂不走沙盒**：Git Bash 装在 `C:\Program Files\Git`，容器可读，但它还需要
   `/tmp`、`/usr/bin` 等一整套 MSYS 运行时目录与用户级配置；这属于 P2 的「解释器最小可读集合」。
   现在 `bash` 仍以宿主身份运行——**这意味着沙盒目前只对 `powershell` 生效**，
   必须让用户知道，不能让人以为全工具都已隔离。
3. **卷根等已可遍历的祖先不会被改 DACL**，因此容器对它们的遍历权依赖系统默认配置
   （`Authenticated Users`/`Users` 一般已有）。若某台机器上祖先链不可遍历且不允许改 DACL，
   该工作区会启动失败并报出具体路径。
4. **容器按「用户 + 工作区」派生**，同一工作区的多次启动复用同一容器与同一批 ACE；
   ACE 不会在退出时撤销（它们对容器本身无害，且下次启动还要用）。

## 18. P2 实施记录（2026-11-05）

### 落地内容

| 文件 | 内容 |
| --- | --- |
| `sandbox/runtime.rs`（新） | 解释器目录发现：按路径特征认领 `node`/`npm`/`python`，剔除嵌套项与全局前缀目录 |
| `sandbox/windows.rs` | `has_access`（显式 ACE 检查）、`prepare_runtimes`（只判定不写 ACL）、`sandboxed_path`（重建 PATH）、`is_system_path` 修正 |
| `tools/registry.rs` | `bash` 拒绝文案改为实测结论；新增解释器可达性与 PATH 两条端到端用例 |

### 真机结论（每一条都实测，不是推断）

| 探针 | 结果 |
| --- | --- |
| `node -e`（`Program Files`） | 授权后**可用**，能写工作区 |
| `npm run <script>` | **可用**（`npm install --offline` 也正常，只差网络） |
| `python x.py` / `python -m pip -V` | **可用**（授权其目录后） |
| `git --version` / `git status` | **不可用**：`could not open '/dev/null'` |
| `bash -c echo` / `bash --version` | **不可用**：启动即 `0xC0000142`（退出码 -1073741502） |
| `python -m venv` | **部分不可用**：`ensurepip` 失败（见下） |
| 宿主用户目录 / `~/.ssh` | 仍**拒绝访问**（隔离未被削弱） |

### 五个必须记住的坑

1. **`GetEffectiveRightsFromAclW` 对 AppContainer SID 会说谎**。它对
   `C:\Program Files\nodejs` 返回"容器可读"，容器内实际是「拒绝访问」。原因是它只按 ACE
   位掩码求和，不走受限令牌的双 SID 链访问检查。判可读性必须自己遍历 DACL 找针对
   package SID 的显式允许 ACE（`GetAce` + 比对 SID）。
2. **`SetNamedSecurityInfoW` 改一次 DACL 就重算整棵子树的继承**，代价与规模成正比：
   `C:\Program Files\nodejs`（0.29 万项）0.26 秒、`D:\ProgramData\Anaconda3`（22 万项）
   **18.4 秒**。即便写入内容与原 DACL 完全相同也照付。把解释器目录直接授成可读是陷阱——
   第一版就是这么写的，首次准备 **413 秒**。
3. **"先数目录有多大再决定"同样不行**：全树遍历 22 万项本身要几十秒。
   P2 最终只做一次有效权限查询（约 37µs），读不到就如实报告并指向 `[sandbox.fs] readable`。
4. **`NUL` 设备在 AppContainer 内完全打不开**（`CreateFileW` → `ERROR_ACCESS_DENIED` 5），
   读 DACL 也失败（status 161）。所有 MSYS/Cygwin 程序（`bash`、`git`、`ls`…）都在
   DLL 初始化阶段就要打开 `/dev/null`，因此**这不是配置问题、无法靠授权目录解决**。
   真因见 §23.1：`\Device\Null` 是 NULL DACL，受限令牌的双 SID 链没有可匹配 ACE。
   （本节此处原写「`git --version` 成功，只有真正干活的子命令失败」，§23 实测已推翻：
   连 `--version` 都失败。）
5. **PATH 必须按容器实际可读重建**。继承宿主 PATH 会让 `npm.cmd` 这类批处理包装在解析
   阶段失败，报"不是内部或外部命令"——与真正的根因（某个目录不可访问）毫无关系。

### 与设计稿的偏离（如实记录）

- §6.2 曾设想"运行时目录授成读执行"，P2 实测后**改为只在配置显式声明时才授权**
  （`[sandbox.fs] readable`）。自动授权任何用户目录都会引入几十秒的启动代价，
  且 `AppData` 祖先一旦放开就把隔离拆了。
- §13 的 P2 验收"`npm install`、`python -c` 能跑"：`python -c` 与本机 `npm run` 已达成，
  但**联网的 `npm install` 仍待 P3**（现在只会因无网络失败，不是权限问题）。

### 验证

- `appcontainer` 四条端到端用例全通过；host 全量 589 通过 / 2 失败，两个失败均为既有基线
  （无效窗口句柄 42、`prompt::templates_dir` 找不到模板目录——已用 `git stash` 在 P1 提交上复现）。
- 准备耗时：首次 14.7 ms（无授权）/ 195 ms（需写一次 `nodejs` 的 ACE），此后复用 0.6 ms 量级。
- 新增文件 `rustfmt --check` 通过，`cargo clippy` 对沙盒代码零告警。

## 19. P3 实施记录：`bash` 的替代路径（2026-11-05）

### 目标与结论

P2 把 `bash` 在沙盒内**明确拒绝**了，但只留下一句"请改用 powershell"——模型没有迁移依据，
会把 bash 脚本原样搬进 `powershell` 工具，然后接连撞墙。本节做的是**让替代路径真的可用**。

结论：Windows PowerShell 5.1 足以承担 `bash` 的日常职责，但前提是先修掉四个缺陷；
四个缺陷里三个是**本次实测才暴露的真实 bug**，不是能力不足。

### 修掉的四个缺陷

| # | 现象 | 根因 | 修法 |
| --- | --- | --- | --- |
| 1 | `Remove-Item` / `rmdir /s` / `Directory.Delete` 一律「访问被拒绝」，连自己刚建的目录都删不掉 | 授权掩码 `FULL_CONTROL_FOR_SANDBOX` 只含 `GENERIC_READ\|WRITE\|EXECUTE`，**不含 `DELETE`**；`FILE_GENERIC_WRITE` 覆盖写数据与追加，不覆盖删除 | 掩码补 `DELETE` + `FILE_DELETE_CHILD` |
| 2 | `$env:FOO` 与 `Get-ChildItem Env:` 报「已添加了具有相同键的项」 | 环境白名单里 `WINDIR` 与 `windir` 各列一次；Windows 变量名大小写不敏感，PowerShell 建 `Env:` 驱动器时键冲突 | 删重复项，并在组装环境块时按大小写不敏感去重 |
| 3 | `npm --version` 报 `Test-Path : Access is denied` | `npm.ps1` 用 `npm-prefix.js` 的输出（`%APPDATA%\npm`，容器不可读）做 `Test-Path`；wrapper 的 `$ErrorActionPreference = 'Stop'` 把这次**无害探测**放大成致命错误 | `npm_config_prefix` 指向工作区内 `npm-global`（并预建目录、加入 PATH） |
| 4 | 执行 `.cmd` 包装（npm 等）报 `AuthorizationManager 检查失败` | AppContainer 读不到执行策略注册表，策略落到 Restricted | **仅沙盒内**启动参数补 `-ExecutionPolicy Bypass`；宿主侧不动用户的执行策略 |

缺陷 1 影响最大：它让工作区变成只增不减，删构建产物这种最日常的动作直接失败，
而报错只说"访问被拒绝"，完全不指向权限位。

### bash → PowerShell 对照（已逐条实测通过）

`tools/registry.rs::BASH_UNAVAILABLE_IN_SANDBOX` 会把这张表交给模型。19 条映射全部在
真机沙盒内跑通（`cat/head/tail/ls/grep/find/append/env/&&/管道/test -f/mkdir -p/rm -rf/
sed -i/node/npm/python/pip/$?`）。

| bash | PowerShell |
| --- | --- |
| `cat f` / `head -n N f` / `tail -n N f` | `Get-Content f`（`-TotalCount N` / `-Tail N`） |
| `ls` | `Get-ChildItem` |
| `grep pat f` | `Select-String -Path f -Pattern pat` |
| `find . -name x` | `Get-ChildItem -Recurse -Filter x` |
| `echo a >> f` | `Add-Content -Path f -Value a` |
| `FOO=1 cmd` | `$env:FOO='1'; cmd` |
| `cmd && next` | `cmd; if ($LASTEXITCODE -eq 0) { next }` |
| `$?` | `$LASTEXITCODE`（外部程序）/ `$?`（cmdlet） |
| `test -f f` | `Test-Path f` |
| `mkdir -p d` | `New-Item -ItemType Directory -Force d` |
| `rm -rf d` | `Remove-Item -Recurse -Force d` |
| `sed -i 's/a/b/' f` | `(Get-Content f) -replace 'a','b' \| Set-Content f` |

沙盒内 `node`、`npm`、`python`、`pip` 实测可用（需其安装目录已在 `[sandbox.fs] readable`
或位于容器的默认可读系统目录）。`npm run` 需要工作区有 `package.json`。

### 为什么不接 Git Bash / 改用别的 shell

`bash` 与 `git` 的不可用由 `NUL` 设备决定（P2 已论证），换授权、换 PATH、换包装都没用。
可选路径只有三条，各自的问题：

- **Git Bash**：DLL 初始化阶段即 `0xC0000142`，无解。
- **WSL**：仍是 `bash`，同样要 `/dev/null`；而且把执行面搬出了 Windows 容器，
  反而扩大隔离边界。
- **PowerShell 7（pwsh）**：曾经「同一容器内加载不出任何 cmdlet」，根因是系统盘根不可遍历，
  P4 已查明并修掉（§22）。现在沙盒优先用 7.*，探测不通才回退 5.1。
- **brush（后补，已落地为 `omnicrawl-brush`）**：纯 Rust 的 bash/POSIX 兼容 shell，
  不经 MSYS、不打开 `NUL`，因此容器内可用。它与 `git → gix` 是同一手法——**换执行形态，
  不放宽隔离**：它仍是独立进程、以容器身份启动，它 spawn 的外部程序同样继承容器令牌。
  反过来说，「在宿主进程内解释脚本」是这条路上唯一的禁区：那会让脚本里每条命令都以
  宿主权限执行，等于没有沙盒。落地细节见 §24。

### 验证

- 19 条 bash→PowerShell 映射在 AppContainer 内**全部通过**（一次性探针，验证后移除）。
- 新增用例：`appcontainer_can_delete_what_it_created`（删除自己创建的内容）、
  `environment_has_no_case_insensitive_duplicates`、`dedupe_vars_keeps_the_first_occurrence`、
  `npm_prefix_points_inside_the_workspace`、`only_the_sandbox_adds_execution_policy_bypass`、
  `bash_refusal_teaches_the_powershell_equivalents`（已随 §24 改名为
  `bash_refusal_names_the_missing_brush_executable`）。
- host 全量 **596 通过 / 2 失败**，两个失败仍为既有基线（`prompt::templates_dir`、
  无效窗口句柄 42），所在文件本次未改动（`git diff` 为空）。
- 改动文件 `rustfmt --check` 通过，clippy 零告警。
- 隔离边界复验：宿主用户目录、`~/.ssh`、工作区外写入仍全部拒绝。

## 20. P3 实施记录：回环代理路线（2026-11-05）

### 20.1 决策与结论

路线技术上可行，但在 PowerShell/.NET 客户端上存在**无法在本层关闭**的绕过口。
2026-11-05 的选择是 **方案 B：保留代理，接受应用层绕过，并把语义写清楚**。

因此本节的定位是「已知边界的实施记录」，不是「待修的缺陷」。选择 B 的理由：

- 方案 A（完全断网）语义最清晰，但会让 `npm install` / `pip install` 只能在沙盒外执行，
  沙盒的实际价值大幅缩水。
- 方案 C（WFP 内核过滤）是唯一能真正封闭的方案，但属 §4.2 明确排除的重型工作。
- 保留代理的代价是「.NET/WinINET 客户端会绕过判定」。这个代价可以接受的前提是
  **用户知情**，所以 §20.4 的绕过链条必须写在文档、`/sandbox` 输出与配置注释里。

一句话边界：**沙盒的网络约束是应用层代理级，不是内核级。**
需要严格不出网时用 `policy = "offline"`（此时连代理都不起）。

### 20.2 三条实测确证的事实

1. **无网络能力时连回环都被拒**：`CapabilityCount = 0` 的容器连宿主回环监听超时。
2. **只加回环豁免即可连通**（不加任何网络能力）：这是最严的组合——容器内核层出不了网，
   唯一通路是宿主代理。绕过代理的裸 `socket()` 是**失败**而非泄漏（实测裸 IP 直连被拒）。
3. **`CheckNetIsolation` 必须用 `-p=<SID>`**，不能用 `-n=<容器名>`：`-n` 要的是**包名**
   （形如 `microsoft.net.native.framework.2.1_8wekyb3d8bbwe`），传 AppContainer 名一律返回
   87（参数错误）。容器 SID 形如 `S-1-15-2-...`，由 `DeriveAppContainerSidFromAppContainerName`
   给出，本实现新增 `Isolation::sid_string()` 暴露它。

### 20.3 已实现部分（保留）

| 位置 | 内容 |
| --- | --- |
| `sandbox/proxy.rs` | 回环 HTTP 代理：`CONNECT` 隧道 + 绝对 URI 转发，判定统一走 `Sandbox::check_target`，`ask` 无审批通道时按拒绝处理，会话内记住已批准目标 |
| `sandbox/windows.rs` | `set_capabilities`（`DeriveCapabilitySidsFromName` + `SID_AND_ATTRIBUTES`）、`exempt_loopback`（按 SID 加豁免，守卫析构时撤销）、`set_proxy`（注入 `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`） |
| `sandbox/mod.rs` | `ensure_network`：`offline` 或 dry-run 不起代理；豁免失败即降级断网；`NetStatus` 如实描述形态 |
| `sandbox/gate.rs` | `plan()` 在真隔离分支调用 `ensure_network`，网络通道与容器身份一起就绪 |
| `tools/command.rs` | 沙盒内补 `-ExecutionPolicy Bypass`；沙盒内把 `DefaultWebProxy` 指向 `HTTP_PROXY` |

`gate.rs` 的接法是「尽力而为、失败即关闭」：`ensure_network` 不返回错误，豁免或代理拉不
起来时它把形态降级为 `NetStatus::Failed` 且**不注入代理变量**，命令照常在容器里跑，只是
出不去。把这种失败变成「命令不执行」反而更差——隔离本身仍然有效。

### 20.4 已知边界：容器能绕过代理直连用户自己的代理

实测链条：

1. 容器能读到宿主用户级 WinINET 设置：
   `HKCU\...\Internet Settings\ProxyServer = 127.0.0.1:10808`（读得到，写不了）。
2. PowerShell 5.1 的 `Invoke-WebRequest` **不使用** `[Net.WebRequest]::DefaultWebProxy`，
   而是取 `[Net.WebRequest]::GetSystemWebProxy()` —— 实测前者指向我们的代理、
   后者指向用户代理 `127.0.0.1:10808`。
3. 我们为代理加了**回环豁免**，该豁免按容器粒度生效、**无法限定端口**，于是用户代理
   也随之可达。请求直接打到用户代理，`[sandbox.net]` 判定完全不参与。
4. 实测表现：`allowlist = [registry.npmjs.org]` 下
   * 只请求 `example.com` → 被拦（403 + 拒绝记录）；
   * **先请求白名单域名、再请求 `example.com` → 返回 200，代理零记录**（连续 3 轮稳定复现）。

`example.com` 本身不解析到回环，说明请求是经用户代理出网的。

### 20.5 为什么没有在本层堵上它

容器**写不了**注册表（`Set-ItemProperty` 报「不允许所请求的注册表访问权」，连自建键
`HKCU\Software\OmniCrawlProbe` 都建不了），所以无法在容器内关掉 WinINET 代理。
宿主侧改 `HKCU` 会动用户自己的浏览器代理设置，不可接受。

沙盒内额外做的两件事只能收窄、不能关闭：

1. 不继承宿主代理变量——容器不会「直接」拿到用户代理的地址。
2. 把 `[Net.WebRequest]::DefaultWebProxy` 指向我们的代理——覆盖走它的客户端
   （`HttpClient`、`WebClient`）。**但** PowerShell 5.1 的 `Invoke-WebRequest` 取
   `GetSystemWebProxy()`（读 WinINET），实测仍指向用户代理，这就是 §20.4 的复现路径。

因此本层能做的到此为止。三个可选方案的取舍与最终决定见 §20.1。

### 20.6 已核验未受影响的部分

- 文件隔离、删除权限、资源限额、解释器可读性（P1/P2）均不受影响。
- `offline` 策略下实测确实取不到数据（`offline-blocked`），失败方向正确。
- `online` 策略下经代理取到真实数据（`registry.npmjs.org` 返回 200）。

### 20.7 上线形态（方案 B 的完整语义）

以方案 B 交付后，`mode = appcontainer` 的实际行为：

| 维度 | 行为 |
| --- | --- |
| 出网能力 | 容器**没有任何网络能力**，裸 `socket()` / 裸 IP 直连直接失败（内核层拒绝） |
| 出网通路 | 只有宿主回环代理；地址经 `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` 注入 |
| 判定 | 统一走 `Sandbox::check_target`：回环 → 已批准缓存 → deny → offline/ask/allowlist/online |
| 覆盖到的客户端 | 尊重代理变量的：`curl`、`pip`、`npm`、`git http(s)`、`cargo`、`go mod`、`HttpClient`/`WebClient` |
| 覆盖不到 | 直接读 WinINET 的 .NET 客户端（PowerShell `Invoke-WebRequest`）、非 HTTP 协议、裸 TCP/UDP |
| 失败方向 | 拉起失败即不注入代理变量（彻底断网），不放宽成「有能力但代理不通」 |
| 严格模式 | `policy = "offline"` 连代理都不起，语义变成「沙盒内不能联网」 |

**`ask` 策略当前的落点**：没有审批通道（`Approver` 注入）时一律**拒绝**并记一条拒绝记录。
这是刻意的——默认配置不能变成静默放行。要做成弹窗审批需另行接 TUI 面板。

### 20.8 TUI 设置页「沙盒」

`[sandbox]` 段此前**只有读取没有写回**，因此只能在 `config.toml` 里手改或走 `/sandbox`。
本节记录补齐的写回路径与界面对映关系。

**配置层**：`omnicrawl_config::features::sandbox::save_sandbox_config`。两条纪律：

1. **先校验后写盘**。`validate` 不通过则一个字都不写（文件保持原样），
   避免存下一份跑不起来的配置（`allowlist` 空白名单、进程上限 < 2）。
2. **只合并、不整段替换**。`fs`/`net`/`limits` 三层子表分别合并：只改网络策略时
   不该丢掉 `fs.writable`；本模块不认识的键也原样保留。

**界面对映**（`FormKind::Sandbox`，12 个字段，下标与写回顺序一一对应）：

| 字段 | 配置键 | 类型 |
| --- | --- | --- |
| 启用 | `enabled` | 开关 |
| 隔离模式 | `mode` | 候选（`off` / `appcontainer`） |
| 额外可写路径 | `fs.writable` | 文本（逗号分隔） |
| 额外可读路径 | `fs.readable` | 文本（逗号分隔） |
| 网络策略 | `net.policy` | 候选（`offline`/`ask`/`allowlist`/`online`） |
| 允许的域名 | `net.allow` | 文本（逗号分隔，支持 `*.suffix`） |
| 拒绝的域名 | `net.deny` | 文本（逗号分隔，优先级高于放行） |
| 审批超时（秒） | `net.approval_timeout_seconds` | 正整数 |
| 记录全部网络请求 | `net.log_all` | 开关 |
| 进程上限 | `limits.max_processes` | 正整数 |
| 内存上限（MB） | `limits.max_memory_mb` | 正整数 |
| CPU 速率上限（%） | `limits.cpu_rate_percent` | 非负整数（0 = 不限） |

**生效时机**：保存后必须重建工具表。沙盒在构造 `ToolRegistry` 时由 `build_sandbox`
折算成运行期 `Sandbox`，只写盘要等下次启动。重建失败时如实回一句「本次会话仍按旧策略
执行，下次启动生效」，不假装成功。

一级项插在「会话管理」之后，是 `ROW_ORDER` 的第 21 项。

## 21. P4 实施记录：沙盒内的工具表与 PowerShell 版本（2026-11-06）

### 目标

两件事，都在「启动后就按沙盒形态把工具表和解释器摆正」这一层：

1. 真隔离下**自动禁用 `bash`、自动启用 `powershell`**（原先只是调用时拒绝）；
2. 沙盒内 PowerShell **优先用 7.\* 最新版本**，不通再回退 5.1（原先硬编码 5.1）。

### 21.1 工具表：启动即摆正，而不是调用时才报错

> **已被 §24 取代（2026-11-10）**：`bash` 在沙盒内改由自带的 `omnicrawl-brush` 执行，
> 因此不再自动禁用 `bash`、也不再自动启用 `powershell`——工具表完全按 `[tools]` 来。
> 下面这段保留的是当时的推理，作为历史记录；其中「沙盒内没有任何执行命令的途径」
> 的前提已不成立。

`[tools]` 段默认把 `powershell` 关掉（其余开）。这在宿主侧合理——多数人用 Git Bash；
但在真隔离下 `bash` 必然失败（§19 已论证 `NUL` 设备不可达，无法靠授权绕过），于是
「关掉 powershell + bash 不可用」等于沙盒内**没有任何执行命令的途径**，而模型只会
看见一个注定报错的 `bash`。

`registry.rs::sandbox_tool_switches` 在构建工具表前做一次折算：

| 输入 | 输出 |
| --- | --- |
| `sandbox.isolates()` 为假 | 原样返回 `[tools]` 的禁用表（沙盒关闭时行为零变化） |
| 真隔离 | 禁用表加入 `bash`，并且**移除** `powershell` |

两条纪律：

- **只改禁用表，不改配置**。用户配置里 `powershell = false` 保持原样，沙盒关闭后
  行为照旧；这是「怎么执行」与「工具存在吗」两个正交维度的边界（§9）。
- **白名单仍然最后生效**。`allowed_tools` 在 specs 生成后再过滤一次，
  它不会因为沙盒补了 `powershell` 就凭空多出权限。

调用期的那道拒绝仍然保留（`registry.rs::run_command` 与 `monitor` 的
`BASH_UNAVAILABLE_IN_SANDBOX`）：工具表是「模型看得见什么」，拒绝文案是「万一还是被
调用，怎么教它改写」。两者不互相替代——白名单之外的路径进不来，但声明与执行都查同一份
specs，留着更稳。

### 21.2 PowerShell 版本：容器内实测，不通即回退

`runtime.rs::powershell_candidates` 给出候选序列：

1. `%ProgramFiles%\PowerShell\<版本>\pwsh.exe`，按目录名里的数字段**倒序**（`7.10` 在
   `7.9` 之前——字符串比较会给出相反答案），同版本正式版排在 `preview` 前面；
2. `PATH` 上的 `pwsh.exe`；
3. `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`（5.1，恒定最后）。

`Isolation::powershell` 逐个在容器里跑 `Write-Output '__omnicrawl_probe__'`，只认
**stdout 里出现探针字符串**。不用退出码做判据：pwsh 7 在容器里可能以 0 退出却一个
cmdlet 都没加载。

### 21.3 实机结论（本机，pwsh 7.5.8）

| 检查项 | 5.1 | pwsh 7.5.8 |
| --- | --- | --- |
| 容器内能启动 | ✅ | ✅ |
| `$PSHOME` 正确 | ✅ | ✅（`C:\Program Files\PowerShell\7`） |
| `$env:PSModulePath` 含自身 `Modules` | ✅ | ✅ |
| 该 `Modules` 目录对容器可读 | — | ✅（`ALL APPLICATION PACKAGES: RX`） |
| `Write-Output` 可用 | ✅ | ❌ 报「无法识别」 |
| `Microsoft.PowerShell.Commands.*` 类型可解析 | ✅ | ❌ |

也就是说 **7.5.8 在 AppContainer 内仍然加载不出 cmdlet**，与 §17 的旧结论一致；
本次补的两条排查项（`$PSHOME`、自身 `Modules` 是否可读）都不是原因，根因尚未定位到
单个环境因子。因此本机的实际结果是**自动回退到 5.1**，行为与改动前一致；
在别的机器上若 pwsh 能跑，就会真的用上 7.*。

`PSModulePath` 仍然做了补充：把各候选解释器自带的 `Modules` 目录放进沙盒环境块
（存在才收）。它不改变上面的结论，但去掉了「模块目录缺失」这一个可能的原因，
也是 7.* 在别的机器上能跑起来的前提。

### 21.4 验证

- `sandbox::runtime` 新增 3 条：版本按数字排序（`7.10` → `7.9` → `7` → `6`）、
  5.1 恒定最后、同版本正式版胜出 `preview`。11 条全通过。
- `tools::registry::tests::appcontainer_swaps_bash_out_of_the_table_for_powershell`：
  真隔离下 `bash` 不在表里、`powershell` 在表里、且 `execute("bash")` 返回 `None`。通过。
- `tools::registry::tests::a_disabled_sandbox_leaves_the_tool_table_alone`：
  沙盒关闭时禁用表一字不改。通过。
- `tools::registry::tests::appcontainer_prefers_powershell_7_when_it_works`：
  装了 pwsh 时，若探测选中它则实际版本必须是 7.*；若探测判定不可用则只要求不报错。
  通过（本机走的是回退分支）。
- 既有端到端 `appcontainer_isolates_the_powershell_tool` 仍通过：隔离标记、工作区可写、
  宿主用户目录不可读。

## 22. P4 补记：PowerShell 7 的根因（2026-11-06）

§21 记录的结论是「pwsh 7.5.8 在容器内加载不出任何 cmdlet，原因未定位」。本节把它查到了底，
结论**推翻了此前「PowerShell 7 在 AppContainer 内根本不可用」的判断**。

### 22.1 真正的根因：系统盘根不可遍历，`C:` 驱动器建不起来

排查路径（每条都是在容器内实跑）：

| 检查 | 结果 |
| --- | --- |
| `$ExecutionContext.SessionState.LanguageMode` | `FullLanguage`（语言引擎正常） |
| `$PSHOME` | `C:\Program Files\PowerShell\7`（正确） |
| `[IO.Directory]::GetDirectories("…\7\Modules").Count` | 14（模块目录**可读**） |
| 直接 `Assembly.LoadFrom("…\Commands.Utility.dll")` | 成功 |
| `Write-Output` / `Get-ChildItem` | ❌「无法识别」 |
| `Import-Module "…\Modules\Microsoft.PowerShell.Utility"` | ❌ `Cannot find drive. A drive with the name 'C' does not exist.` |
| `[IO.Directory]::Exists("C:\")` | **False** |
| `[IO.Directory]::GetFileSystemEntries("C:\Windows")` | 124（子目录可读） |

关键两行：**`C:\` 根不存在于容器的视图里，但它下面的 `C:\Windows` 可读**。原因是
`Isolation::prepare` 只在**工作区所在的祖先链**上补遍历 ACE；本机工作区在 `D:` 盘，
于是 `C:\` 不在祖先链上，容器对它没有 `FILE_TRAVERSE`。

PowerShell 7 启动时要建立 `C:`（系统盘）驱动器来加载自己的内置模块，它先用
`Directory.Exists("C:\\")` 探路，拿到 False 就放弃：

```
The specified drive root "C:\" either does not exist, or it is not a folder.
  → C: 驱动器没建起来
  → Import-Module "…\Modules\Microsoft.PowerShell.*" 全部失败
  → 所有内置 cmdlet 都没有注册
  → 连 Write-Output 都报「无法识别」
```

Windows PowerShell 5.1 不需要这一步（它的 cmdlet 直接从 `$PSHOME` 的模块目录加载，
不依赖盘符驱动器），所以**同一个容器里 5.1 一切正常**。这就是「只有 pwsh 坏掉」的全部原因，
与语言模式、`PSModulePath`、权限都无关。

### 22.2 修法

`Isolation::prepare` 增加一步：给 `system_drive_roots()`（系统盘根）补
`TRAVERSE_ONLY`（不可继承）遍历 ACE，仅在 `can_traverse` 判定不足时才写。

- 代价极小：卷根本来就有一批同类 ACE，且这条不可继承、不触发子树传播；
- 失败不致命：写不进去时容器内仍会回退到 5.1，命令照跑。

修完后同一条容器内实测：`Write-Output` → `write-output-ok`、`Get-ChildItem
"C:\Program Files\PowerShell\7"` → 346 项、`$PSVersionTable.PSVersion` → `7.5.8`。

### 22.3 结论修订

- §17 第 5 条、§19「为什么不接 Git Bash / 改用别的 shell」里「pwsh 在 AppContainer 内
  加载不出任何 cmdlet → 5.1 是唯一可行 PowerShell」的结论**作废**：那是系统盘根缺少
  遍历权导致的，不是 PowerShell 7 的限制。
- `Isolation::powershell` 的容器内实测保留：它现在能真正选到 7.*，同时仍能在
  异常环境里安全回退。回退不再是「常态」，而是兜底。
- `PSModulePath` 补各候选解释器自带 `Modules` 目录的改动保留（无害且是别处的必要前提）。

### 22.4 验证

- 新增 `sandbox::windows::tests::appcontainer_can_traverse_the_system_drive_root`：
  容器内 `Directory.Exists(系统盘根)` 必须为 True（首次会为卷根授权，约 80 秒；
  之后幂等）。
- `tools::registry::tests::appcontainer_prefers_powershell_7_when_it_works` 收紧为
  **必须选中 pwsh** 且版本为 7.*——修复前它会失败（探测一路回退到 5.1），正是这条守住了根因。
- `cargo test -p omnicrawl-host --lib sandbox`：68 通过 / 0 失败。
- `tools::` 全量 482 通过 / 4 失败，4 项仍是既有环境基线（无效窗口句柄 42、
  受限桌面会话下截图被拒），与本次改动无关。

## 23. P5 实施记录：沙盒内的外部进程与配置自愈（2026-11-07）

### 23.1 三个实测根因（都不是「权限不够」）

| 现象 | 真因 | 结论 |
| --- | --- | --- |
| `cargo`/`rustc`/`rustup` 在容器内报 `rustup could not choose a version of cargo to run` | `USERPROFILE`/`HOME` 被改指向沙盒内的空家目录，rustup 的 shim 据此找不到 `~/.rustup` | 授权再多目录也没用，必须显式给 `RUSTUP_HOME` |
| `git --version` 就报 `could not open '/dev/null'` | `\Device\Null` 的 DACL 是 **NULL DACL**（`NO_ACCESS_CONTROL`）；AppContainer 是受限令牌，双 SID 链检查没有任何可匹配 ACE | 任何访问掩码都是 `ERROR_ACCESS_DENIED`，**无法靠授权解决** |
| 设置面板保存报「配置项 sandbox.fs.readable 必须是字符串数组」 | `config.toml` 里被写成了空表头 `[sandbox.fs.readable]`，解析成一个空表 | 空表应等价于空列表，而不是类型错误 |

### 23.2 落地内容

| 文件 | 内容 |
| --- | --- |
| `omnicrawl-config/src/features/sandbox.rs` | `string_list` 接受**空表**（读作空列表）；非空子表返回带修法的错误 |
| `omnicrawl-host/src/sandbox/runtime.rs` | 新增 `rust_roots`：发现 `~/.cargo/bin` 与 `~/.rustup`，**不含**含凭据的 `~/.cargo` 本体 |
| `omnicrawl-host/src/sandbox/windows.rs` | 环境块补 `RUSTUP_HOME`（真实目录，不存在时不给）与 `CARGO_HOME`（沙盒内可写）；`sandboxed_path` 只收录**已授权**的解释器根；新增 `grant_runtime_roots` 按 `GRANT_ENTRY_BUDGET`（1 万项）自动授予只读执行权，已授过的零成本跳过 |
| `omnicrawl-host/src/tools/git.rs`、`tools/registry.rs` | 拒绝文案改为实测根因（NULL DACL + 受限令牌），并保留两条真实替代（关沙盒 / 用文件工具） |

### 23.3 为什么 git 仍然「拒绝」而不是想办法跑起来

`NUL` 打不开是设备对象 DACL 与受限令牌共同决定的，属于**内核对象语义**，不是文件权限。
文档 §19 曾按「`git --version` 能过」记录，本轮实测推翻了它：连 `--version` 都过不去。
给设备对象补 ACE 的路线**有害**：NULL DACL 一旦被写入一条 ACE 就变成「只放行该 ACE」，
会把整台机器的 NUL 一起打坏（本轮实际发生过，只能重启恢复）。因此正确做法是换一种
**执行形态**，而不是用运行期手段去改设备 ACL。这一条已在 §24 落实。

### 23.4 自动授权的成本控制

`SetNamedSecurityInfoW` 每次都会重算整棵子树的继承，代价与条目数成正比。本机实测：
`~/.cargo/bin` 23 项、`~/.rustup` 5221 项、`~/.cargo/registry` 71591 项。所以：

- 前两者落在 1 万项预算内，自动授权，`cargo` 开箱可用；
- `registry`/`git` 缓存目录**不发现**——`CARGO_HOME` 已指进沙盒，本就不该读宿主缓存；
- 已授权的（DACL 上已有容器 ACE）直接跳过，不重复付费。

### 23.5 验证

- `sandbox/runtime.rs`：`rustc --test` 直编直跑 **12 通过 / 0 失败**（含新增
  `rust_roots_cover_bin_and_rustup_only`：`.cargo/bin` 与 `.rustup` 被发现、`.cargo` 本体不被授权）。
- `string_list` 语义：独立复刻 6 条断言全通过（空表→空列表、非空表→可操作错误、数组 trim 去空、
  单字符串、缺项、标量报错）。
- 新增用例（待 `cargo test` 可跑时执行）：`an_empty_table_header_reads_as_an_empty_list`、
  `a_non_empty_table_for_a_list_says_how_to_fix_it`、`rustup_home_is_taken_from_the_host_user_profile`、
  `cargo_home_points_inside_the_workspace`、`only_granted_runtime_roots_enter_the_sandbox_path`。
- **未完成**：`cargo test -p omnicrawl-host --lib sandbox` 与沙盒内 `cargo --version` 真机复测。
  本轮环境中 `\Device\Null` 的 DACL 被误改，`cargo` 无法启动 `rustc`
  （`failed to open NUL device for child stdin`），需重启系统后补跑。

## 24. 沙盒内的 `git`：改走进程内 gix（2026-11-08）

上一节把 `git` 判了「沙盒内不可用」，理由是 MSYS 运行时初始化就要打开 `/dev/null`，
而 `\Device\Null` 的 NULL DACL 对受限令牌一律拒绝。这个根因没变，但它否掉的是**某一种
执行形态**（spawn `git.exe`），不是「沙盒内不能做 git 操作」。

### 24.1 换形态，不放开隔离

`gix`（gitoxide）是纯 Rust 的 git 实现，在宿主进程内跑：不经 MSYS、不打开 `NUL`，
因此容器语义下那些约束一条都拦不住它。**代价是它天然不受 AppContainer 约束**，
所以约束必须由我们自己按子命令施加（`tools/git_native.rs`）：

| 子命令 | 作用域 | 说明 |
| --- | --- | --- |
| `status` / `log` / `diff` | 只读 | 只读对象库与工作区 |
| `rev-parse` / `ls-files` / `show-ref` / `for-each-ref` / `describe` / `cat-file` | 只读 | 解析与枚举，不动任何东西 |
| `remote` | 只读 | 只读配置里的远程名与 URL，不联网 |
| `stash`（无参数 / `list` / `show`） | 只读 | 仅列举 |
| `branch` / `tag`（无参数或 `-l`） | 只读 | 列举分支与标签 |
| `add` | 可写 | 只写工作区内的 `.git/index`，路径先过越界校验 |
| `commit` | 可写 | 只写对象库与 HEAD 引用；`--dry-run` 按只读处理 |
| `rm` | 可写 | 索引移除；带 `--cached` 时工作区文件保留 |
| `branch <name>` / `tag <name>` / 二者的 `-d` | 可写 | 建删引用；重名显式拒绝 |
| `fetch`/`pull`/`push`/`clone`/`ls-remote`/`submodule` | **拒绝** | 见下 |
| `merge`/`rebase`/`cherry-pick`/`checkout`/`restore`/`reset`/`stash` 写形态 | **拒绝** | 需要工作区状态机 |

作用域按**本次调用的实际参数**判定：`branch` 无位置参数是列举（只读），带名字才是新建（可写）。

网络子命令是**不允许**，不是「暂不支持」：沙盒的每个出网请求都要经 `[sandbox.net]`
判定（宿主回环代理），让 git 自己连出去就整段绕过了这套判定。这也是 gix 依赖不启
`blocking-network-client` 的原因——能力不在依赖图里，比「记得别调用」更可靠。

**绝不能**改成「沙盒模式下退回 spawn `git.exe`」：那等于让最该被约束的操作
（`push`、`config`、`submodule`）完全不受约束，比「跑不了」危险得多。

### 24.2 依赖裁剪

`gix 0.90`（`default-features = false`，只开 `basic` / `status` / `blob-diff` /
`attributes` / `excludes` / `command` / `sha1`）。实测：无 C 依赖（zlib 走纯 Rust 的
`zlib-rs`），冷编译约 16 秒，共 112 个依赖包。

### 24.3 两个真机端到端才暴露的缺陷

1. **`head_id()` 是提交，不是树**：构树前必须先取 `tree_id`。把提交 id 直接喂给
   `edit_tree` 会在运行时被判「本该是 tree，却是 commit」。单测的仓库都是新 `init` 的
   （无 HEAD），走空树分支，**覆盖不到**这条；是 AppContainer 真机跑通时暴露的。
2. **重复建同名分支被放行**：`reference(…, PreviousValue::MustNotExist, …)` 在当前
   组合下并不会拒绝已存在的引用，于是「再建一次」也报成功。现在建引用前显式
   `find_reference` 查存，重名直接拒绝——与真 git 的
   `fatal: a branch named 'x' already exists` 对齐。

第 2 条的教训是：**底层约束的行为要当成假设去验证**，不能因为参数名叫
`MustNotExist` 就认为它会拒绝。两条都已固化成用例
（`first_commit_in_a_fresh_repository`、`branch_and_tag_writes_are_visible_to_real_git`）。

### 24.4 仍未覆盖

`merge`/`rebase`/`revert`/`cherry-pick`/`checkout`/`restore`/`reset` 与 `stash` 的写形态，
以及逐行统一 diff 的文本渲染，都还没有进程内实现。原因是它们要跑**工作区状态机**
（冲突解决、部分检出、可中断重放），gix 的高层入口只覆盖 linked worktree，硬接等于把
「半对」的行为交给模型。这些子命令在沙盒内会得到明确拒绝并提示改在宿主侧执行，
不会悄悄退回 spawn `git.exe`。

### 24.5 验证

- `cargo test -p omnicrawl-host --lib`：**647 通过 / 0 失败**（git 相关 16 项）。
- `tools/git_native.rs` 单测 10 项：网络子命令拒绝、不支持动作指出可用集合、作用域判定、
  add→commit→log 往返、首次提交、非仓库目录报错、只读子命令实答、branch/tag 建删可见、
  `rm --cached` 保留工作区文件、写 stash 与 merge 明确拒绝。
- **真机 AppContainer 全量**（`mode = appcontainer`，`plan().is_isolated() == true`）：
  `rev-parse`、`ls-files`、`show-ref`、`for-each-ref`、`describe`、`cat-file`、
  `branch`（列举/新建）、`tag`（列举/新建）、`stash`（列举）、`remote`、`add`、
  `rm --cached` 全部成功；`push` 被拒。
- **真 git 交叉验证**：`git log` 读到 gix 写的提交、`git show --stat` 内容正确、
  `git status` 干净、`git fsck` 无完整性错误；`git show-ref`/`git branch`/`git tag`
  与进程内输出一致；空仓库首次提交同样通过。
- `cargo check --workspace` 通过；`git_native.rs` clippy 零告警（其余为既有基线）。

## 25. `bash` 接上 brush：沙盒内不再禁用 bash（2026-11-10）

### 25.1 背景

§19/§21 的结论是「沙盒内 `bash` 不可用，改用 PowerShell」。根因没变：Git Bash 是
MSYS/Cygwin 程序，初始化时要打开 `/dev/null`，而 `\Device\Null` 是 NULL DACL，
AppContainer 受限令牌拿不到任何 ACE。但可以**换执行形态**：与 §24 的 `git → gix`
同构，用一个纯 Rust 的 bash 兼容 shell 顶上。

### 25.2 实现

- 新 crate `omnicrawl-brush`：`brush-shell::entry::run()` 的薄入口，产出
  `omnicrawl-brush[.exe]`。它是**独立可执行文件**，仍由沙盒以容器身份 spawn。
- `tools/command.rs`：
  - `find_sandboxed_bash()`：先找主程序同目录，再找 PATH（刻意不按名字 `bash` 找，
    那会撞到 Git Bash 或 WSL 启动器）。
  - `invocation_for(..., isolated: true)` 与 `invocation_for_isolation()` 的
    `Shell::Bash` 分支改用它；参数形状与 Git Bash 逐字相同
    （`-o pipefail -lc <script>`，brush 的 `-o` 即 `set -o`）。
- `tools/registry.rs`：删掉 `run_command` 里的 `BASH_UNAVAILABLE_IN_SANDBOX` 拒绝分支；
  删除 `sandbox_tool_switches`（沙盒不再改写工具表），`disabled_tools` 直接用
  `[tools]` 的配置。
- `tools/monitor.rs`：删掉后台任务对 `bash` 的同样拒绝（它走
  `invocation_for_isolation`，自动获得 brush 通路）。
- 分发：`packages/cli/scripts/build-host.mjs`（`-p` 构建列表 + `HOST_FILES` 校验）、
  `prepare.mjs`、`install-smoke.mjs` 三处清单都加入 `omnicrawl-brush`。

### 25.3 为什么不能让 brush 跑在宿主进程内

`brush-core` 是库，能在宿主进程里解释脚本，看似更省事。但它 spawn 外部命令时
用的是宿主的 token——脚本里的每条 `cargo`/`python` 都会以宿主权限执行，Sandbox
形同虚设。`gix` 之所以能进程内跑，是因为它**只操作仓库文件**、不执行外部程序；
brush 的职责恰恰是执行任意命令。这是两者根本的区别，所以必须是独立进程。

### 25.4 未完成的验证（重要）

本次改动**没有经过编译验证**。原因是被测对象正是"沙盒能否编译"：当前宿主进程以
`[sandbox:appcontainer]` 身份运行，实测两条路都不通：

- `cargo build`：`failed to open NUL device for child stdin: 拒绝访问。 (os error 5)`
  —— cargo 每次 spawn rustc 都要打开 `NUL`，与 git/bash 同一根因。
- 直连 `rustc`（绕开 cargo）可以编译，但**读不到 crate 源码缓存、也没有网络**：
  `C:\Users\Administrator\.cargo` 被拒访问，`CARGO_HOME` 被重定向到
  `.omnicrawl/.agent_tmp/cargo`（空），`static.crates.io` 超时。因此无法拉取
  `brush-shell` 及其依赖（`brush-core`、`brush-builtins`、`brush-interactive` 等）。
- 已做的替代检查：四个改动文件用 `rustc --emit=metadata` 单文件解析，**零语法错误**
  （仅剩模块路径/C 依赖类错误，属单文件编译的必然结果）。

落地前必须在能编译的环境里补做：`cargo build --release -p omnicrawl-brush`、
`cargo test -p omnicrawl-host --lib`（关注 `appcontainer_keeps_the_tool_table_as_configured`、
`bash_refusal_names_the_missing_brush_executable` 两条改名后的用例）、
以及真机 AppContainer 下 `bash -c 'echo hi'` 与 `cat/管道/退出码` 的实跑。
