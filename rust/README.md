# Rust 内核工程

OmniCrawl 的 Rust 内核从这里起步，目标是两件事：

1. 能把核心内核编到嵌入式 Linux 上跑（musl 静态二进制，不带 CPython 运行时）；
2. 给 TypeScript 插件生态留出稳定、版本化的进程边界（插件侧继续走 NDJSON JSON-RPC，
   安全裁决留在内核，不下放给插件）。

当前落地三层内核：**协议内核层**（Provider 无关的消息块、工具调用、流事件与归并）、
**回合循环**（模型回复 → 整批工具观察 → 下一次模型请求）、**Provider 运行时**
（请求体组装、分片与 SSE 负载 → 内核流事件、用量归一化），外加**宿主桥接**（协议 v1 的
NDJSON 帧、版本协商与事件/命令映射）。四者都是纯逻辑、无 I/O、零 Python 依赖，可单独单测。

## 目录

```
rust/
├── Cargo.toml                              # workspace 定义
├── crates/omnicrawl-protocol/              # 协议内核 crate
│   ├── src/identity.rs                     # Provider / 协议闭集、模型身份
│   ├── src/message.rs                      # 消息块、工具调用、消息、生成选项、Token 用量
│   ├── src/event.rs                        # 流事件与归并结果
│   ├── src/aggregate.rs                    # 流事件归并
│   ├── src/codec.rs                        # OpenAI 风格历史 ⇄ 会话消息、工具声明编解码
│   ├── tests/parity.rs                     # 与 Python 实现的对照测试
│   └── tests/fixtures/protocol_parity.json # 由 Python 侧生成的对照数据集
├── crates/omnicrawl-core/                  # 回合循环 crate
│   ├── src/types.rs                        # 循环的数据契约（工具调用/结果、回复、预算、错误）
│   ├── src/runner.rs                       # 模型循环执行器与注入式时钟
│   ├── tests/turn_loop_parity.rs           # 与 Python 循环的对照测试
│   ├── tests/fixtures/turn_loop_parity.json# 由 Python 侧生成的对照数据集
│   └── README.md                           # 状态转移清单、边界与已知差异
├── crates/omnicrawl-llm/                   # Provider 运行时 crate（含 HTTP 传输）
│   ├── src/openai_chat.rs                  # 分片归并、参数完整性、首选项
│   ├── src/sse.rs                          # SSE 负载解码与逐条语义
│   ├── src/request.rs                      # 会话消息 → 请求体、工具声明、provider_options、prompt_cache_key
│   ├── src/usage.rs                        # Responses / Chat Completions 负载 → TokenUsage
│   ├── src/errors.rs                       # 运行时错误面与 HTTP 状态码阶梯文案
│   ├── src/transport.rs                    # 阻塞式 HTTP 往返（ureq + rustls）与超时映射
│   ├── src/runtime.rs                      # 一次回合：请求 → 流事件 → 工具收尾 → ModelReply
│   ├── src/desensitization.rs              # 消息脱敏模块根：子系统错误面 + 序号注册表（占位符协议、稳定序号）
│   ├── src/desensitization/stream.rs       # 消息脱敏：流式还原（尾部挂起缓冲、结构化还原、严格模式）
│   ├── src/desensitization/engine.rs       # 消息脱敏：匹配引擎（结构层 / 键名 / 熵兜底 / 占位符分配）
│   ├── src/desensitization/rules.rs        # 消息脱敏：值类型规则层（手写匹配器：PEM/连接串/网址/邮箱/车牌/银行卡/MAC/IP）
│   ├── tests/*_parity.rs                   # 与 Python 实现的对照测试（流/请求/用量/端到端）
│   ├── tests/runtime_loopback.rs           # 本机回环服务端上的内核行为测试
│   ├── tests/common/mod.rs                 # 测试脚手架（fixture 输入、回环服务端、事件接收端）
│   └── tests/fixtures/*_parity.json        # 由 Python 侧生成的对照数据集
├── crates/omnicrawl-session/               # 会话与记忆 crate（数据契约、存储 I/O、锁、投影、记忆、artifact）
│   ├── src/event.rs                        # 转录事件：create / from_dict / to_dict / to_json_line
│   ├── src/index.rs                        # index.json 索引条目
│   ├── src/naming.rs                       # 会话 id、事件类型、转录路径校验与标题折叠
│   ├── src/time.rs                         # 时间戳 ISO-8601（UTC、微秒）解析与格式化
│   ├── src/error.rs                        # 与 Python 同文案的会话错误
│   ├── tests/models_parity.rs              # 与 Python 的对照测试（107 用例）
│   ├── tests/round_trip.rs                 # 写入方自洽性（读回、再写字节一致）
│   └── tests/fixtures/session_models_parity.json
├── crates/omnicrawl-ipc/                   # 宿主桥接（协议 v1）
│   ├── src/frame.rs                        # NDJSON 帧、形状校验、错误码
│   ├── src/version.rs                      # 版本常量与主版本协商
│   ├── src/bridge.rs                       # 宿主事件、命令与工具批次映射
│   ├── tests/frame_codec.rs                # 帧层行为
│   ├── tests/bridge_round_trip.rs          # 方法与负载样本往返
│   └── tests/host_bridge_parity.rs         # 与 Python 宿主接口的契约对照
├── crates/omnicrawl-cli/                   # 内核进程（stdio 上的协议 v1 服务端）
│   ├── src/main.rs                         # 入口：--version / --help
│   ├── src/session.rs                      # 会话：握手、回合、两个宿主端口、取消守卫
│   └── README.md                           # 端口与错误映射、当前发出的事件
├── docs/protocol-v1.md                     # 协议 v1 规格（方法、负载、错误、版本）
└── tools/
    ├── gen_parity_fixture.py               # 协议层对照数据集生成脚本
    ├── gen_core_parity_fixture.py          # 回合循环对照数据集生成脚本
    ├── gen_llm_stream_fixture.py           # Provider 流解析对照数据集生成脚本
    ├── gen_llm_request_fixture.py          # 请求构建对照数据集生成脚本
    ├── gen_llm_usage_fixture.py            # 用量归一化对照数据集生成脚本
    ├── gen_llm_runtime_fixture.py          # 端到端回合对照数据集生成脚本（内建回环服务端）
    ├── gen_session_fixture.py              # 会话层模型对照数据集生成脚本
    └── gen_host_bridge_fixture.py          # 宿主桥接契约 fixture 生成脚本
```

