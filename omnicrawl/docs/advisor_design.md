# OmniCrawl「顾问策略（Advisor Strategy）」移植设计

> 文档性质：方案设计 / 移植蓝图（**已实现**：落地细节与交付清单见
> `omnicrawl/docs/` 对应实现代码与知识库 `advisor-strategy-implementation` 记录；
> 本文保留设计意图、落点与边界作为实现依据）。
> 参考来源：<https://github.com/juicesharp/rpiv-mono/tree/main/packages/rpiv-advisor>（Pi Agent 扩展，MIT）。
> 目标宿主：本仓库 `omnicrawl`（Python，`LocalToolAgent` 组合式 Mixin 架构）。
> 实现状态：**已落地**（2026-09-07；同日补充设置面板「顾问设置」页）。
> 第 3 节给出目标架构，第 4 节为已实现的改造点。

## 1. 背景：rpiv-advisor 是什么

rpiv-advisor 是 Pi Agent（一个 TypeScript 编程终端 Agent）的扩展包，实现**顾问策略模式**：

> 让正在干活的"执行者模型"（executor，通常快而便宜）在需要更强判断时，把**整个会话分支**
> 交给第二个**更强的评审模型**（advisor），拿到 `plan` / `correction` / `stop` 三类指导后继续。

它不引入新的复杂编排，而是**复用宿主已有的单 Agent 工具循环**：

- 新增一个**零参数工具** `advisor()`（不是新 Agent、不是后台任务、不是新会话）；
- 执行者在回合中间调用它 → 插件**自动序列化当前会话分支**（任务 + 每次工具调用 + 每个结果）→
  用第二个模型（无工具、按需带 reasoning effort）做一次**单轮补全** → 把纯文本指导作为
  `advisor()` 的**工具结果**交还执行者 → 执行者消化后继续；
- 另有一个 `/advisor` 斜杠命令选择"顾问模型 + 推理档位"，选择持久化到
  `~/.config/rpiv-advisor/advisor.json`（0600）。

关键事实（从源码 `advisor/*.ts` 核对）：

| 事实 | 出处 |
|---|---|
| 工具 schema 是 `Type.Object({})`，零参数；描述文本很长，明确"自动转发你的全部历史" | `advisor/register.ts` |
| 转发内容 = `buildSessionContext()` 的**已解析 LLM 上下文**（保留压缩摘要/分支摘要），不是未压缩的原始回放 | `advisor/execute.ts` |
| 转发前做"消息按摩"：剥掉正在执行的 `advisor()` toolCall（孤儿调用），并保证尾部是 user 角色 | `advisor/context.ts` |
| 顾问消息前置 `## Available Executor Tools` 工具清单（按键排序 + `stableStringify`，供 prompt 缓存对齐） | `advisor/inventory.ts` |
| 顾问侧 `completeSimple(advisor, { systemPrompt: ADVISOR_SYSTEM_PROMPT, messages, tools: [] })` —— **永不调工具** | `advisor/execute.ts` |
| 顾问系统提示只要求三类输出：`plan` / `correction` / `stop`，从不产生面向用户的输出 | `prompts/advisor-system.txt` |
| 空文本响应**有界重试一次**（相同输入），aborted/error 短路不重试 | `advisor/execute.ts` |
| 结果信封统一 `buildAdvisorResult`：`content` 纯文本 + `details`（advisorModel/effort/usage/stopReason/errorMessage） | `advisor/execute.ts` |
| 三种生命周期钩子实时剥/加工具：`before_agent_start`、`model_select`、`thinking_level_select`，配合 `disabledForModels` 黑名单 | `advisor/handlers.ts`、`advisor/policy.ts` |
| 未选模型 → 工具从 active set 剥离，**其提示词永不进入 system prompt**（零成本） | `advisor/restore.ts`、issue #72 |
| 会话开始恢复：`session_start` → `restoreAdvisorState()` 重新应用持久化选择并激活 | `advisor/restore.ts` |

这套机制在 rpiv 里属于"扩展 + 宿主事件"，与 OmniCrawl 的 Python 插件体系结构不同，但**语义可以 1:1 平移**。

## 2. OmniCrawl 宿主现状（本文落点依据）

以下为本文撰写时从源码核实的现状（`omnicrawl/` 根，`LocalToolAgent` 由多个 Mixin 组合）：

