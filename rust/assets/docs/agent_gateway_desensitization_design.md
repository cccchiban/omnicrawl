# 「Agent 网关」消息脱敏设计文档（可逆占位符 `｛Desensitized:n｝`）

> 文档性质：现行系统设计（已实现并运行）。本文与源码核对于 2026-09-12；如与代码、测试不一致，以代码与测试为准。
> 何时读取：需要理解、调试或修改出网消息脱敏（占位符、匹配引擎、序号注册表、流式还原、`[desensitization]` 配置、设置面板「消息脱敏」页）时。
> 快速落点：核心模块 `omnicrawl/llm/desensitization/`；值类型规则 `rules.py`、gitleaks 规则 `gitleaks.py`（快照 `gitleaks.toml`）；配置 `omnicrawl/config/features/desensitization.py`；接线 `omnicrawl/llm/registry.py`；测试 `tests/test_desensitization.py`、`tests/test_desensitization_settings.py`、`tests/test_desensitization_rules.py`。

## 1. 定位与目标

OmniCrawl 以「本地 Agent + 模型服务」形态工作：宿主把对话消息（用户输入、助手历史、工具调用、工具结果）发往模型服务，模型返回下一步动作。这些消息里可能出现业务敏感值——密钥、口令、令牌、连接串、个人信息等。值一旦随请求出网，就脱离了本机控制。

本层只在「发往模型的那一份消息副本」上做**可逆脱敏**：

1. 命中敏感值 → 替换为占位符 `｛Desensitized:<序号>｝`；模型只能看到占位符；
2. 模型基于占位符继续工作（引用、搬运、写入文件都以占位符表达）；
3. 模型返回后，按序号还原成原文，交回宿主；
4. 本地业务（工具执行、会话、UI）维持原文语义——「脱敏但又不影响业务」。

一句话：**原文不出本机；模型只见代号；代号在会话内可反复还原。**

核心概念：

- **AI 消息**：一次模型请求携带的对话消息（发往模型），以及模型返回的对话内容；含工具调用消息与重思考内容。
- **占位符**：`｛Desensitized:n｝`；`Desensitized` 是固定英文标记，`n` 是序列号。
- **发送-接收周期（周期）**：一次逻辑模型请求从发出到收到最终可用回复的过程；同一请求的重试属于同一周期。
- **还原组装**：把模型返回内容中的占位符替换回原文、组装为最终回复的过程。

### 1.1 与既有「脱敏」机制的区别（防混淆）

本项目另有一套**不可逆**的值级脱敏（落盘、审计、连接器显示用，敏感值替换为 `***`）。两者职责不同、可并存：

| 维度 | 既有值级脱敏（`***`） | 本文「消息脱敏」 |
|---|---|---|
| 方向 | 本地落盘 / 展示 / 审计 | 出网消息（发给模型） |
| 可逆性 | 不可逆 | 可逆（序号注册表还原） |
| 数据面 | 会话事件、日志、飞书卡片等 | AI 消息（含工具调用消息） |
| 实现 | `omnicrawl/mcp/security.py`、`omnicrawl/state/session_artifacts.py` 等 | `omnicrawl/llm/desensitization/`（本层） |

本层**不写任何原文到日志、会话事件、SSE、异常与缓存**；原文只进内存注册表，且用完即销（§7.1、§10.2）。

## 2. 处理范围（IN/OUT）

### 2.1 处理范围（IN）

- 发往模型：统一消息模型（`omnicrawl/llm/protocol.py`）中的
  - `TextBlock.text`（user / assistant 文本）；
  - `ToolCallBlock.arguments`（工具调用参数，dict 递归找值）；
  - `ToolResultBlock.content`（工具结果文本，含其中可解析的结构片段）；
  - `ConversationMessage.reasoning`（思考内容，历史回传用）。
- 模型返回：`TextDelta` / `ReasoningDelta` 累积文本、`ToolCallCompleted.arguments`（工具调用参数）。

### 2.2 明确排除（OUT，附理由）

- **请求参数**：模型名、鉴权、生成参数（temperature、effort 等）、超时与重试、HTTP 外层字段——不改动，避免请求失败。
- **工具 Schema 与动态工具声明**：`ToolSpec.name/description/parameters`、message 携带的 `tools` 字段——属结构定义，改动会破坏工具注册与调用。
- **`system_prompt` 指令文本**：宿主生成、语义为指令；默认豁免（不参与屏蔽，但参与序号碰撞扫描，见 §6.3）。
- **图片块 `ImageBlock`**：base64 图片无文本匹配语义，不参与。
- **结构件**：`role`、`call_id`、工具名、`tools` 字段、`prompt_cache_identity`。
- **本地行为**：宿主、会话、UI、工具执行、既有落盘机制均维持原文与既有语义（本层只在「出网前」和「回收后」两个瞬间变换）。

**边界原则**：屏蔽与还原都只作用于「消息内容中的值」；键、结构、身份标识一律不动。替换只发生在字符串内部，不改变消息结构（不产生非法 JSON、不触发上游 400 类错误）。

## 3. 总体结构与数据流

```text
                 原文                         屏蔽后的副本
┌───────────────────┐    ┌───────────────────────────┐    ┌─────────────────┐
│ Agent 宿主         │    │ Agent 网关（本层）          │    │ 模型服务 / 网关  │
│ 回合循环/工具/会话  │───▶│ ① 匹配（键名/结构+熵兜底） │───▶│ 只能看到         │
│ （始终持原文）      │    │ ② 替换 ｛Desensitized:n｝    │    │ ｛Desensitized:n｝ │
│                   │    │ ③ 注册 n → 原文（仅内存）   │    │                 │
└───────────────────┘    └───────────────────────────┘    └─────────────────┘
        ▲                          │                              │
        │  还原后的回复（原文）      │ ④ 按 n 还原                  │ 带占位符的
        │                          │ ⑤ 周期完成 → 注销            │ 返回内容
        └──────────────────────────┴──────────────────────────────┘
```

当前形态为 **OmniCrawl 进程内中间件**（统一运行时装饰器，§4）。

核心组件（与实现文件的对应关系）：

| 组件 | 职责 | 实现 |
|---|---|---|
| 匹配引擎 | 键名 / 结构感知 + 值类型规则 + 熵检测兜底，产出「待脱敏值」并完成替换 | `omnicrawl/llm/desensitization/engine.py` |
| 值类型规则层 | PEM / 连接串 / 邮箱 / 银行卡 / IP / URL / MAC / 车牌 / gitleaks 的正则识别与重叠去重 | `omnicrawl/llm/desensitization/rules.py`、`gitleaks.py` |
| 序列号注册表 | 分配 / 复用 / 查询 / 注销序号；原文仅内存驻留 | `omnicrawl/llm/desensitization/registry.py` |
| 出站屏蔽器 + 入站还原器 | 运行时装饰器：屏蔽 `request.messages`、逐事件还原 | `omnicrawl/llm/desensitization/middleware.py` |
| 流式还原状态机 | 尾部挂起缓冲、占位符还原、告警 | `omnicrawl/llm/desensitization/stream.py` |
| 旁路一次性脱敏器 | 非统一运行时链路（审批审查）的单次屏蔽 / 还原 | `omnicrawl/llm/desensitization/oneshot.py` |
| 配置与审计 | `[desensitization]` 配置；仅计数 / 规则 / 序号级观测 | `omnicrawl/config/features/desensitization.py` |

生命周期总览：

```text
[出站] 命中敏感值 ──▶ 分配序号 n、注册（n → 原文，内存）
                          │
[入站] 返回含 ｛Desensitized:n｝ ──▶ 还原为原文（可多处）
                          │
[完成] 本周期还原组装结束 ──▶ 释放周期自身状态（会话级序号映射保留）
[异常] 周期未完成（失败 / 取消 / 超时 / 截断）──▶ 不释放，遗留
[关闭] 运行时关闭 ──▶ 丢弃会话级「序号 → 原文」映射（不落盘、不恢复）
```

## 4. 接线与覆盖范围

### 4.1 主接线（统一运行时装饰器）

在 `omnicrawl/llm/registry.py::build_runtime()` 返回处包一层「脱敏运行时」（`maybe_wrap_runtime`）：

