# omnicrawl-llm

Provider 运行时：请求构建、HTTP 传输、流解析、用量归一化，以及把一次回合从请求串到
`ModelReply` 的装配。语义基准是 `omnicrawl/llm/providers/` 下的 `openai_chat.py`、
`openai_common.py`、`openai_responses.py`、`anthropic.py`、`gemini.py` 与 `omnicrawl/llm/usage.py`，
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
- `ModelCapabilities` / `merge_capabilities`：模型能力（流式 / 工具 / 并行工具 / 推理 / 视觉 / 模型发现 /
  prompt_cache + 两个窗口值）的解析与合并。bool 用 `None` 表示「本层未声明」，合并时后者只覆盖**已声明**的字段
  （可显式写 `false`），整数只在正数时覆盖；读取一律先 `resolved()`（`streaming` 缺省为真，其余为假），
  另有四个 Adapter 的保守默认值。
- `resolve_protocol` / `protocol_for_provider` / `validate_protocol_matches_provider`：生效协议的选取
  （调用方指定 > Profile 默认 > Provider 默认）与一致性校验，报错文案与 Python 逐字一致
  （未知 Provider / 不支持的协议 / 协议与 Provider 不匹配）。
- `ModelRuntime`：一次回合的执行入口（`run_turn(input, sink)`），实现有 `OpenAiChatRuntime` 与
  `AnthropicRuntime`；多这一层是为了让 Provider 实现与装饰器（出网脱敏）可互换。
- Responses 请求构建（`responses.rs`）：`messages_to_responses_input`（会话消息 → `input` items——
  system 带工具声明时整条跳过、tool 结果是 `function_call_output`、assistant 文本 + 带工具历史时**必带**的
  reasoning item（id 为 `rs_` + SHA-1 前 16 位，内核手写 SHA-1 以免新增依赖，空 reasoning 用 `…` 兜底）、
  user / system 的 `input_text` 与 `input_image`、空 content 补占位）、`tools_for_responses`（请求 tools
  在前、消息携带的动态声明在后，空 parameters 补空对象模式）、`build_responses_request`（`extra_body`
  照线上形态摊平进请求体顶层、`reasoning.effort` 由 `reasoning_effort` 归一化、timeout 归传输层）、
  `flatten_tool_history_to_text` 与 `has_tool_history_items` / `is_tool_history_rejection`
  （网关不支持工具历史时的 400 降级判定）。
- Responses 流事件映射（`ResponsesStreamState`）：按 `type` 分流事件负载——文本 / 推理增量直接外发，
  工具调用只累进缓冲（**参数分片不发增量**，与 Chat Completions 不同），在 `arguments.done`、
  `output_item.done` 或 `response.completed` 时产出完成事件；`item_id` 与 `call_id` 的别名统一、
  `output_item.added` 里的函数名捕获、`arguments.done` 顶层 name/arguments 的回退分支、
  「已发出的调用不重复产出」，以及流结束时的截断判定（缺 `response.completed` 且没有完整输出 → 报错）
  与缓冲冲刷（名称 / 参数不完整也报错，空参数视为完整）。
- Responses 运行时（`ResponsesRuntime`）：一次回合从请求体、HTTP、SSE 到归并回复。两路降级重试照
  `_create_stream_with_retries`——网关不认 `prompt_cache_key` 就摘字段重发一次；网关对工具调用历史
  item 回 400 时把工具历史展平成纯文本重发一次，并记住该组合（后续请求直接展平，不再先发一次必然 400
  的请求）。两条降级都在流收尾时补发告警，顺序与 Python 一致：截断判定 → 告警 → 缓冲冲刷 → 结束事件；
  为此 `ResponsesStreamState` 的收尾拆成 `check_completeness` / `flush_buffers` / `push_finished`
  三步（`finish` 仍是三步串联）。错误包装同在这一层：`is_retryable_model_request_error`
  （可重试状态码表 + 文案关键字，含 `\b([45]\d{2})\b` 的手写等价物）与
  `Responses 请求失败：/Responses 流式回复中断：{format_openai_error}`。
  未搬：`discover_models`（属 adapter 注册表 / `build_runtime` 批次）。
- Anthropic Claude Messages 请求构建（`anthropic.rs`）：`sanitize_anthropic_options`（白名单 `top_p` /
  `top_k` / `metadata` / `stop_sequences` / `thinking`，Host 字段与白名单外字段都直接拒绝）、
  `to_anthropic_messages`（system 带工具声明时整条跳过、tool_result 与紧随其后的用户内容合并成同一条 user、
  assistant 空内容补空文本块、空 content 退化为空串、图片走原生 Base64 image source）、
  `build_anthropic_request`（请求级 `tools` = 顶层声明 + system 动态声明合并、`system` 非空白才下发、
  `max_tokens` 三级兜底：生成选项 → 模型描述 → 4096（0 视为未声明）、provider_options 摊平进顶层）。
