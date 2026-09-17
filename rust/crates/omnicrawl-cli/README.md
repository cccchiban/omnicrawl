# omnicrawl-cli

`omnicrawl` 二进制：内核进程，在 stdin/stdout 上提供宿主桥接协议 v1（规格见 `../../docs/protocol-v1.md`）。

## 它做什么

- 持有 `omnicrawl-core` 的回合循环：`turn.submit` 进来就跑一轮「模型 ⇄ 工具」。
- 两个宿主端口经协议外发：`tool.batch`（宿主执行整批工具）与 `model.reply`（过渡期宿主代答模型）。
- 回合进行中到达的 `turn.cancel` / `shutdown` 立即中止当前端口调用；其余请求照常应答，不阻塞宿主。
- 未握手前除 `initialize` 外的请求回 `-32600`；协议主版本不匹配回 `-32001`。

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