- 配置未启用或配置不可读 → 原样返回内层运行时（零成本）；
- 启用 → 返回 `DesensitizationRuntime`：实现 `ModelRuntime` 协议，`identity`/`capabilities` 透传内层；
- `stream_turn` 入口对 `request.messages` 做屏蔽并注册序号；出口对事件流逐事件还原、关闭本周期（会话级映射保留）；
- 对上层完全透明：上层协议、事件消费、回调（`on_delta` / `on_reasoning_delta`）、回复组装、后续工具执行全部拿到**还原后**的内容；
- 协议无关：四种 Provider 适配器（OpenAI Chat Completions / Responses、Anthropic Messages、Gemini Generate Content）无需改动；
- `close()`：丢弃未完成周期的请求副本（`drop_all`，**不释放会话级映射**），再关闭内层运行时；
- `store_provider`：会话所有者把当前会话的「序号 → 原文」映射传给运行时（`build_runtime(..., store_provider=...)` → `maybe_wrap_runtime(..., store_provider=...)`），同一会话内重建的运行时共享同一份映射（§7.2）。

### 4.2 覆盖链路

以下链路均在各自 `ModelRuntimeManager().bootstrap()`/`switch()` 中经 `build_runtime()` 创建运行时，自动受本层保护：

- 主回合（`omnicrawl/agent/controllers/turn/loop.py`）；
- SubAgent（`omnicrawl/agent/controllers/subagents/orchestration.py`）；
- 顾问（`omnicrawl/agent/controllers/advisor.py`）；
- 上下文压缩摘要（`omnicrawl/agent/context_compaction/summary.py`）；
- 视觉代理（`omnicrawl/agent/runtime/vision_proxy.py`）；
- 模型切换 / 设置（`omnicrawl/agent/controllers/session/settings.py`）。

### 4.3 旁路处理（不经统一运行时的模型调用）

| 旁路 | 处置 | 说明 |
|---|---|---|
| 审批自动审查（`omnicrawl/agent/controllers/tools/approval.py`，直接走 Responses API） | **已接入** | 用 `OneShotMasker` 对审查请求出站屏蔽、对审查结论还原；屏蔽失败按 fail-closed 中止（fail-open 配置时降级发送原文并告警） |
| 旧直连回退 `AgentLLMProtocol._request_via_openai_client` | **不接入** | 仅在未提供 `runtime_manager`（遗留最小夹具 / 嵌入调用）时触发；生产配置统一走统一运行时；docstring 已标注边界 |
| 旧版 `OpenAIResponseLLM.ask / ask_stream`（`omnicrawl/config/models/llm_client.py`） | **不接入** | 全仓库无调用方（仅类定义与兼容导出） |

### 4.4 配置读取时点

- 配置在**每次构建运行时**时读取（`maybe_wrap_runtime`）；未启用或读取失败不影响运行时构建。
- 设置面板或手工修改 `config.toml` 后，对**已构建**的运行时不生效：需切换模型或重启 TUI（设置面板有同款提示）。

## 5. 匹配引擎

### 5.1 结构感知层（主）

**目标**：对「有键名 / 有结构」的值做高置信匹配。

- 结构来源：
  - 已解析结构：`ToolCallBlock.arguments`（dict）——递归遍历值；
  - 文本内的可解析片段（`mask_text` 依次处理）：
    - `.env` / shell 赋值：行首 `KEY=VALUE`（支持 `export` 前缀与引号包裹）；
    - `key: value`（YAML / TOML 行内片段，冒号后要求空白，避免误伤 URL 等形态）；
    - JSON 字符串值对（含多行文本中的片段）。
- 键名规则：
  - 默认敏感键词表：种子与 `omnicrawl/mcp/security.py::_SENSITIVE_FIELD_NAMES` 同步（`api_key` / `apikey` / `access_key` / `secret_key` / `authorization` / `cookie` / `password` / `secret` / `token` / `access_token` / `refresh_token` / `id_token`，由测试守护一致性），并扩展 `passwd` / `pwd` / `credential` / `private_key` / `session` / `csrf` 与中文词（密码 / 密钥 / 令牌 / 身份证 / 手机号 / 银行卡 / 口令）；
  - 归一化匹配：大小写不敏感，`-` / 空格归一为 `_`，折叠重复下划线；支持下划线分段命中与简单复数（`tokens` → `token`）；
  - 豁免表优先于命中：默认含 `public_key` / `example`（如示例占位值）；敏感键与豁免键均支持用户扩展（`extra_sensitive_keys` / `exempt_keys`）。
- 值约束：
  - 只处理「值」；键、结构、类型标识不动；
  - 只处理字符串值（非字符串标量不参与）；空串、已是 `***` 脱敏串、已是占位符样式串跳过；
  - 敏感键之下的子树：所有字符串叶子都视为值并替换；非敏感键下的字符串仍做文本级匹配（覆盖自由文本，避免长历史回程时的原文回声）。

### 5.2 熵检测兜底层（兜底）

**适用**：无键名、无法结构化、看起来像「裸值」的字符串（自由文本中的令牌、长随机串等）。

候选判定（`is_entropy_candidate`，参数均可配置）：

1. 长度 ≥ `entropy_min_length`（默认 20）；
2. 单类令牌开关（默认关闭）：`entropy_pure_letters` 开启后纯字母长度达标即候选（含 a-f 的十六进制字母串、`Desensitized` 字样、词形标识符仍跳过）；`entropy_pure_digits` 同理处理纯数字。词形豁免按 camelCase / PascalCase / 下划线切段，各段须落在 2–20 字符之间且至少半数段含元音，避免类型名、函数名与普通单词被当作秘密；
3. 形态白名单（`is_entropy_exempt`）跳过：UUID、全十六进制（含纯数字）、十六进制+冒号（MAC/IPv6 类）、前缀哈希（`sha256:…`）、语义化版本、日期时间、URL 与文件路径（含 Windows）、代码/序列化片段（含 `()[]{}'";,<>` 等标点）、命名链（蛇形/点分/命名空间/枚举）、赋值等号片段、短分段词形、无数字标识符；
4. 字符类混合：至少同时具备「字母 + 数字/符号」两类且总数 ≥ 2 类；
5. 香农熵 ≥ `entropy_min_bits`（默认 3.5 bit/char）。

扫描与替换：按长度下限生成可打印 ASCII 连续段正则，剥离两侧标点（`ENTROPY_TOKEN_STRIP_CHARS`）后判定；命中区间只替换值本体，标点与空白保留在原文中。

原则：**高熵 ≠ 秘密**。兜底层只补结构层覆盖不到的盲区，默认以控制误报为先（宁少勿滥）；更严的场景可用单类开关与阈值调参。

### 5.3 值类型规则层（正则，可按类别开关）

**适用**：形态确定、随机性低、且常常没有敏感键名的值类型。键名层要求有键、熵层要求高随机性，两者都覆盖不到这些类型：

| 类别 | 配置开关（默认） | 识别方式 |
|---|---|---|
| PEM 私钥 | `detect_pem_private_key`（**开**） | `-----BEGIN … PRIVATE KEY-----` 到 `-----END … PRIVATE KEY-----` 的整块；无 END 的截断场景匹配头 + base64 正文行 |
| 数据库连接串 | `detect_db_connection_string`（**开**） | URI 形态（`postgresql://` / `mongodb+srv://` / `redis://` / `jdbc:mysql://` / SQLAlchemy `+driver` 后缀等）与 ADO 键值形态（`Server=…;…;Password=…`） |
| 邮箱 | `detect_email`（关） | `local@domain.tld`；`example.com` / `localhost` 判为误报 |
| 银行卡 | `detect_bank_card`（**开**） | 12–19 位数字（可含单个空格 / 连字符分隔）+ Luhn 校验，排除全同数字 |
| 内网 IP | `detect_internal_ip`（**开**） | IPv4 / IPv6，`ipaddress` 判定为私有或链路本地；环回 / 未指定 / 组播不算 |
| 外网 IP | `detect_external_ip`（关） | IPv4 / IPv6，`ipaddress` 判定为公网可路由（`is_global`） |
| 网址 | `detect_url`（关） | `http://` / `https://` / `ftp://` |
| MAC 地址 | `detect_mac_address`（**开**） | `aa:bb:cc:dd:ee:ff` / `aa-bb-…` / Cisco `aabb.ccdd.eeff` |
| 中国大陆车牌 | `detect_license_plate`（**开**） | 省份简称 + 字母 + 5 位（普通）或 + D/F + 5 位数字（新能源） |

- 内置规则清单与实现：`omnicrawl/llm/desensitization/rules.py`；顺序即优先级，重叠区间由先命中的规则占位（如网址优先于其中的 IP）。
- 规则语义：`keywords` 文本级预过滤、`min_entropy` 熵下限、`validator` 形态校验（Luhn / `ipaddress` 分类）、`allowlist` / `stopwords` 豁免。
- 值尾部标点 / 空白留在原文（占位符只替换值本体），不破坏 JSON、引号与句子结构。
- 命中计数：`DesensitizationStats.rules_masked`。

