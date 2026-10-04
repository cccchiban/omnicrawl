# omnicrawl-host

协议 v1 宿主侧的执行层：内核子进程客户端、工具批次（审批/提问/执行）、工具执行体与无头回合运行器。
TUI 与本地 API 共用这一层——界面状态留在 TUI，HTTP/SSE 留在 `omnicrawl-api`。

语义基准是 Python 侧的 `omnicrawl/agent/toolkit/`、`omnicrawl/workspace/`（工具执行体）与
`omnicrawl/agent/loop.py`（回合回调）。

## 模块

| 文件 | 职责 |
| --- | --- |
| `src/kernel.rs` | 内核进程客户端：起子进程、NDJSON 帧读写、读线程、请求/响应配对；另提供**凭据注入**（`model_credentials_env` / `kernel_credentials_env` / `frame_api_key_env`）——协议帧只带变量名，内核只从环境读密钥，所以 `config.toml` 里的字面 `api_key` 必须由宿主在起内核时补进子进程环境 |
| `src/host.rs` | 工具批次：整批定调（`update_todos` / `pause_work` / `ask_user` 就地办，敏感工具等审批）、执行派发、观察构造 |
| `src/tools/` | 工具执行体与注册表：`paths`（保护路径）、`read`、`read_image`、`image_gen`、`write`、`edit`、`command`、`monitor`、`finding`、`grep`、`listing`、`git`、`knowledge`、`memory`、`web_transport`（ureq 阻塞式传输）、`wreq_transport`（浏览器指纹传输，仅 `fetcher` 用）、`web_search`、`fetcher`、`sample`、`tts`（两条后端：接口合成默认、本地 ONNX 需 `omnicrawl-tts/onnx`）、`advisor`、`windows/*`（非 Windows 只保留「仅支持 Windows」分支；`windows::clipboard::write_clipboard_text` 额外供 TUI 的「鼠标拖选即复制」复用）、`declarations`、`registry` |
| `src/approval.rs` | 审批模式（`manual` / `auto`）与字面量解析 |
| `src/turn.rs` | 无头回合运行器：握手、整批定调、并发执行、超时收口、取消与事件出口 |
| `src/process_control.rs` | 跨平台进程树控制（对映 Python `workspace/process_control.py`）：Windows 的 kill-on-close Job Object、Unix 的进程组整组回收、进程组创建与 `process_group_of` 诊断 |
| `src/prompt.rs` | 启动期提示词装配：模板 → system prompt、AGENTS.md 合并、Skill 目录扫描、模式切换与 `context_messages`（对映 `agent/controllers/tools/building.py` 与 `agent/core.py` 的启动准备） |
| `src/prompt_cache.rs` | 稳定 prompt 前缀的身份指纹：`initialize.model.prompt_cache_identity` 的七字段组装（对映 `agent/context/prompt_context.py::build_prompt_cache_identity`），与 Python 逐字节对齐；TUI 与无头运行器都在握手时调用 |

## 提示词装配

`prompt.rs` 是宿主侧唯一的提示词入口，三类输入都在这边读盘，判定与文案在
`omnicrawl-controllers`（`building` / `turn::prompt_context`）：

- **system prompt**：`templates/system_prompt.md`（可执行文件祖先目录优先，缺失用
  `include_str!` 的内嵌副本）→ `build_system_prompt` 拒绝旧动态占位符
  → 顾问准则（启用且未命中黑名单时）→ `<active_mode_prompt>`（模式已启用时）。
- **项目规范**：用户级 `~/.OmniCrawl/AGENTS.md` 与项目级 `<工作区>/AGENTS.md` 合并，
  项目级排在后面并优先；**读不到文件不算错误**，没有规范时不注入该消息。
- **Skill**：`SkillManager.discover(工作区, 额外路径)` 扫描企业 / 个人 / 项目 / 额外路径，
  `format_skills_for_prompt` 产出索引段（不注入正文；命中时才走 `read` 读 SKILL.md）。