- Anthropic 流事件映射（`AnthropicStreamState`）：`message_start` 出用量、`content_block_start` 的 tool_use
  进缓冲并补 `ToolCallStarted`（id 缺失回落 `toolu_{index}`、名称为空不发该事件）、
  text / thinking / input_json 三类增量（非字符串与空串一律忽略）、`content_block_stop` 产出完成事件、
  `message_delta` 更新 stop_reason 与用量、流收尾把没等到 stop 的工具调用按缓冲顺序补齐。
- `AnthropicRuntime`：与 `OpenAiChatRuntime` 共用「传输 + 可放弃读取」骨架；鉴权头是 `x-api-key` +
  `anthropic-version: 2023-06-01`，路径 `{base_url}/v1/messages`；建连与状态码失败按 `_format_anthropic_error`
  包成 `Claude 请求失败：…`（`INVALID_REQUEST`），流中断包成 `Claude 流式回复中断：…`。
- `usage_from_anthropic_payload`：`usage.input_tokens` / `output_tokens` / `cache_read_input_tokens`
  （兼容 `cached_input_tokens`），只认整数、缺字段按 0，有 `usage` 就产出。
- Gemini Generate Content 请求构建（`gemini.rs`）：`sanitize_gemini_options`（白名单 `top_p` / `top_k` /
  `candidate_count` / `stop_sequences` / `response_mime_type` / `safety_settings`，Host 字段与白名单外字段都直接拒绝）、
  `to_gemini_contents`（system 带工具声明时整条跳过、工具结果与紧随的用户内容合并进同一个 user content、
  assistant 走 `model` 角色、图片走 `inline_data`、空内容退化为不产出条目）、`build_generate_content_request`
  （system_instruction 非空白才下发、`max_output_tokens` 为 0 视为未声明、请求级 `tools` = 顶层声明 +
  system 动态声明合并成 `function_declarations`、`automatic_function_calling.disable` 关掉 SDK 自动执行）。
- Gemini 线上映射（`generate_content_body` / `gemini_model_path`）：kwargs 形态 → MLDev `generateContent`
  请求体——`systemInstruction`（补 `role: user`）与 `tools` 提升到顶层、`safetySettings` 顶层、
  生成参数收进 `generationConfig`（`topP` / `topK` / `candidateCount` / `maxOutputTokens` / `stopSequences` /
  `responseMimeType`）、`automatic_function_calling` 不上行；`function_declarations` → `functionDeclarations`，
  Schema 的 `type` 大写成 proto 枚举名（`object` → `OBJECT`）；模型名补 `models/` 前缀。
- Gemini 流事件映射（`GeminiStreamState`）：`usageMetadata` / `usage_metadata` 出用量，文本分片出 `TextDelta`，
  `function_call`（兼容 `functionCall`）出 `ToolCallStarted` + `ToolCallCompleted`——Gemini 不保证稳定
  `call_id`，内核按「名称 + 参数（键序无关）」内容指纹去重，再按发出次序编号 `gemini_{n}`；
  工具参数是 JSON 字符串时解析（非法回落空对象），`finishReason` 更新结束原因（空值保持默认 `stop`）。
- `GeminiRuntime`：与另外两路共用「传输 + 可放弃读取」骨架；鉴权头 `x-goog-api-key`，
  路径 `{base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse`；建连与状态码失败按
  `_format_gemini_error` 包成 `Gemini 请求失败：…`（`INVALID_REQUEST`），流中断包成
  `Gemini 流式回复中断：…`。

- 运行时工厂（`adapter.rs`）：`build_runtime(&ProviderProfile, &ModelDescriptor) -> RuntimeBundle`——
  先解析生效协议（调用方指定 > Profile 默认 > Provider 默认），再按「Provider 保守默认 → 模型声明」
  合并能力（模型描述的 `context_window_tokens` 只在正数时覆盖），`base_url` 留空时用 Provider 默认
  API 根（`default_base_url`），最后按协议造出对应的运行时。返回的 `RuntimeBundle` 带上
  `protocol` / `capabilities` / `base_url`：能力门禁留在调用方，所以要一起交出去。

- 模型列表发现（`discover_models`）：按实测的线上形态直接发 GET——`{base}/models`（OpenAI 两协议，
  `Bearer`）、`{base}/v1/models`（Anthropic，`x-api-key` + `anthropic-version`）、
  `{base}/v1beta/models`（Gemini，`x-goog-api-key`）；解析 `data[].id` / `models[].name`
  （Gemini 去掉 `models/` 前缀）、按 `model_id` 去重、截断 500 条。缺凭据与任何失败都降级成
  `unavailable`（文案前缀分别是「模型列表发现失败：」「Claude 模型列表发现失败：」「Gemini 模型列表发现失败：」），
  不让发现失败打断启动。