#### 5.3.1 gitleaks 开源规则

- 默认加载**内置离线快照** `omnicrawl/llm/desensitization/gitleaks.toml`（上游 `config/gitleaks.toml` 的逐字副本，MIT，共 222 条规则；`pkcs12-file` 仅按文件路径匹配，纯文本链路跳过，实际可用 221 条）。
- `[desensitization].gitleaks_config_path` 指向自定义 `gitleaks.toml` 时，按规则 id 覆盖 / 追加；文件不可读或解析失败回退内置快照，不中断运行时（解析实现：`omnicrawl/llm/desensitization/gitleaks.py`）。
- 遵循 gitleaks 语义：`keywords` 预过滤、`entropy` 下限、`secretGroup` 指定秘密捕获组、规则级 / 全局 allowlist 的 `regexes` 与 `stopwords` 命中即跳过。
- 无法在「纯文本、无文件路径 / 无行上下文」下忠实执行的豁免条件保守跳过（`paths`、`commits`、`regexTarget = "line"`、`condition = "AND"`）——宁可多脱敏，不可漏脱敏。
- 兼容性归一：Go 的 `\z` → `\Z`；出现在模式中部的全局内联标志 `(?i)` 上提到开头；编译失败的规则整条跳过（上游 222 条在 Python 3.9 下全部可编译）。
- 批量扫描成本由 `keywords` 预过滤摊薄，且规则在运行时构建时解析一次、请求间复用（`MaskContext.pattern_rules`）。

#### 5.3.2 局部化扫描与结果缓存（性能）

`gitleaks:generic-api-key` 一条就占了全量扫描耗时的一半以上（42KB 文本：单条约 52ms、全部 229 条约 89ms），是长会话里最贵的单点。两项优化都不改变脱敏语义：

**局部化扫描（`PatternRule.locality`）**：只在「锚点候选起点」上用完整模式重匹配，而不是整段 `finditer`。登记方必须能证明两条前提——每个命中内部都含锚点正则的一次匹配、命中起点最多少于锚点起点 `prefix_max` 个字符。于是所有可能的命中起点都落在某个锚点命中点左侧 `prefix_max` 字符的窗口内；候选点升序 + 游标去重叠，复现 `finditer` 的「最左优先、互不重叠」语义。

**为什么不能用「窗口 finditer」**：`generic-api-key` 的值分支含无上界的 `[a-z0-9][a-z0-9+/]{11,}`，限制 `endpos` 会把长秘密截断，只脱敏前半段——这是漏脱敏。局部化只限制**起点**、`endpos` 仍是文本末尾，因此无上界后缀同样安全。

**形状守卫**：局部化登记值按原文逐字比对（前缀原文 + 上界 + 锚点分支原文），上游把 `[\w.-]{0,50}?` 放宽或重写关键字分支时比对失败，规则自动退回全量扫描（宁慢勿漏）。

**锚点标志**：锚点必须继承模式的全局内联标志（再叠加 `IGNORECASE` 保守放宽），否则 `SECRET = …` / `Key = …` 这类大小写变体会漏脱敏；含否定字符类 `[^…]` 的锚点一律拒绝登记（`IGNORECASE` 会收窄它，破坏「候选点是起点超集」的前提）。

**扫描结果缓存**：扫描是纯函数（规则 + 文本 → 区间），按 (文本, 规则集合) 缓存，带 4MiB 字节预算的 LRU；规则集合只有是**元组**（不可变）时才缓存，列表等可变容器一律不缓存。掩码阶段仍按周期分配占位符，缓存不参与占位符语义。

实测（20 条消息约 42KB 历史，每轮全量重建脱敏，三次取中位数）：

| | 首轮（冷） | 后续轮 |
|---|---|---|
| 优化前（全量扫描、无缓存） | 333.2 ms | 332.5 ms |
| 优化后（局部化 + 缓存） | **47.9 ms**（7.0x） | **7.9 ms**（42x） |

单条最贵的 `gitleaks:generic-api-key`（42KB 文本、绕过缓存）：51.8 ms → 2.2 ms（23x）。剩余耗时为熵兜底（同尺寸约 2.6ms）与结构层，二者都是纯函数，未来可用同一套机制缓存。

逐条等价性由 `tests/test_desensitization_rules.py::LocalityScanTests` 守护（含无上界长值、关键字密集、锚点位于前缀内、无关键字等语料）；观测接口 `scan_cache_stats()` / `clear_scan_cache()`。

### 5.4 优先级与去重

```text
豁免表命中 → 不脱敏
   ↓
结构命中（键名规则）→ 脱敏
   ↓
值类型规则层命中（PEM / 连接串 / 银行卡 / MAC / 车牌 / IP / URL / 邮箱 / gitleaks）→ 脱敏
   ↓
熵兜底命中 → 脱敏
   ↓
NER 语义兜底命中（人名 / 地名 / 机构名）→ 脱敏
   ↓
未命中 → 不动
```

- NER 兜底层接收的是**前几层处理后的文本**（命中值已是 `｛Desensitized:n｝`），因此不会被邮箱 / 手机号等其它类型数据干扰，也不会重复登记已脱敏的值；
- 同一周期内**同一值只分配一个序号**：多处出现（不同消息、不同位置）使用同一占位符，保证模型视角一致、减少注册项。
- 同一位置多层命中只登记一次（最先命中的规则生效）。

### 5.5 已知缺口（记录在案）

- 同值只在「命中位置」被替换：同一值出现在两处、只有一处命中规则时，另一处仍会出网；
- 语义不可见：值藏在意料之外的位置（注释、编码后的内容）时无法保证识别；
- 非字符串标量默认不处理（数字 / 布尔等）；
- 值类型规则层按固定形态匹配，覆盖不到语义变体（如自定义连接串格式）；gitleaks 规则的 `paths` / `regexTarget = "line"` 豁免在纯文本链路不生效，会略微偏多脱敏。

### 5.6 NER 语义兜底层（BiLSTM-CRF，可选）

**适用**：形态普通、随机性低、既无敏感键名也无法用正则刻画的**语义敏感值**——人名、地名、机构名。前几层都覆盖不到它们（熵层要求高随机性，规则层要求固定形态），只有语义模型能识别。

- **模型**：字符级 BiLSTM-CRF（手写 CRF + 非法 BIO 转移约束），BIO 标签体系 `O / B,I-PER / B,I-ORG / B,I-LOC`；权重随包分发（`omnicrawl/llm/desensitization/models/bilstm_crf_best.pt`，约 5MB，dev F1 0.868 / test micro-F1 0.863）；词表内嵌在 checkpoint 中。
- **接线位置**：`engine.mask_text` 的**最末端**，即结构 → 规则 → 熵之后。输入是已屏蔽文本，命中值（含其它层已替换的占位符）不参与候选。只产出「实体区间」，替换仍由 `MaskContext.placeholder_for` 走标准序号分配；**还原 / 流式 / 周期注销全部沿用既有机制，本层不新增还原路径**。
- **能力边界**：只识别 PER / ORG / LOC，**不识别邮箱 / 手机号 / 身份证号**等结构化敏感信息（由值类型规则层负责）。为控制误报：入口做中文片段隔离、丢弃整体未落在中文片段内的实体（模型在拉丁字母片段上的误报多来自邮箱 / 网址 / 编号）、默认丢弃单字实体（`ner_min_entity_chars=2`，规避「日 / 美 / 京」这类歧义单字）、丢弃与既有占位符重叠的实体。
- **中文隔离**：入口把非中文字符等长替换为分隔符后才送进模型（`·` / `・` 视为中文姓名连接符），因此拉丁字母、数字与其它符号不参与推理；出口再要求实体区间**整体**落在中文片段内（至少一个汉字、其余只能是连接符），跨片段实体不成立。偏移与原文一一对应，切块与缓存口径不变。
- **设备**：`ner_device` 取 `auto`（默认）时优先 CUDA、不可用回退 CPU；显式 `cuda` 在不可用或运行期 CUDA 出错（显存 / 驱动）时同样回退 CPU，可用性优先。
- **性能**：长文本按句末标点切块（≤200 字符，与训练口径一致）后批量推理；抽取器按 (权重路径, 设备, 缓存容量) 进程内共享、模型只加载一次。
- **随对话增长不劣化**（缓存粒度与短路）：历史每轮全量重发，因此本层的成本模型必须看「每轮重扫全量历史」而非「单次调用」。
  - 结果缓存落在**块**（`iter_chunks` 的打包结果，≤200 字符）而非整段文本：`iter_chunks` 是贪心前缀打包，只追加 / 只在尾部增长的长文本其已有块逐字不变，只有新增的块需要推理（实测 200 步只追加的长文本：整段文本当键需推理 2958 块，块级缓存只需 7 块）；内容相同的块还能跨文本复用，且淘汰一次最多损失一个块的推理。
  - **纯 ASCII 短路**：不含汉字的文本 / 块直接返回空结果、不进模型（过滤阶段本就丢弃不含汉字的实体），代码 / JSON / 日志类工具结果零推理开销（实测 100KB 纯 ASCII：7.6ms、进模型 0 块；同样的 100KB 混合文本共 596 块，只有 1 块真正进模型）。
  - 不这样做的话会出现**抖动断崖**：整段文本当键时，一旦历史块数超过缓存容量，LRU 的顺序全扫会让命中率直接掉到 0，此后每轮重新推理全部历史，单轮耗时随历史线性上升。实测「300 块历史、每轮全量重扫」：容量 256 时命中率 0%、每轮 2.4s；容量 2048 时每轮 1.8ms（约 1360x）。
  - `ner_cache_size`（单位：块，默认 2048，`0` 表示关闭）可调；`hits` / `misses` 按块计（含被 ASCII 短路跳过的块——它们查键未命中但未推理），`inferred_chunks` 是真正进过模型的块数，`ascii_skips` 是短路次数。
  - 仍需注意：缓存是固定容量的 LRU，工作集（不断增长的历史）最终仍会超过容量。本轮优化把阈值后移并大幅减小单次代价；要彻底消除 O(n²)，需要让**未变化的历史消息不再重新脱敏**（按内容 + 配置代际 memoize `middleware._mask_request` 的逐条结果）。
