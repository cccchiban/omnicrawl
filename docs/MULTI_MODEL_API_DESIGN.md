# OmniCrawl 多模型 API 接入方案设计

> 状态：**已落地主链路（2026-07-13）**；Claude/Gemini 真机联调与部分契约测试仍可继续补强
>
> 适用范围：OmniCrawl TUI、Agent 模型调用层、本地 HTTP API、运行配置与模型目录
>
> 目标协议：OpenAI Responses、OpenAI Chat Completions、Anthropic Claude Messages、Google Gemini Generate Content
>
> SDK 原则：每类接口必须使用对应厂商的 Python 原生 SDK，不通过 OpenAI 兼容层模拟 Claude 或 Gemini。
>
> 实现入口：`omnicrawl/llm/`、`omnicrawl/config/{runtime,llm,llm_multi,model_store,model_catalog,migration}.py`、`omnicrawl/ui/fullscreen/model_picker.py`、`config.example.yaml`、`models.example.yaml`

## 0. 实施状态（相对本设计稿）

| 阶段 | 状态 | 说明 |
|---|---|---|
| 0 锁定当前行为 | ✅ | 保留旧 `/models` 字段、`config.json` 兼容与 OpenAI Chat 回归测试 |
| 1 统一协议 + OpenAI Chat Adapter | ✅ | `omnicrawl/llm/protocol.py` + `providers/openai_chat.py`；Agent 经 `ModelRuntimeManager` |
| 2 YAML / models.yaml / 迁移 | ✅ | 优先 `config.yaml`；仅有 `config.json` 时可迁移并备份 `*.migrated.bak` |
| 3 Runtime Manager 热切换 | ✅ | 不可变 snapshot；失败保留旧模型；回合边界切换 |
| 4 OpenAI Responses Adapter | ✅（代码） | `providers/openai_responses.py` 已接入注册表；Agent 默认仍以 Chat 为主路径 |
| 5 Claude Adapter | ✅（代码） | `providers/anthropic.py` 原生 Anthropic Messages；需安装 `anthropic` 后联调 |
| 6 Gemini Adapter | ✅（代码） | `providers/gemini.py` 原生 `google-genai`；需安装后联调 |
| 7 双列 Model Picker + API catalog | ✅ | TUI `ModelPickerScreen`；`GET /models/catalog`、`POST /models/refresh` |
| 8 文档与兼容清理 | ⏳ | 本文档与 README/API 已同步；旧 JSON 兼容至少再保留一个发布周期 |

### 已实现行为摘要

- `/model` / `/models`：全屏 TUI 打开双列选择器；非 TUI 输出双列文本列表。TUI 会定位当前模型，并用跟随选中项的可视窗口展示长列表，方向键可访问发现结果中的全部模型。
- `/model --refresh`：刷新发现缓存后打开/列出。模型发现只缓存成功结果；连接失败、超时或网关暂时不可用不会进入 300 秒缓存，下一次打开目录会自动重试。
- `/model <key|alias|model_id|profile/model_id>`：直接切换并写回配置。
- 配置：`config.yaml` + `models.yaml`；`AI_CONFIG_FILE` / `AI_MODELS_FILE` 可覆盖路径。
- 环境覆盖：`OMNICRAWL_MODEL`、`OMNICRAWL_PROFILE`；兼容 `OPENAI_MODEL`。
- 依赖：`openai`、`PyYAML`、`anthropic`、`google-genai`（见 `requirements.txt` / `pyproject.toml`）。

### 仍建议补强

- Claude / Gemini 真机流式工具调用联调与更完整 mock 契约测试。
- OpenAI Responses 作为 Agent 主路径的端到端回归。
- 窄终端上下分区 CSS 细化、Picker 交互单测。
- 兼容层弃用周期到期后清理旧 JSON 专用路径。

## 1. 结论

本次改造应在现有 `LocalToolAgent` 与各厂商 SDK 之间新增一层**统一模型运行时协议**，将模型请求、流式文本、思考内容、工具调用、工具结果、Token 用量和错误转换为 Provider 无关的数据结构。

推荐方案如下：

1. OpenAI 拆为两个独立协议适配器：
   - `openai_responses`：使用 OpenAI Python SDK 的 `responses` 接口。
   - `openai_chat_completions`：使用 OpenAI Python SDK 的 `chat.completions` 接口。
2. Claude 使用 Anthropic Python SDK 的 Messages 接口，不经过 OpenAI 兼容协议。
3. Gemini 使用 Google Gen AI Python SDK（`google-genai`）的 Generate Content 接口，不经过 OpenAI 兼容协议。
4. `LocalToolAgent` 保留 Agent 循环、工具执行、审批、Session、Memory、MCP 和工作区生命周期；Provider Adapter 只负责模型协议转换。
5. 新增 `ModelRuntimeManager`，通过不可变运行时快照完成 `/model` 热切换。切换只在回合边界生效，不修改正在运行的请求。
6. 运行配置由 `config.json` 迁移为 `config.yaml`；新增 `models.yaml` 管理用户自定义模型及模型元信息。
7. `/model` 打开 Textual 模型选择界面：左列显示 `models.yaml` 中的自定义模型，右列显示各 Provider API 自动发现的模型；窄终端自动降级为上下分区。
8. 自动发现结果只作为运行时目录和可用性信息，不自动覆盖 `models.yaml`。

## 2. 背景与现状

### 2.1 当前调用链

项目当前存在两条 OpenAI 调用路径：

| 路径 | 现有实现 | 用途 | 当前耦合 |
|---|---|---|---|
| OpenAI Responses | `omnicrawl/config/llm_client.py` 的 `OpenAIResponseLLM` | 普通问答与流式文本 | 直接读取 Responses 事件和 usage |
| OpenAI Chat Completions | `omnicrawl/agent/llm_protocol.py` 的 `AgentLLMProtocol` | Agent 流式输出与 Tool Calls | 固定读取 `choices[0].delta` 和 OpenAI `tool_calls` |

`LocalToolAgent` 当前通过以下路径创建和调用 OpenAI SDK：

```text
LocalToolAgent._request_agent_reply()
  -> LocalToolAgent._llm_protocol()
  -> AgentLLMProtocol.request_reply()
  -> client.chat.completions.create()
```

模型切换目前只修改 `self.config.llm.model`。SDK Client 固定为 OpenAI Client，配置也只有一组 `api_key/base_url/model`，因此无法表达多个 Provider、多个凭据 Profile 或同一个 OpenAI Provider 下的两种 API 协议。

### 2.2 当前模型目录与 TUI

当前模型发现位于 `omnicrawl/config/model_catalog.py`：

- 使用 `urllib.request` 请求当前 `llm.base_url/models`。
- 只兼容 OpenAI 风格的 `{ "data": [{ "id": "..." }] }`。
- `ModelOption` 只有 `id/name/provider`。
- Provider 主要通过模型名称前缀推测。
- `/model` 输出纯文本模型列表，`/model <模型ID>` 修改当前模型并写回 `config.json`。

现有 Textual 命令分派已把 `/model` 标记为慢命令，在 worker 中执行网络 I/O，并通过主线程刷新 HUD。该边界应保留。

### 2.3 当前配置边界

`omnicrawl/config/runtime.py` 当前固定读写 `config.json`，且审批、API、MCP、临时目录、插件等模块都复用该入口。因此 YAML 迁移必须是全局配置仓库迁移，不能只修改 LLM 模块。