### 2.1 回合循环（executor 侧）

- `omnicrawl/agent/core.py`：`class LocalToolAgent(TurnLoopMixin, ..., SubAgentOrchestrationMixin, ToolBuildingMixin, ...)`。
- `omnicrawl/agent/controllers/turn/loop.py`：
  - `run_stream()` 是主入口：`context_messages = self._context_messages(turn_id)` → 构建
    `working_messages` → `AgentLoopRunner().run(...)`；
  - `request_main_reply()` → `self._request_agent_reply(messages, ...)` → `self._llm_protocol().request_reply(...)`；
  - `execute_main_tool_batch()` → `self._execute_tool_batch(calls, first_step, ...)`（先整批规范化/审批，再并发执行，
    结果按模型调用顺序回填）；
  - `_execute_tool_batch` 内：`active_tools = self._tools`、`HostToolCatalog(active_tools)`、
    `_normalize_tool_call_for_batch(...)` 之后 `ThreadPoolExecutor` 并发执行（`can_serialize_tool_runner` 时进子进程）。
- `omnicrawl/agent/runtime/execution.py`：`class AgentLoopRunner`（独立、可取消、可设预算），
  `run(messages, request_reply, execute_tool_batch, limits, cancel_check, stop_check)`。
  循环：`request_reply(messages)` → 若 `reply.tool_calls` 非空 → `execute_tool_batch(...)` → 追加 observation 消息 → 继续；
  无 tool_calls 则返回最终文本。
- `omnicrawl/agent/controllers/turn/loop.py::_request_agent_reply()`：先派发插件钩子 `model.request.before`
  （可改 messages / 采样参数，可 guard），再 `self._llm_protocol().request_reply(...)`，之后派发
  `model.response.after`（observe）。`_llm_protocol()` 每次构造 `AgentLLMProtocol`，绑定 `self.config.llm` 的
  model/client/tools_provider/system_prompt_provider/extra_body_provider/runtime_manager 等。

### 2.2 工具表与系统提示词

- `omnicrawl/agent/controllers/tools/building.py::ToolBuildingMixin._build_tools()`：
  `build_agent_tools(...)` 组装全部工具 → `return {tool.name: tool for tool in tools if tool.name not in disabled_tools}`。
  （已支持 `disabled_tools` 过滤，等价 rpiv 的"剥离"能力。）
- 同一文件的 `_system_prompt()`：基础 system prompt + 活动模式提示词；工具清单**不注入** system prompt，
  而是走 Provider 顶层注册（见 `omnicrawl/docs/TOOL_CALLING.md`）。
- `omnicrawl/agent/toolkit/tools.py::build_agent_tools()`：显式列出每个工具的 `ToolRunner` 参数；
  若新增工具，需要在此签名 + `_build_tools()` 调用处 + 具体实现（`_tool_*` 绑定方法）三处接线。

### 2.3 LLM 配置 / Runtime

- `omnicrawl/config/models/llm_multi.py`：
  - `load_multi_model_llm_config(llm_section)` → 返回当前 `LLMConfig`（model/profile/protocol/凭据/上下文窗/effort）；
  - `apply_model_selection(config, selection)` → 把选择解析为新 `LLMConfig` 视图（支持 models.toml key/alias、
    `profile/model_id`、裸 model_id 三态）；
  - `llm_config_to_profile_and_descriptor(config)` → `(ProviderProfile, ModelDescriptor)`。
- `omnicrawl/llm/runtime.py`：`ModelRuntimeManager`（`bootstrap` / `acquire_turn` / `release_turn` / `switch`，
  回合边界安全切换，`allow_during_turn=True` 可回合中切换，下一请求生效）。
- `omnicrawl/agent/runtime/llm_protocol.py`：`AgentLLMProtocol.request_reply()` / `request_reply_once()`
  封装协议细节：先 `system_prompt_provider()`、`tools_provider()`、`extra_body_provider()` 构建请求，
  再走 `_request_via_runtime`（ModelRuntimeManager 快照）或 `_request_via_openai_client`（旧直连）。
- 关键约束：**主 Agent 的 `self.config.llm` 是单模型**；同一回合内多次 `_request_agent_reply`
  共享同一个 `runtime_snapshot`。跨模型"第二意见"必须自己构造独立的 `LLMConfig` + 独立的 Runtime/协议对象，
  不能复用执行者回合的 snapshot。