装配结果经 `initialize.model.system_prompt` / `context_messages` 交给内核；模式切换
（`/plan`）重算后经 `session.settings` 即时下发。内核每轮把 `context_messages` 插在历史之前，
**不写进会话转录**（它们是本轮读到的环境，不是历史）。

`PromptOptions.system_prompt_override` 只在**显式覆盖**时有值（TUI 的 `--system-prompt` /
`OMNICRAWL_SYSTEM_PROMPT`、嵌入方的自备文本）；它是 `Option` 就是为此——曾经那条调用链把
「没给」也回落成一句占位文案再传进来，于是模板永远被顶掉，模型收到的 system 只有那一句。
无头宿主（本地 API）恒传 `None`：`config.toml` 的 `llm.system_prompt` 是语音客户端的文案，
不参与 Agent 提示词（与 Python 一致）。

## 进程树回收

两条互补路径，`bash` / `powershell` 与 `monitor` 三处共用：

- **Windows**：子进程纳入 Kill-On-Job-Close 的 Job Object（`KillOnCloseJob`），句柄无论正常关闭还是宿主崩溃时由
  操作系统关闭，Job 内的全部后代被递归终止；拿不到 Job（当前进程已在不可嵌套的 Job 里等）时退回 `taskkill /T /F`；
- **Unix**：子进程创建时以 `process_group(0)` 自成进程组（`pgid == pid`），回收时对整组发 `SIGKILL`
  （`kill(-pgid)`），因此 `bash -c '… & …'` 拉起的后代也一起终止——只杀直接子进程会留下孤儿。
  Unix 侧直接声明 `kill` / `getpgid` 的 C 符号，不引入 `libc` 依赖（与 Windows 侧声明 Win32 API 同一风格）。

`terminate_process_tree(child, job, wait)` 的 `wait` 与 Python 同义：正常 timeout / stop / close 路径用 `true`
拿到稳定终态；`Esc` 取消路径用 `false`——发出终止请求就返回（Windows 下不等待 `taskkill`，Unix 下 `killpg`
本身不阻塞），不让界面取消变成另一个阻塞操作。

## 工具表与工具开关

`registry.rs` 的工具表由 `omnicrawl-controllers` 的 `build_agent_tools` 按 `RegistryOptions` 生成：

- `disabled_tools` 来自 config.toml 的 `tools` 段（`omnicrawl-config` 的 `load_disabled_tools`），
  被关掉的工具不进表——模型不可见即不可调用，与 Python 的 `agent.config.disabled_tools` 同一口径；
- 工具开关只影响这张表，所以设置面板改开关时重建工具表会把旧表的 `monitors` / `cancel` 两个句柄
  带进新表（`RegistryOptions.monitors` / `.cancel`）：否则一次开关会把正在跑的后台命令从宿主账上抹掉、
  并丢掉回合取消的进程树回收语义。宿主若不传这两个字段，新表照旧各建一份。
- 取消令牌（`tools::command::CancelToken`）既然跨回合、跨重建沿用，**就必须在每回合开始时复位**
  （`CancelToken::reset`）：TUI 在 `dispatch_submission` 发 `turn.submit` 前复位，无头运行器在
  `TurnRunner::run_turn` 开头复位。不复位时「`Esc` 取消后继续对话」的下一回合里，每个
  `bash` / `powershell` 都会在子进程刚起来时被判定为已取消——令牌只置位不回零，是跨回合状态。

## 决策模型审查（可选，默认关闭）

`review.rs` 是 `approval.mode = review` 的审查闸：静态规则判定为 Review 的调用（删除类、
下载并执行类、高风险 Git）交给审查者做最后一道安全闸，失败一律 fail-closed。默认走对话模型
（独立身份提示词 + 一次单轮补全，`{"approve": ...}` JSON）；`decision_models.toml` 的
`[features] tool_call_review` 打开后改走结构化决策模型（`ReviewChannel`，`review_options_from_config`
由 TUI 与本地 API 共用装配）。