- **零成本降级**：torch 未安装（可选依赖 `.[ner]`）、权重缺失 / 损坏、配置非法时 `build_ner_layer` 返回 None，本层静默跳过，不影响既有脱敏链路；默认 `ner_enabled = false`（沿用整体 opt-in 约定）。
- **命中计数**：`DesensitizationStats.ner_masked`。

## 6. 占位符协议

### 6.1 规范形式

```text
｛Desensitized:<n>｝
```

- 全角花括号，`n` 为十进制整数（无前导零）；`Desensitized` 为固定英文标记（模型只能看到它，看不到原文）；
- 全角是**发送端唯一规范形式**（`format_placeholder` 生成）；字形与 ASCII 花括号区分度更高，在文本与代码环境中不易与 JSON、模板语法混淆；
- 还原侧**同时兼容半角 `{Desensitized:n}`**（含冒号变体、大小写、序号两侧空白，§6.2）：模型偶发把全角归一化为半角时仍可还原；
- 一个占位符对应「一个原始值」；同一周期内同值同号（§5.4）。

### 6.2 还原匹配（容错规则）

还原在「当前周期注册集合」内进行，接受以下变体（宽进严出）：

- 花括号：`｛｝`（规范）或 `{}`（兼容）；
- 冒号：`:` 或 `：`；
- `Desensitized` 大小写任意；
- 序号两侧允许空白。

处理规则：

| 情况 | 行为 |
|---|---|
| 序号已注册（会话内） | 替换为原文（同一序号多处出现全部替换；跨周期、跨请求同样有效） |
| 序号未知（未注册 / 未被本会话登记） | 不动，记录告警（不静默当作原文）；`strict_restore` 开启时中止并报错 |
| 畸形（疑似前缀 / 缺括号 / 非数字） | 不动，记录告警；`strict_restore` 开启时中止并报错 |
| 出现在 dict 键 / 工具名 / 结构件 | 不替换（只处理字符串值与文本） |
| 字符串值**恰好等于**一个占位符 | 将该字符串整体替换为原文 |
| 占位符内嵌于更长字符串 | 按子串替换（拼接原文） |

### 6.3 碰撞与转义

- 出站内容本身可能含「占位符样式」文本（全角或半角，如用户手工输入、文档示例）：**分配序号时跳过这些已出现的序号**（对请求中全部文本——含 `system_prompt` 与工具声明——做碰撞扫描）；
- 碰撞跳过**只作用于本次新分配**：一个值一旦分配过序号，后续请求即使出现同号文本也保持原号——改号会让整段历史在提供方前缀缓存中失配（§7.2）。
- 还原只认本周期注册号；原文不参与占位符语法，无需转义处理。

## 7. 序列号注册表与生命周期

### 7.1 数据结构（仅内存）

```text
SequenceRegistry(
  open_cycles: {cycle_id → PlaceholderCycle},   # 并发周期隔离
  last_cycle:  PlaceholderCycle | None,          # 供重试复用判定
  stats:       审计计数（§10.2）,            # 周期 / 值 / 还原审计计数
  stable_index: StableSequenceIndex,               # 进程级共享：值指纹 → 序号（§7.2）
)

PlaceholderCycle(
  entries:     {seq → 原文},   # 会话级共享：跨周期保留（§7.2）
  reserved:    {已出现的占位符样式序号},    # 分配时跳过
  value_index: {原文 → seq},               # 同值去重（本周期）
  stable_reuses: int,                      # 跨周期复用稳定序号的次数
  closed:      bool
)

StableSequenceIndex(
  entries: {HMAC-SHA256(值) → seq},        # 只存指纹与序号，不存原文
)
```

- 序号分配分两层：**进程级稳定索引**决定「哪个值用哪个序号」（同值永远同号，跨请求逐字可复现，§7.2），**进程级单调计数器**只为新值提供候选序号（从 1 开始，不回收）；
- 原文仅存于内存；**不落盘、不进日志、不进会话事件、不进 SSE、不进异常信息、不缓存**；
- 稳定索引只存 `HMAC-SHA256(值)`（进程启动时随机盐）与整数序号：原文不在其中，不可反推、也不可跨进程关联；条目数随「进程内出现过的不同敏感值数量」增长，不随请求数增长；
- **会话级保留**：`entries`（`seq → 原文`）由**会话所有者**持有并注入各周期（未注入时退化为注册表私有映射）；周期结束不清空，因此同一序号在会话内可反复还原（含上下文里出现过的旧序号）；会话切换 / 关闭即释放，运行时关闭（切换模型）不释放（§7.2）。
- 可观测信息只到「计数 / 规则 / 序号」粒度（§10.2）。

### 7.2 生命周期（会话内保留 + 序号稳定复用）

| 阶段 | 触发 | 动作 |
|---|---|---|
| 注册 | 出站屏蔽完成一次替换 | 分配序号 n；登记 `n → 原文` |
| 使用 | 模型返回含 `｛Desensitized:n｝` | 还原为原文（可多处） |
| 注销 | **本周期还原组装结束**（收到最终可用回复） | 只释放周期自身状态；会话级「序号 → 原文」映射保留（§7.2） |
| 遗留 | 周期未完成（请求失败 / 取消 / 超时 / 截断） | 不关闭周期；条目保留到运行时关闭 |
| 关闭 | 运行时关闭（或进程退出） | 丢弃会话级「序号 → 原文」映射；不落盘、不恢复 |

说明：

- **原文会话内驻留**：`seq → 原文` 在本会话内持续有效，周期结束不再释放；同一序号可跨请求、跨周期反复还原，随会话切换 / 会话关闭统一丢弃、不落盘（切换模型重建运行时不丢弃）；
- **序号稳定复用**：`值指纹 → 序号` 存在进程级稳定索引中，不随周期注销清理——同一段历史在连续请求中被脱敏成逐字一致的文本，提供方前缀缓存才能持续命中（§9.3）。这是与「序号随值改号」的明确取舍：改号能降低同号歧义，但会让整段历史在缓存中失配（实测命中率跌到 3% 量级）；
- 周期完成时**不再清空登记项**：会话级「序号 → 原文」映射由注册表持有、各周期共享，周期结束只释放周期自身状态（`source_request` / `masked_request`）；代价是原文在本会话内持续驻留；
- 周期是否完成以「该逻辑请求的最终结果」为准：回复可用（有文本 / 工具调用 / 推理且未被截断）即关闭本周期；中断重试则保留。
- 稳定索引随进程存活（含运行时重建：切换模型后同一值仍是同号）；进程退出即消失，不落盘；
- **会话级映射的存活期**：`seq → 原文` 由会话所有者（Agent）持有，运行时经 `store_provider` 共享同一份映射：切换模型重建运行时不丢序号；会话切换 / 关闭时 `rebind` / `clear` 丢弃；未注入映射的运行时（顾问、子代理、压缩摘要、视觉代理、工具输出压缩）退化为各自的私有映射，随该运行时关闭释放（§7.2）；

