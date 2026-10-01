# 宿主桥接协议 v1

内核（Rust）与宿主（Node/TS 启动器、TUI、过渡期的 Python 宿主）是两个进程，只用一条
NDJSON 流通信：一行一个 JSON-RPC 2.0 帧。帧形状与插件通路（`omnicrawl/extensions/node_runner.mjs`）
相同，宿主不必为内核另写一套解帧逻辑。

实现与类型定义：`rust/crates/omnicrawl-ipc`（`frame` / `version` / `bridge`）。

## 传输

- 双向管道（内核作为被宿主启动的子进程时即 stdin/stdout），UTF-8 编码。
- 一帧一行，`\n` 结尾。JSON 字符串里的换行会被转义，帧内不得出现裸换行。
- 空行或只有空白的行：忽略。
- 非法 JSON、形状非法或其他无法解析的行：内核记 stderr 日志并**丢弃该行**，不断开连接。
  不回 `-32700` 响应，是因为 `id` 无法解析时响应也无处可去，而静默丢弃更不容易让宿主实现出错。
- stderr 只写日志，不承载协议。

## 握手

宿主先发 `initialize`：

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"1.0","client":{"name":"omnicrawl-cli","version":"0.1.0"}}}
```

内核回：

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0","session_id":"20260922-101500-abcd1234"}}
```

`session_id` 是内核**当前**会话的 id：`initialize.session.session_id` 没给而内核新建了一条时，这是宿主
唯一能知道「现在在哪个会话」的途径（`/sessions`、`/rename`、`/new` 都依赖它）。内核没持有会话时为空串。

主版本不匹配时回错误（`supported` 与 `host` 便于诊断）：

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"不支持宿主协议版本 2.0。","data":{"supported":"1.0","host":"2.0"}}}
```

未完成握手前，内核对其其余请求回 `-32600`。

## 宿主 → 内核

| 方法 | 类型 | params | result |
| --- | --- | --- | --- |
| `initialize` | 请求 | `{protocol_version, client?, model?, session?, plugin_model_hooks?}` | `{protocol_version, session_id}` |
| `turn.submit` | 请求 | `{turn_id, user_text, images?}` | `{}`（回合已结束） |
| `turn.cancel` | 请求 | `{turn_id}` | `{}` |
| `turn.undo` | 请求 | `{}` | `{kind, message_count, side_effects_reverted, unrestorable, history}` |
| `session.settings` | 请求 | `{model?, compaction?}` | `{applied: [字段路径]}` |
| `session.list` | 请求 | `{archived?, limit?}` | `{sessions: [会话索引条目], current_session_id}` |
| `session.rename` | 请求 | `{title}` | `{session: 会话索引条目}` |
| `session.archive` | 请求 | `{}` | `{session: 会话索引条目, new_session_id?}` |
| `session.history` | 请求 | `{query?, limit?}` | `{entries: [提示历史条目]}` |
| `session.events` | 请求 | `{}` | `{session_id, events: [会话事件]}` |
| `session.new` | 请求 | `{}` | `{session_id}` |
| `session.resume` | 请求 | `{session_id}` | `{session_id, session: 会话索引条目, history: [消息]}` |
| `session.append` | 请求 | `{role?, content}` | `{appended}` |
| `workspace.switch` | 请求 | `{path}` | `{switched, from, to}` |
| `subagent.run` | 请求 | `{agent_type, description?, prompt}` | `{agent_type, output, task}` |
| `subagent.query` | 请求 | `{action, task_id?}` | `{unavailable, action, tasks, task, result, worktrees?}` |
| `shutdown` | 请求 | `{}` | `{}` |

- 回合结果只经由 `turn.finished` 通知传递，`turn.submit` 的响应不重复结果，避免两处真相。
- `turn.submit` 的 `images`（可选，缺省 `[]`）是用户随提问粘贴的图片：`[{media_type, data_base64, detail?}]`。
  这些图**是用户消息的一部分**——内核把它们写进 `user_message` 事件的 `images` 字段（转录、`/resume`
  与重启后的投影因此能重建同一条消息），并按 OpenAI 多模态部件形状放进本轮请求。宿主不应在未开启
  原生视觉时送来图片：图片没有去处（既不直送主模型，也没有 `[vision]` 代理），应就地提示用户。
- 会话读写方法都要求**内核自持会话**（`initialize.session`），否则回 `-32600` 与「当前会话不受内核
  持有」；会话归属内核是因为转录、压缩边界与运行期历史都在那边，宿主只做投影。
- `turn.undo` 撤销最近一轮：先把工作区按 `turn_snapshot` 事件的快照换回本轮开始前，再提交会话逻辑回退
  并落 `turn_undone`。副作用没有快照、本轮跑过不可逆工具、或快照绑定的工作区与当前工作区不一致时整轮
  拒绝（回 `-32600` 与中文原因）；提交会话失败会把工作区换回撤销前。
- `turn.cancel` 是建议性的：内核在下一个模型或工具批次边界检查取消。若某一批工具已经交给宿主，

`initialize` 的 `model` 是可选块：**给了它，内核就自己发模型请求**（工具仍由宿主执行），宿主不必再应答
`model.reply`；不给则维持代答路径，旧宿主不受影响。凭据不进帧——`api_key_env` 只给环境变量名。
因此**宿主必须保证这个名字在内核子进程的环境里真的有值**：`config.toml` 里的字面 `api_key`
不在环境里，宿主起内核时要把它按这个名字补进子进程环境（`KernelClient::spawn_with_env` /
`spawn_with_stderr_env` + `kernel_credentials_env`）；名字留空时宿主按 Provider 默认名下发
（`frame_api_key_env`：openai → `OPENAI_API_KEY`，anthropic → `ANTHROPIC_API_KEY`，gemini → `GEMINI_API_KEY`）。
否则内核会在每回合开始就失败：`读取环境变量 … 失败…模型请求无法鉴权`。

`initialize` 的 `plugin_model_hooks`（缺省 false）是宿主对插件模型 Hook 的能力声明：声明后内核
在每次模型请求前发 `model.hook`（见「内核 → 宿主」请求表），未声明则不发，因此只实现协议最小集
的兼容宿主不受影响。TUI 与本地 API 都声明。

```json
{"model": {"model": "gpt-5.2", "base_url": "https://api.openai.com/v1", "api_key_env": "OPENAI_API_KEY",
           "user_agent": "omnicrawl/0.1.0", "system_prompt": "你是助手。",
           "context_messages": [{"role": "user", "content": "<runtime_context>…</runtime_context>"}],
           "tools": [{"type": "function", "function": {"name": "read_file"}}],
           "options": {"temperature": 0.2}, "request_timeout_seconds": 180,
           "prompt_cache_capable": true, "prompt_cache_identity": {"profile": "main"},
           "native_vision": false, "request_retry_count": 1}}