- 决策通道把同一份待审查负载当 `state`，提**两个** choice 提问：`tool_call_verdict`
  （`approve` / `reject`）与 `tool_call_reject_reason`（拒绝理由，候选项键 `r0`、`r1`…，
  候选表见 `DECISION_REVIEW_REJECT_REASONS`：未经允许删除工作区外文件、删除范围越界、
  删除目标不明确、下载脚本后直接执行、高风险 Git 超出任务范围 / 不可逆且未授权、调用与任务
  目标不符）。一次往返取回两个答案，不生成文本、不用解析。
- 判定拒绝时，拒绝文案以**选中的候选理由**开头（附置信度），主模型一眼能看出这是审查者的判断
  而不是决策服务故障；理由答案缺失或键落在候选表之外时退回原有的「决策模型判定拒绝（confidence …，
  响应：…）」诊断文案，不编造理由。
- 出网内容沿用 `[desensitization]` 旁路（`masking_from_config`）；决策渠道不可用、开关打开但没
  渠道、响应取不出答案都算拒绝（fail-closed，不退化成对话模型审查）。
- 回环用例在 `review.rs` 内部自带（`DecisionCassette`：`set_choice` / `set_reason` 两个旋钮，
  覆盖结论解析、选中理由、表外键与缺答回落）。

## 检索重排（可选，默认关闭）

`tools/decision_search.rs` 把 `memory_search` 与 `kb_search` 的候选交给结构化决策模型按相关度
排序。两个开关各自独立（`decision_models.toml` 的 `[features]` 里的 `memory_search_rerank` /
`kb_search_rerank`），装配入口 `rerank_options_from_config(env, switch_key)` 由 TUI 与本地 API
共用（`RegistryOptions.knowledge_rerank` 与 `MemoryOptions.rerank`）。

- 开启后本地检索先取更宽的候选池（`RERANK_CANDIDATE_LIMIT`，最多 20 条），再向决策服务提一个
  `choice` 问题（候选项键 `c0`、`c1`…），按 `answers.<id>.probabilities` 降序排列；**返回条数仍按
  调用方的 `max_results`**，不做固定截断。
- **失败一律 fail-open**：开关没开、没有可用决策渠道、缺凭据、网络失败、响应不可解析都退回本地
  排序结果（与审查通道的 fail-closed 相反，是本功能刻意选的：检索少几条比检索直接失败代价小）。
- 出网内容沿用审查通道的 `[desensitization]` 旁路（`review::masking_from_config`）；脱敏构造失败
  按「重排不可用」处理，绝不外发原文。
- `RerankClient` 是注入点：测试用桩（`memory.rs` 的 `StubRerank`）替换真实 HTTP 调用，回环用例在
  `decision_search.rs` 内部自带。

三个调用点（`review.rs` 的工具调用审查、本模块的检索重排与提问托管）共用 `decision_wire`
这一层线格式：按渠道的 `mode` 把同一份 `state` + `questions` 组装成请求（`jev` 走
`POST /v1/decide`；`chat_completions` 走 `POST /v1/chat/completions`，作为 user 消息发出），
并从响应里取出同一形状的 `answers`（后者从 `choices[0].message.content` 里解析）。新增请求方式
只需改配置域与这一层，调用点的解析逻辑不动。

## 提问托管（可选，默认关闭）

`tools/decision_choice.rs` 把 `ask_user` 的**有选项**提问交给结构化决策模型自动作答
（`decision_models.toml` 的 `[features] ask_user_custody`，`custody_options_from_config(env)`
由 TUI 与本地 API 共用）。

- 请求的 `state` 带三类上下文：`question`（提问正文）、`user_prompt`（用户本回合的请求）、
  `advisor_replies`（本回合已有的顾问答复，没有时该字段不出现）；问题是一个 `choice` 提问
  （候选项键 `o0`、`o1`…），读回 `answers.<id>.choice` 对应的选项作为答案，缺失时退化取
  `probabilities` 最高的一项。
- **没有选项的提问不托管**：决策模型只能从给定候选项里选，写不出自由文本，因此照旧交给用户。
- **失败一律 fail-open**：开关没开、没有可用决策渠道、缺凭据、请求失败、响应不可解析都退回人工
  提问，面板照旧停在那里等用户（与检索重排同一语义，和审查通道的 fail-closed 相反）。
