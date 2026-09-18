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
python rust/tools/gen_desensitization_fixture.py         # 消息脱敏：占位符协议与序号注册表
python rust/tools/gen_desensitization_stream_fixture.py  # 消息脱敏：流式还原
python rust/tools/gen_desensitization_rules_fixture.py   # 消息脱敏：值类型规则层
cd rust && cargo test -p omnicrawl-llm
```

fixture：

- `tests/fixtures/desensitization_parity.json`：占位符用例 11 条、稳定索引 5 步、周期 / 会话缓存 / 注册表生命周期。
- `tests/fixtures/desensitization_stream_parity.json`：13 个场景、71 步操作（分片还原、通道隔离、结构化还原、告警、严格模式、截断判定）。
- `tests/fixtures/desensitization_rules_parity.json`：42 条语料（逐条候选 40、扫描命中 38）、熵 10 例、Luhn 13 例、邮箱豁免 10 例。
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

## 依赖

`ureq`（阻塞式 HTTP/1.1 + rustls）是唯一的传输依赖：内核的回合循环本来就是阻塞的，不需要异步运行时。
交叉编译到 musl 时需要目标平台的 C 工具链（rustls 的 ring 组件）。

## 消息脱敏：序号注册表（`desensitization.rs`）、流式还原（`desensitization/stream.rs`）、值类型规则层（`desensitization/rules.rs`）

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
  已搬**网址、邮箱、银行卡（Luhn）、MAC 地址、大陆车牌**五类，以及整套规则语义——关键字预过滤、
  熵下限（`shannon_entropy_bits`，求和顺序对齐 Python `Counter`，浮点逐位可比）、校验器、值级
  豁免表、停用词、尾部标点留在原文、重叠区间先命中先占位、结果按起点稳定排序。
  内核**不引入正则依赖**（项目决定）：每条规则的正则等价物都是手写匹配器，含环视、交替分支各自的
  尾部环视、以及贪婪量词与回溯（域名「尽量多标签再退让」、本地部分 64 字符上界叠加左侧环视）。

尚未搬运：规则层的 PEM 私钥 / 数据库连接串 / 内外网 IP（多行懒匹配与 `ipaddress` 分类表）、
gitleaks 规则表、`locality` 局部化扫描与扫描结果缓存（纯性能优化，不影响语义）、
引擎（键名 / 结构 / 熵兜底 / NER 兜底与优先级编排）、NER、middleware、oneshot。

三份数据集的占位符与号牌一类「占位符形状」的字面量一律**拼接构造**（`BRACE_OPEN + MARKER + ":" + str(seq) + BRACE_CLOSE`）：
本仓库自己就是宿主，在启用了消息脱敏的会话里写这类完整字面量会被还原成会话注册表里的原文——数据集照旧生成、
测试照常通过，但解析用例全变成「命中为空」的假绿。规则语料还自带每条文本的**期望命中**，
生成器当场断言：语料被写错、被豁免表静默吃掉或优先级不符时立即失败。
