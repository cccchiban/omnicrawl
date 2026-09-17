# omnicrawl-ipc

宿主桥接协议的实现：NDJSON 帧、版本协商，以及宿主事件/命令到方法的映射。
协议规范见 [`rust/docs/protocol-v1.md`](../../docs/protocol-v1.md)。

## 本片范围

- `frame`：一行一个 JSON-RPC 2.0 帧的解析与序列化、形状校验、id 保真、错误码常量。
- `version`：协议版本常量与主版本协商。
- `bridge`：`HostEvent`（内核 → 宿主通知）、`Command`（宿主 → 内核命令）、
  `ToolBatch` / `ToolBatchResult`（工具批次往返），以及方法名常量与错误对象工厂。

纯逻辑、无 I/O、无全局状态：读construction与写管道由宿主或 `omnicrawl-cli` 负责。

## 设计要点

- 帧用**单一结构**而非枚举：JSON-RPC 的三种形状共享 `jsonrpc` 与 `id`，枚举得在反序列化后
  二次校验，错误信息也更含糊；这里把形状校验集中在 `Frame::validate`。
- 帧内的换行由 JSON 转义保证，因此 `Frame::to_line()` 的结果必然是单行；宿主写回时自行补 `\n`。
- 无法解析的行由内核丢弃（`Frame::parse` 返回 `FrameError`），不回 `-32700`：`id` 拿不到时响应
  也无处可去。
- 负载字段名就是协议的一部分，改动即破坏性变更。

## 测试

```bash
cd rust && cargo test -p omnicrawl-ipc
```

- `tests/frame_codec.rs`：单行不变式（含负载内换行）、形状校验、id 保真、错误码。
- `tests/bridge_round_trip.rs`：13 个宿主事件与 4 个命令的负载样本覆盖检查 + 逐行往返，
  另有 `tool.batch` 与其观察结果的往返。
- `tests/host_bridge_parity.rs`：与 Python 真实现的契约对照，期望值由
  `python rust/tools/gen_host_bridge_fixture.py` 反射 `run_stream` 与循环 `run` 的签名生成。

## 已知边界

- 握手状态机、回合忙判定与 `tool.batch` 超时不在本 crate：它们是 `omnicrawl-cli` 的会话职责，
  本 crate 只提供帧与方法映射。
- 未知方法一律回 `-32601`，不断开连接；非法帧只记录日志。