## 3. 目标与非目标

### 3.1 目标

- 支持以下四种模型协议并保持流式输出、工具调用和 Token 遥测：
  - OpenAI Responses。
  - OpenAI Chat Completions。
  - Anthropic Claude Messages。
  - Google Gemini Generate Content。
- 每种协议使用官方原生 Python SDK。
- 允许配置多个 Provider Profile，例如官方服务、代理网关和不同账号。
- 在 TUI 中通过 `/model` 热切换模型，不重启进程，不清空当前会话。
- 模型列表双列展示：自定义模型与 API 自动发现模型相互独立。
- 将 `config.json` 迁移为 `config.yaml`。
- 新增 `models.yaml`，维护模型列表、别名、能力、上下文窗口和展示信息。
- 保持现有工具审批、Agent 工具并发、Session、MCP、插件 Hook 和 Textual 主线程更新规则。
- 保持本地 HTTP API 可扩展，并尽量兼容已有模型接口字段。

### 3.2 非目标

- 本阶段不实现多个模型同时参与同一回合的路由、投票或自动降级。
- 本阶段不在模型生成中途切换 Provider。
- 本阶段不提供云端配置同步或密钥托管。
- 本阶段不把 API 自动发现结果自动写入 `models.yaml`。
- 本阶段不统一暴露所有厂商的私有参数；只支持经过白名单校验的 Provider 参数。
- 本阶段不保证跨 Provider 迁移隐藏思考内容或厂商私有缓存签名。

## 4. 核心设计原则

### 4.1 Agent 与厂商协议解耦

`LocalToolAgent` 只处理统一的消息、工具和流事件，不读取任何 SDK 对象，也不拼装 Claude/Gemini/OpenAI 的原生消息。

### 4.2 OpenAI 两种 API 是两个适配器

虽然 Responses 和 Chat Completions 使用同一个 OpenAI Python SDK，但二者的：

- 请求结构；
- 流事件；
- 工具调用表达；
- 工具结果回传；
- reasoning 与 usage 位置；

均不完全相同。因此应共享 OpenAI Client 工厂和错误映射，但不能共享协议解析器。

### 4.3 热切换只发生在回合边界

一轮 Agent 可能经历多次“模型请求 → 工具调用 → 工具结果 → 再请求”。该完整循环必须绑定同一个运行时快照。只有整轮结束后，下一轮才能使用新模型。

### 4.4 配置与模型目录分离

- `config.yaml`：运行行为、Provider 连接、凭据来源、当前选择。
- `models.yaml`：用户维护的模型目录和模型元信息。
- 自动发现缓存：运行时或缓存目录数据，不作为用户配置真值。

### 4.5 部分 Provider 失败不影响其他 Provider

模型发现、客户端构建和诊断均按 Provider Profile 隔离。Claude 发现失败时，OpenAI、Gemini 和自定义 Claude 模型仍可展示。

## 5. 总体架构

```text
┌───────────────────────────────────────────────────────────────┐
│ TUI / HTTP API                                                │
│ /model、模型目录、当前模型、模型发现刷新                       │
└──────────────────────────────┬────────────────────────────────┘
                               │
┌──────────────────────────────▼────────────────────────────────┐
│ ModelCatalogService                                           │
│ custom(models.yaml) + discovered(native SDK) + current        │
└──────────────────────────────┬────────────────────────────────┘
                               │ resolve ModelDescriptor
┌──────────────────────────────▼────────────────────────────────┐
│ ModelRuntimeManager                                           │
│ active snapshot / switch lock / runtime cache / lifecycle     │
└──────────────────────────────┬────────────────────────────────┘
                               │ acquire snapshot per turn
┌──────────────────────────────▼────────────────────────────────┐
│ LocalToolAgent                                                │
│ Agent loop / tools / approval / session / memory / MCP        │
└──────────────────────────────┬────────────────────────────────┘
                               │ ModelTurnRequest
┌──────────────────────────────▼────────────────────────────────┐
│ ModelProviderAdapter                                          │
│ OpenAI Responses / OpenAI Chat / Anthropic / Gemini           │
└──────────────────────────────┬────────────────────────────────┘
                               │ native SDK
┌──────────────────────────────▼────────────────────────────────┐
│ Provider API                                                  │
└───────────────────────────────────────────────────────────────┘
```

## 6. 推荐模块划分

```text
omnicrawl/
├── llm/
│   ├── __init__.py
│   ├── protocol.py                 # Provider 无关类型与 Protocol
│   ├── runtime.py                  # ModelRuntimeManager 与运行时快照
│   ├── registry.py                 # Provider Adapter 注册与工厂
│   ├── errors.py                   # 统一错误分类与用户提示
│   ├── capabilities.py             # 能力定义和参数裁剪
│   ├── usage.py                    # Token usage 归一化
│   └── providers/
│       ├── openai_common.py        # OpenAI Client、错误和公共工具
│       ├── openai_responses.py     # Responses Adapter
│       ├── openai_chat.py          # Chat Completions Adapter
│       ├── anthropic.py            # Claude Messages Adapter
│       └── gemini.py               # Gemini Generate Content Adapter
├── config/
│   ├── runtime.py                  # config.yaml 安全读取与原子写回
│   ├── llm.py                      # LLM/Profile 配置模型
│   ├── model_store.py              # models.yaml 读取和校验
│   ├── model_catalog.py            # 自定义/发现/当前模型目录
│   └── migration.py                # config.json -> config.yaml
├── ui/fullscreen/
│   ├── model_picker.py             # 双列 ModelPickerScreen
│   └── ...
└── commands/slash.py               # /model 命令语义，不直接依赖 SDK
```

兼容期可保留：

- `omnicrawl/config/llm_client.py`：转为 OpenAI Responses Adapter 的兼容门面。
- `omnicrawl/agent/llm_protocol.py`：转为统一运行时的兼容门面，逐步移除 OpenAI 专用解析。
- 既有 `from omnicrawl.llm import ...` 导入路径：继续再导出公共类型。

## 7. 统一内部协议

### 7.1 模型身份

模型唯一身份不能只使用上游 `model_id`，因为同一个模型可能来自多个网关，或同时支持 Responses 与 Chat Completions。

```python
@dataclass(frozen=True)
class ModelIdentity:
    profile_id: str
    provider: str
    protocol: str
    model_id: str
    catalog_key: str = ""
```

推荐协议常量：

```text
openai_responses
openai_chat_completions
anthropic_messages
gemini_generate_content
```

### 7.2 模型能力

```python
@dataclass(frozen=True)
class ModelCapabilities:
    streaming: bool = True
    tools: bool = False
    parallel_tool_calls: bool = False
    reasoning: bool = False
    vision: bool = False
    model_discovery: bool = False
    prompt_cache: bool = False
    context_window_tokens: int = 0
    max_output_tokens: int = 0
```

能力信息来源优先级：

```text
models.yaml 用户显式配置
> Provider 自动发现可确认的信息
> Adapter 保守默认值
```

无法确认的能力必须使用保守值，不能依据模型名称猜测后静默开启工具或推理参数。

### 7.3 Provider 无关消息

