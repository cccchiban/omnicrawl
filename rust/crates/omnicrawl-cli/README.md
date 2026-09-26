# omnicrawl-cli

`omnicrawl` 二进制：内核进程，在 stdin/stdout 上提供宿主桥接协议 v1（规格见 `../../docs/protocol-v1.md`）。

## 它做什么

- 持有 `omnicrawl-core` 的回合循环：`turn.submit` 进来就跑一轮「模型 ⇄ 工具」。
- 两个宿主端口经协议外发：`tool.batch`（宿主执行整批工具）与 `model.reply`（过渡期宿主代答模型）。
- 回合进行中到达的 `turn.cancel` / `shutdown` 立即中止当前端口调用；其余请求照常应答，不阻塞宿主。
- `shutdown` 会先收尾当前会话（对映 Python `Agent._append_session_closed_event`）：按最后一次
  事件类型补写 `session_closed`、并丢弃没有真实内容的启动占位；已中断的会话保留不动。
- 未握手前除 `initialize` 外的请求回 `-32600`；协议主版本不匹配回 `-32001`。

## 会话与压缩

`initialize` 带 `session` 块时内核自己持有会话：转录落在 `root`（与 Python 侧同一套布局），
回合开始就把 `user_message` 落盘（与 Python `loop.py` 同序），结束再写 `assistant_message`，
然后按 `compaction` 阈值跑一次压缩。

**失败/取消也留上下文**（语义基准 Python `loop.py` 的两个 `except` 分支）：上游报错或用户取消时
内核补写终态事件并把投影写回运行期历史——取消写 `turn_cancelled`
（`{user_text, pending_user_text, reason, summary}`，`summary` 是「已执行工具：read×2，bash」这类文本），
其余错误写 `session_interrupted`（`{user_text, pending_user_text, reason}`）。因此「报错 → ESC →
再发一句」时，下一轮照样看得到上一轮的任务与已执行的工具，而不是把整轮丢在转录之外。

**自动重试（刻意偏离 Python）**：`omnicrawl-llm` 的 `is_retryable_model_request_error` 只重试
408/409/429/5xx 与传输层文案（与 Python 同表），而内核这一层把**任何非取消的请求失败**都先按
0.4s→0.8s→1.6s→封顶 2s 退避重试，最多 `initialize.model.request_retry_count`（缺省 5）次：
网络抖动、网关 5xx、限流、甚至瞬时被拒都不该直接打断整段会话；确定性错误重试几次后仍如实上报。
上下文超限不在此列（它要走压缩恢复路径），取消也不重试。每次重试前先抽干入站帧，所以退避期间
按 ESC 依然立刻止血。流式回复期间同样每个流事件抽一次入站帧，ESC 不再要等整段流跑完。

摘要提示词模板在编译期嵌进二进制（`omnicrawl/templates/summary_prompt.md`），不依赖运行时目录。

工具事件也在内核侧落盘（语义基准 `omnicrawl/agent/controllers/turn/loop.py`）：每批工具调用前写
`tool_call_requested`（参数走 `public_tool_arguments` 投影，另带本批发往 Provider 的
`assistant_content` / `assistant_reasoning_content` / `function_name`——恢复后同一段历史的写法才与
运行期一致），整批执行且输出预算/视觉/压缩处理过之后写 `tool_result`（`output` 取展示全文、
`model_output` 是模型可见输出，超长输出的 artifact 化与 `output_sha256` / `storage` 由会话存储补齐），
宿主拒绝的调用另写 `tool_call_denied`。协议原文（`arguments_json`）只在内存投影里，绝不落盘；
子代理内部的工具调用不落父会话（对映 Python 的 `persist_session_events=False`）。

内核自己持有会话时，`recall_session_evidence` 由内核本地作答（不发给宿主）：它按最后一个有效摘要的
授权读取转录事件与 `archive/compacted/` 归档，artifact 正文经会话 artifact 区读取，返回紧凑 JSON 信封。

`session.events` 只读当前自持会话的回退投影后事件流（`read_active_events`），宿主据此重建历史页：
消息、工具卡、计划清单与 SubAgent 进度树都按事件还原，而不是只投影 user/assistant 文本。
`subagent.query` 除 `list` / `get` / `cancel` 外还支持 `list_worktrees`（读托管根里的 worktree
元数据），供宿主在 `/workspace` 切换前做 pending worktree 拦阻。

运行中切换工作区走 `workspace.switch`（宿主侧 `/workspace` 的落点）：内核把自持会话的工作区指到新根，
并按 Python 的口径转录 `workspace_switched`（`{from, to}`）；会话不重建，转录/历史/`/undo` 账本不受影响。