### 7.3 并发与重试

- 多个运行时 / 子代理并行：注册表以**周期**为隔离单位，还原只查询「本周期注册集合」；容器线程安全；
- 同一逻辑请求的重试：**复用同一份屏蔽结果与同一注册表条目**（不重新分配序号；判定条件为「上一个未关闭周期的源请求与新请求全量相等」）；重试后丢弃的半截回复不做还原；
- 流式尝试实例（`StreamRestorer`）每个尝试一个，重试 / 回滚时清空缓冲，注册表不受影响。
- 稳定索引为进程级共享、内部加锁：多个运行时 / 子代理 / 旁路一次性脱敏器对同一值得到同一序号，互不串号；各周期的 `entries` 仍然隔离，还原只查本周期；

## 8. 流式响应还原

### 8.1 为什么不能「看到就换」

流式分片可能把占位符截断在中间（例如 `…｛Desensitized:1` + `2｝…`）。对半截文本做朴素替换会漏还原或误还原。

### 8.2 设计：尾部挂起缓冲

- 对 `TextDelta` / `ReasoningDelta` 维护「占位符感知缓冲」（文本 / 推理两路独立）：只挂起「可能是占位符前缀 / 未闭合占位符」的尾串（上限 64 字符），安全前缀照常向下游输出；
- 完整占位符出现即还原并输出；流结束（`ResponseCompleted`）时 flush 剩余缓冲（未闭合前缀按原样保留 + 告警）；
- 工具调用参数：`ToolCallArgumentsDelta` 原样透传（不落宿主消息）；在 `ToolCallCompleted`（参数已解析为 dict）处做**结构化还原**（递归替换字符串值，键不动）；
- 重试 / 回滚（`on_stream_rollback`）时以当前尝试为准；注册表不受影响。

### 8.3 周期完成判定

- `StreamRestorer.reply_usable`：有文本 / 推理 / 工具调用且 `finish_reason` 不属于截断类（`length` / `incomplete` / `max_tokens` / `content_filter` / `failed`）→ 周期按成功注销；
- 截断类 finish_reason 表示上层会重试重启本请求，周期不注销（重试复用屏蔽结果）。

## 9. 失败与边界策略

### 9.1 策略表

| 场景 | 注册表 | 还原 | 说明 |
|---|---|---|---|
| 请求成功，回复完整 | 周期完成 → 注销 | 正常还原 | 主路径 |
| 空响应 / 流中断重试 | 保留（同周期） | 丢弃半截结果 | 重试复用同一屏蔽结果（§7.3） |
| 请求最终失败 / 取消 / 超时 / 截断 | 不注销 → 遗留 | 无输出 | 遗留到关闭丢弃（§7.2） |
| 还原遇未知 / 畸形占位符 | 不受影响 | 保留 + 告警 | 不静默当原文；严格模式可阻断（§9.2） |
| 屏蔽阶段自身异常 | 不注销 | — | 默认 fail-closed（§9.2） |

### 9.2 安全默认（可配置）

- 屏蔽阶段异常（匹配 / 替换失败）→ **默认中止本次请求并明确报错**（`fail_closed=true`，不静默发送原文）；提供「可用性优先」降级开关（`fail_closed=false`：发送原文 + 告警）；
- 还原阶段未知 / 畸形 → 默认「保留 + 告警」；`strict_restore=true` 时升级为中止并报错。

### 9.3 协议与链路

- 四种协议（Chat Completions / Responses / Anthropic / Gemini）：在统一消息层处理，协议无关；
- 压缩摘要 / SubAgent / 顾问 / 视觉代理：经统一运行时，自动受保护；
- 审批自动审查与旧直连路径：见 §4.3 接线清单；
- **实现注意**：`omnicrawl/state/session_projection.py` 与 `turn/loop.py::_raw_tool_call_arguments` 维护「运行期 / 恢复 / 压缩三路逐字一致 + 前缀缓存保护」的约束。宿主侧唯一事实仍是原文，屏蔽只发生在序列化边界；对同一历史内容的屏蔽结果需可复现（否则影响前缀缓存命中，见 §10.2 指标）。该约束的落点是 §7.2 的稳定序号：同值同号保证脱敏后的历史逐字可复现（回归测试 `tests/test_desensitization_prefix_cache.py`）。

### 9.4 安全与威胁模型（摘要）

- **还原是一个受控的「原文回插信道」**：若注入内容诱导模型输出 `｛Desensitized:n｝` 并借工具把值写到别处，存在借道外泄的理论风险。缓解：`n → 原文` 映射只在会话（运行时存活期）内有效、随运行时关闭丢弃（§7.2），还原范围仅限模型返回；可观测 / 可审计；既有工具审批链不变；
- **内存驻留**：注册表在内存中暂存原文（进程崩溃转储风险由宿主环境承担）；关闭即丢弃、不落盘；
- **日志 / 遥测**：本层不记录原文（§10.2）；
- 本机制只保证「模型侧看不到原文」，不消除其它泄露路径（网络抓包、宿主日志配置错误等）。

## 10. 配置与可观测性

### 10.1 配置（config.toml `[desensitization]` 段）

```toml
[desensitization]
enabled = false                # 默认不启用（opt-in；未启用零成本）
fail_closed = true             # 屏蔽异常时中止请求而非放行原文
strict_restore = false         # 还原缺失时是否阻断（默认保留 + 告警）
extra_sensitive_keys = []      # 追加敏感键名
exempt_keys = []               # 豁免键名（优先）
entropy_enabled = true         # 熵兜底开关（关闭则仅键名 / 结构匹配）
entropy_min_length = 20        # 熵兜底：长度下限
entropy_min_bits = 3.5         # 熵兜底：熵阈值
entropy_pure_letters = false   # 纯字母令牌：长度达标即脱敏，词形标识符仍跳过（默认关闭）
entropy_pure_digits = false    # 纯数字令牌：长度达标即脱敏（默认关闭）
# 值类型规则层（高危默认开、泛化类默认关）
detect_pem_private_key = true         # PEM 私钥
detect_db_connection_string = true    # 数据库连接串（URI / ADO 键值）
detect_email = false                  # 邮箱地址
detect_bank_card = true               # 银行卡号（Luhn 校验）
detect_internal_ip = true             # 内网 IP（私有 / 链路本地）
detect_external_ip = false            # 外网 IP（公网可路由）
detect_url = false                    # 网址（http / https / ftp）
detect_mac_address = true             # MAC 地址
detect_license_plate = true           # 中国大陆车牌
gitleaks_enabled = true               # gitleaks 开源规则（内置离线快照）
gitleaks_config_path = ""             # 自定义 gitleaks.toml（按 id 覆盖 / 追加）
# NER 语义兜底层（BiLSTM-CRF：人名 / 地名 / 机构名；可选依赖 torch）
ner_enabled = false                   # 默认关闭；开启后作为结构 / 规则 / 熵之外的语义兜底
ner_model_path = ""                   # checkpoint 路径；留空用随包权重 / 环境变量 OMNICRAWL_NER_MODEL
ner_device = "auto"                   # auto 优先 CUDA、不可用回退 CPU
ner_entity_types = ["PER", "ORG", "LOC"]  # 需要识别的实体类型
ner_min_entity_chars = 2              # 最小实体长度（2 规避单字地名歧义）
ner_cache_size = 2048                 # 推理结果缓存容量（单位为「块」，0 关闭）
```

- 读 / 写：`load_desensitization_config` / `save_desensitization_config`（`omnicrawl/config/features/desensitization.py`）；写回时保留 config.toml 其他段；
- 键位说明见 `omnicrawl/config/templates/config.example.toml`。

### 10.2 可观测性（只记录「数量级」信息）

- 计数（`DesensitizationStats`）：周期开始 / 复用数、屏蔽值数、值类型规则命中数（`rules_masked`）、熵兜底数（`entropy_masked`）、NER 兜底数（`ner_masked`）、还原命中数、未还原数、畸形数、跳过数、屏蔽耗时、屏蔽失败数、稳定序号复用数（`sequence_reuses`：同值跨请求复用同一序号的次数，用于观察前缀缓存稳定性）
- 维度：规则类别、序号、周期 ID；
- 明确禁止：任何原文、占位符 → 原文映射、原文长度 / 前缀等可直接或间接泄露原值的信息。

