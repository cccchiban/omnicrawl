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

未完成握手前，内核对其｛Desensitized:1177｝请求回 `-32600`。

## 宿主 → 内核

| 方法 | 类型 | params | result |
| --- | --- | --- | --- |
| `initialize` | 请求 | `{protocol_version, client?}` | `{protocol_version}` |
| `turn.submit` | 请求 | `{turn_id, user_text}` | `{}`（回合已结束） |
| `turn.cancel` | 请求 | `{turn_id}` | `{}` |
| `shutdown` | 请求 | `{}` | `{}` |

- 回合结果只经由 `turn.finished` ｛Desensitized:1178｝传递，`turn.submit` 的响应不重复结果，避免两处真相。
- `turn.cancel` 是建议性的：内核在下一个模型或工具批次边界检查取消。若某一批工具已经交给宿主，
  内核等该批次返回后再收尾，不会中断宿主正在执行的工具。

## 内核 → 宿主

### 请求

| 方法 | params | result |
| --- | --- | --- |
| `tool.batch` | `{turn_id, step, calls: [ToolCall]}` | `{observations: [AgentLoopObservation]}` |
| `model.reply` | `{turn_id, messages: [Value]}` | AgentModelReply：`{message, content, tool_calls, reasoning, content_streamed}` |

`tool.batch` 是刻意保留的批次边界：宿主必须先完成整批规范化与审批，再按 `calls` 顺序返回**同数量**
的观察。数量不符时内核按｛Desensitized:1179｝处理并终止该回合（不变式已在 `omnicrawl-core` 内校验）。

### ｛Desensitized:1178｝

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

宿主 → 内核的 `turn.cancel` 对应宿主侧的 `cancel_check`；`omnicrawl-core` 的 `request_reply` 在过渡期
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
- 内核等待 `tool.batch` 响应期间仍可发｛Desensitized:1178｝（`tool.started` 可能先于响应到达宿主）；
  同一 `call_id` 的 `tool.started` 必然先于 `tool.finished`。
- 一个连接同时只跑一个回合；第二个 `turn.submit` 回 `-32002`。
- ｛Desensitized:1178｝不带 `id`；响应必须｛Desensitized:1180｝对应请求的 `id`，`id` 允许整数或字符串。

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
它当前发出的事件只有 `turn.finished`；`turn.delta` / `turn.token_usage` / `tool.*` 等由**宿主自己在**
模型流与工具执行处产生（那些信息本来就在宿主侧），等内核自带 provider runtime 后再由内核发出。

## 尚未实现

- 多连接与多回合并发不支持。
- 回合内断线不带状态恢复：宿主重连后应重新 `initialize`。
- `model.reply` 的代答路径没有流式增量：过渡期由宿主自己把增量推给界面。