上游判定上下文超限时（`ModelErrorCode::ContextLengthExceeded` 归类文案），内核压缩当前未完成回合、
把续接指令写进会话并自动重试同一回合；恢复失败则保留原错误返回给宿主。

压缩成功后：会话里出现 `compact_summary`，测量事件 `context_compaction_measurement` 逐回合记录，
`recall_session_evidence` 之外的可恢复信息（被压缩窗口的原始事件）归档到 `archive/compacted/`；
下一轮请求只带「摘要 + 保留窗口 + 当前输入」。

## 工具名的线上形态（发给上游前收敛）

上游对 `function.name` 有硬约束（OpenAI 兼容网关是 `^[a-zA-Z0-9_-]+$`，Gemini 还要求首字符
是字母或下划线），而 MCP 暴露给模型的名字天生带分隔符：工具是 `server.tool`，资源是
`mcp_read_resource__<uri>`（URI 里有 `:` 与 `/`）。原样发出去，网关直接 400：

```text
Invalid 'tools[0].function.name': string does not match pattern.
Expected a string that matches the pattern '^[a-zA-Z0-9_-]+$'.
```

因此两个模型端口（主回合 `KernelModelPort`、子 Agent `BackgroundModelPort`）在发请求前调
`omnicrawl_protocol::conform_tool_names`：非法字符换 `_`、数字开头补 `_`、超长截断（64）、
撞名追加 `_2`/`_3`（按声明顺序仲裁，因此每次都是同一组线上名，不会白改 prompt cache 前缀）；
模型按线上名回传的调用再用同一张对照表**还原成内部原名**（同时改写 `tool_calls[].name` 与
assistant 原文里的 `function.name`）。所以除了线上那一层，内核派发、宿主工具表与审批、
审计、转录事件与界面上的工具卡看到的都还是 `server.tool`。

Python 侧目前是把原名直接发出去（`tool_specs_to_openai_functions` 不做任何收敛），带 MCP 的
会话在严格网关上必然失败——这是 Rust 单侧的修正，不是新增功能。

## 并发工具调用的回合顺序

一条 assistant 消息里的并发工具调用会打包成**一个** `tool.batch` 交给宿主，内核等这个批次的
完整响应（每条调用一条观察，数量不符即 `ObservationMismatch`）之后才发下一轮模型请求——
不会出现「上一个工具还没完就开始下一轮」；宿主侧的 `PendingBatch` 也只在每条调用都回填结果后
才产出观察。

## 并发压缩与失败上报

一批里有多条合格结果时，压缩是**并发**的：每条一个工作线程、线程内各自按同一份连接配置
建运行时（`ModelRuntime` 是顺序会话，共用就只能排队）。完成顺序即回报顺序，先压完的先显示。
取消回合时主线程立刻返回，慢请求在后台自生自灭（它们只往通道塞结果，不碰 `Conn`）。

压缩的三种结局都会报给宿主：`started`（正在压缩）/ `finished`（带压缩后正文与前后字符数）/
**`failed`**（带可直接展示的文案）。失败文案由内核备好：错误信息里出现「超时/timeout」时是
`压缩超时（{秒数} 秒），保留原始输出：{错误}`，其余是 `压缩失败，保留原始输出：{错误}`；
模型没压小也会报一条 `压缩未缩小结果（a → b 字符），保留原始输出。`。
以前这些只写内核 stderr，界面上看不到。

## 工具调用的流式通知

模型流的 `ToolCallStarted` / `ToolCallArgumentsDelta` 会转成 `turn.tool_call_started` /
`turn.tool_call_arguments` 两条宿主通知（Python 只在批次执行时才画卡片，这是 Rust 侧刻意的
增量渲染）：宿主先把卡片立起来，参数分片到达时逐段补，`write_file` / `Edit_file` 的内容预览
因此能跟着模型输出一起长出来。压缩旁路另外发 `turn.tool_output_compression` 的阶段通知
（`started` / `finished` + 压缩前后字符数，`finished` 还带**压缩后的正文**），
宿主据此显示「已压缩 a → b 字符」并把卡片正文替换成压缩后的内容（用户要求「压缩后替换原内容」）。

## 工具输出压缩旁路

`[tool_output_compression]` 启用时，内核在拿到宿主回传的整批观察后会额外跑一次压缩请求：
只有 `bash` / `powershell` / `git` / `grep` 且模型可见文本达到 `min_chars` 的结果才送压；
模型请求带内置系统提示（`omnicrawl/templates/tool_output_compression_system.md`，编译期嵌入）与
`<<<TOOL_OUTPUT_START>>>` 包裹，`thinking_enabled=false` 时显式下发 `reasoning_effort=none`。
模型没把文本压小、返回工具调用或空文本、请求失败都保留原文；采纳时观察正文换成精简文本，
`full_output` 换成「已压缩：<原始> → <精简> 字符，模型 <name>」的展示文案。