### 2.4 子代理（SubAgent）—— 与本方案最接近的既有模式

- `omnicrawl/agent/subagents/definitions.py`：`AgentDefinition`（`model: str = "inherit"`），
  `AgentDefinitionRegistry.discover()` 按"项目 > 用户 > 内置 > 插件"优先级扫描 `.md` 定义。
- `omnicrawl/agent/subagents/execution.py`：`SubAgentModelSnapshot`（selection + llm_config + profile + descriptor），
  `SubAgentExecutionContext`（fork_messages / parent_system_prompt / skill_context / plugin_dispatch 等，均为冻结快照）。
- `omnicrawl/agent/controllers/subagents/orchestration.py`：
  - `_freeze_subagent_model_snapshot()`：按 `[subagents.models.<角色>]` 项目级配置 → `apply_model_selection(parent_llm, selection)`
    → `replace(...)` 复制 → `llm_config_to_profile_and_descriptor()` → 冻结 `SubAgentModelSnapshot`；未配置/inherit 沿用父模型；
  - `_build_subagent_protocol()`：用冻结的 model_config 构造**独立** `AgentLLMProtocol`（独立 client/runtime_manager、
    独立 system prompt、独立 tools、`request_retry_count=3`），并写独立的 `prompt_cache_identity`；
  - `run_subagent_task()`：同步运行一个 SubAgent 任务并返回文本结果。
- **本方案的顾问调用**与该模式高度同构：冻结"顾问模型"→ 构造独立协议对象 → 发起一次无工具补全。
  区别是：顾问不需要 SubAgent 的工具面、权限面、worktree/隔离、事件通知这些重机制；只需要"冻结模型 + 独立单轮请求 + 纯文本返回"。

### 2.5 插件钩子（plugin hooks）

- `omnicrawl/extensions/plugin_models.py`：Core Hook 白名单：
  `turn.start / turn.end / turn.error / turn.cancelled / context.build.before / context.build.after /
  model.request.before / model.request.error / tool.call.before / tool.approval.before / tool.approval.after /
  tool.execute.before / tool.execute.after / tool.execute.error`；另有自定义事件 `plugin.<name>`。
- 目前**没有** rpiv 依赖的 `session_start` / `model_select` / `thinking_level_select` / `before_agent_start` 这类事件。
  移植时需要把"会话开始恢复选择"和"模型/effort 切换时剥/加工具"映射到 OmniCrawl 已有的事件与入口：
  - 会话开始：OmniCrawl 的 session restore / 新会话入口（见 `omnicrawl/docs/session_design.md`，`session_start` 事件在 plugin 中没有；
    落地可选：在 `run_stream` 的 turn.start 或 Agent 初始化处恢复；或为插件新增 session_start 事件——见 §4.4）；
  - 模型切换：OmniCrawl 有 `ModelRuntimeManager.switch()`（可 `allow_during_turn`）与 `/model` 切换逻辑；
    工具剥/加可挂在切换后重建工具表的路径（`_build_tools()` 已天然按 `disabled_tools` 过滤）。

### 2.6 消息结构与协议约束

- 消息形态：`list[dict]`，role ∈ {system, user, assistant, tool}；assistant 消息带 `tool_calls`，
  tool 消息带 `tool_call_id`（见 `TOOL_CALLING.md`）。OpenAI/Anthropic 都要求：assistant tool_calls 与其
  tool 结果**相邻**、**不成对/孤儿 toolCall 会被拒**、**部分模型拒绝 assistant 结尾**（需要 user 尾巴）。
- OmniCrawl 已有上下文压缩（`context_compaction`）与"会话级记忆"摘要；`AgentLoopRunner` 会保留
  `messages` 直到返回（`loop_result.messages`），但**没有**像 rpiv `buildSessionContext` 那样"从会话存储重建已压缩分支"的工具。
  转发给顾问时应优先使用"当前工作消息（存活于压缩的投影）"，而不是把 Session JSONL 全量倒出。

## 3. 目标架构（OmniCrawl 落地形态）

设计原则：**尽可能薄**。不新增 SubAgent 类型、不新增后台任务、不新增持久化会话；只新增
"一个零参数工具 + 一个配置 + 一个独立单轮补全 + 少量引导提示词"。