- 自动作答在会话流里留一条可见提示（`custody_notice`：提问 → 选中项），用户看不到面板也能知道
  发生了什么。出网内容沿用审查通道的 `[desensitization]` 旁路。
- `ChoiceClient` 是注入点：回环用例在 `decision_choice.rs` 内部自带；流程用例在
  `tests/turn_flow.rs`（`custody_answers_the_question_without_asking_the_user`）用 `FixedChoice` 桩。

## 与两端的边界

- **内核**：只走协议 v1（`rust/docs/protocol-v1.md`）。模型请求由内核自己发（`initialize.model`），
  宿主只执行工具并把整批观察按模型顺序回填。
- **界面（TUI）**：`omnicrawl-tui` 依赖本 crate，并按原路径再导出 `tools` / `host` / `kernel`，
  因此历史调用点与集成测试无需改动。渲染与按键仍留在 TUI。
- **API（`omnicrawl-api`）**：用 [`turn::TurnRunner`] 驱动回合，用 [`turn::Interactor`] 把审批与提问
  接到 HTTP（阻塞等待用户决定）；事件出口直接翻成 SSE。

## 无头回合运行器

```rust
let runner = TurnRunner::new(kernel, options, &registry_options)?;   // 起表与握手前准备
runner.handshake(&mut |event| { /* 事件出口 */ })?;
let outcome = runner.submit("你好", &control, &mut interactor, &mut |event| { /* 事件出口 */ })?;
```

- `TurnControl` 是跨线程取消开关：置位后 `submit` 先发 `turn.cancel`，再回收本回合的进程树与后台任务，
  最后以 `TurnError::Cancelled` 收尾。运行器与工具表跨回合复用（API 服务持同一份 `TurnRunner`），
  因此 `run_turn` 开头会复位工具表的取消令牌；取消只对当回合生效，下一回合照常执行命令。
- `Interactor` 是唯一的「问人」入口：`decide`（审批）与 `answer`（提问）。API 侧的实现会阻塞等待
  HTTP 提交的决定；`None` 一律按拒绝/未作答处理，与 Python 的超时语义一致。
- 事件出口收到的是协议通知原文（[`omnicrawl_ipc::bridge::HostEvent`]），另加宿主产生的
  `tool.started` / `tool.finished` / `todo.update`（协议规定工具生命周期由宿主发出）。
  每个调用只报一次开始与一次完成：清单工具成功时不产生工具卡，只发 `todo.update`。
- 批次超时按**绝对截止时间**算（与 Python 一致）：到点未回填的调用写成超时结果，后台线程继续跑但结果被丢弃。
- `RunnerOptions.prompt` 给 `Some(PromptRuntime)` 时，握手以它为准发 system prompt 与
  `context_messages`；`None`（嵌入与测试自备运行器）才用 `KernelModelConfig.system_prompt`。

## 验证

```bash
cd rust
cargo test -p omnicrawl-host        # 176 个单元测试 + tests/turn_flow.rs 的 6 组流程测试
cargo clippy -p omnicrawl-host --all-targets -- -D warnings
```

`tests/turn_flow.rs` 用一对内存管道做脚本化假内核，钉住握手（含被拒）、整批工具定调、
提问作答、取消、取消后继续与内核退出六条路径；`tools/` 的执行体另有与 Python 真实现的对照测试，见
`crates/omnicrawl-tui/tests/*_parity.rs`（按原路径驱动本 crate 的工具表）。

## 插件运行期

[`plugins::PluginHost`] 是插件 Hook 的宿主编排层：进程级 `PluginRuntime`（Worker 生命周期、
执行计划、熔断与审计）加各节点的载荷拼装，判定层复用 `omnicrawl-controllers::plugins`
（fail-closed 判定、拒绝事实、拒绝文案）。

```rust
let plugins = Arc::new(PluginHost::from_environment(&env, &workspace));
let diagnostics = plugins.start();          // 拉 Worker、建执行计划、发 app.start.before
plugins.notify_app_started();               // app.start.after
let text = plugins.turn_start(text, session, Some(&turn_id))?;   // 可改写 userText
// 工具批次：tool.call.before / approval.before / approval.after / execute.before / execute.after
```