## 构建与验证

```bash
cd rust
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

嵌入式 Linux 交叉编译（产物静态链接，便于塞进镜像）：

```bash
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

## 与 Python 的对应关系

语义基准是 `omnicrawl/llm/protocol.py`。除下表标注处，Rust 侧类型名与 Python 同名：

| Python | Rust |
| --- | --- |
| `ModelIdentity` | `ModelIdentity`（`triple()` 保留、`as_ref()` 改名 `reference()`） |
| `MessageBlock` 联合类型 | `MessageBlock` 枚举 |
| `TextBlock` / `ImageBlock` / `ToolCallBlock` / `ToolResultBlock` | 同名 |
| `ConversationMessage` | `ConversationMessage`（`role` 为 `Role` 枚举） |
| `ToolSpec` / `GenerationOptions` / `TokenUsage` | 同名 |
| `ModelStreamEvent` 联合类型 | `ModelStreamEvent` 枚举 |
| `TextDelta` / `ReasoningDelta` / `ToolCallStarted` / `ToolCallArgumentsDelta` / `ToolCallCompleted` / `UsageReported` / `ProviderWarning` | 同名 |
| 完成事件类 | 在内核里改为枚举的内联变体 `ModelStreamEvent::Finished { finish_reason }` |
| `aggregate_stream_events` 的返回类型 | `ModelReply` |
| `conversation_from_openai_messages` / `tools_from_conversation_messages` / `tool_spec_from_openai_item` / `_blocks_from_openai_content_parts` | 同名（最后一个去掉下划线前缀） |

Provider 与协议都是闭集：`openai` / `anthropic` / `gemini`，以及四种协议；未知取值解析为
`None`，与 Python 侧 `SUPPORTED_PROTOCOLS`、`PROVIDER_DEFAULT_PROTOCOL` 的判定一致。

## 对照（parity）工作流

Python 侧是语义基准，不靠人读代码对齐：

```bash
python rust/tools/gen_parity_fixture.py   # 用 omnicrawl/llm/protocol.py 生成期望值
cd rust && cargo test                     # tests/parity.rs 用同一份输入跑 Rust 实现逐字段比对
```