```text
用户 ──> run_stream() ──> AgentLoopRunner 循环
         │                     │  request_reply(messages)  ← 执行者模型（快/便宜）
         │                     ▼
         │              [执行者模型自己判断] 复杂/卡住/完工前/换方向
         │                     │ 调用 advisor()
         ▼                     ▼
   _execute_tool_batch ──> advisor 工具 runner（Host 侧）
                              │  ① 冻结 advisor 模型（apply_model_selection + llm_config_to_profile_and_descriptor）
                              │  ② 构造独立 AgentLLMProtocol（client/runtime_manager 独立，tools=[]）
                              │  ③ 消息按摩：剥 in-flight advisor toolCall + 保证 user 尾
                              │  ④ 前置工具清单（可选，prompt 缓存对齐）
                              │  ⑤ request_reply_once(messages, systemPrompt=advisor-system)
                              ▼
                   纯文本 plan/correction/stop 或错误信封
                              │
                              ▼
             作为 advisor() 工具结果（content+details）回到执行者回合
                              │
                              ▼
               执行者消化 → 继续循环（可再次申请）
```

### 3.1 组件映射表（rpiv → omnicrawl）

| rpiv 概念 | rpiv 文件 | OmniCrawl 落点（设计） |
|---|---|---|
| 零参数 `advisor` 工具 | `advisor/register.ts` | 新增工具定义 `advisor`，runner 挂在 `ToolImplementationsMixin` 或独立 `AdvisorMixin`；注册进 `build_agent_tools()` |
| `/advisor` 斜杠命令 | `advisor/command.ts` | 新增配置入口：`/advisor` 斜杠命令（OmniCrawl 已有命令框架）或设置面板；持久化选择 |
| 持久化配置 `advisor.json` | `advisor/config.ts` | OmniCrawl 配置段 `[advisor]`（写入 config.toml / models.toml 旁，仅存选择与 effort；**不存 key**） |
| `disabledForModels` | `advisor/policy.ts` | `[advisor] disabled_for_models`（executor 黑名单，支持 min_effort） |
| 执行器提示词准则 | `DEFAULT_PROMPT_GUIDELINES`（register.ts） | 注入 `system_prompt.md` 或作为 `_system_prompt()` 的 advisor 区块；提示何时调用/如何消化/需在可见回复中转述 |
| 顾问系统提示 | `prompts/advisor-system.txt` | 内置模板 `omnicrawl/templates/advisor_system.md` 或常量 |
| 分支序列化 | `buildSessionContext` | 使用当前 `working_messages`（存活于压缩的投影），必要时按 §4.3 增强 |
| 消息按摩 | `advisor/context.ts` | 复用同逻辑：剥 `advisor()` toolCall + 补 user nudge |
| 工具清单前置 | `advisor/inventory.ts` | 从 `self._provider_tools()` 生成 `## Available Executor Tools`（键排序稳定序列化） |
| 顾问侧补全 | `completeSimple(..., tools: [])` | `AgentLLMProtocol.request_reply_once()` + `tools_provider` 返回 `[]` + 独立 system prompt |
| 空响应重试一次 | `execute.ts` | 相同输入最多重试 1 次；aborted/error 不重试 |
| 结果信封 | `buildAdvisorResult` | `ToolResult(content=[text], details={advisorModel, effort, usage, stopReason, errorMessage})` |
| 生命周期剥/加工具 | `handlers.ts` / `restore.ts` | 会话开始恢复选择 + 模型切换后按黑名单重建工具表（`_build_tools` disabled_tools 过滤） |
| 未配置零成本 | `restore.ts` | 未配置时 advisor 不进工具表 → 提示词永不注入 |

### 3.2 关键设计决策

1. **顾问是一次"旁路单轮补全"，不是子代理**：不产生会话转录、无工具、无审批、无 worktree/隔离。
   只在 `request_reply_once` 层面新增一次模型请求，把纯文本装进 ToolResult。
2. **顾问模型 = 独立冻结的 LLMConfig**：参照 `_freeze_subagent_model_snapshot`，用
   `apply_model_selection(parent_llm, advisor_selection)` 得到 advisor 的 `LLMConfig`，
   再 `llm_config_to_profile_and_descriptor` 构造独立 `ModelRuntime`。绝不复用执行者回合的
   `runtime_snapshot`（不同模型/凭据），也绝不让 advisor 配置进入 Session/公开 ToolResult。