```python
@dataclass(frozen=True)
class TextBlock:
    text: str

@dataclass(frozen=True)
class ToolCallBlock:
    call_id: str
    name: str
    arguments: dict[str, Any]
    provider_call_id: str = ""

@dataclass(frozen=True)
class ToolResultBlock:
    call_id: str
    ok: bool
    content: str

@dataclass(frozen=True)
class ConversationMessage:
    role: str  # user | assistant | tool
    blocks: tuple[TextBlock | ToolCallBlock | ToolResultBlock, ...]
```

`LocalToolAgent` 继续负责生成稳定的内部 `call_id`、执行工具、审批和排序。Adapter 只负责把内部块转换为厂商要求的原生结构。

### 7.4 统一请求

```python
@dataclass(frozen=True)
class ModelTurnRequest:
    identity: ModelIdentity
    system_prompt: str
    messages: tuple[ConversationMessage, ...]
    tools: tuple[ToolSpec, ...]
    generation_options: GenerationOptions
    prompt_cache_identity: Mapping[str, str]
```

### 7.5 统一流事件

Adapter 只向上层产生以下事件：

```text
TextDelta(text)
ReasoningDelta(text)
ToolCallStarted(call_id, name)
ToolCallArgumentsDelta(call_id, delta)
ToolCallCompleted(call_id, name, arguments)
UsageUpdated(input_tokens, output_tokens, cached_input_tokens, reasoning_tokens)
ResponseCompleted(finish_reason)
ProviderWarning(code, message)
```

公共聚合器将流事件归并为：

```python
@dataclass(frozen=True)
class ModelTurnResult:
    assistant_message: ConversationMessage
    content: str
    reasoning: str
    tool_calls: tuple[ToolCall, ...]
    usage: TokenUsage | None
    finish_reason: str
    content_streamed: bool
```

### 7.6 Provider 私有状态

部分厂商可能返回继续同一协议所需的私有块或签名。首版遵循以下规则：

- 可作为 `provider_state` 暂存在单轮运行时中。
- 只允许回传给同 Provider、同 Profile、同协议的下一次请求。
- 不进入通用文本历史，不跨 Provider 发送。
- 写入 Session 前必须脱敏；无法确认安全性时不持久化。
- 隐藏思考内容不作为跨 Provider 上下文的一部分。

## 8. Provider Adapter 设计

### 8.1 公共接口

```python
class ModelProviderAdapter(Protocol):
    provider_type: str

    def create_runtime(
        self,
        profile: ProviderProfile,
        model: ModelDescriptor,
    ) -> ModelRuntime:
        ...

    def discover_models(
        self,
        profile: ProviderProfile,
        *,
        timeout_seconds: float,
    ) -> DiscoveryResult:
        ...


class ModelRuntime(Protocol):
    identity: ModelIdentity
    capabilities: ModelCapabilities

    def stream_turn(
        self,
        request: ModelTurnRequest,
        *,
        cancel_check: Callable[[], None],
    ) -> Iterable[ModelStreamEvent]:
        ...

    def close(self) -> None:
        ...
```

### 8.2 OpenAI Responses Adapter

SDK：`openai`。

职责：

- 使用 OpenAI SDK `responses` 接口创建流式请求。
- 将统一 system prompt 映射为 Responses 的 instructions/system 输入。
- 将工具定义映射为 Responses function tools。
- 将统一历史映射为 Responses input items。
- 将 function call 和 function call output 映射为内部 ToolCall/ToolResult。
- 只把最终可见回答映射为 `TextDelta`，思考/摘要事件单独映射。
- 从最终响应或流事件中归一化 usage。
- 不复用 Chat Completions 的 `choices[0].delta` 解析器。

### 8.3 OpenAI Chat Completions Adapter

SDK：`openai`。

职责：

- 迁移现有 `AgentLLMProtocol` 的 `chat.completions.create()` 行为。
- 保留现有流式 `tool_calls` 参数分片聚合。
- 保留 `prompt_cache_key` 不支持时移除参数并重试一次的兼容策略，但只对明确支持的 OpenAI 模型/Profile 启用。
- `extra_body` 只能来自该 Provider Profile 的白名单配置，不能成为跨 Provider 公共参数。
- 将 assistant tool calls 和 role=tool 消息限制在 Adapter 内部。

### 8.4 Anthropic Claude Adapter

SDK：`anthropic`。

职责：

- 使用 Anthropic Messages 原生接口及流式能力。
- system prompt 使用 Anthropic 的独立 system 字段。
- 将工具定义转换为 Claude tools schema。
- 将 Claude `tool_use` 内容块转换为内部 ToolCall。
- 将内部 ToolResult 转换为 Claude `tool_result` 内容块。
- 将 text、thinking、usage 和 stop reason 转换为统一事件。
- 不构造 OpenAI `messages/tool_calls`，也不使用 OpenAI Client 指向 Anthropic 网关。

### 8.5 Google Gemini Adapter

SDK：Google Gen AI Python SDK（包名 `google-genai`）。

职责：

- 使用 Gemini Generate Content 原生接口及流式能力。
- 将 system prompt 转换为 system instruction。
- 将统一工具 schema 转换为 Gemini function declarations。
- 将 Gemini `functionCall` 转换为内部 ToolCall。
- 将内部 ToolResult 转换为 Gemini function response。
- 显式禁用 SDK 自动函数执行；所有函数调用必须回到 OmniCrawl Host，继续经过 schema 校验、工具审批、删除意图检测和串并行调度。
- 将 Content/Part、流式文本和 usage metadata 转换为统一事件。
- 不通过 OpenAI 兼容路径调用 Gemini。

### 8.6 SDK 参考入口

实施阶段应以对应官方 SDK 当前版本文档为准：

- OpenAI Python SDK：<https://github.com/openai/openai-python>
- Anthropic Python SDK：<https://github.com/anthropics/anthropic-sdk-python>
- Google Gen AI Python SDK：<https://github.com/googleapis/python-genai>

由于厂商 SDK 的流事件类名和模型发现能力可能随版本变化，实施前必须锁定兼容 Python `>=3.9` 的版本，并使用 SDK mock 契约测试固定项目实际依赖的对象形态。

## 9. 配置设计

### 9.1 文件职责

| 文件 | 是否包含密钥 | 职责 |
|---|---:|---|
| `config.yaml` | 可兼容，但推荐只引用环境变量 | Provider Profile、当前模型、审批、API、MCP、插件、临时目录等运行配置 |
| `models.yaml` | 否 | 用户自定义模型列表、别名、能力、上下文窗口和 UI 信息 |
| 自动发现缓存 | 否 | Provider API 返回的模型列表、刷新时间和诊断，不作为用户配置真值 |

建议同时提供：

- `config.example.yaml`：提交到仓库。
- `models.example.yaml`：提交到仓库。
- `config.yaml`、`models.yaml`：本地文件，加入 `.gitignore`。

### 9.2 `config.yaml` Schema