```

`context_messages` 是 system 之外的上下文消息（项目规范、Skill 索引、工具能力说明、运行环境）：由宿主按「稳定 → 动态」组装好整段交进来，内核每轮把它们**原样插在历史之前**，且不写进会话转录——它们是本轮读到的真实环境，不是会话历史。缺省空数组表示旧宿主不给（行为与迁移前一致）。

`initialize` 的 `session` 也是可选块：给了它，内核自己持有会话——回合消息落进转录、
下一轮上下文由转录恢复、回合结束后按阈值跑一次压缩（摘要请求走同一个 `model` 配置）。
不给则维持「无会话」行为，宿主仍可用 `model.reply` 代答。

`session_id` 决定这次是新建还是恢复：不给（或给空串）就新建，新会话的 ID **只经内核 stderr 报出**
（`[kernel] 会话已就绪：<id>`），帧里不回带；给了就是恢复，且该会话必须已存在，否则 `initialize` 回
`-32602`（`会话初始化失败：未找到会话：<id>`）。ID 形如 `20260101-000000-abcdef`（日期-时间-6 位十六进制），
格式不符同样按 `-32602` 拒绝。

落盘保证：`turn.finished` 发出**之前**，本轮的 `user_message` / `assistant_message` 已写入转录并 `fsync`。
宿主收到 turn.finished 即可认为这一轮可恢复——内核先落盘再通知，宿主此刻退出或被强杀都不会丢已完成回合。
相应地，压缩提示与摘要请求会出现在 `turn.finished` 之前。

```json
{"session": {"root": "/path/to/.agent_sessions", "session_id": "20260919-011424-abcdef",
             "memory_root": "/path/to/.omnicrawl",
             "compaction": {"trigger_context_tokens": 120000, "target_summary_tokens": 2000,
                            "context_window_tokens": 128000, "preserve_exact_evidence": true,
                            "archive_compacted_events": true, "auto_memory_recall": true}}}
