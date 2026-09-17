# omnicrawl-llm

Provider 运行时：请求构建、HTTP 传输、流解析、用量归一化，以及把一次回合从请求串到
`ModelReply` 的装配。语义基准是 `omnicrawl/llm/providers/openai_chat.py`、
`omnicrawl/llm/providers/openai_common.py`、`omnicrawl/llm/usage.py`，
错误文案基准是 `omnicrawl/llm/errors.py` 的状态码阶梯。

## 本片范围

- `arguments_json_complete`：参数完整性判定（半截 JSON 不能当正常调用收尾，否则会被静默降级成空参数误执行）。
- `emit_tool_call_deltas` ＋`ToolCallArgumentsDelta`按 `index` 归并分片，产出 `ToolCallStarted` 与参数增量。
- `decode_sse_data` / `step_payload` / `payload_of_line`：SSE 一行 → 负载文本 → 语义
  （`[DONE]` 终止、`error` 负载转错误、非对象负载跳过）。
- `first_choice`：取首个 choice。
- `to_openai_messages`：会话消息 → Chat Completions `messages`（system 提示词非空才下发、动态工具声明按名去重、
  assistant 的 `reasoning_content`/`tool_calls`、工具结果消息、视觉图片走 content 数组）。
- `tool_specs_to_openai_functions`：`ToolSpec` → functions 声明；空 `parameters` 补成空对象模式。
- `sanitize_provider_options`：provider_options 白名单校验（Host 字段禁止覆盖，白名单外字段直接拒绝）。
- `build_chat_request`：组装 `ChatRequest { body, timeout_seconds }`；`thinking` / `reasoning_effort` 扩展、
  `tools` 与 `tool_choice`、`max_tokens`、`temperature`、`prompt_cache_key` 按 Python 侧同序合并。
- `build_prompt_cache_key` / `is_openai_gpt_model`：prompt_cache_key 计算（sha256 取前 32 位十六进制）与 GPT 系列判定。
- `usage_from_openai_payload`：Responses / Chat Completions 负载 → 统一 `TokenUsage`。
- `ChatEndpoint` ＋ `OpenAiChatRuntime::run_turn`：一次回合从请求体组装、HTTP 往返、流事件回调、
  工具调用收尾校验到 `ModelReply` 归并；建连与读取放在可放弃的后台线程里，取消按 50ms 轮询生效。
- `RuntimeError` / `RuntimeErrorKind`：`retryable` 标记与 HTTP 状态码阶梯文案。

不搬（留在调用方）：通用重试与能力门禁（`streaming` / `tools` / `prompt_cache` 开关）、Provider 注册与能力表、
会话落盘、上下文压缩触发。

## 边界要求

- 只有一次 HTTP 往返，外加 `prompt_cache_key` 不受支持时的摘字段重发；其余失败以 `retryable` 交给调用方。
- `timeout_seconds` 是传输层参数、不写进请求体（Python 侧它是 SDK 参数，由 SDK 消费）。
- 凭据由调用方注入；本 crate 不读配置、不读环境变量。
- 流事件与最终回复同时交付：事件按发生顺序喂给 `TurnSink`（进程主据此写 NDJSON），`run_turn` 同时返回归并结果。

## parity 工作流

```bash
python rust/tools/gen_llm_stream_fixture.py    # 流解析
python rust/tools/gen_llm_request_fixture.py   # 请求构建
python rust/tools/gen_llm_usage_fixture.py     # 用量归一化
python rust/tools/gen_llm_runtime_fixture.py   # 端到端回合
cd rust && cargo test -p omnicrawl-llm
```

fixture：

- `tests/fixtures/openai_chat_stream_parity.json`：参数完整性 16、分片归并 8、SSE 解码 10、SSE 流 9、首选项 6。
- `tests/fixtures/openai_chat_request_parity.json`：请求 33、provider_options 8、GPT 判定 13、prompt_cache_key 5、
  参数串 16、浮点写法 6。
- `tests/fixtures/openai_chat_usage_parity.json`：用量 30。
- `tests/fixtures/openai_chat_runtime_parity.json`：端到端 9 个用例。

请求组的期望值是 Python 真实现的 SDK 调用参数：生成器用替身客户端在 `chat.completions.create` 处拦下 kwargs，
再把传输层的 `timeout` 拆出来，因此消息转换、动态工具去重、选项合并、prompt_cache_key 都是照真跑结果对照的。

端到端组更进一步：生成器起一个本机回环 HTTP 服务端，把固定 SSE 喂给
`OpenAIChatCompletionsRuntime`（真 SDK 客户端 + 仓库自己的 httpx 客户端工厂），
记录它收到的事件、它实际发出的**线上请求体**与失败文案；Rust 侧用同一份 SSE 重放并逐字段比对。
这一组正是靠「线上请求体」把 `extra_body` 摊平这类差异逼出来的。

## 已知差异

- **SDK 对象分支不搬**：Python `_decode_sse_data` 的 `.json()` 快捷分支、`_emit_tool_call_deltas` 的
  `getattr(func, ...)` 对象分支、usage 系列的 `getattr` / `model_dump` 分支，都依赖 Python SDK 的对象形态；
  跨进程边界上只有 JSON。
- **内核不内置重试**：Python 的 SDK 自己会重试 5xx/超时（httpx/SDK 内置 2 次），内核没有这一层，
  只把 `retryable` 交给调用方按 `request_retry_count` 决定；端到端 fixture 里 503 用例
  Python 发了 3 次、内核 1 次，测试据此只比对首个请求体。
- **流内 `error` 负载**：与 Python 一致——经「未识别」通用文案
  （`Agent 流式回复中断：模型请求失败，但未能识别具体原因。…错误类型：APIError。`）且**不**标记可重试，
  上游 message 不落到用户可见文本里。
- **传输中断的文案不同**：Python 会带 SDK 异常类型名（如 `ReadTimeout`），内核只能给出 IO 描述
  （`Agent 流式回复中断：{io}`），类型名不可对齐。
- **超时语义**：映射 Python 的 `timeout` 为建连、等响应头、以及**每次**读取响应体的空闲超时
  （不是整个响应体的总时限，否则长回复会被拦腰截断）。
- **未配置 User-Agent 时**用 ureq 默认 UA（Python 侧是 httpx 默认 UA）；配置了 `user_agent` 时两侧一致。
- **错误分类只搬了状态码阶梯**：`errors.py` 里基于错误文案的启发式（配额、鉴权、上下文超限等关键词）尚未移植。
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

## 依赖

`ureq`（阻塞式 HTTP/1.1 + rustls）是唯一的传输依赖：内核的回合循环本来就是阻塞的，不需要异步运行时。
交叉编译到 musl 时需要目标平台的 C 工具链（rustls 的 ring 组件）。