```yaml
version: 2

llm:
  active_model:
    source: custom
    key: gpt-main

  defaults:
    request_timeout_seconds: 180
    request_retry_count: 5
    discovery_timeout_seconds: 10
    discovery_cache_ttl_seconds: 300
    reasoning_effort: ""

  profiles:
    openai-main:
      provider: openai
      enabled: true
      base_url: https://api.openai.com/v1
      api_key_env: OPENAI_API_KEY
      default_protocol: openai_responses
      discovery:
        enabled: true

    openai-gateway:
      provider: openai
      enabled: true
      base_url: https://gateway.example/v1
      api_key_env: OPENAI_GATEWAY_API_KEY
      default_protocol: openai_chat_completions
      discovery:
        enabled: true

    anthropic-main:
      provider: anthropic
      enabled: true
      api_key_env: ANTHROPIC_API_KEY
      default_protocol: anthropic_messages
      discovery:
        enabled: true

    gemini-main:
      provider: gemini
      enabled: true
      api_key_env: GEMINI_API_KEY
      default_protocol: gemini_generate_content
      discovery:
        enabled: true

approval:
  mode: manual

api:
  bearer_token_env: OMNICRAWL_API_TOKEN
  host: 127.0.0.1
  port: 8765
  allowed_origins:
    - http://localhost:5173
  confirmation_timeout_seconds: 300

agent_temp:
  enabled: true
  directory: .agent_tmp
  cleanup_enabled: true
  cleanup_interval_hours: 24

plugins:
  enabled: false

mcp:
  enabled: false
```

### 9.3 Provider Profile 字段

| 字段 | 必填 | 说明 |
|---|---:|---|
| `provider` | 是 | `openai`、`anthropic`、`gemini` |
| `enabled` | 否 | 是否参与运行和模型发现，默认 true |
| `base_url` | 视 Provider 而定 | 官方默认地址可省略；代理网关必须显式设置 |
| `api_key_env` | 推荐 | 存放 API Key 的环境变量名 |
| `api_key` | 兼容 | 本地明文密钥，不推荐，不允许出现在 example 文件 |
| `default_protocol` | 是 | 自动发现模型被选中时使用的默认协议 |
| `discovery.enabled` | 否 | 是否自动请求模型列表 |
| `provider_options` | 否 | 经 Adapter 白名单校验的厂商私有参数 |

凭据优先级：

```text
统一临时环境覆盖
> api_key_env 指向的环境变量
> profile.api_key
> 缺失配置错误
```

建议新增统一覆盖变量：

```text
OMNICRAWL_MODEL
OMNICRAWL_PROFILE
```

旧 `OPENAI_MODEL` 只在兼容迁移期对默认 OpenAI Profile 生效，并在 TUI 显示弃用提示。

### 9.4 `models.yaml` Schema

```yaml
version: 1

models:
  gpt-main:
    display_name: GPT Main
    profile: openai-main
    model_id: gpt-example
    protocol: openai_responses
    enabled: true
    aliases:
      - gpt
    description: 日常 Agent 默认模型
    tags:
      - tools
      - general
    context_window_tokens: 128000
    max_output_tokens: 16384
    capabilities:
      streaming: true
      tools: true
      parallel_tool_calls: true
      reasoning: true
      vision: false
    provider_options: {}
    sort_order: 10

  gateway-chat:
    display_name: Gateway Chat Model
    profile: openai-gateway
    model_id: upstream-model-id
    protocol: openai_chat_completions
    enabled: true
    context_window_tokens: 128000
    capabilities:
      streaming: true
      tools: true

  claude-main:
    display_name: Claude Main
    profile: anthropic-main
    model_id: claude-example
    protocol: anthropic_messages
    enabled: true
    context_window_tokens: 200000
    capabilities:
      streaming: true
      tools: true
      reasoning: true

  gemini-main:
    display_name: Gemini Main
    profile: gemini-main
    model_id: gemini-example
    protocol: gemini_generate_content
    enabled: true
    context_window_tokens: 1000000
    capabilities:
      streaming: true
      tools: true
      vision: true
```

约束：

- 模型 key 必须唯一，建议使用小写字母、数字、`-` 和 `_`，禁止 `/`，以便与发现模型引用区分。
- `profile` 必须引用 `config.yaml` 中已启用的 Profile。
- `protocol` 必须与 Profile 的 Provider 匹配。
- `models.yaml` 不允许出现 `api_key`、Token、Cookie 等凭据字段。
- `context_window_tokens` 必须为正整数，是自定义模型 TUI `CTX` 遥测的权威配置。
- 自动发现模型若无法从 SDK 获得可靠上下文窗口，按“发现项元信息 → Profile 的 `default_context_window_tokens` → 未知”降级；未知时 HUD 显示 `CTX --`，不得猜测或沿用上一模型的上限。
- `provider_options` 必须由对应 Adapter 严格校验，不允许任意透传。
- aliases 全局不能冲突；冲突时启动诊断应明确列出模型 key。

### 9.5 当前模型持久化

自定义模型：

```yaml
llm:
  active_model:
    source: custom
    key: gpt-main
```

自动发现模型：

```yaml
llm:
  active_model:
    source: detected
    profile: openai-main
    model_id: discovered-model-id
    protocol: openai_responses
```

自动发现模型必须把 `profile/model_id/protocol` 一并持久化，保证重启时不依赖再次成功发现模型才能启动。

## 10. 模型发现与目录合并

### 10.1 原生 SDK 发现

| Provider | 发现方式 | 失败策略 |
|---|---|---|
| OpenAI | OpenAI SDK 的模型列表接口；兼容网关也通过 OpenAI SDK Client 调用 | 记录 Profile 诊断，自定义模型仍可用 |
| Anthropic | Anthropic SDK 当前版本提供的模型列表接口；若账号、服务或版本不支持则标记“不支持发现” | 不视为全局失败，只显示自定义 Claude 模型 |
| Gemini | Google Gen AI SDK 的模型列表接口 | 记录 Profile 诊断，自定义模型仍可用 |

要求：

- 不再使用 `urllib.request` 直接拼装所有 Provider 的模型接口。
- 每个 Provider 负责分页、超时、错误映射和响应字段转换。
- 每个 Profile 最多保留设定数量的模型，默认建议 500，避免异常响应拖垮 TUI。
- 模型发现结果缓存默认 300 秒；`/model` 优先显示缓存并后台刷新。
- 手动刷新必须允许取消。
- 发现过程不发送生成请求，避免产生推理费用。

### 10.2 目录项结构

```python
@dataclass(frozen=True)
class CatalogModel:
    source: str               # custom | detected | current_missing
    key: str
    profile_id: str
    provider: str
    protocol: str
    model_id: str
    display_name: str
    capabilities: ModelCapabilities
    context_window_tokens: int
    availability: str         # available | unavailable | unknown
    matched_custom_key: str = ""
    diagnostic: str = ""
```

### 10.3 双列数据规则

左列“自定义模型”：

- 只来自 `models.yaml`。
- 保持用户排序：`sort_order`，其次按 display name。
- 若 API 发现到相同 `profile/model_id/protocol`，显示 `已检测` 标记。
- 即使未发现，也保留并允许选择；可用性显示为“未验证”而不是直接判定不可用。

右列“自动检测”：

- 只来自 Provider API 发现结果。
- 显示 Profile、Provider、模型 ID 和能力摘要。
- 若已匹配左列自定义模型，显示对应自定义 key，但仍保留在右列以满足来源分栏。
- 不自动写入 `models.yaml`。

当前模型若两列都不存在：

- 生成 `current_missing` 项。
- 在列表顶部显示警告。
- 允许用户切换到其他模型，但不静默修改当前配置。

### 10.4 去重键

匹配自定义项与发现项时使用：

```text
(profile_id, protocol, model_id)
```

不能仅按 `model_id` 去重。

## 11. TUI `/model` 设计

