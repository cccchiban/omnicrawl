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
│   ├── tests/parity.rs                     # 冻结对照契约测试
│   └── tests/fixtures/protocol_parity.json # 冻结的对照数据集
├── crates/omnicrawl-core/                  # 回合循环 crate
│   ├── src/types.rs                        # 循环的数据契约（工具调用/结果、回复、预算、错误）
│   ├── src/runner.rs                       # 模型循环执行器与注入式时钟
│   ├── tests/turn_loop_parity.rs           # 冻结对照契约测试
│   ├── tests/fixtures/turn_loop_parity.json# 冻结的对照数据集
│   └── README.md                           # 状态转移清单、边界与已知差异
├── crates/omnicrawl-llm/                   # Provider 运行时 crate（含 HTTP 传输）
│   ├── src/openai_chat.rs                  # 分片归并、参数完整性、首选项
│   ├── src/sse.rs                          # SSE 负载解码与逐条语义
│   ├── src/request.rs                      # 会话消息 → 请求体、工具声明、provider_options、prompt_cache_key
│   ├── src/usage.rs                        # Responses / Chat Completions 负载 → TokenUsage
│   ├── src/errors.rs                       # 运行时错误面与 HTTP 状态码阶梯文案
│   ├── src/transport.rs                    # 阻塞式 HTTP 往返（ureq + rustls）与超时映射
│   ├── src/runtime.rs                      # 一次回合：请求 → 流事件 → 工具收尾 → ModelReply
│   ├── src/gemini.rs                       # Gemini Generate Content：请求、线上映射、流事件与错误文案
│   ├── src/desensitization.rs              # 消息脱敏模块根：子系统错误面 + 序号注册表（占位符协议、稳定序号）
│   ├── src/desensitization/stream.rs       # 消息脱敏：流式还原（尾部挂起缓冲、结构化还原、严格模式）
│   ├── src/desensitization/engine.rs       # 消息脱敏：匹配引擎（结构层 / 键名 / 熵兜底 / 占位符分配）
│   ├── src/desensitization/rules.rs        # 消息脱敏：值类型规则层（手写匹配器：PEM/连接串/网址/邮箱/车牌/银行卡/MAC/IP）
│   ├── tests/*_parity.rs                   # 冻结对照契约测试（流/请求/用量/端到端）
│   ├── tests/runtime_loopback.rs           # 本机回环服务端上的内核行为测试
│   ├── tests/common/mod.rs                 # 测试脚手架（fixture 输入、回环服务端、事件接收端）
│   └── tests/fixtures/*_parity.json        # 冻结的对照数据集
├── crates/omnicrawl-session/               # 会话与记忆 crate（数据契约、存储 I/O、锁、投影、记忆、artifact）
│   ├── src/event.rs                        # 转录事件：create / from_dict / to_dict / to_json_line
│   ├── src/index.rs                        # index.json 索引条目
│   ├── src/naming.rs                       # 会话 id、事件类型、转录路径校验与标题折叠
│   ├── src/time.rs                         # 时间戳 ISO-8601（UTC、微秒）解析与格式化
│   ├── src/error.rs                        # 冻结文案的会话错误
│   ├── src/store.rs                        # 会话存储 I/O、生命周期编排、一致性报告入口与 undo 事务
│   ├── src/undo.rs                         # 最近一轮回退的事件计划（complete / incomplete 判定）
│   ├── tests/models_parity.rs              # 冻结对照契约测试（107 用例）
│   ├── tests/round_trip.rs                 # 写入方自洽性（读回、再写字节一致）
│   └── tests/fixtures/session_models_parity.json
├── crates/omnicrawl-ipc/                   # 宿主桥接（协议 v1）
│   ├── src/frame.rs                        # NDJSON 帧、形状校验、错误码
│   ├── src/version.rs                      # 版本常量与主版本协商
│   ├── src/bridge.rs                       # 宿主事件、命令与工具批次映射
│   ├── tests/frame_codec.rs                # 帧层行为
│   ├── tests/bridge_round_trip.rs          # 方法与负载样本往返
│   └── tests/host_bridge_parity.rs         # 宿主接口的契约对照
├── crates/omnicrawl-controllers/           # Agent 控制器域（判定、校验、文案、预算）
│   ├── src/shared.rs                       # 常量表、整数配置校验、未知工具/超时文案、哈希名反查
│   ├── src/settings.rs                     # 审批模式/推理强度归一化、压缩阈值换算、工具与 SubAgent 开关校验
│   ├── src/control.rs                      # 插件状态文案、退出收尾动作、关闭/停用前的排空决策
│   ├── src/approval.rs                     # 审批归属、shell 分流、git 风险分级、删除意图、审查结论解析
│   ├── src/advisor.rs                      # 顾问可用性、消息分支、工具清单、结果信封
│   ├── src/plugins.rs                      # Hook fail-closed 判定、拒绝事实与文案、分发结局
│   ├── src/tool_args.rs                    # 工具名/参数名归一化、参数投影、Schema 压缩与校验、结果信封
│   ├── src/tool_catalog.rs                 # 工具目录与注册规则
│   ├── src/context_compaction/             # Token 估算、回合预算测量、压缩批次、触发决策、摘要校验、投影
│   ├── src/json.rs                         # 兼容语义基准的 json.dumps / repr 子集（信封与摘要共用）
│   ├── src/undo.rs                         # undo 安全性判定、副作用账本、快照事件与恢复预检
│   ├── src/workspace.rs                    # 工作区切换校验与拒绝文案、内部目录保护
│   ├── src/memory.rs                       # 三类作用域记忆目录解析与会话级清理
│   ├── src/output.rs                       # 输出预算与落盘预览、工具结果消息、视觉旁路
│   ├── src/compression.rs                  # 工具输出压缩：选取、文案、压缩请求与回包清洗
│   ├── src/building.rs                     # 模式模板装载与 system prompt 组装
│   ├── src/turn/turn_loop.rs               # 回合接线：13 回调面、两个循环端口与守卫、收尾补发、失败分类
│   └── tests/controllers_parity.rs         # 冻结对照契约测试（825 用例）
├── crates/omnicrawl-commands/              # 斜杠命令框架与内置命令（`omnicrawl/commands/` 的 Rust 移植）
│   ├── src/framework.rs                    # 注册、解析、分发、候选/帮助派生
│   ├── src/agent.rs                        # CommandAgent 能力面（宿主注入）与命令用的值类型
│   ├── src/slash.rs                        # registry()/build_registry() 与 27 条内置命令、全部展示文案
│   └── README.md                           # 对映表、两处必要差异、尚未接线的宿主入口
├── crates/omnicrawl-compaction/            # 上下文压缩的会话/记忆编排与摘要模型适配器
│   ├── src/adapter.rs                      # 摘要请求：复用主请求前缀与工具面、tool_choice=none、用量累计
│   ├── src/driver.rs                       # 回合边界：测量落盘、压缩触发、归档、记忆回写与召回、历史重建
│   └── tests/{driver_round_trip,summary_adapter}.rs
├── crates/omnicrawl-cli/                   # 内核进程（stdio 上的协议 v1 服务端）
│   ├── src/main.rs                         # 入口：--version / --help
│   ├── src/session.rs                      # 会话：握手、回合、两个宿主端口、取消守卫
│   └── README.md                           # 端口与错误映射、当前发出的事件
├── crates/omnicrawl-host/                  # 宿主执行层（TUI 与本地 API 共用）
│   ├── src/kernel.rs                       # 内核进程客户端：NDJSON 帧读写、读线程与请求配对
│   ├── src/host.rs                         # 工具批次：审批/提问判定、执行派发、观察构造
│   ├── src/tools/                          # 工具执行体（30 个模块：文件、搜索、命令、监控、知识库、记忆、联网、视觉、桌面）
│   ├── src/approval.rs                     # 审批模式：manual / auto
│   ├── src/turn.rs                         # 无头回合运行器：握手、定调、并发执行、超时收口、取消
│   ├── src/prompt.rs                       # 启动期提示词装配：模板 / AGENTS.md / Skill 索引 / 模式区块 / 上下文消息
│   ├── tests/turn_flow.rs                  # 脚本化假内核上的回合流程测试（5 组）
│   └── README.md                           # 模块对映、与界面/API 的边界、已知差异
├── crates/omnicrawl-tui/                   # 全屏终端工作台（协议 v1 的 Rust 宿主前端）
│   ├── src/main.rs                         # 二进制入口：选内核、握手、进出全屏、事件循环
│   ├── src/app.rs                          # 接线层：内核帧 ↔ 状态机 ↔ 写回内核
│   ├── src/state.rs                        # 界面状态机：消息记录、输入框、遥测
│   ├── src/ui/                             # 渲染：HUD、消息流、输入框、面板
│   └── README.md                           # 本阶段边界与尚未实现清单
├── crates/omnicrawl-connectors/            # 消息平台连接器（Telegram Bot 与飞书自建应用）
│   ├── src/agent.rs                        # 连接器 ↔ 宿主边界：回合事件、驱动 trait、确认/提问桥
│   ├── src/telegram/                       # 配置、分段与裁剪、文件接收、更新路由、Bot API、轮询服务
│   ├── src/feishu/                         # 配置、文本、卡片渲染、资源、去重、时间线条目
│   ├── src/autostart.rs                    # 自动启动监督器：配置探测、子进程拉起、单例锁、退出回收
│   ├── tests/*_parity.rs                   # 冻结对照契约测试
│   ├── tests/autostart_process.rs          # 真子进程测试（拉起 → 采集日志 → 连后代回收 → 锁释放）
│   └── README.md                           # 模块分工、对照工作流、尚未移植清单
├── crates/omnicrawl-extensions/            # 扩展子系统（插件模型与注册表、Hook 分发、Skill、安装器）
│   ├── src/models.rs                       # Hook 表与策略、manifest 解析、JSON Patch、Handler 排序
│   ├── src/registry.rs                     # 注册表读写、user/project 合并、执行计划与 replaces 解析
│   ├── src/skill.rs                        # Skill 校验、frontmatter 解析、扫描与匹配、渐进式披露
│   ├── src/protocol.rs                     # Worker NDJSON JSON-RPC 客户端与环境变量白名单
│   ├── src/manager.rs                      # HookDispatcher / PluginManager / PluginRuntime
│   ├── src/install.rs                      # npm 安装、本地包、启停、卸载、回滚、doctor
│   ├── tests/extensions_parity.rs          # 冻结对照契约测试（23 组）
│   └── README.md                           # 模块分工、已知差异、尚未纳入对照的面
├── crates/omnicrawl-tts/                   # 语音合成：接口合成（默认）+ 可选 MOSS-TTS-Nano ONNX 本地推理
│   ├── src/api.rs                          # OpenAI 兼容 audio/speech、长文本分块与波形拼接
│   ├── src/runtime.rs                      # 8 个 ONNX session、prefill/decode、local 采样分支、codec 全量/流式解码（onnx）
│   ├── src/engine.rs                       # 文本分块、音色解析、参考音频编码、逐块合成与 WAV 写出（onnx）
│   ├── src/sampler.rs                      # PCG64 随机数与 top-k/top-p 采样
│   ├── tests/tts_runtime_parity.rs         # greedy 生成帧的冻结对照（需要模型与 --features onnx）
│   └── README.md                           # 模块对映、关键决策、验证与已知差异
├── crates/omnicrawl-mcp/                   # MCP 子系统（配置、传输、管理器、本地 Server）
│   ├── src/config.rs                       # [mcp] 配置段、环境变量覆盖、Server/传输/风险等级校验
│   ├── src/registry.rs                     # Tool/Resource/Prompt 元数据、去重诊断、命名空间化
│   ├── src/security.rs                     # 参数体积与轻量 JSON Schema 校验、密钥脱敏
│   ├── src/audit.rs                        # 工作区内 JSONL 审计、先脱敏后截断、时间源注入
│   ├── src/jsonrpc.rs                      # Content-Length 分帧、JSON-RPC 拆包、SSE 解析、能力分页
│   ├── src/stdio.rs                        # stdio 传输：子进程、常驻读线程、stderr 排空、超时回收重启
│   ├── src/http.rs                         # Streamable HTTP：会话头、协议头、SSE 响应
│   ├── src/client.rs                       # 多 Server 管理器：并发发现、状态与诊断、失败降级
│   ├── src/server.rs                       # 本地 stdio MCP Server（只读文档 / 内置文档 / Prompt）
│   ├── src/bundled.rs                      # 内置文档（include_str! 打进二进制）
│   ├── tests/*_parity.rs                   # 六组对照 + stdio 端到端自测
│   └── README.md                           # 模块对映、已知差异、宿主接线与未接线清单
├── crates/omnicrawl-api/                   # 本地 HTTP/SSE API 服务端（`omnicrawl/api/` 的 Rust 移植）
│   ├── src/config.rs                       # APIConfig 与 load_api_config（校验顺序、文案、环境变量优先级）
│   ├── src/error.rs                        # ApiError 与 {data} / {error} 信封
│   ├── src/app.rs                          # 路由装配、Bearer 鉴权、CORS、框架级 404/405
│   ├── tests/config_parity.rs              # 配置面逐条对照（15 组构造 + 26 组装载）
│   ├── tests/server.rs                     # 真实回环 HTTP 上的装配层端到端测试
│   └── README.md                           # 已搬范围、资源进度表、已知差异
├── crates/omnicrawl-config-chat/           # 配置对话（`omnicrawl/config_chat/` 的 Rust 移植）
│   ├── src/router.rs                       # 从句切分、BIO 解码、别名向量检索、命令列表
│   ├── src/router_weights.rs               # 内核权重（魔数 + 头部 JSON + f32 数据块）装载与双向 GRU 前向
│   ├── src/service.rs                      # 类型校验、TOML 写回、运行态同步（ConfigChatAgent）
│   ├── src/assets.rs                       # labels.json / aliases.json 资源面
│   ├── data/                               # 权重与两份 JSON 资源
│   ├── tests/config_chat_parity.rs         # 冻结对照契约测试（5 组）
│   └── README.md                           # 模块对映、算法差异、尚未接线
├── crates/omnicrawl-workspace/             # 工作区层（`omnicrawl/workspace/` 的 Rust 移植：slug + 隔离区 + 临时目录 + 进程控制 + 连接器单例锁）
│   ├── src/slug.rs                         # 路径段安全校验（fail-closed，隔离区实例 ID / 分支名共用）
│   ├── src/paths.rs                        # `Path.resolve()` / `expanduser()` / `home()` 的可用子集
│   ├── src/agent_isolation.rs              # worktree / local 隔离区：创建与复用、apply、四层门禁、退出收尾、启动清扫
│   ├── src/temp.rs                         # Agent 临时目录：分类子目录、间隔清理、启动补清理与后台线程
│   ├── src/process_control.rs              # 进程树控制（Windows Job Object / Unix 进程组）与跨平台 PID 存活探测
│   ├── src/connector_singleton.rs          # 连接器子进程的跨进程单例锁（锁文件路径、PID 行解析、粘滞接管）
│   ├── tests/workspace_isolation_parity.rs # 冻结对照契约测试（7 组：slug / diff 统计 / gitdir 解析 / 清扫条目 / 元数据 / 门禁）
│   ├── tests/workspace_temp_parity.rs      # 临时目录对照测试（配置 / 目录解析 / 子路径 / 删除 / 清理 / 间隔 / 状态文案）
│   ├── tests/connector_singleton_parity.rs # 单例锁对照测试（锁文件名 / PID 行解析 / 陈旧锁接管 / 存活判定）
│   ├── tests/isolation_git_roundtrip.rs    # 真 git 集成测试（worktree 往返、复用、local 镜像、收尾摘要）
│   ├── tests/connector_lock_process.rs     # 真文件锁与真进程测试（同进程互斥、跨进程互斥、持有者退出后接管）
│   └── README.md                           # 模块对映、四层门禁、实现差异、尚未移植的同包模块
├── crates/omnicrawl-entry/                 # 统一启动入口（`entry.py` 路由 + `cli.py` 插件 CLI）
│   ├── src/main.rs                         # 二进制 `omnicrawl-host`：help/version、kernel/api/TUI 子进程路由与退出码传播
│   ├── src/cli.rs                          # `plugin ...`：参数面、作用域解析、十个子命令与退出码阶梯
│   ├── src/startup.rs                      # 启动编排：首次配置与诊断、Node/插件探测、连接器自动启动与子进程形状
│   ├── src/channel_setup.rs                # 首次配置的渠道向导（行式，复用配置域的原子写与校验）
│   ├── tests/plugin_cli_parity.rs          # 冻结对照契约测试（参数面 44 例 + 作用域 10 例 + 退出码）
│   └── README.md                           # 已迁移 / 尚未迁移、参数面差异、验证边界
├── docs/protocol-v1.md                     # 协议 v1 规格（方法、负载、错误、版本）
├── docs/python-free-build.md               # 纯 Rust 构建环境要求（C/C++ 工具链、交叉编译）
└── tools/
    └── btls-msvc-runtime.cmake             # MSVC 下 BoringSSL 的 CMake toolchain file
```

## 构建与验证

```bash
cd rust
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

构建 `omnicrawl-host` 及其上层（`omnicrawl-tui` / `omnicrawl-api` / `omnicrawl-cli`）会
编译 BoringSSL（`fetcher` 的浏览器指纹传输用 wreq + btls），因此还要求 C/C++、CMake、
perl、libclang 与 NASM；Windows 上必须设 `CMAKE_TOOLCHAIN_FILE`，仓库路径含非 ASCII
字符时还要把 `CARGO_TARGET_DIR` 指到纯 ASCII 目录。完整清单与排障见
[`docs/python-free-build.md`](docs/python-free-build.md)。

嵌入式 Linux 交叉编译（产物静态链接，便于塞进镜像）：

```bash
rustup target add aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf
cargo build --release --target aarch64-unknown-linux-musl
cargo build --release --target armv7-unknown-linux-musleabihf
```

## 命名对应关系

除下表标注处，Rust 侧类型名与语义基准同名：

| 语义基准 | Rust |
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
`None`，与冻结契约里 `SUPPORTED_PROTOCOLS`、`PROVIDER_DEFAULT_PROTOCOL` 的判定一致。

## 对照（parity）工作流

各 crate 的 `tests/*_parity.rs` 与 `tests/fixtures/*.json` 是**冻结的对照契约**：fixture 记录了
Rust 实现必须复现的逐字段结果（部分用例是字节级比对，键序也是契约），随仓库提交，跑

```bash
cd rust && cargo test
```

即可全量校验，不依赖任何外部运行时。77 份 fixture 覆盖协议编解码、回合循环、四路 Provider 的
请求构建/流解析/用量与错误分类、会话模型与存储、连接器、Agent 控制器、扩展层、MCP、配置对话、
工作区隔离与临时目录、插件 CLI、TTS 与 TUI 工具面。各 crate 的覆盖范围与已知差异写在对应
`crates/<name>/README.md` 里。

改动了被测实现却让 fixture 失配时，先确认是**实现回归**还是**契约本身要更新**：前者改实现，
后者在同一个提交里同时改实现与 fixture，并在提交信息里说明是契约变更。fixture 是唯一事实来源，
不再有生成脚本——需要新用例时按既有用例的形状手工补进对应 JSON。

真动进程、网络或文件系统的面（内核 e2e、回环服务端、真 `git` worktree、子进程锁、stdio 端到端）
由各 crate 的 `tests/` 里的独立用例覆盖，环境缺 `git` 等前置时整组跳过，清单见各 crate README。

## 已知与语义基准的差异

1. 非字符串字段（`role`、`tool_call_id`、工具名、`description` 等）不再走 Python 的 `str()`
   转换，一律回落默认值；实际模型请求里这些字段恒为字符串。
2. JSON 参数解析用 `serde_json`：不接受 `NaN`/`Infinity`（Python `json.loads` 接受），
   非法输入同样退化为空对象。
3. data URL 解析是手写的大小写不敏感解析；判定条件与 Python 正则一致
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
的回合循环，把两个宿主端口经协议外发——`tool.batch` 交宿主执行、`model.reply` 由宿主代答模型。
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
请求构建、传输与回合运行已落地，内核自带 runtime 已接上（见下文），会话落盘与压缩触发也已在内核侧。
分发路线见知识库「分发路线与内核宿主边界决策」：主程序最终走 npm，内核与宿主的边界以
「进程 + NDJSON JSON-RPC / 平台二进制」为一等公民，PyO3 内联不再是路线图项。

**启动编排已脱离 Python**：`omnicrawl-host` 的默认路由自己跑首次配置与渠道向导
（`crates/omnicrawl-entry/src/channel_setup.rs`）、Node/插件启动诊断、连接器自动启动与退出回收；
提示词装配（模板 / AGENTS.md / Skill 索引 / 模式区块 / 上下文消息）在
`crates/omnicrawl-host/src/prompt.rs`，经 `initialize.model.{system_prompt,context_messages}`
进入真实请求路径，模式切换（`/plan`）经 `session.settings` 即时下发。
npm 平台包的宿主载荷也换成 Rust 产物：`packages/cli/scripts/build-host.mjs` 默认走
`cargo build --release`（宿主 + 内核 + TUI + API + MCP Server + 模板），
`--target <三元组>` 指到交叉目标并让产物目录跟三元组走（Windows 侧 CI 会传它，产物与内核同处）。
旧的 `--legacy-python` PyInstaller 冻结路径已随「彻底脱离 Python 宿主」删除
（`packaging/pyinstaller/` 一并移除）：构建期不再需要 Python。Linux 侧宿主载荷原先只能构建原生 gnu 目标
（`omnicrawl-tts → ort-sys` 没有 musl 预编译库）；TTS 改成「接口合成为主、本地 ONNX 为可选 feature」后
`ort` 退出了默认依赖树，musl 宿主载荷已可在本机交叉编出（五个产物全静态链接，
详见 `docs/python-free-build.md` 第 7 节）。

`omnicrawl-llm` 与内核接线都已落地：宿主在 `initialize` 里给出可选的 `model` 块，内核就自己发模型请求，
增量经 `turn.delta` / `turn.reasoning_delta` / `turn.token_usage` 外发，重试与文案按 `request_retry_count`
保持与 Python 一致；没有该块时退回 `model.reply` 代答，新旧宿主可以同时存在。这条链由
`crates/omnicrawl-cli/tests/kernel_model_e2e.rs` 钉住（真拉起内核进程 + 本机回环服务端，
断言全程不出现 `model.reply`、请求体里带上了系统提示词与工具声明）。

`omnicrawl-session` 已覆盖会话与记忆两条链：数据契约（事件 / 索引模型、命名与时间校验、转录行字节布局）、
存储 I/O（目录初始化、新建、追加、读回、索引维护）、跨进程写锁与耐久写、会话投影与有状态投影、
记忆层（格式、排序与相似度、索引与读写、检索入口、写入清理、提示词段落、旧目录迁移）、
artifact 转存与核心凭据脱敏（`redaction.rs`）都有对照，会话生命周期编排
（重命名 / 导出 Markdown / 归档 / 取消归档 / 删除 / 丢弃空会话 / 带筛选的列表 / 项目路径 / artifact 读回）
与 `append_event` 的载荷整理（超长输出转 artifact + 值级脱敏）也已落地；一致性诊断同样是完整链路：
转录/artifact 发现、条目重建与对照、全量扫描报告、备份与索引回写（`consistency.rs` + `check_consistency` /
`rebuild_index`），最近一轮回退的存储事务（`undo.rs` 的计划构建 + `prepare_undo_last_turn` / `commit_undo_plan` / `undo_last_turn`）与 `read_active_events` 有效事件视图也已落地。运行期「已发往 Provider 的参数原文」提供者已落到投影器（`with_raw_arguments_provider`）；子任务事件与 Python 一样只落转录，不进会话投影。

`omnicrawl-llm` 的消息脱敏已落地模块根（错误面 + 序号注册表）、流式还原、值类型规则层全部 11 条规则
（PEM / 连接串 / 网址 / 邮箱 / 车牌 / 银行卡 / MAC / 内外网 IP，全部手写匹配器）、匹配引擎
（结构层 / 键名规则 / 熵兜底 / 占位符分配）、middleware 编排件与 oneshot 一次性脱敏器（对照已转正，
3 例全绿）、gitleaks 规则表、NER 兜底层（前向 + 权重 + 对照）、运行时装饰器与**屏蔽计划缓存**
（`desensitization/plan_cache.rs`，按文本指纹重放匹配计划；逐消息缓存与计划缓存都由运行时持有、
`close()` 一起清空）；余下把 NER 层接进 `mask_text` 末尾、`locality` 局部化扫描。

屏蔽计划缓存的对照数据集与说明见 `crates/omnicrawl-llm/README.md`。

`omnicrawl-llm` 的运行时契约也已落地：`ModelCapabilities` / `merge_capabilities`（能力解析、合并优先级、
四个保守默认值）、`resolve_protocol` / `protocol_for_provider` / `validate_protocol_matches_provider`
（协议选取与一致性校验），以及 `ModelRuntime` trait——`omnicrawl-cli` 经 trait 对象持有运行时，
Provider 实现与出网脱敏装饰器都从这里换入。

接下来：运行时的组装面（`build_runtime` 工厂 + 四路 Provider 的 `discover_models`）已进内核，
下一批是把 `omnicrawl-cli` 的 `KernelModelPort::runtime()` 从「只造 OpenAiChatRuntime」改成经
`build_runtime` 按协议选择；脱敏侧接 `DesensitizationRuntime` 装饰器（trait 已就位）；
会话侧补 undo 存储事务与有状态投影。上下文压缩这条链已在 Rust 侧补齐到「除内核接线外」的全部：`omnicrawl-controllers` 的
`context_compaction`（账本、证据恢复、结构化摘要生成、压缩编排）与 `turn/compaction` 的判定面共有
13 例对照；`omnicrawl-compaction` 提供会话/记忆编排（测量事件落盘、压缩触发、二级归档、
记忆回写与自动召回、历史重建）与摘要模型适配器（复用主请求前缀与工具面、`tool_choice=none`），
由 `omnicrawl-session` 的 `archive_compacted_events` / `read_compacted_events` 支撑二级归档。
**已接线**：`initialize` 可以带一个可选的 `session` 块（会话根、会话 id、记忆根、压缩策略）。
内核据此自己持有转录与多轮历史：回合结束把用户消息与最终回复落盘，按阈值跑一次压缩
（摘要请求复用主请求前缀与工具面、`tool_choice=none`），把 `context_compaction_measurement` /
`compact_summary` / `context_compaction_failed` 写进会话，并把「已压缩」提示经协议外发；
下一轮上下文由压缩后的历史给出。端到端用例见 `crates/omnicrawl-cli/tests/compaction_e2e.rs`
（真二进制 + 本机回环服务端）。

上下文超限时内核压缩当前未完成回合并自动续接同一个回合：恢复提示（
`请依据上方的结构化工作摘要继续完成当前任务。`）随事件落盘，重试上下文只含摘要与续接指令。

`recall_session_evidence` 也已在核心里闭环：内核自己持有会话时，这个「只读当前会话」的工具由内核
直接作答（读转录 + `archive/compacted/` 归档 + 摘要授权校验），不占用宿主的 `tool.batch`。

`omnicrawl-controllers` 已覆盖判定层与回合接线：`agent/controllers/` 的判定层（`shared`、`approval`、
`undo`、`workspace`、`memory`、`settings`、`control`、`advisor`、`plugins`、`tool_args`、`tool_catalog`、`context_compaction`，以及
`tools/` 的 `output`／`compression`／`building`）已落地并有 825 例对照；回合计线（`src/turn/turn_loop.rs`）已把宿主的模型请求与工具批次接到 `omnicrawl-core` 的循环，并把过程报告接到 `omnicrawl-ipc` 的回调面；setter 事务与资源关闭、`approval` 编排段、
插件 Runtime 与顾问模型面、`_build_tools` 工具表构建、`turn/compaction` 与 `turn/loop.py` 的编排壳（依赖
`agent/context_compaction/`）、`subagents/*` 依赖 `toolkit`／`session`／`core`／
`context_compaction` 的既有实现，随后续批次收口。

模型这条链上还剩两件宿主侧的事：
把真实的 Provider 配置（Python 侧的 models 配置 / 启动器）接进 `initialize.model`，
以及全部宿主迁移后让 `model.reply` 退役。

## 全屏终端工作台：`omnicrawl-tui`

`crates/omnicrawl-tui` 产出 `omnicrawl-tui` 二进制，是协议 v1 的 Rust 宿主前端：起内核进程、
渲染内核通知（HUD、消息流、思考段、工具卡）、把输入与审批/提问决定回给内核。
它按「判定留内核、渲染与交互留宿主」分工，因此同一个内核可以同时被启动器、连接器与 TUI 驱动。

已落地：HUD、消息流、输入框、审批面板、提问面板、任务清单、取消/退出收尾，以及**工作区工具执行体**——
`read` / `write_file` / `Edit_file` / `bash` / `powershell` / `list` / `find` / `grep` / `git` / `monitor` 十个工具按
既定语义实现（保护路径、行窗口与 footer、count 语义、行尾风格、文件锁 + 原子写、显式解释器与超时回收、
ripgrep 同族的遍历与忽略规则、git argv 直调与有界输出、后台命令的环形缓冲与游标轮询），
并由工具表生成 `initialize.model.tools` 声明；工具表、参数归一化与 Schema 校验复用 `omnicrawl-controllers`，
声明与行为由冻结契约钉住（`tests/workspace_tools_parity.rs`、`tests/search_tools_parity.rs`）。
审批语义沿用 manual 分支：只对 shell 命令与非只读 git 操作确认。

界面层按目录对映既有设计：渲染侧已落 `difflib`（`SequenceMatcher` 等价）、`tool_diff`、`widgets`、
`markdown`、`welcome_logo`、`logo_anim` 与 `latex`（LaTeX → Unicode 近似文本，含 `split_blocks` / `has_block_formula`），
状态侧已落 `hud` / `indicators`，启动画面与 `tool_labels` 也已接入；对照片为
`tests/latex_parity.rs`（107 例转换 + 9 例分段 + 26 例快判）。

已接线的运行期能力（不再列在“未接线”里）：`/advisor`、`/memory:clean`、`/workspace` 的运行中切换
（宿主做子 Agent 排空与 pending worktree 拦阻、在工作线程装配新工具表/MCP/提示词运行时、请内核在同一
会话转录 `workspace_switched`，见 `crates/omnicrawl-tui/README.md`）、会话历史页的事件流回放
（`session.events` + `AppState::replay_events`；内核按 Python 口径落 `tool_call_requested` /
`tool_result` / `tool_call_denied`，工具卡、计划清单与 SubAgent 进度树都能按事件还原，消息投影只在
事件流读不到时兜底）、慢命令的后台执行（`/workspace` 候选装配与 `/mcp` 状态文本在线程里跑）、
`prompt_cache_identity` 的组装与后台任务日志的界面轮询（分别由宿主同步顾问选项并重建工具表、按三个作用域清理过期记忆、
在 TUI 与无头运行器的 `handshake()` 组装七字段身份、按 0.5 秒节流把 `monitor` 增量追回消息流；
后者见 `crates/omnicrawl-tui/src/monitor.rs`）。
`prompt_cache_capable` 也不再恒为假：自定义模型条目的 `capabilities.prompt_cache` 经 `LlmConfig`
流到 `initialize.model`；退出收尾的 `session.close.before` / `after` 已在 TUI 与本地 API 接线，
内核在两者之间补写 `session_closed` 并丢弃空占位。
工具名的**线上形态**也收敛过了：MCP 的 `server.tool` 与资源名里的 `:`/`/` 会被上游的
`^[a-zA-Z0-9_-]+$` 拒掉（`Invalid 'tools[0].function.name'`），内核在发请求前用
`conform_tool_names` 换成合法名、拿回调用后再还原成内部原名（宿主工具表、审批、审计、
转录与界面看到仍是原名；Python 侧未收敛，见 `crates/omnicrawl-cli/README.md`）。
身份指纹与 Python 逐字节对齐，见 `crates/omnicrawl-tui/README.md`。
工具执行体本身（`read_image`、`web_search`、`fetcher`、
`image_gen`、`tts_synthesize`、知识库、记忆、Windows 桌面、`advisor`）已在
`omnicrawl-host/src/tools/` 落地，缺口在 `crates/omnicrawl-host/README.md` 与本文件的
`crates/omnicrawl-tui/README.md` 里逐项列出。
边界与缺口清单见 `crates/omnicrawl-tui/README.md`；Python 侧 Textual 工作台在迁移完成前仍在服役。