```

- `root` 必填，目录布局与 Python 侧 `.agent_sessions` 一致（`sessions/`、`archive/compacted/` 等）。
- `session_id` 空则由内核新建一条会话。
- `memory_root` 是会话级记忆的用户数据根；空则不做记忆回写与自动召回。
- `compaction` 缺字段一律用内核默认值；阈值取自「回合结束后实际上下文」的估算与供应商回报的较大值。
  **宿主应把用户的 `[context_compaction]` 整段下发**（内核不读配置文件，缺字段就是默认值：摘要预算
  2000 token，会把「`target_summary_tokens = 0` 不限预算」变成「只能写 2000」），映射见
  `omnicrawl-controllers` 的 `settings::kernel_compaction_settings`；`memory_root` 同理，不给就等于
  关掉压缩后的记忆回写与自动召回。
- 压缩发生后，内核发出的下一轮请求只带「摘要 + 保留窗口 + 当前输入」，被摘要取代的旧消息不再进上下文。
- 上游判定上下文超限时，内核压缩当前未完成回合、把续接指令（`请依据上方的结构化工作摘要继续完成当前任务。`）
  写进会话并重试同一回合；恢复失败则把原错误返回给宿主。


- `model` 必填；`base_url` 空则用运行时默认（OpenAI 官方地址）。
- `tools` 是 OpenAI functions 形状的静态声明；历史里出现的工具声明不重复下发。
- `options` 用 `GenerationOptions` 的 JSON 形状。
- `request_retry_count` 是空响应与可重试错误的最大请求次数（默认 1），语义与 Python 侧同名配置一致。
  内核等该批次返回后再收尾，不会中断宿主正在执行的工具。
- `native_vision` 为真表示主模型自己能看图：带图观察**直送主模型**，内核不再启用独立视觉模型
  代理。为假（或旧宿主不给）时带图观察交给 `[vision].models` 里的模型分析，把结论换成不可信
  文本观察；代理未启用时图片被掉，主模型只收到图片元数据。优先级与 Python
  `route_image_result` 一致：原生视觉优先于代理。

`session.settings` 让宿主在运行期改内核持有的设置，用于设置面板的即时生效：

```json
{"model": {"model": "gpt-5.2", "provider": "openai", "protocol": "openai_responses",
           "base_url": "https://api.openai.com/v1", "api_key_env": "OPENAI_API_KEY",
           "options": {"reasoning_effort": "high"}, "reasoning_effort": "high",
           "system_prompt": "你是助手。…<active_mode_prompt name=\"plan\">…</active_mode_prompt>",
           "context_messages": [{"role": "user", "content": "<project_instructions>…</project_instructions>"}],
           "tools": [{"type": "function", "function": {"name": "read_file"}}],
           "context_window_tokens": 200000},
 "compaction": {"trigger_context_tokens": 160000, "context_window_tokens": 200000}}