不搬（留在调用方）：通用重试与能力门禁（`streaming` / `tools` / `prompt_cache` 开关）、
出网脱敏装饰器、会话落盘、上下文压缩触发。

## 已知差异（Responses 请求构建）

- **`flatten_tool_history_to_text` 不就地改写入参**：Python 的实现会把工具调用直接追加进入参里那条
  assistant 消息（调用方传的正是自己要发出去的 items）；内核返回新数组、不改入参。对照片因此两侧都用副本。
- 请求体的键序由 SDK 决定：对照片按**键集合 + 逐字段值**比对，不追键序（与 `chat.completions` 同口径）。
- **流事件负载要仿 SDK 对象**：Python 侧先 `getattr(event, "type")`、再 `isinstance(event, dict)` 回退，
  真实链路拿到的是 SDK 的 pydantic 对象。驱动同一份 Python 代码的对照片必须用「属性可读的 dict」
  （生成器里的 `DotDict`），否则 `response.status` 读不到，`finish_reason` 会永远是 `stop`——
  那是个只在 dict 载荷下成立的假行为。内核按 JSON 键读取，等价于真实链路的属性访问。

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
python rust/tools/gen_llm_anthropic_fixture.py # Anthropic：请求构建、流映射、用量与错误文案
python rust/tools/gen_llm_gemini_fixture.py    # Gemini：contents / config / 线上请求体 / 流映射 / 用量 / 错误文案
python rust/tools/gen_desensitization_fixture.py         # 消息脱敏：占位符协议与序号注册表
python rust/tools/gen_desensitization_stream_fixture.py  # 消息脱敏：流式还原
python rust/tools/gen_desensitization_rules_fixture.py   # 消息脱敏：值类型规则层
python rust/tools/gen_desensitization_engine_fixture.py  # 消息脱敏：匹配引擎
python rust/tools/gen_desensitization_plan_cache_fixture.py  # 消息脱敏：屏蔽计划缓存
python rust/tools/gen_stream_registry_fixture.py         # 回合资源注册表：注册 / 注销 / 计数 / 关闭轨迹
cd rust && cargo test -p omnicrawl-llm
```

fixture：

- `tests/fixtures/desensitization_parity.json`：占位符用例 11 条、稳定索引 5 步、周期 / 会话缓存 / 注册表生命周期。
- `tests/fixtures/desensitization_stream_parity.json`：13 个场景、71 步操作（分片还原、通道隔离、结构化还原、告警、严格模式、截断判定）。
- `tests/fixtures/desensitization_rules_parity.json`：98 条语料（逐条候选 96、扫描命中 83）、熵 10 例、Luhn 13 例、邮箱豁免 12 例、IP 判定 50 例。
- `tests/fixtures/desensitization_engine_parity.json`：文本 24 + 3 条（两套熵参数）、结构用例 3、键名 24、跳过 8、形态 22、候选 11、词形 8、扫描 6。
- `tests/fixtures/desensitization_plan_cache_parity.json`：文本指纹 8、计划构建器 6 例、缓存 10 组操作序列（含计数、淘汰、失效、停用与清空）。
- `tests/fixtures/openai_chat_stream_parity.json`：参数完整性 16、分片归并 8、SSE 解码 10、SSE 流 9、首选项 6。
- `tests/fixtures/openai_chat_request_parity.json`：请求 33、provider_options 8、GPT 判定 13、prompt_cache_key 5、
  参数串 16、浮点写法 6。
- `tests/fixtures/openai_chat_usage_parity.json`：用量 30。
- `tests/fixtures/openai_chat_runtime_parity.json`：端到端 9 个用例。
- `tests/fixtures/openai_responses_request_parity.json`：input items 20、请求级 tools 4、历史展平 6、
  工具历史判定 4、`create()` 参数 16、可重试判定 20。
- `tests/fixtures/anthropic_parity.json`：消息 8、provider_options 9、请求 7、流 12、用量 6、错误文案 10。
- `tests/fixtures/gemini_parity.json`：contents 8、provider_options 11、请求 kwargs 6、真 SDK 线上体 4、
  流 14、用量 7、错误文案 12。
- `tests/fixtures/stream_registry_parity.json`：12 条操作轨迹（注册 / 注销 / 计数 / 关闭 / 作用域进出），
  逐例比对每步返回值、关闭顺序与收尾计数。

Anthropic 组的请求 kwargs 同样由替身客户端在 `client.messages.create` 处拦下；流组把 fixture 里的 JSON
负载转成属性可读的对象再喂给真实现，两侧读的是同一份字节。

请求组的期望值是 Python 真实现的 SDK 调用参数：生成器用替身客户端在 `chat.completions.create` 处拦下 kwargs，
再把传输层的 `timeout` 拆出来，因此消息转换、动态工具去重、选项合并、prompt_cache_key 都是照真跑结果对照的。

端到端组更进一步：生成器起一个本机回环 HTTP 服务端，把固定 SSE 喂给
`OpenAIChatCompletionsRuntime`（真 SDK 客户端 + 仓库自己的 httpx 客户端工厂），
记录它收到的事件、它实际发出的**线上请求体**与失败文案；Rust 侧用同一份 SSE 重放并逐字段比对。
这一组正是靠「线上请求体」把 `extra_body` 摊平这类差异逼出来的。

## 已知差异

HTTP 4xx/5xx 的**响应正文**会一起交给分类阶梯（`http_status_error_with_body`）：Python 的 SDK 异常
`str(exc)` 里带着正文，只传状态码会让「上下文超限」这类话术被丢掉；正文进阶梯后，
429 这类响应可能先命中关键词分支（如限流），文案随之不同。

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
- **错误分类**（`errors.rs`）：状态码阶梯（`http_status_error`）与基于错误文案的启发式（`map_exception`）
  都已落地，分支顺序与关键词表逐条对齐 `errors.py` 的 `map_openai_exception`——模型不存在、HTML 错误页、
  限流 / 配额、上下文超限、连接提前断开、超时、鉴权、权限、连接、TLS，最后才是「未能识别」兜底。
  内核没有 SDK 异常对象，输入用 `ExceptionView` 描述：错误文本、`type(exc).__name__`、`exc.body`、
  `exc.response.json()`、两个状态码属性；结构化错误体按 Python 口径只取 `message` / `type` / `code` /
  `error` / `detail` 五个字段（深度 ≤ 4、最多 12 段、每段 512 字符）。传输层不再自己编文案：
  `TransportFailure` 只给类型与 SDK 等价文案（`Request timed out.` / `Connection error.`），
  分支判定仍走同一张阶梯，免得两处各写一套「什么算超时、什么算连接失败」。
  未搬：流内 `error` 负载的分类——它取决于 SDK 对这类负载的 `str(exc)` 形状，当前按「未能识别」处理。
- **工具调用参数串的键序不同**：Python 的 `json.dumps` 与 `serde_json` 现在都保留插入序（workspace 开了
  `preserve_order`），但两侧的插入序来源不同：Python 侧由 SDK 按其签名顺序序列化请求体，内核按自己的组装顺序。
  语义等价（Provider 按 JSON 解析），比较前已规范化。会话文件那种长期存续的格式另有要求，
  见 `crates/omnicrawl-session/README.md`。
- **浮点写法不同**：Python 用 `repr`（`1e+20`、`1e-07`），Rust 用 ryu（`1e20`、`1e-7`），`1e-5` 这类还会写成小数。
  语义等价，「浮点写法」组只校验解析回 `f64` 后相等。
- **provider_options 多处违规时报哪一条**：Python 按调用方插入序、Rust 按字典序，两者都拒绝。
- **用量取值的边界**：`TokenUsage` 是无符号整数，负数一律归零（Python 原样返回负数）；超出 `i64` 的整数按缺失处理
  （Python 用任意精度整数）。
- JSON 解析用 `serde_json`：不接受 `NaN`/`Infinity`（Python 的 `json.loads` 接受）。
- 数值/布尔的 str 化按 JSON 写法（`true`），Python `str(True)` 给出 `True`；实际模型请求里这些字段恒为字符串。
- **消息脱敏的两处字符类差异**：Python 的 `\s` / `str.isspace()` 含 `\x1c`–`\x1f` 这类控制分隔符，
  Rust 的 `char::is_whitespace` 不含；占位符里的序号只认 ASCII 数字，Python 的 `\d` 还认 Unicode 数字。
  两者都不出现在真实语料里。

### Anthropic（未搬与差异）

- **未搬**（按设计或留待后续批次）：`close()` 与 `_closed` 关闭态（Rust 运行时无关闭态，取消由 sink 表达）；
  能力门禁（`request.tools && !capabilities.tools` → `UNSUPPORTED_CAPABILITY`）与其他门禁一样留在调用方；
  `discover_models`（属 adapter 注册表 / `build_runtime` 批次）；`interruptible_stream_events`
  的**机制**已抽成独立模块 `stream_reader.rs`（`omnicrawl/llm/stream_reader.py` 的移植：
  可放弃读取线程 + 按轮询窗口转发事件 + Drop 置位放弃信号），供长流消费方复用；
  四路 Provider 运行时目前的取消仍由运行时内置的读取线程 + 50ms 轮询表达，尚未改接到该模块；
  `stream_registry` 本身已搬，见上文模块，运行时未接线）；
  SDK 客户端级配置（`default_headers` / 客户端超时由 `ChatEndpoint` 承载）；
  `_sanitize_options` 的「provider_options 必须是对象」分支（类型系统下不可达）；
  `reasoning_effort` → `thinking` 的那个分支（Python 侧本身就是 no-op 空分支）。
- **`index` 的读法**：Python 走 SDK 对象的 `event.index`，内核读负载里的 `index` 字段；真实链路上两者同源同值，
  驱动 Python 的对照片因此用带 `index` 的对象，而不是裸 dict（裸 dict 会让 `getattr` 恒取默认 0）。
- **缺凭据文案**：沿用内核既有的配置错误文案（Python 是 `Profile {id} 缺少 Anthropic API Key。`）。
- **超时来源**：`timeout_seconds` 取 Profile 值——Python 把超时挂在 SDK 客户端上，不是每请求参数。
- **流内 `error` 负载**：内核走通用 `provider_error_stream` 文案（Python 是 SDK 抛 `APIError` →
  `STREAM_INTERRUPTED`）；错误码一致，`retryable` 标记不同。
- 用量负数按无符号归零（与 OpenAI 一路同口径）。

### Gemini（未搬与差异）

- **未搬**（按设计或留待后续批次）：`close()` 与 `_closed` 关闭态、能力门禁
  （`request.tools && !capabilities.tools` → `UNSUPPORTED_CAPABILITY`）、`discover_models`
  （属 adapter 注册表 / `build_runtime` 批次）、`interruptible_stream_events` 的机制已抽到
  `stream_reader.rs`（同 Anthropic 条目），运行时尚未改接；
  `_create_gemini_client` 的
  「缺少 google-genai 依赖」分支（内核不带 SDK）、`_sanitize_options` 的
  「provider_options 必须是对象」分支（类型系统下不可达）。
- **`wire` 组的基准是真 SDK**：生成器起本机回环服务端，让 google-genai 1.47 实际发一次请求，
  记下路径与请求体；喂进去的 contents / config 是本仓库真实现的产物。因此这一组钉住的是
  「内核的线上体与 SDK 一致」，不是「与 Python 那边的字符串一致」。
- **Schema 只做大写化**：SDK 还会把 `additionalProperties` 改写成 `additional_properties`、
  把 `topK` 这类整型生成参数写成浮点；内核不复刻（对照片的 schema 与参数刻意避开这两处）。
- **`topK` 的数值写法**：proto 里是浮点，SDK 输出 `12.0`；内核按输入原样输出（`12` 或 `12.0`），
  语义等价。
- **`functionCall` 只在裸负载下才命中**：Python 侧先读属性 `function_call`、失败才回退字典键，
  真实 SDK 对象的属性名就是 snake_case；内核跨进程边界只见到 REST 的 camelCase，
  因此两侧读法天然不同（对照片里这条用例用**裸 dict 负载**驱动，见生成器的 `raw` 标记）。
- **`index` 一类的 SDK 对象细节不参与映射**：Gemini 的流映射按分片内容而非下标推进，
  没有 Anthropic 那样的 `content_block_*` 缓冲。
- **缺凭据文案**：沿用内核既有的配置错误文案（Python 是 `Profile {id} 缺少 Gemini API Key。`）。
- **超时来源**：`timeout_seconds` 取 Profile 值（Python 把它挂在 SDK 客户端上）。
- **错误文案的两处不可对齐**：`_format_gemini_error` 的兜底分支用 `type(exc).__name__`，
  内核只能拿到等价类型名（`APIStatusError` / `APIConnectionError`）；流内 `error` 负载与另外两路一样
  走通用文案。

## 回合资源注册表（`stream_registry.rs`）

对齐 Python `omnicrawl/llm/stream_registry.py`：注册进来的模型流与外部资源按「对象身份」归属到
某个可独立取消的回合，取消或关停时只关闭该回合的注册项。

- **身份与归属**：Python 用 `is` 比对象、`ContextVar` 存当前回合；内核用 `Arc` 指针身份（`ScopeOwner`
  令牌）与线程局部作用域栈（`stream_scope` 守卫，离开作用域恢复上一层）。
- **关闭语义**：显式 `close_callback` 优先，其次资源自带的关闭动作；两者都没有的句柄（对应 Python 里
  `object()` 这类没有 `close` 的对象）注册照常、但不计入成功关闭数，且关闭后一律注销。
- **守卫**：`registered_stream_events`（迭代器丢弃即注销，正常结束与提前退出都算）与
  `registered_resource`（RAII 守卫）。
- 计数与关闭都支持按归属过滤，`None` 表示全部；同一个句柄重复注册只保留一条。

两处已知差异：作用域是线程局部的——跨线程注册要像 Python 侧「owner 由调用线程解析」那样显式传
`owner`（没有 `copy_context` 的等价物）；关闭动作 panic 不会被吞掉（Python 用 `except Exception`
兜住并继续回收其余资源）。**运行时尚未接线**：内核现有的取消仍由可放弃读取线程 + 50ms 轮询表达。

## 运行时管理器（`runtime_manager.rs`）

对齐 Python `omnicrawl/llm/runtime.py` 的 `ModelRuntimeManager` / `RuntimeSnapshot`：
不可变快照（代次 + 模型描述 + 运行时 + 能力 + 上下文窗口 + Profile）与回合边界热切换。

- **切换顺序**：造候选运行时 → 持久化 → 原子替换活跃快照；任一步失败都保留旧模型
  （`persist` 失败时把该错误原样抛出）。`allow_during_turn` 为假且存在活动回合时拒绝切换
  （`INVALID_REQUEST`，文案与 Python 一致）。
- **占用与回收**：`acquire_turn` 与 `switch` 共用切换锁，避免「检查无活动回合 → 交换 active」
  与取快照之间出现竞态；旧快照进退役列表，引用归零后移出。
- **闭锁**：`close()` 清空活跃与退役快照；`set_context_window_tokens` 只替换快照里的窗口值，
  不重建运行时。
- **与 Python 的差异**：内核的 `ModelRuntime` 没有 `close()` 关闭态（取消由 `TurnSink` 表达），
  所以「退役运行时在引用归零后关闭」在这里等价于**移出退役列表并释放 `Arc`**。
  `RuntimeSnapshot.capabilities` 取 `descriptor.capabilities`（未声明时为默认值），
  `context_window_tokens` 仍是「描述值非零则用描述值，否则用能力里的窗口」。
- 对照：`tests/runtime_manager_parity.rs`，数据集
  `tests/fixtures/llm_runtime_manager_parity.json`（5 条轨迹：未初始化取回合、启动与占用、
  回合中拒绝/放行切换、持久化失败保留旧模型、窗口写回与关闭），
  生成器 `rust/tools/gen_llm_runtime_manager_fixture.py`。

## 依赖

`ureq`（阻塞式 HTTP/1.1 + rustls）是唯一的传输依赖：内核的回合循环本来就是阻塞的，不需要异步运行时。
交叉编译到 musl 时需要目标平台的 C 工具链（rustls 的 ring 组件）。

## 消息脱敏：序号注册表（`desensitization.rs`）、流式还原（`stream.rs`）、规则层（`rules.rs`）、匹配引擎（`engine.rs`）

对齐 Python `omnicrawl/llm/desensitization/`。原文只进内存注册表：**不落盘、不进日志、
不进会话事件**；可观测信息只到「计数 / 规则 ID / 序号」粒度。`desensitization.rs` 是模块根
（子系统错误面 + 序号注册表，对应 `registry.py`）。

- **占位符协议**：生成端唯一规范是全角 `｛Desensitized:n｝`（序号无前导零）；还原端宽松兼容半角花括号、
  全角/半角冒号、大小写与序号两侧空白。另有「疑似前缀」识别，用于把半截/畸形占位符保留原文并告警。
- **稳定序号**：值经进程级随机盐做 HMAC-SHA256 得到指纹，指纹 → 序号永久稳定——同一值在连续请求中
  复用同一序号，未变历史脱敏后逐字一致，从而命中提供方前缀缓存。只存指纹与整数序号，不存原文；
  新值分配会跳过「本请求已出现的占位符样式序号」。
- **周期与会话映射**：`PlaceholderCycle` 负责一次发送-接收周期的登记与还原；「序号 → 原文」映射按
  会话共享（`SessionSequenceCache`），周期结束只释放周期自身状态，会话切换才丢弃原文。
- **注册表**：`SequenceRegistry` 管理周期生命周期——紧邻的同一请求重试会复用同一周期，
  请求一旦换新就地注销（避免原文与屏蔽副本随失败/取消无限累积）。
- **流式还原**（`StreamRestorer`）：文本 / 推理 / 各工具参数通道各有独立缓冲；只挂起「可能是占位符
  前缀」的尾串（≤64 字符，超过就按普通文本输出），其余照常输出，分片把占位符切在中间也能正确还原。
  未注册序号保留原样 + 告警（`strict` 时中止），畸形前缀同样保留 + 告警，告警按类别各报一次；
  `restore_arguments` 递归还原参数里的字符串值（键与结构件不动），`reply_usable` 按
  `TRUNCATED_FINISH_REASONS`（截断类 finish_reason 不按成功注销周期）判定回复是否可用。
- **值类型规则层**（`PatternRule` / `scan_pattern_rules`）：识别「形态确定、随机性低」的敏感值，
  11 条内置规则全部落地——**PEM 私钥（完整块 / 无 END 的截断正文）、数据库连接串（URI / ADO 键值）、
  网址、邮箱、银行卡（Luhn）、MAC 地址、大陆车牌、内外网 IP**，以及整套规则语义——关键字预过滤、
  熵下限（`shannon_entropy_bits`，求和顺序对齐 Python `Counter`，浮点逐位可比）、校验器、值级
  豁免表、停用词、尾部标点留在原文、重叠区间先命中先占位、结果按起点稳定排序。
  本层每条规则的正则等价物都是手写匹配器（不引入运行时依赖），含环视、交替分支各自的
  尾部环视、贪婪量词与回溯（PEM 头尾的类字符段、域名「尽量多标签再退让」、本地部分 64 字符上界
  叠加左侧环视、ADO 键值里的惰性扫描）；IP 的私有 / 链路本地 / 公网判定按 Python `ipaddress`
  的常量表逐条对齐（IPv4 14 条私有网段 + 共享段 100.64/10，IPv6 10 条私有网段 + 链路本地 + 组播）。

- **匹配引擎**（`MaskContext` / `mask_text` / `mask_structured_value`）：三层顺序与设计稿一致——
  结构层（行首 `.env` 赋值含 `export` 与前缀空白、`key: value`、JSON 字符串值对，均为手写扫描器；
  已解析结构体按敏感键递归，敏感键之下的字符串叶子整体替换）→ 值类型规则层 → 熵兜底
  （长度下限 + 单类开关 + 形态白名单 + 字符类混合 + 香农熵；白名单含 UUID / 十六进制 / 前缀哈希 /
  版本号 / 时间戳 / 路径 / 代码片段 / 命名链 / 词形标识符）。命中值统一走 `placeholder_for`
  分配占位符，空串 / 已脱敏（`***`）/ 整串恰为占位符的值按跳过规则处理并计数。
  未搬：NER 语义兜底层（Python 侧可选依赖 torch + 5MB 权重，默认关闭）。

- **编排件与一次性脱敏器**（`middleware.rs` / `oneshot.rs`）：逐消息屏蔽（文本块 / 工具调用参数 /
  工具结果 / 思考内容；system 文本、工具声明与图片块豁免）、碰撞扫描的文本收集、工具参数里
  「已分配但无法还原」序号的 fail-closed 检查、事件逐条还原（`map_event`），以及单次
  「屏蔽 → 还原 → 注销」的 `OneShotMasker`。
  **parity 已转正**（`tests/desensitization_middleware_parity.rs`，3 例全绿）。核对抓出三处：
  工具参数的文本收集此前用紧凑 JSON，Python 是 `json.dumps(..., ensure_ascii=False)`（分隔符带空格、
  键按插入序）；规则集合必须**跟着数据走**——Python 侧由 `build_enabled_rules(config)` 按
  `[desensitization]` 的 `detect_*` 开关裁剪（邮箱 / 外网 IP / 网址默认关闭），内核侧收同一组类别，
  否则邮箱会被多屏蔽一次；泄露检查必须在**独立注册表**上生成，否则期望值的序号依赖前六条消息的
  处理顺序，无法独立重放。
  **运行时装饰器已落地**（`desensitization/live.rs`）：`DesensitizationRuntime` 包住任意
  `ModelRuntime`——出站请求先屏蔽再发、入站事件逐条还原（工具参数照旧做 fail-closed 检查），
  `maybe_wrap` 在未启用时零成本原样返回；最外层归并结果由还原后的事件重新聚合，因此调用方拿到的是
  还原态回复。验收见 `tests/desensitization_runtime.rs`（4 例：屏蔽与还原、未启用透传、
  未注册序号保留 + 告警、工具参数还原）。
  两处实现差异（语义等价）：**不复用**上一周期的掩码请求（总是重新脱敏——稳定序号索引保证同值同号，
  代价只是重复扫描）；计数分两层（周期计数在注册表里，屏蔽 / 还原计数在装饰器里）。
  逐消息屏蔽缓存与屏蔽计划缓存都已接线：历史里逐字未变的消息走 `MessageMaskMemo`（命中即复用，
  并校验引用的序号在本周期可还原），文本重复出现时再走 `MaskPlanCache`（按指纹重放匹配计划）。

gitleaks 规则表已落地（`desensitization/gitleaks.rs`）：内嵌上游快照 `data/gitleaks.toml`
（与 Python 侧同名文件逐字节一致，由 `tests/gitleaks_parity.rs` 的 sha256 断言看住），按 Python 语义解析
`keywords` / `entropy` / `secretGroup` / 规则级与全局 allowlist，并支持自定义 `gitleaks.toml` 按 id 覆盖 / 追加。
「内核不引入 `regex`」的决定**已由用户取消**：本层用 `regex` crate 编译上游模式，两处实现差异都收在
编译期适配里（`\Z` → `\z`；非量词的 `{` / `}` 转义），且适配只用于编译不过的模式——能原样编译的一律不动。
出厂配置里 `gitleaks_enabled = true`，内核侧 `DesensitizationOptions.gitleaks_enabled` 默认仍为关，
等配置层搬完再对齐出厂默认。
未接线：`locality` 局部化扫描（Python 侧重正则的性能优化，内核的手写匹配器没有整段回溯的开销、
结果与全量扫描一致）、`oneshot` 旁路尚未接 gitleaks、NER 语义兜底层（`ner.rs` 前向与区间过滤
已落地并对照，但还没接进 `mask_text` 的末尾——接线的前提是配置域给出模型路径与静默降级开关）。

三份数据集的占位符与号牌一类「占位符形状」的字面量一律**拼接构造**（`BRACE_OPEN + MARKER + ":" + str(seq) + BRACE_CLOSE`）：
本仓库自己就是宿主，在启用了消息脱敏的会话里写这类完整字面量会被还原成会话注册表里的原文——数据集照旧生成、
测试照常通过，但解析用例全变成「命中为空」的假绿。规则语料还自带每条文本的**期望命中**，
生成器当场断言：语料被写错、被豁免表静默吃掉或优先级不符时立即失败。

## NER 兜底层（`desensitization/ner.rs` + `ner_weights.rs`）

对齐 Python `omnicrawl/llm/desensitization/ner.py`：BiLSTM-CRF 语义兜底（PER / ORG / LOC）。
内核侧是**完整实现**，不是接口占位：

- **纯逻辑**：句末标点切句 + 超长硬切（`split_units` / `iter_chunks`）、中文片段等长隔离、
  BIO 解码、实体过滤（类型 / 最小长度 / 中文区间 / 占位符重叠）、块级 LRU 结果缓存、
  按 token 预算分批、共享抽取器池、`build_ner_layer` 的静默降级。
- **真实推理**：`ner_weights.rs` 从 `data/ner_bilstm_crf.bin` 加载权重（Embedding → BiLSTM
  1 层双向 → Linear → 带约束的 CRF，Viterbi 解码）。
- **权重来源**：`rust/tools/gen_ner_fixture.py` 把 Python checkpoint（zip + pickle 的 torch
  存档）转成「魔数 + 头部 JSON + f32 数据块」的自描述二进制；内核不实现 pickle 解析。
- **对照**：`tests/ner_parity.rs`（6 项）——纯逻辑逐例对照，外加 **10 条文本的端到端实体区间
  与 Python/torch 逐位一致**。
- **与 Python 的差异**：设备只有 CPU 实现（`auto` / `cuda` 一律回 CPU，Python 侧优先 CUDA）；
  `predict` 逐条前向（Python 按 token 预算打包批处理，结果等价）；`extract_entities` 显式接收
  文本长度参数（Python 收原文本）。

## 掩码记忆、计划缓存与扫描缓存（性能层）

- `middleware.rs` 的 `MessageMaskMemo`：逐消息屏蔽结果缓存 + 本轮新增 (序号, 原文) 对；
  命中后按引用的序号校验本周期可还原（否则退回重扫并重新登记）。淘汰口径是「最近用过的」
  而不是 LRU——与 Python 的 `_MessageMaskMemo` 一致（历史是每轮顺序全扫，LRU 会正好淘汰
  下一轮马上要用的条目）。运行时（`live.rs`）走 `mask_messages_cached`，与 Python 的
  `_mask_message_cached` 同一路径。
- `plan_cache.rs` 的 `MaskPlanCache`：按文本指纹（SHA-256）缓存**匹配计划**——
  只记「阶段 → 待替换区间 + 稳定序号」，不含原文；命中时按区间从当前文本重新取值并走标准
  `placeholder_for` 重新登记，因此还原 / 注销 / 并发周期隔离的语义与未命中路径完全一致。
  序号对不上（规则或稳定索引漂移）就按未命中重算，宁慢勿错：失效的那次在计数里从命中改记为
  未命中（`invalid`），条目仍留在缓存里。条目数与字节预算双上限，淘汰是真正的 LRU。
  阶段划分与 Python 一致：三步结构层各占一个无名阶段，规则层与熵兜底各带标签
  （`rules` / `entropy` / `ner`，命中重放时按标签补对应层计数）。
- `rules.rs` 的 `ScanCache`：按 (文本, 规则集合身份) 记忆扫描结果，按字节预算淘汰；
  `locality` 局部化扫描是 Python 侧重正则的性能优化，内核的手写匹配器没有整段回溯的开销、
  结果与全量扫描一致。
- Python 侧的模块级单例在内核里都是**实例**（由调用方持有），便于测试与配置变更后失效。
  计划缓存与逐消息缓存都由 `DesensitizationRuntime` 持有，`close()` 时两处一起清空（计数保留）。

计划缓存的对照数据集由 `gen_desensitization_plan_cache_fixture.py` 生成。注意该脚本
**按文件路径**加载 `plan_cache.py`：Python 侧的 `engine.py` / `middleware.py` 当前带着未合并的
冲突标记，走包导入会直接 `SyntaxError`；模块本身只依赖标准库，按路径加载不受影响。
合并收口后可以把加载方式换回包导入（模块级行为不变），引擎重放路径的对照用例也可随那一批补。