- **Hook 落点**：`turn.start` / `turn.end` / `turn.error` / `turn.cancelled` 在 `TurnRunner::submit`
  的回合边界上分发；`tool.*` 在批次定调与执行线程里分发（每个调用只过一次守卫）；
  `session.*` 由宿主在握手拿到会话标识后调 `notify_session_lifecycle`。
- **`PluginHost` 归宿主持有**：TUI 与 API 各自建一个（`app.start.*` 的时机在服务装配完成后），
  运行期开关走 `set_enabled`（事务式重建，对应 Python 设置面板），安装 / 启停 / 卸载走
  `install` / `set_plugin_enabled` / `uninstall` / `rollback` 后自动 `reload`。
- **无插件时零开销**：`plugins: None` 或总开关关闭时所有方法原样放行，不产生额外分支。
- **Worker 启动路径**由 `omnicrawl-extensions::protocol::WorkerLauncher::resolve()` 统一解析，
  与 CLI 的安装期冒烟测试同一份（`OMNICRAWL_RUNNER_DIR` → 可执行文件祖先的 `extensions/`
  与 `rust/assets/extensions/`（仓库检出）或 `omnicrawl/extensions/` → 工作目录）；解析失败只留一条「无插件模式」诊断，
  不再逐个插件报「握手失败」。

## 已知差异

- TUI 的回合循环实现在 `omnicrawl-tui` 内部（历史原因），工具级 Hook 在 TUI 侧按同一顺序、
  同一文案实现了一份（`app.rs` 的 `plugin_tool_guards`），两处改动需同步；
  参数改写（`tool.call.before` 的 transform 结局）在 TUI 侧走 `PendingBatch::rewrite_arguments` 回写，
  审批面板的摘要仍按改写前的参数渲染。
- 插件对 `context.build.*` / `model.request.*` / `model.response.*` / `context.compaction.after_turn`
  这些回合内节点**已接线**（`context_build_before` / `context_build_after` / `model_request_before` /
  `model_response_after` / `model_request_error` / `compaction_after_turn`）：内核（`omnicrawl-core`
  与 `omnicrawl-cli` 的回合循环）不持有 Plugins 配置，事件与扇出仍由宿主分发——压缩计量由内核在
  回合收尾回传，宿主据此触发 `context.compaction.after_turn`。
- `session.close.before` / `session.close.after` 已接线：TUI 在 `request_shutdown()` 前发 before、
  进程退出后（`main` 的 `wait_or_kill` 之后）发 after；本地 API 在 `close()` 里 `shutdown()` 前发 before、
  等内核退出后发 after。内核在两者之间补写 `session_closed` 并丢弃空占位。会话切换走的是重建内核
  （不是关闭当前会话），因此不触发这对钩子。
- `initialize.model.prompt_cache_identity` 在 `TurnRunner::handshake` 里装配：工具表就位后按
  system prompt / 工作区 / 项目规范 / Skill 索引 / 工具声明算出七字段身份。`RunnerOptions.prompt`
  有值（API 与 TUI 都走这条路）时用**真实的**项目规范与 Skill 索引参与哈希；嵌入了自备
  `TurnRunner` 且没给装配结果时按空集，与它实际发出的稳定前缀一致。
- `RunnerOptions.prompt` 是提示词装配结果的入口：有它时握手用装配出来的 system prompt 与
  `context_messages`（模板 / AGENTS.md / Skill 索引 / 运行环境），无装配运行期才沿用
  `KernelModelConfig` 里的文本。无头宿主与 TUI 因此共用同一份提示词口径。
- `resume` / `undo` 之外的会话动作仍由内核与 `omnicrawl-session` 承担，这里不做会话落盘。
- 工具声明的 `update_todos` 成功路径不发 `tool.finished`（对齐 Python），失败路径仍发。
- `pause_work` 不产生工具生命周期事件（Python 会发一次开始/完成）；当前 API 未使用该工具。