3. **advisor() 的工具结果回到同一个 `AgentLoopRunner` 循环**：由于它只是普通工具，天然享受
   OmniCrawl 已有的"工具批次并发/审批/超时/缓存"语义；执行者收到 result 后可继续。
4. **默认关闭、零成本**：无配置时工具表里根本没有 `advisor`，system prompt 也不含其准则
   （与 rpiv issue #72 的教训一致：不要在 active set 里留一个永远失败的 stub）。
5. **不注入转录**：advisor 输出只作为工具结果返回；同时提示词要求执行者在**下一条可见回复**
   转述关键指导（用户看不到折叠的工具卡片）。
6. **成本即意识**：整个分支按顾问模型计费，因此提示词明确"短任务不调用、仅在关键时刻调用，
   长任务至少方案前一次 + 完工前一次"。

### 3.3 数据流（时序）

```text
回合内（executor）:
   executor 调 advisor()
   ──> _execute_tool_batch 命中 advisor runner
        ├─ 读 [advisor] model_key + effort（失败 → 错误信封，不发起请求）
        ├─ 构造 advisor LLMConfig / Runtime（独立）
        ├─ branch = 当前 messages
        │         - 剥掉尾部 in-flight advisor() toolCall
        │         - 若尾是 assistant → 追加 user nudge
        │         - 前置工具清单消息（可选缓存）
        ├─ request_reply_once(branch,
        │        system_prompt=advisor_system, tools=[])
        │        → 文本 / 空(重试1次) / aborted/error(短路)
        └─ ToolResult { content: [advisor 文本或错误文案],
                        details: { advisor_model, effort, usage, stop_reason } }
   executor 读取 result → 采纳/再协商 → 继续
```

## 4. 可落地的改造点（按依赖排序）

> 以下为"如果要实现"的建议顺序与落点；不包含完整代码。

### 4.1 新增 advisor 系统提示模板（独立文件）

- 落点：`omnicrawl/templates/advisor_system.md`（与 `templates/plan.md` 同目录），
  或作为 `AgentConfig` 常量。内容复刻 rpiv `prompts/advisor-system.txt` 语义：
  三类输出（plan / correction / stop）、绝不调工具、绝不面向用户输出、简洁指令式、点名文件/函数/行号。
- 注：`building.py::activate_mode()` 已经示范了"从 templates 目录读模板"的既有模式。

### 4.2 新增 `[advisor]` 配置解析 + 选择持久化

- 落点：`omnicrawl/config/features/` 仿照 `subagents.py` 新增 `advisor.py`（dataclass + 校验），
  字段：`enabled: bool`、`model_key: str`（"provider/model_id" 或 models.toml key）、`effort: str`、
  `disabled_for_models: list[str | {model, min_effort}]`。
- 写入：由 `/advisor` 命令或设置面板调 `save_active_model_ref` 同款持久化辅助（只写选择与 effort，
  复用 `models.toml` 的 profile 凭据解析，**不复制 key 到额外文件**）。
- 解析校验：复用 `apply_model_selection(parent_llm, selection)`（已支持 key/alias、`profile/model_id`、裸 model_id）。
  若选择不可用 → 与 rpiv 一致地"停用并剥离"，而不是留 stub。

### 4.3 消息按摩 / 分支来源

- 建议放 `omnicrawl/agent/controllers/turn/` 或新 `omnicrawl/agent/controllers/advisor.py` Mixin：
  - `strip_inflight_advisor_call(messages)`：剥掉尾 assistant 消息中 name=advisor 的 toolCall（孤儿调用）；
  - `ensure_user_tail(messages)`：尾为 assistant 时补最小 user 消息；
  - `branch_for_advisor()`：默认使用当前 `AgentLoopRunner` 的 `working_messages`（已含压缩后投影），
    并前置工具清单（`_provider_tools()` 按键排序 + 稳定序列化，供 prompt 缓存对齐）。
- 若希望"压缩摘要也被转发"，增强点：在压缩发生时（`context_compaction`）把摘要消息保留在
  Session 的"advisor 可重建分支"中；这与 rpiv 的 `buildSessionContext`（从 session store 重建）
  对齐，但属于可选增强。

### 4.4 工具注册与生命周期剥/加