```

- 两个块都可选，**只覆盖给出的字段**，其余保持原值；结果里的 `applied` 是本次真正改动的字段路径
  （如 `model.tools`、`model.reasoning_effort`、`model.base_url`、`compaction.trigger_context_tokens`）。
- `model.reasoning_effort` 只写生成选项里的 `reasoning_effort` 这一个键（别名按 `llm.reasoning_effort`
  的别名表归一化后写入），不像 `model.options` 那样整体替换——后者会顺手把别的生成选项打回默认值。
- `provider` / `protocol` / `base_url` / `api_key_env` 是**渠道字段**：宿主切换模型渠道时整套下发，
  凭据本身仍然不进帧（只给环境变量名）；给空串等同于不给。
- `model.system_prompt` / `model.context_messages` 是**提示词整体替换**：宿主启用主 Agent 模式
  （`/plan`）后重算 system prompt（末尾追加 `<active_mode_prompt>`）与上下文消息并下发；
  上下文消息只在请求体里生效，不写进会话转录。
- 设置对**后续**回合生效：正在跑的回合在开始时已快照模型配置，本次更新不会改变它已经发出的请求。
  回合进行中也能应答这个方法，不必等回合结束。
- 校验与写入分两段：任一字段非法则一个字节都不改（原子），回 `-32602` 并在 `data.kind` 给出可判定原因：
  - `model_unavailable`：`initialize` 没给 `model`（宿主走 `model.reply` 代答），模型段不可改；
  - `session_unavailable`：内核没有自持会话，压缩段不可改；
  - `empty_settings`：两个块都没给；
  - `invalid_settings`：字段取值非法（模型 ID 为空、工具项不是对象、阈值非正整数、未知推理强度等）。
- 宿主据此区分「写盘失败」与「内核拒绝即时更新」：后者配置已落盘，宿主如实提示「下次会话生效」，
  不假装即时生效。
- 工具声明（`model.tools`）由宿主负责生成与替换：工具仍由宿主执行，内核只持有声明。

`session.list` / `session.rename` / `session.archive` / `session.history` / `session.events` /
`session.new` / `session.resume` 是斜杠命令（`/sessions`、`/archives`、`/rename`、`/archive`、
`/history`、`/new`、`/resume`）与历史页回放在宿主侧的落点，语义与 Python 的 `SessionFacade`
一一对应：

- `session.list`：`archived=false` 列未归档，`archived=true` 只看归档；`limit` 由内核收敛到 1..=100，
  且**不**按工作区过滤（与 Python「列出全部会话，不再绑当前工作区」一致）。`current_session_id` 让宿主
  给当前项打标记，也顺便校准自己记的 id。
- `session.rename`：只改当前会话的标题；索引条目直接回给宿主展示。
- `session.archive`：归档当前会话**并自动开一条新会话**（新 id 在 `new_session_id` 里，可能缺失——归档
  已成功但新建失败时只在 stderr 记日志，不回滚归档）。归档会话从默认列表隐藏。
- `session.resume`：切到目标会话并用转录重建运行期历史，`history` 一并回给宿主重放对话视图；目标必须
  已存在（否则 `-32600`），已归档的先自动解除归档，切换前会丢掉当前的空占位会话。
- `session.new`：清空当前对话并开新会话。
- `session.history`：只读的用户提示历史（独立于会话转录），空 `query` 表示不过滤。
- `session.events`：只读的**回退投影后**的有效事件流（`read_active_events`，被 `turn_undone` 撤掉的轮次
  不出现），与 Python `current_session_events()` 同源。宿主用它重建历史页：消息、工具卡、计划清单、
  SubAgent 进度树与压缩边界都按事件还原，而不是只投影 user/assistant 文本。与其他只读查询一样，
  回合进行中照常应答；没有自持会话时 `-32600`。
- `turn.undo` 与 `session.resume` 都会带回 `history`：宿主先据此对齐视图，随后再用 `session.events`
  做完整回放（撤回的消息与工具卡才能真正消失），事件流读不到时才停在消息投影上。

`subagent.query` 的 `action` 支持 `list` / `get` / `cancel`，另加 `list_worktrees`：它不查任务表，
而是用 `action` 字段旁路到托管根（`~/.omnicrawl/agent-worktrees`）里的 SubAgent worktree 元数据，
回 `{worktrees: [{task_id, branch, worktree_path, base_ref, repo_root}]}`——宿主在 `/workspace` 切换前
用它做 pending worktree 拦阻（与 Python `list_subagent_worktrees` 同义，`cancel` 则用于切换前的
子 Agent 排空）。

`session.append` 与 `subagent.run` 供「模型循环之外」的宿主入口使用，当前只有 `/review`：

- `subagent.run`：派生**单个**子 Agent（`agent_type` 取 `subagents.toml` 定义目录里的角色名）并等它
  结束，回的是子 Agent 未截断的收尾文本（`output`）与公开结果（`task`）——公开的 `summary` 会按
  `result_summary_chars` 截断，会把评审 JSON 打碎。失败时回 `-32600`，`data.kind` 给 `SUBAGENT_DISABLED`
  / `SUBAGENT_TASK_FAILED` 这类可判定原因。子任务的工具批次仍回到宿主执行，因此**宿主不能同步等待
  本方法的响应**（会与 `tool.batch` 互相卡死）：要么异步等回执，要么先继续处理帧。
- `session.append`：把宿主产生的文本作为 assistant 消息注入内核会话历史，下一轮请求即可见（对映
  Python 的 `remember_review_report`）。只接受 `role=assistant`——不借这个入口伪造用户输入或工具结果；
  内容为空白时不做任何事，回 `{"appended": false}`。

`workspace.switch` 是宿主侧 `/workspace <路径>` 的运行中切换在会话上的落点：宿主已经解析并校验
过目标路径（存在且是目录），内核只负责会话一致性：

- 把自持会话的工作区指到新根，并按 Python `_append_session_event("workspace_switched", …)` 的口径
  转录一条 `workspace_switched` 事件（payload `{from, to}`，`from` 是切换前的会话工作区）。
- **会话不重建**：转录、运行期历史与 `session_started.workspace_root` 都不动（会话已全局化、不绑工作区），
  因此切换后对话上下文与 `/undo` 账本不变。
- `path` 为空白回 `-32602`；目标与当前工作区相同时不写事件，回 `switched=false`；内核没有自持会话时回
  `-32600` 与「当前会话不受内核持有」。
- 工具表、MCP 连接、临时目录等宿主侧资源由宿主自己重建，协议不回传也不下发；宿主通常先发
  `session.settings`（`model.tools` / `model.context_messages`）再发本方法，让内核的工具声明与新工作区
  一致。
- 回合进行中也能应答：正在跑的回合已拍过工作区快照，更新只对后续回合生效。

## 内核 → 宿主

### 请求

| 方法 | params | result |
| --- | --- | --- |
| `tool.batch` | `{turn_id, step, calls: [ToolCall], workspace_root?}` | `{observations: [AgentLoopObservation]}` |
| `model.reply` | `{turn_id, messages: [Value]}` | AgentModelReply：`{message, content, tool_calls, reasoning, content_streamed}` |
| `model.hook` | `{messages: [Value], model}` | `{messages: [Value]}`（插件改写后的消息）；拒绝时回错误响应 |

`model.hook` 只在宿主于 `initialize` 声明 `plugin_model_hooks: true` 时使用：内核在发出模型请求前
请宿主跑 `model.request.before`（transform + guard）。插件放行时宿主回（可改写的）消息；插件拒绝时
宿主回错误响应，`error.message` 就是插件的拒绝文案，内核据此中止本轮（与 Python `_plugin_denial_error`
同口径）。未声明的宿主不会收到该请求，也就不会因等不到响应而阻住回合。

`tool.batch` 是刻意保留的批次边界：宿主必须先完成整批规范化与审批，再按 `calls` 顺序返回**同数量**
的观察。数量不符时内核按协议错误处理并终止该回合（不变式已在 `omnicrawl-core` 内校验）。

宿主拒绝执行某个调用时，观察里回 `result.error_code = "denied"`（常量 `omnicrawl_ipc::DENIED_ERROR_CODE`），
`result.output` 写拒绝原因；内核据此把这次拒绝落成 `tool_call_denied` 会话事件。拒绝不额外设协议方法。

带图观察（`followup_messages` 里正文含 `image_url` 部件的 user 消息）由宿主注入、内核可以改写：
内核启用 `[vision]` 代理时把这条观察换成视觉模型的文本结论，代理未启用时原样使用。这条通路也不额外设协议方法。

`workspace_root` 是可选字段：带上它表示这批工具要在**隔离根**下执行（当前只有 `subagent` 的
`isolation=worktree` 子任务会带）——宿主应把工作目录与路径保护都切到该根，缺省则用宿主自己的
工作区。宿主可以忽略该字段（行为退回共享工作区），但那样隔离就不成立。

### 通知

| 方法 | params | Python 侧来源（`loop.py`） |
| --- | --- | --- |
| `turn.delta` | `{text}` | `on_delta` |
| `turn.reasoning_delta` | `{text}` | `on_reasoning_delta` |
| `turn.status` | `{message}` | `on_status` |
| `turn.notice` | `{message}` | 无（Rust 侧新增）：一条落在**会话流**里的提示，宿主应追加进对话而不是写运行状态行（用于出网脱敏的占位符还原告警，以及压缩边界：上下文压缩的「---已压缩 xxk~xxk ---」与回合末工具调用压缩的「已压缩 a → b 字符」）。Python 侧这些消息也走 `on_status`、由 UI 按前缀特判落进对话区；Rust 侧直接用这条通知表达落点，免得既顶掉状态行又依赖文案前缀匹配 |
| `turn.retry_status` | `{message}` | `on_retry_status` |
| `turn.protocol_wait` | `{}` | `on_protocol_wait` |
| `turn.stream_rollback` | `{}` | `on_stream_rollback` |
| `turn.token_usage` | `{input_tokens, output_tokens, cached_input_tokens}` | `on_token_usage` |
| `turn.finished` | `{turn_id, final_text, reasoning, model_turns, tool_calls, paused, post_compaction_context_tokens?}` | `run_stream` 的返回值 |
| `turn.context_compaction` | `{post_turn_context_tokens, trigger_context_tokens, turn_id, post_compaction_context_tokens?}` | 回合收尾的压缩触发点（`_trigger_context_compaction_after_turn`）；宿主据此分发 `context.compaction.after_turn` |
| `turn.model_response_after` | `{model, content, tool_call_count}` | 模型请求返回点（`_request_agent_reply` 里 `model.response.after`）；宿主据此分发 `model.response.after` |
| `turn.model_request_error` | `{error, model}` | 模型请求以 `AgentProtocolError` 终结（`_request_agent_reply` 里 `model.request.error`）；宿主据此分发 `model.request.error` |
| `turn.tool_call_started` | `{call_id, tool}` | 无（Rust 侧新增）：模型开始吐一个工具调用，参数还在流里 |
| `turn.tool_call_arguments` | `{call_id, delta}` | 无（Rust 侧新增）：工具调用参数的增量，可能是半截 JSON |
| `turn.tool_output_compression` | `{call_id, tool, phase, before_chars, after_chars, output, error}` | 无（Rust 侧新增）：工具输出压缩的 `started` / `finished` 两个阶段，`finished` 带压缩后的正文 |
| `tool.started` | `{step, call}` | `on_tool_start` |
| `tool.finished` | `{call, result}` | `on_tool_result` |
| `tool.output_update` | `{call, result}` | `on_tool_output_update` |
| `subagent.event` | `{name, payload}` | `on_subagent_event` |
| `todo.update` | `{todos}` | `on_todo_update` |

宿主 → 内核的 `turn.cancel` 对应宿主侧的 `cancel_check`；`stop_check` 由内核判定、不是协议方法。
`request_reply` 有两条实现：宿主给了 `initialize.model` 时，内核经 `omnicrawl-llm` 自己发请求
（增量用上面的 `turn.delta` 等通知外发）；没给时才经 `model.reply` 由宿主代答——那是过渡形态，
新旧宿主因此可以同时存在。
经 `model.reply` 由宿主代答（Python 侧本来就把模型客户端放在宿主），`stop_check` 由内核判定、
不是协议方法。内核自带 provider runtime（`omnicrawl-llm`）后 `model.reply` 不再被使用。

映射的完整性由 `rust/tools/gen_host_bridge_fixture.py` 反射 Python 真实现生成 fixture 来钉住：
回调多一个、少一个或改名，`cargo test -p omnicrawl-ipc` 就会红。

### 负载结构

```json
ToolCall              {"name": "read_file", "arguments": {"path": "a.py"}, "id": "c1", "function_name": "read_file"}
ToolResult            {"ok": true, "output": "done", "full_output": "", "error_code": null, "retryable": false}
AgentLoopObservation  {"tool_call": <ToolCall>, "result": <ToolResult>,
                       "message": {"role": "tool", "content": "done"}, "followup_messages": []}
