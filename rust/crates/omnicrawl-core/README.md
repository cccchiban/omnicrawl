# omnicrawl-core

Provider 无关的 Agent 回合循环。语义基准是 Python 侧的
`omnicrawl/agent/runtime/execution.py`：只承载「模型回复 → 整批工具观察 → 下一次模型请求」
的纯状态转移。

## 迁移范围

搬进来：

- 循环本体：三类预算（模型回合 / 工具调用 / 时间）、取消检查、停止检查。
- 数据契约：工具调用、工具结果、Agent 层模型回复、循环结果、循环错误。

不搬（留在 Python 或由 Host 通过 trait 实现）：

- `omnicrawl/agent/controllers/turn/loop.py::run_stream` 的编排壳：插件钩子、会话事件落盘、
  回合快照、压缩触发、run_guard 续跑、上下文溢出恢复、取消摘要。
- 工具批次的规范化与审批顺序、denied 短路、未知工具结果、线程池并发、批次共享截止时间、
  超时结果与后续丢弃、输出预算与压缩、视觉 followup 构造。
- 纯展示字段（`ui_artifact`、`completed_at`、`model_images`）。

## 状态转移清单

| 序 | 触发 | 判定 | 动作 / 返回 |
| --- | --- | --- | --- |
| 1 | 循环开始、每批工具回填之后 | 取消检查抛出 | 原样终止，不包装 |
| 2 | 同上 | `now - started_at >= timeout_seconds`（仅设置时读时钟） | 抛「Agent Loop 已超过时间预算 N 秒。」 |
| 3 | 请求模型之前 | `model_turns >= max_model_turns` | 抛「Agent Loop 已达到模型回合预算 N。」 |
| 4 | — | `request_reply` 失败 | 原样传播；成功则 `model_turns += 1`，非空 reasoning 累积 |
| 5 | 模型未请求工具 | — | 收尾：`final_text = content.strip()`，`content_streamed` 取本次回复，`last_reply` 为本次回复，`paused = false` |
| 6 | 模型请求工具 | `tool_calls + len(calls) > max_tool_calls` | 抛「Agent Loop 工具调用预算为 N，当前批次将累计到 M。」（先于 append assistant 消息） |
| 7 | 预算通过 | — | 先 append assistant 消息，再 `execute_tool_batch(calls, tool_calls + 1)`（`first_step` 为 1 基） |
| 8 | 批次返回 | `len(observations) != len(calls)` | 抛「工具批次观察数量与模型调用数量不一致：期望 X，实际 Y。」 |
| 9 | 校验通过 | — | 先按序 append 全部 `observation.message`，再按序 append 各 observation 的 `followup_messages` |
| 10 | 回填完成 | — | `tool_calls = next_tool_count`，重复第 1 项的边界检查 |
| 11 | 边界检查通过 | `stop_check()` 为真 | 返回 `final_text = ""`、`content_streamed = false`、`paused = true`、`last_reply = None` |

## 预算校验（构造期）

- `max_model_turns` / `max_tool_calls`：正整数；`0` 非法 → 「{name} 必须是正整数或 None。」
- `timeout_seconds`：正数 → 「timeout_seconds 必须是正数或 None。」；NaN 与 Python 一致地通过校验。

## parity 工作流

```bash
python rust/tools/gen_parity_fixture.py      # 由 Python 真实实现产出期望值
cd rust && cargo test -p omnicrawl-core      # 同输入重放并逐字段比对
```

fixture 位于 `crates/omnicrawl-core/tests/fixtures/turn_loop_parity.json`，每个用例记录脚本化的
模型回复、工具批次、取消/停止触发点、时钟序列与调用日志，以及期望结果或「错误标签 + 错误文本」。

## 已知差异

- 时钟：Python 用 `time.monotonic()`，Rust 用注入的 `Clock`（默认提供系统单调时钟）；两侧的
  取时刻次数按用例对齐。
- 消息列表：Python 就地修改调用方传入的 list 并由结果内嵌引用；Rust 由调用方持有消息列表，
  结果不复制。
- 超时秒数文本：复刻 `{value:g}` 的整数与常规小数写法；极大/极小量级的指数写法未对齐。
- `ToolResult` 只保留模型上下文使用的字段，展示与视觉字段留在 Host。