### 10.3 设置面板「消息脱敏」页

- 位置：`/settings` 左侧列表（`agent_workspace` 之后，`_SETTING_ORDER` 中 `desensitization`，行标签「消息脱敏」）；
- 表单：27 个字段（基础开关 ×4、两个键名列表、长度 / 熵阈值、两个单类令牌开关、九个值类型类别开关、gitleaks 总开关与自定义路径、NER 总开关 / 设备 / 权重路径 / 实体类型 / 最小实体长度 / 缓存容量）；保存写回 config.toml；
- 生效条件：保存后需**切换模型或重启 TUI**（对已构建的运行时不生效，面板有同款提示）。

## 11. 实现落点

| 职责 | 文件 |
|---|---|
| 匹配引擎（键名 / 结构 / 值类型规则 / 熵兜底 / NER 兜底） | `omnicrawl/llm/desensitization/engine.py` |
| NER 兜底层（设备 / 路径解析、抽取器、过滤、缓存、共享实例） | `omnicrawl/llm/desensitization/ner.py` |
| NER 模型结构（BiLSTM-CRF，仅 torch；延迟导入） | `omnicrawl/llm/desensitization/ner_model.py` |
| NER 模型权重（随包分发） | `omnicrawl/llm/desensitization/models/bilstm_crf_best.pt` |
| 值类型规则定义与扫描 | `omnicrawl/llm/desensitization/rules.py` |
| gitleaks 规则解析（内置快照 + 自定义） | `omnicrawl/llm/desensitization/gitleaks.py`、`gitleaks.toml` |
| 序列号注册表与周期 | `omnicrawl/llm/desensitization/registry.py` |
| 运行时装饰器（出站屏蔽 / 入站还原） | `omnicrawl/llm/desensitization/middleware.py` |
| 流式还原状态机 | `omnicrawl/llm/desensitization/stream.py` |
| 旁路一次性脱敏器 | `omnicrawl/llm/desensitization/oneshot.py` |
| 模块导出 | `omnicrawl/llm/desensitization/__init__.py` |
| 配置 | `omnicrawl/config/features/desensitization.py`、`omnicrawl/config/templates/config.example.toml` |
| 主接线 | `omnicrawl/llm/registry.py`（`build_runtime` → `maybe_wrap_runtime`） |
| 审批旁路接入 | `omnicrawl/agent/controllers/tools/approval.py` |
| 设置面板 | `omnicrawl/ui/fullscreen/screens/desensitization_settings.py`、`settings.py` |
| 测试 | `tests/test_desensitization.py`、`tests/test_desensitization_settings.py`、`tests/test_desensitization_rules.py` |

## 12. 行为样例与回归矩阵

### 12.1 端到端样例

**场景**：工具读取 `.env` 文件，模型基于结果把该值写入另一个文件。

1. 工具结果消息（原文，本地）：`API_KEY=sk-live-8f3…`（工具执行、会话落盘均照旧）；
2. 出站屏蔽：`API_KEY=｛Desensitized:7｝`，注册 `7 → sk-live-8f3…`（内存）；
3. 模型请求写文件：`write_file(content="｛Desensitized:7｝")`；
4. 入站还原：工具参数还原为 `sk-live-8f3…` → 工具按原文写文件（业务不受影响）；
5. 周期完成：只释放周期自身状态；序号 7 在会话内继续有效（后续回复再次引用该序号仍还原为原文），运行时关闭才丢弃。

**流式分片**：模型流式输出 `…｛Desensitized:1` + `2｝…` 两片 → 尾部挂起缓冲在完整占位符到齐后一次还原（§8.2）；若模型把全角归一化为半角，按兼容匹配同样可还原（§6.2）。

**重试**：同一次请求第 1 次尝试流中断 → 丢弃半截输出；第 2 次尝试复用同一屏蔽结果与序号（不重新分配）；成功回复后周期完成注销。

### 12.2 回归矩阵

| # | 场景 | 预期 |
|---|---|---|
| 1 | 工具结果含 `.env`（`API_KEY=…` / `PASSWORD=…`） | 出网值替换为占位符；模型返回引用处还原 |
| 2 | 嵌套 JSON（数组 / 对象多层） | 递归命中、结构不变 |
| 3 | 同一值多处出现（同周期） | 同一序号、多处还原 |
| 4 | 工具调用参数含占位符 | 执行前还原为原文 |
| 5 | 用户消息直接粘贴敏感值 | 键名 / 熵规则命中并替换 |
| 6 | 自由文本中的高熵令牌 | 兜底命中；普通代码 / 哈希不误伤 |
| 7 | 流式分片恰好切断占位符；模型将全角归一化为半角 | 不误还原、不漏还原；半角变体可还原 |
| 8 | 模型返回未知序号 | 保留 + 告警（strict 时阻断） |
| 9 | 畸形占位符（缺括号 / 非数字） | 保留 + 告警（strict 时阻断） |
| 10 | 出站内容已有占位符样式文本（全角 / 半角） | 分配跳过冲突序号 |
| 11 | 并发周期（主循环 + 子代理） | 互不串号；还原严格限定本周期 |
| 12 | 重试 / 取消 / 截断 | 注册表按 §9 处置；无泄露（日志无原文） |
| 13 | 运行时关闭 | 未注销项丢弃、不落盘、不可恢复 |
| 14 | 四协议冒烟（Chat / Responses / Anthropic / Gemini） | 行为一致 |
| 15 | 豁免表 / 敏感键扩展 | 配置生效、优先级正确 |
| 16 | 大消息 / 长历史性能 | 无明显卡顿；缓存影响可观测 |
| 17 | 连续两次请求：历史只追加、内容不变 | 未变历史脱敏后**逐字一致**（同值同号）；前缀缓存可持续命中 |
| 18 | 周期注销后同一值再次出站 | 复用同一序号；会话级映射保留，跨周期仍可还原 |
| 19 | 工具结果含 PEM 私钥 / 连接串 / 银行卡 / MAC / 车牌 / 内网 IP | 值类型规则命中并替换；模型返回引用处还原 |
| 20 | 自由文本含邮箱 / 外网 IP / 网址（对应开关开启时） | 命中；`example.com` / 环回地址 / 版本号不误伤 |
| 21 | 历史中含 gitleaks 类密钥（GitHub PAT / AWS / Stripe / Slack 等） | 命中替换；上游 allowlist 与熵阈值生效（示例密钥不脱敏） |
| 22 | 网址与其中的 IP 同时命中 | 网址优先占位（重叠区间只登记一次） |
| 23 | 自定义 gitleaks.toml（覆盖 / 追加）或路径不可用 | 自定义生效；不可用回退内置快照 |
| 24 | `ner_enabled = true`，历史含人名 / 地名 / 机构名 | NER 兜底命中并替换；模型返回引用处还原 |
| 25 | NER 与其他层同时命中同一文本（如邮箱与其中的人名） | NER 只见前几层处理后的文本，不重复登记 / 不破坏占位符 |
| 26 | `ner_enabled = false` 或未安装 torch / 权重缺失 | 本层静默跳过，既有链路行为不变（零成本） |
| 27 | 长文本（>200 字符） | 按句末标点切块推理，偏移正确回填；结果缓存命中的重复文本不再推理 |
| 28 | `ner_device = auto` 且存在 CUDA | 优先 CUDA；不可用 / 运行期 CUDA 出错回退 CPU |
| 29 | 同一份回复里多次写出同一序号（不同位置、文本 / 推理 / 工具参数） | 每处都还原为同一原文 |
| 30 | 引用上下文里出现过、但源消息已离开上下文的旧序号 | 会话级映射仍命中并还原（此前为「保留 + 告警」） |
| 31 | 运行时关闭（切换模型 / 退出）后同一序号 | 映射已丢弃：按未注册处置（保留 + 告警） |

### 12.3 测试与回归

```bash
python -m pytest tests/test_desensitization.py tests/test_desensitization_settings.py tests/test_desensitization_prefix_cache.py tests/test_desensitization_rules.py tests/test_desensitization_ner.py
```

`tests/test_desensitization_ner.py` 覆盖准确性（实体级 P/R/F1 下限与三类实体覆盖）、性能（单句推理预算、结果缓存命中）、流式处理（分片回包逐字还原、工具参数结构化还原）与降级（未启用 / torch 缺失 / 权重无效静默跳过）；未安装 torch 或缺少随包权重时依赖真实模型的用例整体跳过。

## 13. 已知边界与形态说明