```

`message` 是回填给模型的原始消息，`followup_messages` 用于工具结果之后的补充观察（如视觉截图）；
宿主必须先回填整批 tool 消息，再追加这些 user 消息，否则模型侧工具协议会失效。

## 顺序与并发

- 同一流内的帧顺序即事件顺序。
- 内核等待 `tool.batch` 响应期间仍可发通知（`tool.started` 可能先于响应到达宿主）；
  同一 `call_id` 的 `tool.started` 必然先于 `tool.finished`。

- **工具调用的流式渲染**（Rust 侧新增，Python 没有）：内核在**模型还在写参数**时就发
  `turn.tool_call_started`（带 `call_id` / `tool`）与 `turn.tool_call_arguments`（参数分片），
  宿主据此先把卡片立起来、逐段补参数（`write_file` / `Edit_file` 的内容预览也跟着长）。
  批次真正执行时仍然是宿主自己发的 `tool.started` / `tool.finished`，两者靠 `call_id` 对齐。
- **工具输出压缩的阶段提示**：内核压缩旁路在每个被压缩的观察前后发
  `turn.tool_output_compression`（`phase=started` 时不带计量，`finished` 带 `before_chars` /
  `after_chars`，单位是字符数），宿主据此在结果上方显示「正在压缩…」/「已压缩 a → b 字符」。
- **会话区提示**：`turn.notice` 与 `turn.status` 的差别只在宿主落点——前者是「一条已经落在
  对话里的提示」（当前用于出网脱敏的占位符还原告警，以及压缩边界：上下文压缩的
  「---已压缩 xxk~xxk ---」与回合末工具调用压缩的「已压缩 a → b 字符」），宿主应追加进对话流；
  后者是「正在做什么」的运行状态，宿主写状态行。压缩边界必须走前者：写状态行会在
  `turn.finished` 时被清掉，对话里只剩摘要/概括，用户看不到「上一段已被替换」的痕迹。
- 一个连接同时只跑一个回合；第二个 `turn.submit` 回 `-32002`。
- 通知不带 `id`；响应必须带回对应请求的 `id`，`id` 允许整数或字符串。

## 错误码

| 码 | 含义 |
| --- | --- |
| `-32700` | 解析错误（当前实现：丢弃该行，不回响应） |
| `-32600` | 帧无效，或未握手就发其他请求 |
| `-32601` | 方法名不在协议 v1 里 |
| `-32602` | 负载字段不符 |
| `-32603` | 内核内部错误（含工具批次观察数量不符） |
| `-32001` | 协议主版本不受支持 |
| `-32002` | 回合忙（已有回合在跑） |
| `-32003` | 回合被取消（`turn.cancel` 或 `shutdown` 在回合内到达） |
| `-32004` | 回合失败：预算超限、模型回复来源失败、工具批次失败等，`data.kind` 给出 `LoopError::tag()` |

## 版本与兼容

- 主版本不匹配 → `-32001`，宿主应换版本重试。
- 次版本约定为「只增不改语义」：宿主可用任意次版本与同主版本内核通信。
- 新增方法或新增可选字段属于次版本范围内的兼容变更；宿主应忽略未知字段，内核对未知方法回
  `-32601` 而不是断开连接。
- v1 只协商版本，不协商能力位；等真的出现可选能力再加，避免空壳字段。

## 与插件通路的关系

插件走自己的 NDJSON JSON-RPC（`hook.invoke` / `hook.cancel` 等），由内核的插件宿主拉起独立 Node
进程；宿主不与插件直接通信。两条通路的帧形状相同，但方法空间互相独立。

## 当前实现：`omnicrawl-cli`

`rust/crates/omnicrawl-cli` 提供 `omnicrawl` 二进制，已实现握手状态机（未握手前其他请求回 `-32600`）、
`turn.submit` / `turn.cancel` / `turn.undo` / `session.settings` / `session.compact` / `subagent.query` /
`session.list` / `session.rename` / `session.archive` / `session.history` / `session.events` / `session.new` / `session.resume` /
`session.append` / `subagent.run` / `workspace.switch` / `shutdown`，并作为 `tool.batch` 与 `model.reply` 的请求方。

`session.settings` 的写入面在 `src/settings.rs`（校验与应用分离，失败即整体不写），压缩字段的映射
与 `initialize` 共用 `compaction.rs::overlay_compaction_config`，两处不会漂移。会话状态的读写面在
`compaction.rs::KernelSession`（`open` / `reopen` / `start_new` / `reload_history` / `append` /
`switch_workspace`）与
`session.rs` 的各 `respond_session_*` 之间，命令只做协议校验与结果投影。
`subagent.run` 与 `subagent` 工具共用 `SubAgentRuntime::prepare` 与同一套结果投影
（`require_completed_result`），因此角色发现、工具白名单与失败文案不会出现两份。

事件归属按「信息在哪一侧产生」划分：

- **内核发出**（`omnicrawl-llm` 已接线，内核自带 provider runtime）：`turn.delta`、`turn.reasoning_delta`、
  `turn.token_usage`、`turn.status`、`turn.notice`、`turn.retry_status`、`turn.stream_rollback`、`turn.finished`。给了
  `initialize.model` 后模型请求由内核自己发，增量也由内核转出。
- **内核自产的收尾事件**：`turn.finished`、`turn.context_compaction`、`turn.model_response_after`、
  `turn.model_request_error`。`turn.context_compaction` 的触发点在回合收尾的压缩判定
  （`_trigger_context_compaction_after_turn`）：只有 `trigger_reached` 的回合才发，载荷是压缩前的
  上下文计量。模型的两条分别对应一次模型请求成功返回与以协议错误终结。宿主收到这些通知后
  分发同名插件 Hook（`context.compaction.after_turn` / `model.response.after` / `model.request.error`）
  ——插件运行期在宿主侧，而触发点在 agent runtime（内核侧）。
- **压缩后的上下文计量**（`post_compaction_context_tokens`，可选字段）：压缩发生在回合收尾之后，
  此后不会再发模型请求，`turn.token_usage` 会一直停在压缩前那次的用量上。内核在两条压缩路径上
  把这个数发给宿主，宿主据此把上下文占用刷成压缩后的真实大小：
  `turn.context_compaction`（阈值/溢出触发的上下文压缩，只有 `trigger_reached` 的回合才发）与
  `turn.finished`（回合末的整轮工具调用压缩）。`session.compact`（显式 `/compact`）的响应里带同名字段。
  字段缺失表示该回合没有压缩（或算不出稳定上下文），宿主保持现有遥测不动。
- **宿主发出**（宿主执行 `tool.batch` 时自行产生）：`tool.started` / `tool.finished` / `tool.output_update`，
  以及 `todo.update`。同一件事不在协议上出现两份，所以内核不重复转出工具生命周期事件。
- **两者都可能发出**：`subagent.event`——子代理由宿主执行时宿主发，由内核自持执行（当前实现）时内核发。
- **代答路径**：不给 `initialize.model` 时内核只发 `model.reply` 请求，模型侧增量由宿主自己推给界面。

工具批次边界：内核把**整批**调用交宿主（`tool.batch` 的 `{turn_id, step, calls}`），宿主必须先完成整批
规范化与审批、再按模型调用顺序回 `{observations}`；内核不接受逐工具回调。例外是**内核自持工具**——
内核自己作答，不占宿主批次：

- `recall_session_evidence`：只读当前会话，信息本来就在内核侧；
- `subagent`：子任务要跑独立子回合（子模型请求 + 子工具批次），而模型运行时在内核，因此由内核执行；
  子回合里被允许的工具仍走同一 `tool.batch` 通道，`turn_id` 用子任务号，宿主据此把审批与生命周期分开。
  子代理的生命周期事件（`subagent.event`）也由内核发出。

`subagent` 的可用角色来自内核读到的 `subagents.toml` 与 Markdown 定义（环境变量 `AI_SUBAGENTS_FILE`
指定配置文件、`OMNICRAWL_SUBAGENTS_DIR` 指定定义目录）；未启用时工具仍在表里，调用会得到
`SUBAGENT_DISABLED` 的稳定错误。宿主只负责把角色名填进工具声明的 `enum`。

`subagent` 的执行方式：`action=run` 按 `max_concurrency` 并发跑子任务，`action=spawn` 交给内核的
后台线程池、父回合立刻收尾，`list` / `get` / `cancel` 用来查这些后台任务；`fail_fast` 在前序任务
失败后停止调度，未跑的任务落成 `cancelled`。**并发对宿主是透明的**——所有子任务的工具批次都由内核
汇总后**串行**发出，任一时刻只有一个 `tool.batch` 在途，宿主不必支持多批次并存；后台任务的工具批次
发生在回合之外，`turn_id` 用子任务号。

`isolation=worktree` 的角色由内核建独立工作树（`~/.omnicrawl/agent-worktrees/sw-<task>`），
并把隔离根随子任务的 `tool.batch` 下发；成果**不自动写回**，由父 Agent 用 `subagent` 的
`list_worktrees` / `apply_worktree` / `discard_worktree` 审查处理（`discard` 默认受变更保护，
需要 `force=true` 才能丢掉未应用的改动）。已知差异：不做目录复用（残留目录会明确报错），
也没接共享注册表，因此崩溃残留不会被启动清扫自动回收。

## 尚未实现

- 多连接与多回合并发不支持。
- 回合内断线不带状态恢复：宿主重连后应重新 `initialize`。
- `model.reply` 的代答路径没有流式增量：过渡期由宿主自己把增量推给界面。