- 注册：`build_agent_tools()` 增加 `advisor` 参数（runner 由 `ToolImplementationsMixin` 提供），
  与 `subagent`/`ask_user`/`pause_work` 同模式；`_build_tools()` 传入。
- 剥/加：OmniCrawl 工具表每回合由 `_build_tools()` 构建，天然按 `disabled_tools` 过滤。
  建议把 `[advisor] disabled_for_models` 的计算结果并入 `disabled_tools` 来源
  （在 `_build_tools()` 顶部与 config.disabled_tools 合并），这样：
  - 未配置 advisor → `advisor` 直接不在表内（零成本，满足 §3.2-4）；
  - 执行者模型命中黑名单 → `advisor` 不在表内；
  - 回合中 `/model` 切换 → 下一回合 `_build_tools()` 自动按新模型重算（无需额外事件）。
- 会话开始恢复：在 Agent 初始化/会话恢复路径（`run_stream` 前）调用一次 `restore_advisor_state()`，
  与 rpiv `restoreAdvisorState` 语义一致（重新应用持久化选择，不可用则剥离并提示一次）。
- 若希望插件化：可在 `plugin_models.py` 增加 `session_start` 等 Core Hook（需评估是否影响
  现有插件协议版本），或直接作为内置 Mixin 实现（推荐，改动面小）。

### 4.5 顾问侧独立补全调用

- 新增 `AdvisorMixin._call_advisor(messages) -> ToolResult`：
  1. 解析 `[advisor]` 选择 → `advisor_llm = apply_model_selection(self.config.llm, advisor_model_key)`；
     `profile, descriptor = llm_config_to_profile_and_descriptor(advisor_llm)`；
  2. 构造独立 `ModelRuntimeManager` 或直接 `build_runtime(profile, descriptor)`，
     并构造独立 `AgentLLMProtocol`（client=None 走 runtime；`tools_provider=lambda: []`；
     `system_prompt_provider=lambda: advisor_system`；`reasoning_effort_provider` 返回配置 effort；
     `request_retry_count=1`——空响应由本逻辑有界重试一次，其余错误直接短路）；
  3. `request_reply_once(branch, ...)`（复用协议的重试/错误分类能力，但空响应额外做一次
     "相同输入重试"再报错；aborted/error 不重试）；
  4. 组 `ToolResult(ok=..., output=text, details=...)` 返回。
- 关键：**不经过** `run_stream` 主循环、**不写** Session/转录、**不触发**审批（只读旁路），
  但**要响应取消**（传 `cancel_check`/`signal`），避免 ESC 后顾问请求仍在后台烧钱。

### 4.6 执行器引导提示词

- 在 `omnicrawl/agent/system_prompt.md`（或模式模板）追加"顾问使用准则"小节，内容复刻 rpiv
  `DEFAULT_PROMPT_GUIDELINES`：
  - 实质性工作前 / 卡住 / 完工前（先落盘再调用）/ 换方向时调用；
  - 短任务、下一步由刚读到的工具输出决定时不调用；
  - 长任务至少方案前一次 + 完工前一次；
  - 给指导实质权重；有原始证据冲突时用一次 `advisor` 做 reconcile，不盲从不盲弃；
  - 每条指导要在下一条**可见回复**中转述。
- 注意：OmniCrawl 的 system prompt 是"工具+规则"模板，新准则应作为条件区块——未启用
  `[advisor]` 时不渲染该区块（与工具剥离保持一致）。
- 实现：`building.py::_system_prompt()` 调用 `_advisor_guidelines_block()`，仅当
  `config.advisor.active` 且当前模型不在黑名单时在基础 system prompt 之后、活动模式区块
  之前追加准则文本（system_prompt.md 保持静态不变）。

### 4.7 命令/UI

- 新增 `/advisor` 斜杠命令：列出当前已认证模型（复用 models.toml 中 enabled profile + model 发现），
  选择顾问模型与 effort（none/low/medium/high/xhigh/max，缺省 high），保存选择；"No advisor" 清除。
  实现：`commands/slash.py::handle_advisor_command`（文本式，与其他 connector 一致；无参数列出候选，
  带参直接设置，`off` 清除）。