- **覆盖缺口**：同值全局回声不掩蔽、语义不可见位置、非字符串标量、规则层的形态盲区（§5.5）。
- **形态边界**：本文核心（匹配引擎、占位符协议、注册表生命周期、还原规则）与进程形态无关。当前落点是进程内中间件；若未来改为独立网关进程 / 上游网关侧实现，边界从「函数调用」变为「服务调用」，注册表生命周期改与网关进程绑定，OmniCrawl 侧改为指向网关或旁路转发；「只动消息、不动请求参数」与「序号在会话内稳定复用、可反复还原」语义保持不变。
- **生效性**：配置在运行时构建时读取，运行中修改需切换模型或重启 TUI（§4.4）。
- **不替代**：不替代既有不可逆值级脱敏（两者并存，§1.1）；不提供跨进程 / 跨机同步的映射（稳定索引为进程级、序号映射随运行时生命周期）。
- **同号歧义（已接受）**：稳定序号优先于碰撞改号（§6.3）。若出站历史中残留了不可还原的旧占位符文本、且与某值的稳定号相同，同一请求内会出现同号两义；还原仍以会话级映射为准，风险与既有「未知序号保留 + 告警」路径同级。

## 14. 参考

- 消息与协议模型：`omnicrawl/llm/protocol.py`；运行时管理与构建：`omnicrawl/llm/runtime.py`、`omnicrawl/llm/registry.py`；
- 协议适配器：`omnicrawl/llm/providers/openai_chat.py`、`openai_responses.py`、`anthropic.py`、`gemini.py`；
- 链路使用方与既有脱敏：见 §4.2 / §1.1；
- 内置文档 URI：`omnicrawl://docs/agent_gateway_desensitization_design.md`。

## 15. 变更记录：长上下文性能与缺陷修复（2026-09-20，v0.1.58）

本节记录与代码同步的**已落地行为**；涉及 §5.1 / §5.6 / §7.2 / §7.3 的表述，以本节与代码为准。

### 15.1 逐消息屏蔽结果缓存（消除「每轮全量重脱敏」的 O(n^2)）

- 落点：`omnicrawl/llm/desensitization/middleware.py` 的 `_MessageMaskMemo`。键是消息内容的 blake2b-16 摘要，覆盖**全部决定复用安全的字段**（role / 文本块 / 工具参数 / 工具结果 / 思考，以及 `tool_call_id` / 函数名 / `provider_call_id` / `ok` / 图片数据）；复用结果按「当前消息 + 缓存里的屏蔽后字段」重建，因此未参与屏蔽的字段（如 per-message 工具声明）不会因复用而丢失。
- 命中时用 `PlaceholderCycle.adopt` 把该消息的 (序号, 原文) 重新登记进**本周期的还原集合**：跳过扫描但还原照旧，模型回引历史占位符不会落入「未注册序号」分支。
- 预算 `_MEMO_MAX_CHARS = 1024 * 1024`（1MB 字符；同时是「被屏蔽值原文」在缓存里的驻留上限），设为 0 关闭缓存（退回每轮全量重扫），`close()` 清空。
- 淘汰策略为 **MRU**（淘汰最近用过的条目）：历史是每轮从头到尾顺序全扫，LRU 在「历史总量 > 预算」时会抖动到 0 命中，MRU 等价于把历史头部钉在缓存里，命中率随预算线性下降。
- 实测（全量历史 + 2 条新消息、扫描缓存已预热）：66KB 12.6ms → 0.37ms；366KB 95.7ms → 2.71ms（约 35x）；超预算时 1512KB → 2.8x、2412KB → 8.2x。残余成本是内容摘要本身（约 0.007ms/KB）。
- 代价：缓存持有被屏蔽值的原文（配对 (序号, 值) 对），把「原文单次使用」的驻留延长到缓存存活期；预算即上限，`close()` 释放。

### 15.2 周期生命周期（取消 / 失败不再遗留）

- `SequenceRegistry.begin_cycle` 在**请求换新**时注销并移除上一个未完成周期：只有紧邻的上一个未完成周期可能被同一逻辑请求的重试复用，请求一旦变化就再也不会命中，遗留只会累积原文与整份屏蔽副本。
- `PlaceholderCycle.close` 顺带释放 `source_request` / `masked_request`。
- 新增 `PlaceholderCycle.adopt`（缓存复用时的重登记）与 `pairs_from`（取区间内新增的 (序号, 值) 对）。

### 15.3 屏蔽语义修正（engine）

- `should_skip_value`：只有「整串恰为一个占位符」才跳过。此前值里只要嵌有占位符样式文本就整串跳过，同一条值里的真实秘密会随之出网。
- 赋值形态（`.env` 风格行与 `key: value`）：**只取配对引号内的值**。
  - 被换行截断、引号未配对的片段不再被当作值（此前会把那个引号本身当值替换成占位符）；
  - 引号后跟注释或代码时只屏蔽引号内的值，注释与尾段保持原样（此前整行尾段会被吞进占位符）；
  - 无引号的值在行内注释起点前收尾（`KEY = v # 说明` 只替换 `v`；`#` 前无空白时仍整体视为值）。
- 实现：`engine._assignment_value_body()` 取代 `_mask_assignment_value()` 取代 `_mask_assignment_value()`，后续 splice 只在「值本体区间」内替换。

### 15.4 回归与验证

- 项目测试：主仓库 `pytest tests/test_desensitization.py tests/test_desensitization_ner.py tests/test_desensitization_rules.py tests/test_desensitization_settings.py tests/test_desensitization_prefix_cache.py -q` → **196 passed**（含 §15.6 后新增的 3 个配对完整性用例）。

- 自建验证脚本（本机 `.omnicrawl/.agent_tmp/scripts/`）：缓存开/关逐字一致、历史回声还原、`write_file` 参数还原、周期生命周期、赋值片段十六进制逐条比对、性能对照表。

### 15.5 已核实、非缺陷的现象（避免重复排查）

「工具参数里 `.env` 风格行落盘成未还原占位符」经三次独立验证判定为**模型侧生成**而非管道缺陷：写入工具对敏感键参数值的「屏蔽 → 还原」往返字节无损（同一标记值分别放在敏感键与普通键下对照）；把此前被改写的行原样重写，87 字节全为正确 ASCII、零非 ASCII 码点；会话转录里该调用参数被不可逆层记为掩码值，而模型推理文本本身带有占位符字样。机制上：参数里的值已是占位符样式时 `should_skip_value` 按设计跳过，因此原样落盘。

### 15.6 逐消息缓存的协议身份缺陷修复（2026-09-14）

- **故障**：同一会话第二轮的模型请求被网关以 HTTP 400 拒收——`An assistant message with 'tool_calls' must be followed by tool messages responding to each 'tool_call_id'`（真实会话：`20260913-183608-61fe05`，两个 `memory_search` 都返回 `[]`）。
- **根因**：`_message_digest` 只摘要正文，`tool_call_id` 与 `ok` 不在键里；同一批里两条正文相同的工具结果因此共用一条缓存条目，第二条命中后整块复用第一条的 `ToolResultBlock(call_id=...)`，`tool_calls` 里被声明过的另一个 id 就没有配对结果。图片块更早被摘要成块类型名，两条「文案相同、图片不同」的消息也会互相顶替（模型收到错图）。
- **修复**：`_message_digest` 纳入调用/结果身份字段与图片内容（`call_id` / `name` / `provider_call_id` / `ok` / `media_type` / `detail` / `data_base64`）。这只是让「正文相同但身份不同」不再命中缓存，正常追加历史的命中率不变。
- **不变量**：脱敏层不得改动消息序列结构与工具配对；屏蔽只发生在文本与工具参数内容层。
- **回归测试**：`tests/test_desensitization_prefix_cache.py::ToolPairingIntegrityTest`（相同结果 / 相同调用 / 相同文案不同图片三条；对旧摘要实现逐条失败、对修复实现全绿）。

### 15.7 无引号赋值的行内注释与缓存复用可还原性

两个「内容被写坏」的缺陷，都落在出站屏蔽与还原的交界处：