### 11.1 命令语义

| 命令 | 行为 |
|---|---|
| `/model` | 打开模型选择界面 |
| `/models` | `/model` 的兼容别名 |
| `/model <自定义key或alias>` | 直接切换自定义模型 |
| `/model <profile>/<model_id>` | 直接选择自动发现模型，协议使用 Profile 默认协议 |
| `/model --refresh` | 刷新所有启用 Profile 的自动发现结果后打开界面 |

模型 ID 中即使包含 `/`，解析时也只分割第一个 `/`，前半部分作为 Profile，余下部分原样作为模型 ID。

### 11.2 双列界面

宽终端示意：

```text
┌─ 模型切换 ───────────────────────────────────────────────────────────┐
│ 搜索: gpt_                                                          │
│                                                                      │
│ 自定义模型                         自动检测                           │
│ ─────────────────────────────     ───────────────────────────────    │
│ ● GPT Main                         ✓ gpt-example                     │
│   openai-main · Responses            openai-main · OpenAI            │
│   128K · tools · reasoning           custom: gpt-main                │
│                                                                      │
│   Claude Main                      ! claude-example                  │
│   anthropic-main · Messages          anthropic-main · Anthropic      │
│                                                                      │
│ ↑↓ 选择  ←→ 切换列  Enter 切换  R 刷新  Esc 取消                    │
└──────────────────────────────────────────────────────────────────────┘
```

交互规则：

- 左右键切换列，上下键移动选择。
- 输入字符进行模糊过滤，匹配 key、别名、display name、model ID、Provider 和 tag。
- Enter 切换模型。
- `R` 刷新自动发现列表。
- Esc 关闭，不改变当前模型。
- 当前模型使用 `●` 和文本共同标识，不只依赖颜色。
- 发现失败显示 Provider 级诊断，不弹出阻断全局使用的错误框。
- 模型切换中锁定输入，避免重复提交。

### 11.3 窄屏降级

当可用宽度不足以保证两列最小宽度时，界面切换为：

```text
自定义模型
...

自动检测
...
```

仍保留来源分区，不通过强制截断实现“双列”。

### 11.4 Textual 边界

新增 `ModelPickerScreen`，遵循现有稳定性规则：

- SDK 发现和运行时构建在 worker 中执行。
- worker 不直接修改 Widget。
- UI 更新通过 Textual 主线程完成。
- `/model` 继续作为慢命令或专用异步 Screen 生命周期执行。
- 切换成功后同时刷新 HUD 的 `MDL`、`THK` 和 `CTX`。
- 切换后 Token 最近值清零，避免旧模型用量与新模型上下文上限混合显示。

## 12. 热切换设计

### 12.1 不可变运行时快照

```python
@dataclass(frozen=True)
class RuntimeSnapshot:
    generation: int
    descriptor: ModelDescriptor
    runtime: ModelRuntime
    capabilities: ModelCapabilities
    context_window_tokens: int
```

`ModelRuntimeManager`：

```text
active_snapshot
switch_lock
active_turn_count / snapshot 引用计数
retired_snapshots
```

### 12.2 回合绑定

每轮 Agent 开始时只获取一次快照：

```python
snapshot = runtime_manager.acquire_turn()
try:
    run_agent_turn(snapshot.runtime)
finally:
    runtime_manager.release_turn(snapshot)
```

该轮后续所有工具调用后的模型请求继续使用相同 snapshot，不再次读取全局当前模型。

### 12.3 切换状态机

```text
IDLE
  -> RESOLVING       解析 custom key / alias / detected ref
  -> BUILDING        校验 Profile、凭据、协议和能力，创建候选 runtime
  -> VALIDATING      只做本地和目录校验，不发送收费生成请求
  -> PERSISTING      原子写入 config.yaml
  -> COMMITTING      switch_lock 下替换 active snapshot
  -> REFRESHING_UI   更新 HUD、列表和 Token 遥测
  -> RETIRING_OLD    旧 snapshot 无引用后 close
```

### 12.4 原子性与回滚

- 候选模型解析或 Client 创建失败：旧模型不变。
- `config.yaml` 写入失败：旧模型不变，候选 runtime 关闭。
- 持久化成功后再替换内存 snapshot。
- 内存替换动作只包含不可失败的引用交换和 generation 增加。
- 旧 runtime 不在切换瞬间强制关闭；必须等待持有它的回合结束。

### 12.5 并发边界

TUI 首版：

- 生成期间输入锁定，因此不允许回合中提交 `/model`。
- 模型选择与发现期间也锁定输入。
- 切换完成后，下一轮使用新模型。

本地 HTTP API：

- 当前服务全局只允许一个生成任务，模型切换继续使用 `ensure_mutation_allowed()`。
- 若未来允许并发 run，则已开始的 run 持有旧 snapshot，新 run 使用新 snapshot。
- SSE 的 `run.started` 和 `usage.updated` 应携带实际 `model_ref` 与 `generation`。

### 12.6 会话连续性

切换模型时：

| 状态 | 处理 |
|---|---|
| 用户/助手可见历史 | 保留 |
| Session ID、Memory、MCP、工具列表 | 保留 |
| 已完成工具事件 | 保留为会话审计，不跨 Provider 伪造原生消息 |
| 当前未完成工具调用 | 不允许切换 |
| Provider 私有思考/签名 | 跨 Provider 不发送 |
| SDK Client | 按目标 Profile/协议创建或从安全缓存获取 |
| prompt cache identity | 按 Profile、协议、模型重新计算 |
| context window | 切换为目标模型配置 |
| reasoning 设置 | 按目标能力映射；不支持时提示并禁用或要求用户确认 |

## 13. 工具调用适配

### 13.1 统一 ToolSpec

现有 `ToolDefinition` 可继续作为来源，但发送前转换为统一 JSON Schema：

```python
@dataclass(frozen=True)
class ToolSpec:
    name: str
    description: str
    parameters: dict[str, Any]
```

继续复用当前稳定的工具函数名映射，防止包含 `.`、`-` 等字符的 MCP Tool 名称违反厂商限制。

### 13.2 各协议映射

| 内部结构 | OpenAI Responses | OpenAI Chat | Claude | Gemini |
|---|---|---|---|---|
| ToolSpec | function tool | function tool | tools schema | function declaration |
| ToolCall | function call item | assistant tool_calls | tool_use block | functionCall part |
| ToolResult | function call output | role=tool | tool_result block | function response part |

### 13.3 并行工具调用

- Provider 返回多个工具调用时，继续使用现有“只读工具并行，写文件/删除串行屏障”策略。
- `parallel_tool_calls` 只描述模型是否可能一次返回多个调用，不改变 Host 的安全执行策略。
- 工具执行结果必须按模型原始调用顺序回传。
- Provider Adapter 不得绕过现有审批和删除意图检测，也不得使用 SDK 的自动工具/函数执行能力直接调用本地 Python 函数。

## 14. 推理参数与 Provider 私有参数

### 14.1 公共参数

首版公共参数建议只保留：

- `max_output_tokens`。
- `temperature`（目标模型支持时）。
- `reasoning_effort`（作为抽象意图，不直接透传）。
- 请求超时和重试次数。

### 14.2 映射规则

- Adapter 根据模型 capability 决定是否支持公共参数。
- 不支持的可选参数应忽略并产生一次清晰 warning。
- 工具调用、流式输出等必要能力缺失时应阻止请求。
- `thinking: {type: ...}`、Claude thinking 配置和 Gemini thinking 配置不得相互复用。