`tests/fixtures/protocol_parity.json` 随仓库提交，覆盖三类用例：消息转换、动态工具声明去重、
流事件归并（含未收到 Completed 的工具调用补全顺序、空流、告警与用量）。改了任一侧的协议
实现，都要重跑生成脚本再跑测试。

回合循环同理：

```bash
python rust/tools/gen_core_parity_fixture.py   # 用 omnicrawl/agent/runtime/execution.py 生成期望值
cd rust && cargo test -p omnicrawl-core        # 同输入重放并逐字段比对
```

`crates/omnicrawl-core/tests/fixtures/turn_loop_parity.json` 覆盖工具派发顺序、取消、超时、
空回复、模型错误、工具错误六类转移；状态转移清单与边界见 `crates/omnicrawl-core/README.md`。

流解析同理：

```bash
python rust/tools/gen_llm_stream_fixture.py   # 用 omnicrawl/llm/providers/openai_chat.py 生成期望值
cd rust && cargo test -p omnicrawl-llm        # 同输入跑 Rust 实现逐字段比对
```

`crates/omnicrawl-llm/tests/fixtures/openai_chat_stream_parity.json` 覆盖参数完整性、分片归并、
SSE 解码与 SSE 流消费；边界与已知差异见 `crates/omnicrawl-llm/README.md`。

请求构建与用量归一化同理：

```bash
python rust/tools/gen_llm_request_fixture.py  # 拦下 openai_chat.py 的 chat.completions.create 取真实 kwargs
python rust/tools/gen_llm_usage_fixture.py    # 用 omnicrawl/llm/usage.py 生成期望值
cd rust && cargo test -p omnicrawl-llm        # 同输入跑 Rust 实现逐字段比对
```

请求组的 fixture（`openai_chat_request_parity.json`）把 Python 交给 SDK 的 kwargs 拆成请求体与传输层
`timeout` 两部分记录，因此消息转换、动态工具去重、生成选项合并、prompt_cache_key 都是照真跑结果对照；
用量组（`openai_chat_usage_parity.json`）覆盖输入/输出字段名、缓存命中写法与推理 token 的三种来源。

端到端回合同理：

```bash
python rust/tools/gen_llm_runtime_fixture.py  # 内建回环服务端，喂真 runtime 固定 SSE
cd rust && cargo test -p omnicrawl-llm --test runtime_parity
```

`openai_chat_runtime_parity.json` 记录 Python 真实现收到的事件序列、它实际发出的**线上请求体**与失败文案；
Rust 侧用同一份 SSE 重放后逐字段比对事件、请求体、归并结果与错误文案。这一组把「SDK 参数」和「线上请求」
的差别逼了出来（`extra_body` 必须摊平进请求体顶层，否则会给上游发一个非标准字段）。

会话层的模型/校验同理：

```bash
python rust/tools/gen_session_fixture.py     # 期望值来自 session_models.py 真实现
cd rust && cargo test -p omnicrawl-session
```

`session_models_parity.json` 共 107 个用例（会话 id / 事件类型 / 相对路径 / 标题折叠 / 时间戳 /
事件与索引条目校验 / 载荷计数 / 事件创建 / 转录行），其中转录行一组是**字节级**比对：
会话文件是两个实现共享的长期格式，一侧多一个空格或换一个键序都必须被抓住。

宿主桥接同理：

```bash
python rust/tools/gen_host_bridge_fixture.py  # 反射 loop.py 的 run_stream 与循环 run 的签名
cd rust && cargo test -p omnicrawl-ipc        # 校验回调↔方法一一对应与负载往返
```

消息脱敏同理：

```bash
python rust/tools/gen_desensitization_fixture.py         # 期望值来自 registry.py 真实现
python rust/tools/gen_desensitization_stream_fixture.py  # 期望值来自 stream.py 真实现
python rust/tools/gen_desensitization_rules_fixture.py   # 期望值来自 rules.py 真实现
cd rust && cargo test -p omnicrawl-llm
```

脱敏组的期望值是把同一串操作喂给 Python 真实现后记下的（返回值、严格模式错误文案、三路还原计数、
逐条规则候选与整段扫描的区间）。两份数据集里的占位符都是**拼接构造**的：本仓库自己就是宿主，
在启用了消息脱敏的会话里写完整占位符字面量会被还原成会话原文，数据集照旧生成、测试照常通过，
解析用例却全变成「命中为空」的假绿（`desensitization_parity.json` 的 11 条解析用例曾因此失效）。
规则语料还自带每条文本的期望命中，生成器当场断言，避免语料被豁免表静默吃掉。