- **设置面板「顾问设置」页（新增）**：`/settings` 左侧列表在“模型渠道”之后新增“顾问设置”行
  （`_SETTING_ORDER` 插入 `advisor`），右侧挂载 `ui/fullscreen/screens/advisor_settings.py::
  AdvisorSettingsPane`：
  - 内嵌双列 `ModelPickerPane(selection_only=True, current_override=<已选顾问>)`——与主模型行同款
    选择器但**不切换主模型**，选择结果仅暂存为顾问 model_key；`current_override` 让已配置的
    顾问模型在目录中高亮；
  - 启用开关 + effort 下拉 + 当前顾问摘要 + 保存/取消按钮；保存先写 `config.toml [advisor]`
    段，再调 `agent.set_advisor_configuration`（`SessionSettingsMixin` 新增 setter：同步
    `config.advisor` 并事务式重建工具表，失败回滚），任一步失败回滚磁盘；
  - 启用但未选模型时拒绝保存；`AdvisorSettingsScreen` 薄壳保留独立入口协议。
- UI 提示：调用中显示 `Consulting advisor (<provider>/<model>, <effort>)…`（可仿照现有
  `on_status` 通道），与 rpiv `msgConsulting` 对应。

## 5. 与 OmniCrawl 现有子代理的关系（差异与互补）

| 维度 | SubAgent | Advisor（本方案） |
|---|---|---|
| 目的 | 并行/隔离执行子任务、审查 diff | 给执行者一个更强的"第二意见" |
| 是否产生独立循环 | 是（独立 AgentLoopRunner，fresh/fork 上下文） | 否（旁路单轮补全） |
| 工具面 | 有（受限角色工具表 / 只读） | 无（`tools: []`） |
| 权限/审批/隔离 | 有（permissionMode/isolation/worktree） | 无（只读、不落盘、不进转录） |
| 模型来源 | `[subagents.models.<角色>]`（inherit 沿父） | `[advisor] model_key`（独立于执行者） |
| 结果 | 结构化工作报告 / JSON | 纯文本 plan/correction/stop |
| 成本 | 独立子任务完整循环 | 每次调用转发整个分支给强模型 |

两者互补：已有 `review` 子代理适合"事后审 diff"；advisor 适合"回合中、决策前/卡住时"的轻量把关。
若未来要"顾问也有工具"，可让 advisor 退化为受限 SubAgent，但默认形态保持旁路单轮。

## 6. 边界与风险

- **成本放大**：每次 `advisor()` 都按顾问模型计费整个分支；必须有提示词纪律 + 可选
  `[advisor] enabled`/黑名单开关。可加"每回合最多 N 次 advisor 调用"预算（仿 `AgentLoopLimits`），
  但默认不做硬限制以免妨碍 reconcile。
- **消息协议**：孤儿 toolCall / assistant 结尾必须按摩，否则 Anthropic/GLM/OpenAI 拒收；
  建议在实现时对齐 `context.ts` 两条规则，并补协议级单测。
- **凭据隔离**：advisor 若跨 profile，其 api_key 解析必须走 `_resolve_credentials`（env 或 profile 字段），
  绝不把 key 写进 advisor.json/工具结果/日志（现有 SubAgent 冻结快照已示范"llm_config 不进 Session"）。
- **与压缩/记忆的关系**：转发"存活于压缩的投影"即可，避免把 Session JSONL 全量倒给顾问
  （token 爆炸且可能含冗余）。
- **模型不支持 reasoning effort**：effort 只在该模型支持时发送（rpiv 的 picker 也只列支持档位），
  否则省略，避免 400。
- **回合内切换**：executor 回合中 `/model` 切换不应影响已冻结的 advisor 选择（设计上与 SubAgent
  冻结快照一致：创建时复制，后续切换只影响下一次 advisor 调用）。

## 7. 参考资料

- rpiv-advisor 源码（MIT）：`packages/rpiv-advisor/advisor/{register,execute,context,inventory,handlers,restore,config,command,state,policy}.ts`、
  `prompts/advisor-system.txt`、`README.md`。
- OmniCrawl 宿主代码：`omnicrawl/agent/controllers/turn/loop.py`、`omnicrawl/agent/runtime/execution.py`、
  `omnicrawl/agent/runtime/llm_protocol.py`、`omnicrawl/llm/runtime.py`、`omnicrawl/config/models/llm_multi.py`、
  `omnicrawl/agent/subagents/*`、`omnicrawl/agent/controllers/subagents/orchestration.py`、
  `omnicrawl/extensions/plugin_models.py`。