### 14.3 `provider_options`

Provider 私有参数允许配置，但必须：

- 按 Provider 分 schema 校验。
- 禁止凭据、URL、Header 等敏感传输字段从模型项注入。
- 禁止覆盖 `model`、messages、tools、stream、timeout 等 Host 权威字段。
- 错误时在启动或切换阶段失败，不在请求时静默忽略拼写错误。

## 15. 错误、重试与取消

### 15.1 统一错误分类

```text
AUTHENTICATION_FAILED
PERMISSION_DENIED
MODEL_NOT_FOUND
RATE_LIMITED
REQUEST_TIMEOUT
CONNECTION_FAILED
SERVICE_UNAVAILABLE
INVALID_REQUEST
UNSUPPORTED_CAPABILITY
STREAM_INTERRUPTED
MODEL_DISCOVERY_FAILED
CONFIGURATION_ERROR
```

Adapter 将 SDK 异常映射到统一错误，再由 TUI/API 转换为中文提示和稳定错误码。

### 15.2 重试原则

- 只重试连接错误、明确可重试的超时、限流和临时服务错误。
- 尊重 SDK/响应中的 Retry-After 信息。
- 一旦已经向 UI 输出可见文本或完整 ToolCall，默认不自动从头重试，避免重复文本或重复工具调用。
- `prompt_cache_key` 参数不支持时的无副作用回退只适用于请求尚未建立的 OpenAI Chat 场景。
- 认证、权限、模型不存在、参数错误和能力不支持不重试。
- 用户取消立即终止，不进入重试。

### 15.3 流中断

流已产生部分内容后中断时：

- 终止当前回合并标记 `STREAM_INTERRUPTED`。
- 不执行未完成解析的 ToolCall。
- 已完成并已执行的 ToolCall 保持审计记录，不自动再次调用。
- UI 明确说明已收到部分响应，用户可重新发送或继续。

## 16. 本地 HTTP API 调整

### 16.1 兼容原则

现有 `/api/v1/models` 返回扁平列表。为避免直接破坏潜在客户端，建议：

- 保留 `GET /api/v1/models`，继续返回扁平模型项，并保留 `id/name/provider` 字段。
- 新增 `GET /api/v1/models/catalog`，返回双列数据和诊断。
- 新增 `POST /api/v1/models/refresh`，刷新自动发现缓存。
- 扩展 `PUT /api/v1/models/current`，支持 canonical model selection，同时兼容旧 `{ "model": "..." }`。

### 16.2 Catalog 响应示例

```json
{
  "data": {
    "current": {
      "source": "custom",
      "key": "gpt-main",
      "profile": "openai-main",
      "protocol": "openai_responses",
      "model_id": "gpt-example"
    },
    "custom": [],
    "detected": [],
    "diagnostics": [
      {
        "profile": "anthropic-main",
        "status": "unavailable",
        "message": "模型列表发现失败，自定义模型仍可使用。"
      }
    ]
  }
}
```

### 16.3 切换请求

新请求：

```json
{
  "source": "custom",
  "key": "gpt-main"
}
```

或：

```json
{
  "source": "detected",
  "profile": "openai-main",
  "model_id": "gpt-example",
  "protocol": "openai_responses"
}
```

服务必须继续在活动 run 存在时返回 409。兼容旧 `{ "model": "..." }` 时，服务按“自定义 key 精确匹配 → alias 唯一匹配 → 当前 Profile 内 model ID 唯一匹配”解析；存在多个候选时返回 409 并列出候选，不得静默选第一个。

## 17. YAML 迁移方案

### 17.1 目标文件

- 默认运行配置：`config.yaml`。
- 默认模型目录：`models.yaml`。
- `AI_CONFIG_FILE` 继续支持显式指定路径；目标实现应根据扩展名解析 `.yaml/.yml`。
- 可新增 `AI_MODELS_FILE` 指定模型目录路径。

### 17.2 启动解析顺序

```text
显式 config_path
> AI_CONFIG_FILE
> 项目根目录 config.yaml
> 兼容迁移项目根目录 config.json
> 空配置/环境变量
```

### 17.3 自动迁移条件

默认路径下：

1. `config.yaml` 存在：以 YAML 为权威，不读取 JSON。
2. YAML 不存在且 `config.json` 存在：执行一次迁移。
3. 两者都不存在：提示复制 `config.example.yaml`。
4. YAML 与 JSON 同时存在：使用 YAML，并输出一次“旧 JSON 已忽略”警告。
5. `AI_CONFIG_FILE` 指向外部 JSON 时：兼容读取，但不擅自迁移外部文件；由用户显式指定新路径。

### 17.4 旧 LLM 配置映射

旧配置：

```json
{
  "llm": {
    "api_key": "...",
    "base_url": "https://example/v1",
    "model": "old-model",
    "context_window_tokens": 128000
  }
}
```

迁移为：

```yaml
llm:
  active_model:
    source: custom
    key: migrated-default
  profiles:
    migrated-openai:
      provider: openai
      base_url: https://example/v1
      api_key_env: OPENAI_API_KEY
      default_protocol: openai_chat_completions
      discovery:
        enabled: true
```

并创建：

```yaml
# models.yaml
models:
  migrated-default:
    display_name: old-model
    profile: migrated-openai
    model_id: old-model
    protocol: openai_chat_completions
    context_window_tokens: 128000
    capabilities:
      streaming: true
      tools: true
```

默认迁移为 `openai_chat_completions`，因为当前 `LocalToolAgent` 实际使用该协议执行工具调用。Responses 兼容客户端不能作为 Agent 当前行为的迁移依据。

### 17.5 非 LLM 配置迁移

以下 section 保持字段语义并完成 YAML 化：

- `approval`。
- `api`。
- `agent_temp`。
- `plugins`。
- `mcp`。

迁移必须对完整配置做回归，不能只验证 LLM 启动。

### 17.6 原子写入与备份

迁移流程：

1. 完整读取并校验 JSON。
2. 在内存构建 `config.yaml` 与 `models.yaml`。
3. 写入同目录临时文件。
4. flush，并在平台允许时执行 fsync。
5. 先原子替换 `models.yaml`，重新读取并校验模型 key；再原子替换引用该 key 的 `config.yaml`，避免配置先指向尚不存在的模型。
6. 重新读取两个目标文件并做跨文件语义校验。
7. 将旧文件保留为 `config.json.migrated.bak`，不直接删除。
8. 写入迁移来源 hash，保证重复启动不会再次覆盖用户已修改的 YAML。

两个文件无法形成真正的单文件系统事务，因此启动加载器还必须检测 active model 悬空引用：若 `config.yaml` 指向不存在的自定义 key，应拒绝覆盖任何文件，显示恢复建议，并允许用户从 `models.yaml` 中重新选择；不得静默切到任意模型。

运行时 `/model`、`/approval`、`/reasoning` 写回同样必须使用原子替换。

### 17.7 YAML 实现选择

建议使用 PyYAML 的 `safe_load/safe_dump`：

- 优点：依赖小、成熟、足以支持当前 schema。
- 代价：程序写回时不会保留全部用户注释和原始排版。