错误分类同理：

```bash
python rust/tools/gen_llm_errors_fixture.py              # 期望值来自 errors.py 真实现
cd rust && cargo test -p omnicrawl-llm --test llm_errors_parity
```

`llm_errors_parity.json` 有 55 个用例，覆盖全部 10 个分类码。数据集记录的是分类函数真正读到的字段
（`str(exc).strip()`、类型名、`exc.body`、`exc.response.json()`、两个状态码属性）与它给出的
码 / 文案 / 可重试标记 / 状态码——内核没有 SDK 异常对象，只能照这些字段等价重建。

模型能力与协议解析同理：

```bash
python rust/tools/gen_llm_registry_fixture.py             # 期望值来自 capabilities.py / registry.py
cd rust && cargo test -p omnicrawl-llm --test registry_parity
```

`llm_registry_parity.json` 覆盖能力解析（含非法值与窗口值边界）、合并优先级、四个保守默认值，
以及协议解析的 18 个用例与 `protocol_for_provider` 的 5 个用例（含三条报错文案）。
对象一律按**序列化后的字符串**比对：workspace 开了 `preserve_order`，键序也是契约的一部分。

OpenAI Responses 的请求构建同理：

```bash
python rust/tools/gen_llm_responses_fixture.py              # 期望值来自 providers/openai_responses.py
cd rust && cargo test -p omnicrawl-llm --test openai_responses_request_parity
```

`openai_responses_request_parity.json`：`input` items 20 例（含 reasoning item 的 SHA-1 id 与 55/56/63/64 的
分块边界）、请求级 tools 4 例、历史展平 6 例、工具历史判定 4 例、`create()` 参数 16 例。
`input` items 与 tools **逐字节**比对（键序也是契约），请求体按键集合与逐字段值比对（键序由 SDK 决定）。

## 已知与 Python 的差异

1. 非字符串字段（`role`、`tool_call_id`、工具名、`description` 等）不再走 Python 的 `str()`
   转换，一律回落默认值；实际模型请求里这些字段恒为字符串。
2. JSON 参数解析用 `serde_json`：不接受 `NaN`/`Infinity`（Python `json.loads` 接受），
   非法输入同样退化为空对象。
3. data URL 解析改为手写大小写不敏感解析，不引入 `regex` 依赖；判定条件与 Python 正则一致
   （整体锚定、媒体类型取 `png`/`jpeg`/`webp`/`gif`、payload 限 base64 字符集且非空）。
4. `Role` 是闭集加 `Other(String)` 透传：OpenAI Chat 分支会把未知角色原样写进请求体，
   透传语义与 Python 一致。
5. 请求体的键序可能不同：两侧都保留插入序（workspace 开了 `serde_json/preserve_order`），但 Python 侧的顺序
   由 SDK 按其签名序列化决定，内核按自己的组装序。请求体是临时报文，不追字节一致；需要逐字节一致的长期格式
   （会话文件）另有字节级对照，见 `crates/omnicrawl-session/README.md`。浮点写法也不同（`1e+20` vs `1e20`），
   逐项说明见 `crates/omnicrawl-llm/README.md` 的「已知差异」。
6. 重试位置不同：Python 的 SDK 自己会重试 5xx/超时（内置 2 次），内核不内置这一层，
   只把 `retryable` 交给调用方；`prompt_cache_key` 不受支持时的摘字段重发在内核内部完成。
7. 错误分类的分支判定已与 `errors.py` 对齐（含传输层：内核只给失败类型与 SDK 等价文案）；
   但**流内 `error` 负载**仍按「未能识别」处理——它的分类取决于 SDK 对这类负载的 `str(exc)` 形状，
   尚未钉住，见 `crates/omnicrawl-llm/README.md`。

## 内核进程：`omnicrawl-cli`

`crates/omnicrawl-cli` 产出 `omnicrawl` 二进制：在 stdin/stdout 上跑协议 v1，自己持有 `omnicrawl-core`
的回合循环，把两个宿主端口经协议外发——`tool.batch` 交宿主执行、`model.reply` 在过渡期由宿主代答模型。
npm 侧分发骨架在 `packages/cli`（启动器 + 三个平台分包）。