连接与模型按模型切换同一套口径解析：`model_key` 支持自定义 key/alias、`profile/model_id`
与裸 model_id，走 `apply_model_selection` 一并把模型名、基地址、协议与凭据换成被选中的那条
（以前是把整个 key 当模型名直接发给上游，`channel-2/Qwen/…` 这种带 Profile 前缀的 key 会被
上游回「模型不存在或当前账号无权使用该模型」）。配置文件路径与其它配置一致：
显式路径 > `AI_CONFIG_FILE` > 用户目录。

**凭据口径（与主渠道不同，别改成「环境变量优先」）**：压缩渠道优先用它在 config.toml 里
写下的明文 `api_key`；没写明文才回落到 `resolve_api_key`（环境变量优先 → 明文）与同名环境变量。
原因是内核进程里的 `OPENAI_API_KEY` 是宿主为**当前主渠道**注入的（`omnicrawl-host/src/kernel.rs`
的 `kernel_credentials_env`），而压缩渠道往往是另一家服务——用户的配置里主渠道是聚合网关、
压缩渠道是硅基流动，两边都写着 `api_key_env = "OPENAI_API_KEY"`；若按环境变量优先，就会
把聚合网关的 key 发给硅基流动，得到 `模型服务鉴权失败（HTTP 401）`。Python 侧读的是用户自己的
shell 环境（同名变量通常根本没设），因此明文优先在常见配置下与它同结果；差异只在
「仅在环境变量里轮换密钥、config.toml 里留着旧明文」时 Rust 会用明文。
对应回归：`crates/omnicrawl-cli/tests/tool_output_compression_e2e.rs` 的
`compression_model_key_is_resolved_through_the_profile_connection` 断言压缩请求带的是
渠道明文 key，而主请求仍走帧里声明的环境变量名。

## 工具输出预算与落盘归档

内核拿到宿主回传的整批观察后，先按 Python 的批次预算口径裁剪模型可见文本：单个工具输出超过
50K 字符、或本回合未落盘总量超过 200K 字符时，完整内容写进会话 artifact（`artifacts/<会话 id>/`），
模型上下文只留头尾预览与「输出太大（NKB），完整内容已保存到：<路径>」，模型可按路径用 `read` 取回全文。
落盘失败或会话不在内核时降级为纯预览（提示「完整内容未能保存到磁盘。」），不阻断工具执行；
`full_output` 始终保留完整原文。这一步在压缩旁路之前跑。

## 审批审计

审批发生在宿主（面板在 TUI），但拒绝事实要进会话转录：宿主在观察里回 `result.error_code = denied`，
内核据此补一条 `tool_call_denied` 事件（载荷 `tool` / `arguments` / `reason`，参数按
`public_tool_arguments` 投影，不把 shell 全文写进转录）。会话投影与历史靠这个事件还原「用户拒绝执行」，
因此拒绝只走观察、不额外加协议方法（错误码常量见 `omnicrawl_ipc::DENIED_ERROR_CODE`，
`tool.batch` 的响应负载约定记在协议文档里）。批准事实（`tool_call_approved`）与插件钩子
`tool.approval.after` 仍由宿主侧编排，内核不参与。

## 独立视觉模型代理

宿主把图片作为带图观察回传（`followup_messages` 里形如「文本 + `image_url`」的 user 消息）时，
内核若读到 `[vision]` 配置且已启用，就把这条观察交给配置里的视觉模型：按 `models` 顺序尝试，
前一个候选失败就换下一个，每次沿用 `initialize.model` 的连接、只换 model 名，请求不带工具与系统提示、
`reasoning_effort=none`，单候选最多重试两次。成功后模型上下文里换成 `<vision_observation>` 包裹的
不可信文本观察（图片不再进请求），展示文本追加「视觉模型分析（模型）：…」；全部候选失败则把该结果
改成「视觉模型分析失败：…」。

代理未启用、候选都装配不出来或观察里没有图片时，内核原样保留宿主给的观察（图片通路仍由宿主决定）。
判定面（候选选择、标签、失败汇总、文本截断）在 `omnicrawl-controllers/src/vision_proxy.rs`。

## 撤销最近一轮（`turn.undo`）

宿主发 `turn.undo` 时，内核按「先副作用、后会话」的顺序撤销最近一轮：读 `turn_snapshot` 事件 → 按
`undo/{begin,end}.patch` 与未跟踪清单把工作区换回本轮开始前 → 提交会话回退并落 `turn_undone`，
随后按会话重建运行期历史。副作用没有快照、本轮跑过不可逆工具、快照绑定的工作区与当前工作区不一致时
整轮拒绝（含中文原因）；提交会话失败会把工作区换回撤销前。判定面与恢复预检在
`omnicrawl-controllers/src/undo.rs`，宿主侧编排（git 子进程、artifact 读写、提交）在
`omnicrawl-cli/src/undo.rs`。