首版将 `config.yaml` 定义为“程序可管理配置”，写回时规范化格式。若后续明确要求保留注释，再评估 round-trip YAML 库，不在首版过度设计。

## 18. 安全设计

- `config.example.yaml` 和 `models.example.yaml` 不包含真实密钥。
- 推荐 Profile 只配置 `api_key_env`。
- `models.yaml` 禁止凭据字段。
- 日志、Session、SSE、插件 Hook 和错误信息继续执行敏感信息脱敏。
- Provider Adapter 不向插件暴露 Client、API Key、Authorization Header 或原始异常响应体。
- `base_url` 只允许来自 Provider Profile，不允许模型返回内容修改。
- 自动发现响应设置数量、分页、超时和内存上限。
- 模型显示名称和描述作为不可信文本处理，不解析 Rich markup 或终端控制字符。
- YAML 使用 `safe_load`，禁止任意对象构造。

## 19. 插件 Hook 兼容

现有 Hook：

- `model.request.before`。
- `model.response.after`。
- `model.request.error`。

改造后：

- Hook payload 增加 `profile`、`provider`、`protocol`、`modelId` 和 `generation`。
- `model.request.before` 继续只允许修改统一消息内容和白名单生成参数。
- 插件不得注入 Provider 原生 SDK 对象或凭据。
- `model.response.after` 继续保持 observe-only，避免流式 UI、Session 和返回值分叉。
- 切换时可新增只读事件：`model.changed`，用于通知插件刷新模型相关缓存；插件不能否决 Host 已完成的切换。

## 20. 依赖调整

实施预计新增：

```text
anthropic
google-genai
PyYAML
```

继续保留：

```text
openai
```

具体版本必须在实施阶段根据以下条件锁定：

- 支持项目声明的 Python `>=3.9,<4.0`。
- 具备所需流式、工具调用和模型发现能力。
- 单元测试能够稳定 mock SDK 对象。
- Windows 环境安装和导入通过。

如某 SDK 的新版本不再支持 Python 3.9，应先明确项目 Python 最低版本是否允许提升，不能静默改变运行要求。

## 21. 分阶段实施计划

### 阶段 0：锁定当前行为

- 为现有 OpenAI Chat 工具流、Responses 流、Token usage、`/model`、模型发现和配置写回补齐契约测试。
- 固定当前 API `/models` 的兼容字段。
- 不改变运行行为。

### 阶段 1：引入统一协议

- 新增 Provider 无关消息、事件、usage、错误和 capability 类型。
- 将当前 OpenAI Chat 流解析迁入 `OpenAIChatCompletionsAdapter`。
- `LocalToolAgent` 改为消费 `ModelRuntime`。
- 保留旧模块导入兼容。

退出条件：现有 OpenAI Chat Agent 行为和全量测试无回归。

### 阶段 2：YAML 与模型目录

- 将 `runtime.py` 改为 YAML 配置仓库。
- 新增 `models.yaml` loader 和 schema 校验。
- 新增 `config.json` 迁移器、备份和幂等标记。
- 更新所有 approval/API/MCP/plugin 配置读取和写回测试。

退出条件：非 LLM 配置行为保持一致，迁移可回滚。

### 阶段 3：Runtime Manager 与热切换

- 引入不可变 snapshot 和切换锁。
- `/model <key/ref>` 使用完整模型描述符切换。
- 切换失败保持旧模型。
- HUD 同步刷新模型、推理能力和上下文窗口。

退出条件：空闲切换、失败回滚、持久化失败和环境覆盖测试通过。

### 阶段 4：OpenAI Responses Adapter

- 将 Responses 接入统一 Agent 工具循环。
- 完成 Responses 文本、工具、usage、reasoning 和错误契约测试。
- 同一 OpenAI Profile 可由自定义模型选择不同 protocol。

### 阶段 5：Claude Adapter

- 引入 Anthropic SDK。
- 完成 Messages、流式 content blocks、tool use/result、usage 和错误适配。
- 完成 OpenAI ↔ Claude 回合边界切换测试。

### 阶段 6：Gemini Adapter

- 引入 Google Gen AI SDK。
- 完成 Content/Part、function call/response、usage 和错误适配。
- 完成 Gemini 与其他 Provider 的回合边界切换测试。

### 阶段 7：双列 Model Picker

- 实现 `ModelPickerScreen`。
- 加入搜索、列切换、刷新、诊断和窄屏降级。
- API 新增 catalog/refresh，并保留旧字段。

### 阶段 8：文档与兼容清理

- README、API、TUI 和 example 配置全部更新为 YAML。
- 标记 `config.json`、`OPENAI_MODEL` 和旧客户端门面的弃用周期。
- 至少保留一个发布周期后再删除兼容代码。

## 22. 测试方案

### 22.1 统一协议测试

- 文本流、思考流、单工具、多工具。
- 工具参数多分片聚合。
- 空响应、只有工具调用、只有思考内容。
- usage 缺失、中途更新和最终更新。
- finish reason 归一化。
- 取消和流中断。
- 已输出内容后不自动重试。

### 22.2 Provider Adapter 测试

全部使用 SDK mock，不依赖摄像头、真实凭据或外网。

| Adapter | 必测场景 |
|---|---|
| OpenAI Responses | 文本事件、function call/output、usage、reasoning、HTTP 错误 |
| OpenAI Chat | delta、tool_calls 分片、prompt cache 回退、extra body 白名单 |
| Anthropic | text/thinking block、tool_use、tool_result、stop reason、usage |
| Gemini | text Part、functionCall、functionResponse、stream chunk、usage metadata |

### 22.3 模型目录测试

- `models.yaml` schema 校验。
- alias 冲突。
- Profile 不存在或 Provider/协议不匹配。
- 自定义和发现模型按三元组匹配。
- 相同 model ID、不同 Profile 不冲突。
- 当前模型未出现在两列时仍可见。
- 单 Provider 发现失败不影响其他列。
- 自动发现不修改 `models.yaml`。
- 缓存 TTL、分页和数量上限。

### 22.4 热切换测试

- 空闲时切换成功。
- 活动回合期间拒绝切换。
- 候选 Client 创建失败时保留旧模型。
- `config.yaml` 写入失败时保留旧模型。
- 旧 runtime 在引用归零前不关闭。
- 切换后 model、protocol、CTX 和 reasoning 状态同时更新。
- Session 和 Memory 不被清空。
- 跨 Provider 不发送私有状态。
- 环境变量覆盖提示正确。

### 22.5 配置迁移测试

- 只有 `config.json`。
- 只有 `config.yaml`。
- 两者同时存在。
- 非法 JSON、非法 YAML。
- 迁移中断和临时文件失败。
- 二次启动幂等。
- approval/API/MCP/plugin 字段无损。
- API Key 不进入 `models.yaml`。
- 显式外部 `AI_CONFIG_FILE` 不被擅自迁移。
- 原子写回后的文件可重新解析。

### 22.6 TUI 测试

- 双列选择、左右/上下键、Enter、Esc、刷新。
- 搜索和 alias 匹配。
- CJK、长模型名和模型 ID 含 `/`。
- 窄屏上下分区。
- Provider 发现错误诊断。
- 切换时输入锁定。
- 成功后 HUD 刷新。
- Textual worker 不直接更新 Widget。

### 22.7 验证命令

```powershell
python -m unittest discover -s tests -v
python -m compileall -q omnicrawl main.py
git diff --check
```