```bash
cargo build --release -p omnicrawl-cli
node packages/cli/scripts/prepare.mjs        # 暂存发布产物到 dist/npm
npm test -w @omnicrawl/cli                   # e2e：启动器 + 真二进制 + 协议 v1
```

## 后续 crate 规划

**协议 v1 与内核进程均已落地**（`docs/protocol-v1.md`、`crates/omnicrawl-ipc`、`crates/omnicrawl-cli`）；
插件侧按用户决策改用 Cordis 本体，宿主骨架在 `packages/plugin-host`（不再自研插件框架）；npm 分发骨架在
`packages/cli`（启动器 + 三个平台分包，发布产物由 `scripts/prepare.mjs` 暂存到 `dist/npm`）。继续推进：
`packages/plugin-host` 已接上协议 v1（`tool.batch` 的宿主对端、钩子挂到回合事件）；`omnicrawl-llm` 的
请求构建、传输与回合运行已落地，接下来是把 `omnicrawl-cli` 的回合切到内核自带 runtime，再把 Python 编排壳
（会话落盘、压缩触发）收到内核侧。
分发路线见知识库「分发路线与内核宿主边界决策」：主程序最终走 npm，内核与宿主的边界以
「进程 + NDJSON JSON-RPC / 平台二进制」为一等公民，PyO3 内联不再是路线图项。

`omnicrawl-llm` 与内核接线都已落地：宿主在 `initialize` 里给出可选的 `model` 块，内核就自己发模型请求，
增量经 `turn.delta` / `turn.reasoning_delta` / `turn.token_usage` 外发，重试与文案按 `request_retry_count`
保持与 Python 一致；没有该块时退回 `model.reply` 代答，新旧宿主可以同时存在。这条链由
`crates/omnicrawl-cli/tests/kernel_model_e2e.rs` 钉住（真拉起内核进程 + 本机回环服务端，
断言全程不出现 `model.reply`、请求体里带上了系统提示词与工具声明）。

`omnicrawl-session` 已覆盖会话与记忆两条链：数据契约（事件 / 索引模型、命名与时间校验、转录行字节布局）、
存储 I/O（目录初始化、新建、追加、读回、索引维护）、跨进程写锁与耐久写、会话投影与有状态投影、
记忆层（格式、排序与相似度、索引与读写、检索入口、写入清理、提示词段落、旧目录迁移）、
artifact 转存与核心凭据脱敏（`redaction.rs`）都有对照。未搬：归档、导出、一致性诊断、
运行期"已发往 Provider 的参数原文"提供者、子任务结果投影。

`omnicrawl-llm` 的消息脱敏已落地模块根（错误面 + 序号注册表）、流式还原、值类型规则层全部 11 条规则
（PEM / 连接串 / 网址 / 邮箱 / 车牌 / 银行卡 / MAC / 内外网 IP，全部手写匹配器）、匹配引擎
（结构层 / 键名规则 / 熵兜底 / 占位符分配）、middleware 编排件与 oneshot 一次性脱敏器（对照已转正，
3 例全绿）；余下运行时装饰器、gitleaks 规则表、locality 与扫描缓存、NER（torch 依赖）。

`omnicrawl-llm` 的运行时契约也已落地：`ModelCapabilities` / `merge_capabilities`（能力解析、合并优先级、
四个保守默认值）、`resolve_protocol` / `protocol_for_provider` / `validate_protocol_matches_provider`
（协议选取与一致性校验），以及 `ModelRuntime` trait——`omnicrawl-cli` 经 trait 对象持有运行时，
Provider 实现与出网脱敏装饰器都从这里换入。

接下来：Provider 逐个落地——OpenAI Responses 的**请求构建**已落地，下一步是它的流事件映射与回合运行，
随后 Anthropic 与 Gemini；之后是 adapter 注册表与 `build_runtime` 工厂；脱敏侧接
`DesensitizationRuntime` 装饰器（trait 已就位）；会话侧补归档、导出与一致性诊断。模型这条链上还剩两件宿主侧的事：
把真实的 Provider 配置（Python 侧的 models 配置 / 启动器）接进 `initialize.model`，
以及全部宿主迁移后让 `model.reply` 退役。
