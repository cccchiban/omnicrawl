# omnicrawl-cli

`omnicrawl` 二进制：内核进程，在 stdin/stdout 上提供宿主桥接协议 v1（规格见 `../../docs/protocol-v1.md`）。

## 它做什么

- 持有 `omnicrawl-core` 的回合循环：`turn.submit` 进来就跑一轮「模型 ⇄ 工具」。
- 两个宿主端口经协议外发：`tool.batch`（宿主执行整批工具）与 `model.reply`（过渡期宿主代答模型）。
- 回合进行中到达的 `turn.cancel` / `shutdown` 立即中止当前端口调用；其余请求照常应答，不阻塞宿主。
- 未握手前除 `initialize` 外的请求回 `-32600`；协议主版本不匹配回 `-32001`。

## 会话与压缩

`initialize` 带 `session` 块时内核自己持有会话：转录落在 `root`（与 Python 侧同一套布局），
回合结束写 `user_message` / `assistant_message`，再按 `compaction` 阈值跑一次压缩。
摘要提示词模板在编译期嵌进二进制（`omnicrawl/templates/summary_prompt.md`），不依赖运行时目录。

内核自己持有会话时，`recall_session_evidence` 由内核本地作答（不发给宿主）：它按最后一个有效摘要的
授权读取转录事件与 `archive/compacted/` 归档，artifact 正文经会话 artifact 区读取，返回紧凑 JSON 信封。

上游判定上下文超限时（`ModelErrorCode::ContextLengthExceeded` 归类文案），内核压缩当前未完成回合、
把续接指令写进会话并自动重试同一回合；恢复失败则保留原错误返回给宿主。

压缩成功后：会话里出现 `compact_summary`，测量事件 `context_compaction_measurement` 逐回合记录，
`recall_session_evidence` 之外的可恢复信息（被压缩窗口的原始事件）归档到 `archive/compacted/`；
下一轮请求只带「摘要 + 保留窗口 + 当前输入」。

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
