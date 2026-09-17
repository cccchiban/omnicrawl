# omnicrawl-llm

Provider 运行时的纯逻辑层：流解析、请求构建与用量归一化。语义基准是
`omnicrawl/llm/providers/openai_chat.py`、`omnicrawl/llm/providers/openai_common.py`
与 `omnicrawl/llm/usage.py`。

## 本片范围

- `arguments_json_complete`：参数完整性判定（半截 JSON 不能当正常调用收尾，否则会被静默降级成空参数误执行）。
- `emit_tool_call_deltas` ＋`ToolCallArgumentsDelta`按 `index` 归并分片，产出 `ToolCallStarted` 与参数增量。
- `decode_sse_data` / `iter_raw_sse_events`：SSE 负载解码与消费（`[DONE]` 终止、`error` 负载转错误、非对象负载跳过）。
- `first_choice`：取首个 choice。
- `to_openai_messages`：会话消息 → Chat Completions `messages`（system 提示词非空才下发、动态工具声明按名去重、
  assistant 的 `reasoning_content`/`tool_calls`、工具结果消息、视觉图片走 content 数组）。
- `tool_specs_to_openai_functions`：`ToolSpec` → functions 声明；空 `parameters` 补成空对象模式。
- `sanitize_provider_options`：provider_options 白名单校验（Host 字段禁止覆盖，白名单外字段直接拒绝）。
- `build_chat_request`：组装 `ChatRequest { body, timeout_seconds }`；`thinking` / `reasoning_effort` 扩展、
  `tools` 与 `tool_choice`、`max_tokens`、`temperature`、`prompt_cache_key` 按 Python 侧同序合并。
- `build_prompt_cache_key` / `is_openai_gpt_model`：prompt_cache_key 计算（sha256 取前 32 位十六进制）与 GPT 系列判定。
- `usage_from_openai_payload`：Responses / Chat Completions 负载 → 统一 `TokenUsage`。

不搬（留在调用方）：HTTP 建连与重试、连接关闭、SDK 对象建模、Provider 注册与能力表，
以及能力门禁（`streaming` / `tools` / `prompt_cache` 开关的判定）。

## 边界要求

产出的每个事件都能独立序列化成一行 NDJSON，可直接被进程主增量消费；
`timeout_seconds` 是传输层参数、不写进请求体（Python 侧它是 SDK 参数，由 SDK 消费）；
本 crate 无 I/O、无全局状态。

## parity 工作流

```bash
python rust/tools/gen_llm_stream_fixture.py    # 流解析
python rust/tools/gen_llm_request_fixture.py   # 请求构建
python rust/tools/gen_llm_usage_fixture.py     # 用量归一化
cd rust && cargo test -p omnicrawl-llm
```

fixture：

- `tests/fixtures/openai_chat_stream_parity.json`：参数完整性 16、分片归并 8、SSE 解码 10、SSE 流 9、首选项 6。
- `tests/fixtures/openai_chat_request_parity.json`：请求 33、provider_options 8、GPT 判定 13、prompt_cache_key 5、
  参数串 16、浮点写法 6。
- `tests/fixtures/openai_chat_usage_parity.json`：用量 30。

请求组的期望值是 Python 真实现的 SDK 调用参数：生成器用替身客户端在 `chat.completions.create` 处拦下 kwargs，
再把传输层的 `timeout` 拆出来，因此消息转换、动态工具去重、选项合并、prompt_cache_key 都是照真跑结果对照的。

## 已知差异

- **SDK 对象分支不搬**：Python `_decode_sse_data` 的 `.json()` 快捷分支、`_emit_tool_call_deltas` 的
  `getattr(func, ...)` 对象分支、usage 系列的 `getattr` / `model_dump` 分支，都依赖 Python SDK 的对象形态；
  跨进程边界上只有 JSON。
- **工具调用参数串的键序不同**：Python 的 `json.dumps` 保留 dict 插入序，Rust 的 `serde_json::Map` 是字典序。
  语义等价（Provider 按 JSON 解析），但同一段历史在两侧的请求字节不同——过渡期切到内核后，
  上层网关的提示前缀缓存可能不命中一次。书写形式（`", "` / `": "` 分隔符、转义、整数写法）由 parity 的
  「参数串」组逐字节钉住，键序差异由「请求」组在比对前规范化。
- **浮点写法不同**：Python 用 `repr`（`1e+20`、`1e-07`），Rust 用 ryu（`1e20`、`1e-7`），`1e-5` 这类还会写成小数。
  语义等价，「浮点写法」组只校验解析回 `f64` 后相等。
- **provider_options 多处违规时报哪一条**：Python 按调用方插入序、Rust 按字典序，两者都拒绝。
- **用量取值的边界**：`TokenUsage` 是无符号整数，负数一律归零（Python 原样返回负数）；超出 `i64` 的整数按缺失处理
  （Python 用任意精度整数）。
- JSON 解析用 `serde_json`：不接受 `NaN`/`Infinity`（Python 的 `json.loads` 接受）。
- 数值/布尔的 str 化按 JSON 写法（`true`），Python `str(True)` 给出 `True`；实际模型请求里这些字段恒为字符串。
- 传输层行为（关闭 HTTP 响应、把 request 附着到错误对象）不在本 crate。
