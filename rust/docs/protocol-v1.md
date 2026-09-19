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
{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}
```

主版本不匹配时回错误（`supported` 与 `host` 便于诊断）：

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"不支持宿主协议版本 2.0。","data":{"supported":"1.0","host":"2.0"}}}
```

未完成握手前，内核对其其余请求回 `-32600`。

## 宿主 → 内核

| 方法 | 类型 | params | result |
| --- | --- | --- | --- |
| `initialize` | 请求 | `{protocol_version, client?, model?, session?}` | `{protocol_version}` |
| `turn.submit` | 请求 | `{turn_id, user_text}` | `{}`（回合已结束） |
| `turn.cancel` | 请求 | `{turn_id}` | `{}` |
| `shutdown` | 请求 | `{}` | `{}` |

- 回合结果只经由 `turn.finished` 通知传递，`turn.submit` 的响应不重复结果，避免两处真相。
- `turn.cancel` 是建议性的：内核在下一个模型或工具批次边界检查取消。若某一批工具已经交给宿主，

`initialize` 的 `model` 是可选块：**给了它，内核就自己发模型请求**（工具仍由宿主执行），宿主不必再应答
`model.reply`；不给则维持代答路径，旧宿主不受影响。凭据不进帧——`api_key_env` 只给环境变量名。

```json
{"model": {"model": "gpt-5.2", "base_url": "https://api.openai.com/v1", "api_key_env": "OPENAI_API_KEY",
           "user_agent": "omnicrawl/0.1.0", "system_prompt": "你是助手。",
           "tools": [{"type": "function", "function": {"name": "read_file"}}],
           "options": {"temperature": 0.2}, "request_timeout_seconds": 180,
           "prompt_cache_capable": true, "prompt_cache_identity": {"profile": "main"},
           "request_retry_count": 1}}
```

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
- 压缩发生后，内核发出的下一轮请求只带「摘要 + 保留窗口 + 当前输入」，被摘要取代的旧消息不再进上下文。
- 上游判定上下文超限时，内核压缩当前未完成回合、把续接指令（`请依据上方的结构化工作摘要继续完成当前任务。`）
  写进会话并重试同一回合；恢复失败则把原错误返回给宿主。


- `model` 必填；`base_url` 空则用运行时默认（OpenAI 官方地址）。
- `tools` 是 OpenAI functions 形状的静态声明；历史里出现的工具声明不重复下发。
- `options` 用 `GenerationOptions` 的 JSON 形状。
- `request_retry_count` 是空响应与可重试错误的最大请求次数（默认 1），语义与 Python 侧同名配置一致。
  内核等该批次返回后再收尾，不会中断宿主正在执行的工具。

## 内核 → 宿主

### 请求

| 方法 | params | result |
| --- | --- | --- |
| `tool.batch` | `{turn_id, step, calls: [ToolCall], workspace_root?}` | `{observations: [AgentLoopObservation]}` |
| `model.reply` | `{turn_id, messages: [Value]}` | AgentModelReply：`{message, content, tool_calls, reasoning, content_streamed}` |

`tool.batch` 是刻意保留的批次边界：宿主必须先完成整批规范化与审批，再按 `calls` 顺序返回**同数量**
的观察。数量不符时内核按协议错误处理并终止该回合（不变式已在 `omnicrawl-core` 内校验）。

`workspace_root` 是可选字段：带上它表示这批工具要在**隔离根**下执行（当前只有 `subagent` 的
`isolation=worktree` 子任务会带）——宿主应把工作目录与路径保护都切到该根，缺省则用宿主自己的
工作区。宿主可以忽略该字段（行为退回共享工作区），但那样隔离就不成立。

### 通知

| 方法 | params | Python 侧来源（`loop.py`） |
| --- | --- | --- |
| `turn.delta` | `{text}` | `on_delta` |
| `turn.reasoning_delta` | `{text}` | `on_reasoning_delta` |
| `turn.status` | `{message}` | `on_status` |
| `turn.retry_status` | `{message}` | `on_retry_status` |
| `turn.protocol_wait` | `{}` | `on_protocol_wait` |
| `turn.stream_rollback` | `{}` | `on_stream_rollback` |
| `turn.token_usage` | `{input_tokens, output_tokens, cached_input_tokens}` | `on_token_usage` |
| `turn.finished` | `{turn_id, final_text, reasoning, model_turns, tool_calls, paused}` | `run_stream` 的返回值 |
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
`turn.submit` / `turn.cancel` / `shutdown`，并作为 `tool.batch` 与 `model.reply` 的请求方。

事件归属按「信息在哪一侧产生」划分：

- **内核发出**（`omnicrawl-llm` 已接线，内核自带 provider runtime）：`turn.delta`、`turn.reasoning_delta`、
  `turn.token_usage`、`turn.status`、`turn.retry_status`、`turn.stream_rollback`、`turn.finished`。给了
  `initialize.model` 后模型请求由内核自己发，增量也由内核转出。
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

`isolation=worktree` 的角色由内核建独立工作树（`~/.omnicrawl/agent-worktrees/sw-<task>`），
并把隔离根随子任务的 `tool.batch` 下发；成果**不自动写回**，由父 Agent 用 `subagent` 的
`list_worktrees` / `apply_worktree` / `discard_worktree` 审查处理（`discard` 默认受变更保护，
需要 `force=true` 才能丢掉未应用的改动）。已知差异：不做目录复用（残留目录会明确报错），
也没接共享注册表，因此崩溃残留不会被启动清扫自动回收。

## 尚未实现

- 多连接与多回合并发不支持。
- 回合内断线不带状态恢复：宿主重连后应重新 `initialize`。
- `model.reply` 的代答路径没有流式增量：过渡期由宿主自己把增量推给界面。
- `subagent` 目前只支持 `action=run` 且**逐个**执行任务（`max_concurrency` 未生效）；`action=spawn`
  回 `SUBAGENT_BACKGROUND_DISABLED`——后台线程池已经在 `controllers/subagents/tasks.rs` 备好，
  接进来需要把连接从 `Rc<RefCell<Conn>>` 改成可跨线程共享。`fail_fast` 已实现：前序任务失败后
  停止调度，未跑的任务落成 `cancelled`（原因见 `fail_fast 已在前序任务失败后停止调度该任务。`）。