新增依赖后还应在受支持 Python 版本执行 SDK 导入与最小 mock 测试。

## 23. 验收标准

### 23.1 协议接入

- [x] OpenAI Responses 使用 OpenAI 原生 SDK 的 Adapter 已实现（Agent 默认主路径仍为 Chat）。
- [x] OpenAI Chat Completions 使用 OpenAI 原生 SDK 完成流式文本和工具调用。
- [x] Claude 使用 Anthropic 原生 SDK Adapter 已实现（需依赖与真机联调确认）。
- [x] Gemini 使用 Google Gen AI 原生 SDK Adapter 已实现（需依赖与真机联调确认）。
- [x] Agent 主请求路径通过统一 `ModelRuntime` 消费流事件（遗留 Responses 审查路径仍可直连 OpenAI client）。

### 23.2 热切换

- [x] `/model` 无需重启即可切换 Provider、协议和模型（取决于已配置 Profile）。
- [x] 切换只影响下一轮，不改变正在进行的回合。
- [x] 切换失败时旧模型继续可用。
- [x] 切换后会话、Memory、MCP 和工具配置保持不变。
- [x] HUD 的模型、推理能力和上下文窗口可同步刷新；切换后最近 Token 显示清零。

### 23.3 模型列表

- [x] 左列显示 `models.yaml` 自定义模型。
- [x] 右列显示各 Provider API 自动发现模型。
- [x] 单 Provider 发现失败不会阻断其他模型。
- [x] 自动发现不覆盖用户模型文件。
- [~] 窄屏上下分区：当前双列容器可自适应；专用窄屏样式仍可细化。

### 23.4 配置

- [x] 优先使用 `config.yaml`；无 YAML 时兼容 `config.json` 并支持迁移。
- [x] 新增 `models.yaml` 模型目录。
- [x] 旧 `config.json` 可安全、幂等迁移并保留备份。
- [x] 配置写回采用原子替换。
- [x] 示例文件不包含真实密钥。

### 23.5 质量

- [~] Provider Adapter 具备基础 Runtime/迁移/协议门面测试；各 SDK 全量 mock 契约可继续补齐。
- [x] 现有核心 Agent、TUI 命令分派、API、模型目录相关回归保持通过。
- [x] README / API / 本设计文档与示例配置已同步主链路行为。

## 24. 关键取舍

| 选择 | 收益 | 成本/放弃项 |
|---|---|---|
| 统一内部协议，而非让所有 Provider 伪装 OpenAI | 正确表达各厂商工具、流和 usage；长期可维护 | 首次改造范围较大 |
| OpenAI Responses 与 Chat 分 Adapter | 避免协议分支持续堆积 | 有少量公共代码需要抽取 |
| 回合边界切换 | 简单、可验证，不会破坏工具链 | 不支持生成中途切换 |
| `config.yaml` 与 `models.yaml` 分离 | 运行配置和模型目录职责清晰 | 多一个本地配置文件 |
| 自动发现不写模型文件 | 不覆盖用户配置，可处理临时模型 | 发现模型的详细元信息默认不持久化 |
| PyYAML 规范化写回 | 实现简单、稳定 | 不能完整保留注释和原格式 |
| 自定义模型未发现仍允许选择 | 兼容不开放列表接口的网关 | 真正可用性要到首次请求才能最终确认 |

## 25. 风险与缓解

| 风险 | 等级 | 缓解措施 |
|---|---|---|
| Agent 历史仍泄漏 OpenAI tool_calls 结构 | 高 | 先落统一消息块，再接 Claude/Gemini |
| SDK 版本与 Python 3.9 不兼容 | 高 | 实施前核验并锁定版本；如需提升 Python 下限必须单独确认 |
| 流式工具调用映射错误导致重复执行 | 高 | Adapter 契约测试；完整 ToolCall 后才交给 Host；流中断不执行半成品 |
| YAML 迁移破坏非 LLM 配置 | 高 | 全 section 迁移测试、备份、重新读取校验、幂等标记 |
| 热切换导致内存与磁盘状态不一致 | 高 | 候选构建 → 原子持久化 → 引用交换；失败不改旧状态 |
| 自动发现无权限或接口不存在 | 中 | 自定义列表独立可用；按 Profile 展示诊断 |
| 模型能力元信息过时 | 中 | 保守默认；请求错误继续由 Provider 归一化，不盲信静态配置 |
| 双列在窄屏不可用 | 中 | 自动降级为上下分区 |
| YAML 写回丢失注释 | 低 | 文档声明配置会规范化；后续有明确需求再引入 round-trip YAML |

## 26. 预计主要改动文件

| 文件/目录 | 改动 |
|---|---|
| `omnicrawl/agent/core.py` | 从 OpenAI Client/Protocol 改为获取运行时快照 |
| `omnicrawl/agent/llm_protocol.py` | 迁移为兼容门面或统一协议聚合器 |
| `omnicrawl/agent/types.py` | 增加 Provider 无关消息块和结果类型 |
| `omnicrawl/config/runtime.py` | JSON 专用读写改为 YAML、安全加载、原子写入 |
| `omnicrawl/config/llm.py` | 单 LLMConfig 改为 Profile + active model 配置 |
| `omnicrawl/config/model_catalog.py` | 聚合自定义与原生 SDK 发现结果 |
| `omnicrawl/config/model_store.py` | 新增 models.yaml schema 与校验 |
| `omnicrawl/config/migration.py` | 新增 JSON 到 YAML 迁移 |
| `omnicrawl/llm/` | 新增统一协议、Runtime Manager 和四类 Adapter |
| `omnicrawl/commands/slash.py` | `/model` 解析 canonical selection，不直接操作 SDK |
| `omnicrawl/ui/fullscreen/model_picker.py` | 新增双列模型选择 Screen |
| `omnicrawl/api/routes/configuration.py` | 扩展模型目录、刷新和切换接口 |
| `requirements.txt`、`pyproject.toml` | 增加 Anthropic、Google Gen AI、YAML 依赖 |
| `config.example.yaml` | 新增 YAML 运行配置示例 |
| `models.example.yaml` | 新增模型目录示例 |
| `README.md`、`docs/API.md`、`docs/TERMINAL_UI.md` | 同步配置、协议和交互说明 |
| `tests/` | 增加 Adapter、迁移、目录、热切换和 Model Picker 测试 |

## 27. 最终建议

按“统一协议 → YAML/模型目录 → Runtime Manager → 各 Provider Adapter → 双列 TUI”的顺序实施。不要先在现有 `AgentLLMProtocol` 中堆叠 `if provider == ...`，也不要让 Claude/Gemini 返回结构伪装为 OpenAI Chat Completions。

该顺序虽然前两阶段暂时看不到新增 Provider，但能先建立可测试边界，避免在工具调用、流式输出、Session 和热切换同时变化时难以定位问题。首个可交付里程碑应是：**现有 OpenAI Chat 行为通过统一 Adapter 完整回归；随后再逐个接入 Responses、Claude 和 Gemini。**

### 当前落地结论

主链路已按上述顺序完成：OpenAI Chat 经统一 Runtime 回归可用；YAML/`models.yaml`、热切换、双列 Picker 与 API catalog 已接通；Responses/Claude/Gemini Adapter 代码已注册。后续工作以真机联调、契约测试补强和兼容层清理为主，而不是重开架构。