## 运行期设置更新（`session.settings`）

宿主可以在回合间隙改内核持有的设置，用于设置面板即点即存：

- `model`：`model` / `options` / `reasoning_effort`（只改生成选项里的这一个键）/ `tools`（静态工具声明整体替换）/ `context_window_tokens`，以及渠道字段 `provider` / `protocol` / `base_url` / `api_key_env`（切换模型渠道时整套下发，空串等同于不给，凭据本身不进帧）；
  内核的 `read_api_key` 只按 `api_key_env` 这个**名字**读环境变量（读不到就整回合失败：
  `读取环境变量 … 失败…模型请求无法鉴权`），所以凭据只写在 `config.toml` 时，宿主必须把它注入
  内核子进程的环境（TUI 与本地 API 都在起内核/重起内核时做这件事）。
- `compaction`：压缩阈值与窗口等字段，映射与 `initialize.session.compaction` 共用
  `compaction.rs::overlay_compaction_config`，两处不会漂移。

只覆盖给出的字段，结果回 `{applied: [字段路径]}`；任一字段非法则整体不写，回 `-32602` 且
`data.kind` 取 `model_unavailable` / `session_unavailable` / `empty_settings` / `invalid_settings`，
宿主据此区分「写盘失败」与「内核拒绝即时更新」。设置对**后续**回合生效：正在跑的回合在开始时已快照
模型配置，不会被中途换掉；因此回合进行中也照常应答这个方法（`handle_inbound` 的回合内分支）。

## 会话生命周期（`session.*` 与 `subagent.run`）

会话状态的唯一真相在内核：摘要边界、运行期历史与转录投影都在这边，宿主只做展示与视图同步。
因此斜杠命令对应的读写全部是协议方法（语义逐条见 `../../docs/protocol-v1.md`）：

| 方法 | 对映命令 | 行为要点 |
| --- | --- | --- |
| `session.list` | `/sessions`、`/archives` | 按 `archived` 列未归档 / 已归档，`limit` 收敛到 1..=100，**不**按工作区过滤；回包带 `current_session_id` |
| `session.rename` | `/rename` | 只改当前会话标题，回索引条目 |
| `session.archive` | `/archive` | 归档并自动开一条新会话（`new_session_id`）；归档失败不新建，新建失败不回滚归档 |
| `session.history` | `/history` | 只读的用户提示历史（`<session_root>/history.jsonl`），空 `query` 不过滤 |
| `session.new` | `/new` | 清空当前对话并开新会话 |
| `session.resume` | `/resume` | 切会话并用转录重建历史；目标必须存在，已归档先解除归档，`history` 一并回给宿主重放 |
| `session.append` | （`/review` 的报告注入） | 只接受 `role=assistant`；先落盘再进运行期历史，下一轮请求即带上 |
| `subagent.run` | `/review` | 派生单个子 Agent 等它跑完，回**未截断**的收尾文本（公开 `summary` 会按 `result_summary_chars` 截断，会把评审 JSON 打碎） |

两个实现约束：

- `turn.undo` 与 `session.resume` 都把重建后的 `history` 一并回给宿主，否则 UI 无法让撤回的消息与工具卡真正消失；
- `subagent.run` 的子任务工具批次仍回到宿主执行，因此宿主**不能同步等待**它的响应（会与 `tool.batch` 互相卡死）。

`subagent.run` 与 `subagent` 工具共用 `SubAgentRuntime::prepare`（角色发现、工具白名单、worktree 隔离）
与同一套结果投影（`require_completed_result`），失败回 `-32600` 且 `data.kind` 给 `SUBAGENT_DISABLED` /
`SUBAGENT_TASK_FAILED` 这类可判定原因。

## 错误映射

| `LoopError` | 协议错误码 | `data.kind` |
| --- | --- | --- |
| `Cancelled` | `-32003` | `cancelled` |
| `InvalidBudget` | `-32602` | `invalid_budget` |
| 其余（预算超限、观察数量不符、回复或批次失败） | `-32004` | `LoopError::tag()` |

## 当前发出的事件

只有 `turn.finished`。`turn.delta` / `turn.token_usage` / `tool.*` 的信息本来就在宿主侧（模型流与工具
都在宿主执行），过渡期由宿主自己产生；等内核自带 provider runtime（`omnicrawl-llm` 的请求构建）后，
才由内核发出。

## 本地验证

```bash
cargo build --release -p omnicrawl-cli
node ../../packages/cli/scripts/prepare.mjs
npm test -w @omnicrawl/cli        # e2e：启动器 → 真二进制 → 协议 v1（含取消与错误码）
```