- **赋值值本体**：无引号赋值此前把行尾整体视为值，`KEY = v # 注释` 会把行内注释一起登记并替换——注释在模型侧消失，模型改写该行时无从恢复原文。现在无引号值在**行内注释起点**（`#` 位于值首或前一个字符为空白）前收尾，`v#frag` 与值中的普通 `#` 仍整体视为值；值本体为空（`KEY = # 说明`）时按形态不明确处理，整行不动。实现：`engine._inline_comment_start()`。
- **逐消息缓存复用**（§15.1）：缓存条目回放的屏蔽文本，可能引用**本周期没有登记项的序号**——登记过该值的消息已被压缩 / 丢弃，而条目里的 `pairs` 只记录「屏蔽本条消息时新增的登记」，复用同值时不产生新条目。复用会把无法还原的占位符交给模型；模型把它回写进工具参数（如 `write_file` 的 content）就是一处被改坏的内容。现在缓存条目额外记住文本引用的序号，命中时逐个校验本周期可还原性，任一缺失即回落重扫、把序号重新登记进本周期。实现：`middleware._mask_message_cached()`、`_referenced_sequences()`、`_iter_message_texts()`。
- **回归**：`tests/test_desensitization_comment_and_cache.py`。两个缺陷各有一条判据（注释在模型侧可见、回写内容不含无法还原的占位符），修复前两条判据均失败。

## 16. 变更记录：序号会话级缓存（2026-09-15，待发布）

本节记录与代码同步的**已落地行为**；涉及 §1 / §3 / §6.2 / §7.1 / §7.2 / §9.4 / §12.1 / §12.2 / §13 的表述，以本节与代码为准。

### 16.1 行为变化：一个序号在会话内可反复替换

- **旧行为**：`seq → 原文` 只在本周期有效，周期结束（收到可用回复）即释放；模型在后续回复里回引「上下文里出现过的旧序号」时命中不了映射，落进「未注册序号 → 原样保留 + 告警」分支——模型把该占位符写回文件 / 请求参数时，落盘的就是占位符文本。
- **新行为**：`seq → 原文` 提升为**会话级共享映射**，由 `SequenceRegistry` 持有并注入各周期（`PlaceholderCycle.entries` 指向同一份 dict）。
  - 同一序号在一份回复里出现任意多次（不同位置：文本 / 推理 / 工具参数）都还原为同一原文；
  - 跨周期、跨请求仍可还原：源消息即使已被压缩或丢弃，只要模型引用了该序号，仍能还原；
  - 周期结束只释放周期自身状态（`source_request` / `masked_request` / `value_index` / `closed`），不再清空映射；
  - 会话级映射由会话所有者（Agent）持有并传给运行时（`store_provider`）：运行时关闭（切换模型 / 重建运行时）不释放映射，映射随会话切换 / 关闭释放；未注入映射的运行时退化为注册表私有映射（等价于按运行时隔离）。
  - 落点：`SequenceRegistry(store_provider=...)`、`SessionSequenceCache`（`rebind` / `clear`）、`build_runtime(..., store_provider=...)`、`maybe_wrap_runtime(..., store_provider=...)`、Agent 的 `current_desensitization_sequences()` 与会话内运行时构建点（`_ensure_runtime_manager`、模型切换 `switch(..., runtime_factory=...)`）。详见 §16.4。
- **代价**：被屏蔽值的原文在本会话内持续驻留内存（此前只在本周期内驻留）；可还原窗口从「单次请求」变为「整个会话」，同号歧义（§13）的暴露面随之变大——属已接受的取舍，窗口仍随会话切换 / 关闭结束。

### 16.2 落点

| 职责 | 文件 |
|---|---|
| 会话级映射的持有 / 注入 / 释放 | `omnicrawl/llm/desensitization/registry.py`（`SequenceRegistry._entries`、`PlaceholderCycle.entries`、`PlaceholderCycle.close`、`SequenceRegistry.drop_all`） |
| 还原路径（未改动） | `omnicrawl/llm/desensitization/stream.py`（`StreamRestorer` 逐处还原）；`middleware.py` 的逐消息缓存命中校验（§15.7）因此更易满足 |

### 16.3 回归与验证

- 项目测试：`python -m pytest tests/test_desensitization.py tests/test_desensitization_prefix_cache.py tests/test_desensitization_ner.py tests/test_desensitization_rules.py tests/test_desensitization_settings.py -q` → **197 passed / 1 failed**（`ner` 的 CUDA 回退用例在无 GPU 机器上失败，与本次改动无关）。
- 新增 / 更新用例：
  - `tests/test_desensitization.py::SessionSequenceCacheTest::test_same_sequence_restored_at_many_positions`：同一序号三处（文本 ×2 + 工具参数）全部还原；
  - `tests/test_desensitization.py::SessionSequenceCacheTest::test_sequence_referenced_in_context_restored_after_source_dropped`：源消息已离开上下文，引用旧序号仍还原；**回退实现后该用例失败**（还原结果停在占位符文本）；
  - `tests/test_desensitization.py::RegistryTest::test_close_and_drop_all` 与 `tests/test_desensitization_prefix_cache.py::StableSequenceIndexTest::test_same_value_keeps_sequence_across_closed_cycles`：按新语义更新（周期结束不清空；`drop_all` 后回到未注册）。

### 16.4 会话级映射的所有权：跨模型切换保留（2026-09-15 追加）

§16.1 的第一版把映射放在注册表实例里，而注册表随运行时创建 / 关闭：**切换模型会重建运行时并关闭旧运行时**，映射随之丢失，与「单会话内缓存」的目标不一致。本轮把映射的所有权上移到**会话所有者**：

- 新类型 `SessionSequenceCache`（`omnicrawl/llm/desensitization/registry.py`）：持有 `session_id` 与 `seq → 原文` 映射，`rebind(session_id)` 在会话标识变化时丢弃上一会话的原文，`clear()` 供会话关闭使用；原文仍只在本进程内存驻留。
- `SequenceRegistry(store_provider=...)`：每个发送周期开始时向 `store_provider` 取当前会话的映射；注册表不再自己保管映射的归属——**注入的映射不随 `drop_all`（运行时关闭）释放**，未注入时退化为注册表私有映射（等价于旧行为，旁路 / 测试不受影响）。
- `maybe_wrap_runtime(runtime, *, store_provider=...)`、`build_runtime(profile, model, *, store_provider=...)`：把会话映射透传到运行时装饰器。
- 会话所有者：Agent 的 `current_desensitization_sequences()`（`omnicrawl/agent/controllers/session/store.py`）持有一个映射实例，调用时按 `current_session_id` `rebind`，因此会话新建 / 恢复 / 工作区切换后自动换新。
- 会话内运行时构建点注入该映射：`omnicrawl/agent/controllers/turn/loop.py::_ensure_runtime_manager`（首次 bootstrap 与模型变化 switch）以及模型切换路径 `omnicrawl/agent/controllers/session/settings.py`（`switch(..., runtime_factory=partial(build_runtime, store_provider=...))`）。
- 未注入映射的运行时（顾问、子代理、压缩摘要、视觉代理、工具输出压缩）保持原语义：各自的私有映射随该运行时关闭释放（它们的请求自带上下文，屏蔽 / 还原在本次运行内闭环）。

行为归属总览：

| 事件 | 映射 |
|---|---|
| 同一会话内重建运行时（切换模型 / 改设置） | **保留**：新运行时经 `store_provider` 拿到同一份映射 |
| 会话新建 / 恢复 / 工作区切换 | **丢弃**：`rebind` 清空上一会话原文 |
| 会话关闭 / Agent 结束 | **丢弃**（`clear`，不落盘、不恢复） |
| 旁路运行时（顾问 / 子代理 / 摘要等）关闭 | 释放该运行时自己的私有映射 |

回归（`tests/test_desensitization.py`，本轮新增/更新）：

- `SessionSequenceCacheTest::test_sequence_survives_runtime_rebuild_in_same_session`：运行时 A 关闭后用同一 `store_provider` 重建运行时 B，B 仍能还原 A 分配的序号；
- `SessionSequenceCacheTest::test_other_session_cannot_restore`：另一个会话的运行时拿不到本会话映射（未注册 → 原样保留 + 告警）；
- `SessionSequenceCacheTest::test_rebind_drops_previous_session_mapping`：`rebind` 只在会话标识变化时清空；
- `SessionSequenceCacheTest::test_session_mapping_is_per_registry`：未注入映射的注册表之间互不可见。

最终验证（本机，2026-09-15）：

```bash
python -m pytest tests/test_desensitization.py tests/test_desensitization_prefix_cache.py \
  tests/test_desensitization_ner.py tests/test_desensitization_rules.py \
  tests/test_desensitization_settings.py -q
# 198 passed / 1 failed（ner 的 CUDA 回退用例：本机无 GPU，与本次改动无关）

python -m pytest tests/test_desensitization*.py tests/test_model_runtime.py \
  tests/test_agent_module_boundaries.py tests/test_session_module_boundaries.py \
  tests/test_llm_module_boundaries.py tests/test_agent_context.py -q
# 285 passed / 1 failed（同上）
```
