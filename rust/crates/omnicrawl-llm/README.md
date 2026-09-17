# omnicrawl-llm

Provider 流解析层的第一片：把 Provider 的分片与 SSE 负载映射成内核的流事件。
语义基准是 `omnicrawl/llm/providers/openai_chat.py`。

## 本片范围

- `arguments_json_complete`：参数完整性判定（半截 JSON 不能当正常调用收尾，否则会被静默降级成空参数误执行）。
- `emit_tool_call_deltas` ＋ ｛Desensitized:1030｝：按 `index` 归并分片，产出 `ToolCallStarted` 与参数增量。
- `decode_sse_data` / `iter_raw_sse_events`：SSE 负载解码与消费（`[DONE]` 终止、`error` 负载转错误、非对象负载跳过）。
- `first_choice`：取首个 choice。

不搬（留在 Host）：HTTP 请求与重试、连接关闭、SDK 对象建模、Provider 注册与能力表、用量统计、请求体构建。

## 边界要求

产出的每个事件都能独立序列化成一行 NDJSON，可直接被跨进｛Desensitized:1028｝主增量消费；
本 crate 无 I/O、无全局状态。

## parity 工作流

```bash
python rust/tools/gen_llm_stream_fixture.py   # 用 openai_chat.py 生成期望值
cd rust && cargo test -p omnicrawl-llm        # 同输入跑 Rust 实现逐字段比对
```

fixture：`tests/fixtures/openai_chat_stream_parity.json`，含参数完整性 16、
分片归并 8、SSE 解码 10、SSE 流 9、首选项 6 个用例。

## 已知差异

- **SDK 对象分支不搬**：Python `_decode_sse_data` 的 `.json()` 快捷分支、`_emit_tool_call_deltas` 的
  `getattr(func, ...)` 对象分支，都依赖 Python SDK 的对象形态；跨进｛Desensitized:1029｝界上只有 JSON。
- JSON 解析用 `serde_json`：不接受 `NaN`/`Infinity`（Python 的 `json.loads` 接受）。
- 数值/布尔的 str 化按 JSON 写法（`true`），Python `str(True)` 给出 `True`；实际模型请求里这些字段恒为字符串。
- 传输层行为（关闭 HTTP 响应、把 request 附着到错误对象）不在本 crate。
