# OmniCrawl SubAgent 子 Agent 与任务分发设计

> 文档状态：实施完成；Phase 0–3、后台与审批控制面、受控 Fork/模型覆盖、跨进程恢复、Plugin dispatch、Worktree 写隔离、边界回归及最终安全不变量审计均已于 2026-07-17 前完成
>
> 文档版本：1.0
>
> 编写日期：2026-07-14
>
> 适用项目：OmniCrawl Python Host、全屏 TUI、本机 HTTP/SSE API
>
> 参考资料：飞书知识库《第13章：SubAgent，子Agent与任务分发》及其理论、实战、Python/Go/Java/TypeScript 源码解析子文档

## 实施状态（2026-07-17）

2026-07-22 补充：全屏 TUI 已将逐事件追加的“排队 / 运行 / 完成”状态行改为按
`batch_id` 聚合、按 `task_id` 原地更新的进度树。等待、运行、等待审批、完成、失败
和取消均使用图标、文字与语义色共同表达；树中不保存或展示 prompt、完整结果和原始异常。

已完成：

- Phase 0：新增内部 `AgentLoopRunner`，主 Agent 继续独占 Session、Plugin turn、Runtime 快照、历史提交和公开回调；工具批次仍保持整批审批、读操作并行、写/删除屏障和原序回填。
- Phase 1A：新增默认关闭的 `subagents` 配置、环境变量只收紧语义、项目/兼容项目/用户/内置/已批准插件五级 Markdown 定义来源、包内 `explore`/`plan`、统一 `subagent(action=run)` 工具和单个同步 `fresh`/`read_only` 子执行。
- Phase 1B：`subagent(action=run)` 扩展为 1–4 个同步任务，Coordinator 按 Host 配置限制任务并发，模型请求使用独立信号量限制 Provider 在途请求；支持输入顺序聚合、单任务失败隔离、批次 `partial` 和可选 `fail_fast`。
- Phase 1C：Coordinator 新增批次/任务生命周期事件和统一安全结果投影；父 Session 记录 queued/started/completed/failed/cancelled additive 事件且不进入模型恢复上下文；大结果在脱敏后写入任务级 JSON artifact；API SSE 与全屏 TUI 可观察单任务开始、完成、失败和取消，外层 `subagent` 工具事件不再暴露完整任务 prompt。
- Phase 2：新增进程内 `SubAgentTaskManager`，统一工具支持 `run|spawn|list|get|cancel`；后台任务默认关闭，显式 `allow_background=true` 才能 spawn。任务结果有界、脱敏并复用现有 artifact；owner/session 隔离、终态任务与未消费通知的 TTL 自动清理、批次/任务取消、exactly-once 通知 drain 和创建时冻结的 observer 均已接入。
- 生命周期取消闭环：Coordinator 持有活动批次、组合取消令牌、暂停/关闭入口和单 deadline 有界等待；父 Run 取消会级联运行中与排队任务；`LocalToolAgent.close()` 超时后保留共享资源并在任务 idle 时自动完成关闭；工作区切换超时会保留旧工作区并拒绝提交；进程级 PluginRuntime 通过 Agent close callback 延迟到所有子任务退出后再关闭。
- 隔离边界：`fresh` 子执行使用独立 messages、独立预算和独立 Runtime；受控 Fork 仅在创建时复制一份已脱敏的父公开协议消息快照。二者都不修改父 `_history`、`_pending_user_text`、`_active_skills` 或 `_active_runtime_snapshot`；内部只读工具复用 Host 路径保护与 Plugin 工具 Hook，但不写父 Session 普通工具事件。

- Provider Fake Runtime 契约：补齐 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Gemini Generate Content 四种协议身份的 SubAgent 集成契约，覆盖 fresh 上下文与用量、只读工具往返、Runtime 内取消传播；测试只使用统一 Runtime 事件，不依赖真实 SDK 或网络。
- Phase 2 API/TUI 控制面：新增受鉴权的当前 Session `list/get/cancel` 路由；不提供远程创建入口。`/tasks`、`/task <task_id>`、`/task cancel <task_id>` 在终端和全屏 TUI 复用同一控制逻辑，TUI 可展示 queued/running/终态安全元数据。
- ApprovalBroker Core：新增 Agent 范围 FIFO 单确认 Broker，以 `ContextVar` 绑定 task/batch/角色来源，多个子任务同时请求时只展示一个确认；任务、批次、父取消、关闭和工作区切换会拒绝待决请求，活跃确认的迟到批准无效。用户策略已收窄为仅“所有删除意图”和“变更性 Git 操作”逐次确认；`git status`/`diff`/`log`/`show` 等只读查询和普通子任务分发不弹窗。公开 waiting/running 事件、终端/全屏来源提示、以及 API `PendingConfirmation` 的 additive 来源字段均已接入，且不传递完整 prompt。
- Verify profile：新增默认关闭的内置 `verify`，通过 `enable_verify_agent=true` 显式启用。它只获得 read_only 工具和子任务私有的 `verify_command`，后者只能选择固定的 `unit_tests`、`compileall`、`git_diff_check` 检查；Host 以静态 argv、`shell=False` 启动 Python/Git，不接受原始命令、Shell、路径、环境变量或网络参数。
- 1A-Full 后台审批控制面：API 服务按任务创建时的可信 Run/Session 来源维护受限的进程内审批队列和独立 SSE 流。父 Run 结束后，当前会话可查询并决议等待中的后台审批；任务、批次、服务取消和超时会拒绝请求，迟到批准无效。终端/全屏 TUI 继续复用单确认模态与安全来源提示，不开放远程创建任务或跨会话访问。
- Phase 3.1B（受控 Fork 与模型覆盖）：`allow_fork=true` 时，`context=fork` 在父 Agent 本回合开始时冻结公开 OpenAI 风格消息、完成凭据脱敏后再交给子循环；Fork 注入父基础提示与既定 boilerplate，仍不获得 `subagent` 工具。每个任务在排队前按“调用参数 `model` > `AgentDefinition.model` > 父模型”解析模型，并以独立 `ModelRuntimeManager` 执行；父 `/model` 只影响后续任务。模型、Profile 凭据和 Fork 原始 prompt 均不写入 Session、SSE、artifact 或公开结果。

已完成跨进程恢复（2026-07-16 补齐）：从父 Session additive 事件重建任务快照并导入 TaskManager，仅恢复 list/get 控制面可见性；非终态任务折叠为 `failed` + `SUBAGENT_INTERRUPTED`，不自动重跑，不恢复审批/Runtime/Fork/原始 prompt，也不注入通知队列。Phase 3.2 独立 Plugin dispatch context 已落地。Worktree 写隔离与 default-off 通用写 Agent 亦已落地。2026-07-17 又补齐 MCP/Skill/Memory 专项边界：模型任务参数不能注入工具、Skill、MCP Server 或 permission profile；read_only/standard 均不能获得 MCP、Skill 控制面或 `memory_write`；fresh 不继承父 Skill 状态，Fork 只消费创建时冻结的公开 Skill 上下文。最终安全审计进一步修复了三个生命周期缺口：待处理 Worktree 会阻止跨工作区切换；Plugin/Fork 上下文冻结发生在创建 Worktree 之前，登记失败会回滚 Git 资源；父 Session 归档/恢复会先取消旧会话子任务并在超时时保留当前会话。隐藏 reasoning 也通过统一 Runtime 契约证明不会进入公开结果或 artifact。

## 0. 结论

OmniCrawl 应采用“**主 Agent 唯一调度、子 Agent 独立执行、结果有界回传**”的主从式 SubAgent 架构，而不是群聊式多 Agent。

最终目标保留参考章节中的核心能力：

1. 用一个稳定的 `subagent` 工具承载任务分发，不为每个角色注册独立工具；
2. 支持“定义式专家”和“Fork 临时助手”两种创建模式；
3. 子 Agent 使用独立对话、取消令牌、Token 计数、任务状态和工具策略；
4. 父子共享工作区、安全基础设施和统一模型协议，但不共享可变 Agent 回合状态；
5. 提供有界并发、后台任务、状态查询、取消和结构化结果；
6. 所有子 Agent 工具调用继续经过 OmniCrawl 现有审批、Hook、路径保护、MCP 策略和脱敏链路。

但不能直接照搬参考实现。OmniCrawl 当前的工具审批、PluginManager 回合快照、Session、API 单活动 Run 和 TUI 单确认模态都有明确边界。因此建议分三阶段落地：

| 阶段 | 能力 | 关键限制 |
|---|---|---|
| Phase 1 | 定义式 SubAgent；同步、有界批量分发；只读/受限角色 | 深度固定为 1；不提供 Fork；不允许后台写操作 |
| Phase 2 | 后台任务、任务查询/取消、父取消级联、SSE/TUI 进度 | 人工审批串行；Memory 默认只读；结果摘要回填 |
| Phase 3 | Fork 模式、模型覆盖、可选 worktree 隔离 | 必须先完成父上下文快照、插件策略隔离和写冲突治理 |

第一版不应让多个线程共享同一个 `LocalToolAgent` 实例，也不应把子任务伪装成 API 顶层 Run。

---

## 1. 背景与问题

OmniCrawl 当前是单 Agent Harness。一次任务中的项目说明、对话历史、模型回复、工具调用和工具结果都进入同一条 Agent Loop。该模式适合连续完成一个目标，但在以下场景中会出现明显问题：

- 一个复杂任务需要探索、规划、实现、验证等不同角色；
- 多个彼此独立的子问题可以并行分析；
- 主对话已经积累大量工具结果，继续处理新子任务会产生上下文污染；
- 希望用更便宜或更快的模型完成代码搜索、资料整理等低风险工作；
- 希望验证 Agent 独立检查实现，避免主 Agent 被自己的实现思路影响；
- 长时间任务需要后台执行，同时允许主 Agent继续工作。

SubAgent 的价值不是简单“多开几个模型请求”，而是建立以下隔离：

```text
父 Agent：掌握用户目标、产品决策、最终写入和结果整合
  ├─ Explore：独立搜索代码，只返回证据
  ├─ Plan：独立制定方案，只返回计划
  └─ Verify：独立运行验证，只返回命令与结论
```

父 Agent 仍是唯一调度者和最终决策者，子 Agent 不直接与用户建立平级会话。

---

## 2. 设计目标与非目标

### 2.1 目标

1. **降低上下文污染**：子任务中间过程不进入父 Agent 主历史，父上下文只接收有界结果。
2. **稳定工具入口**：Agent 定义增减不改变工具列表和模型 Tool Schema 数量；统一 `subagent` 工具的角色枚举按当前配置动态收窄。
3. **角色化能力边界**：可通过 Markdown 定义角色、模型、工具范围和预算。
4. **安全继承而非安全绕过**：子 Agent 的能力不得超过父 Agent 和 Host 安全策略。
5. **有界并发**：限制深度、任务数、并发、超时和结果大小。
6. **可观察与可取消**：父 Session、TUI 和 API 能看到任务状态，父取消会级联所有子任务。
7. **Provider 无关**：继续支持 OpenAI Chat/Responses、Anthropic、Gemini 统一 Runtime。
8. **渐进式落地**：Phase 1 不改变 API 单顶层 Run、Session v1 格式和现有公共导出。

### 2.2 非目标

首期不实现：

- 多 Agent 群聊、投票、自由协商或自治团队；
- 分布式任务队列、跨机器执行和远程 Worker；
- 子任务作为独立 API Run 出现在顶层 Run 列表；
- 无限递归或由子 Agent 自行扩大权限；
- 默认并行修改同一工作区文件；
- 把 Plugin Worker、MCP Server 或 Skill 当成 SubAgent；
- 持久化隐藏推理或完整模型思维过程；
- 通过批准一次 `subagent` 调用自动批准子任务内所有副作用工具。

---

## 3. 参考章节的核心思想与 OmniCrawl 取舍

| 参考思想 | OmniCrawl 采用方式 | 项目化调整 |
|---|---|---|
| Agent 与 Tool 同构 | 注册统一 `subagent` 工具 | 工具名遵循项目现有 snake_case 约定 |
| 定义式专家 | Markdown + YAML frontmatter | 复用 Skill 的解析与诊断风格，但字段和语义独立 |
| Fork 临时助手 | Phase 3 支持父上下文快照 | 不直接复制父 `LocalToolAgent` 可变字段 |
| 子 Agent 后台运行 | Phase 2 引入 `SubAgentTaskManager` | 不复用外部进程 `BackgroundMonitorManager` |
| 多层工具过滤 | 全局禁用 + 运行模式 + 定义限制 + Host 安全策略 | MCP 不直通，仍按风险和审批策略过滤 |
| `dontAsk` 自动批准 | 不作为通用默认 | 当前 read_only 分发免确认；未来子工具只对删除意图和变更性 Git 操作逐次确认 |
| task-notification 回传 | 父 Session additive event + 有界上下文通知 | 高频 delta 不写入父 Session |
| Worktree 隔离 | Phase 3 可选 | 当前工作树有未提交修改时必须阻止自动创建/合并写入工作树 |

### 3.1 明确不照搬的两个点

#### MCP 工具不直接放行

参考实现将 MCP Tool 视为自带权限控制并跳过 SubAgent 过滤。OmniCrawl 的 MCP Tool 具有 `trusted/restricted/external` 风险级别，且默认保守确认。子 Agent 必须继续通过 Host 注册的 `ToolDefinition` 调用 MCP，不能直接访问底层连接，也不能因为进入子 Agent 而降低风险等级。

#### 不提供无条件 `dontAsk`

OmniCrawl 当前内置文件工具默认需要确认，但 SubAgent 不会因此获得无条件自动批准。设计采用“**受限分发 + 子工具风险控制**”：

- 当前 read_only SubAgent 分发及其内部只读调用免人工确认；默认关闭的 verify 仅开放固定 `verify_command` 检查，二者仍执行定义权限收窄、路径、参数、Hook 和脱敏校验；
- 写文件、外部网络和高风险 MCP 仍不在当前 profile 中，不能因为免确认而被放行；
- 后续 profile 中，删除意图和变更性 Git 操作必须逐次经过带任务来源的串行 `ApprovalBroker`；
- 未知 Git 子命令按变更性处理；父 Run 已结束的后台风险请求由 Core 安全拒绝，不能伪造无来源的确认。

---

## 4. 当前架构约束

### 4.1 `LocalToolAgent` 是单回合有状态对象

`omnicrawl/agent/core.py` 中的 `LocalToolAgent` 持有以下可变字段：

- `_history`、`_pending_user_text`、`_active_skills`；
- `_cancel_check`、`_reasoning_delta_callback`；
- `_active_runtime_snapshot`；
- 当前 Session、Memory、MCP、WorkspaceTools 和 PluginManager；
- 工作区切换及关闭生命周期。

`run_stream()` 会在一次回合中反复修改这些字段。因此禁止以下实现：

```python
# 禁止：同一个 Agent 实例并发递归进入 run_stream
executor.submit(parent_agent.run_stream, child_prompt, ...)
```

否则会造成父子历史混写、取消错配、Runtime 快照互相覆盖、Plugin turn 计划错乱和 Session 事件串扰。

### 4.2 现有工具链必须保留

当前工具调用链为：

```text
模型 Tool Call
  -> normalize_tool_call
  -> tool.call.before
  -> tool.approval.before
  -> Host 审批
  -> tool.approval.after
  -> tool.execute.before
  -> ToolDefinition.run
  -> tool.execute.after/error
  -> ToolResult 裁剪、脱敏和 Session 持久化
```

SubAgent 不能创建第二套绕过上述链路的工具执行器。

### 4.3 API 顶层只有一个活动 Run

`AgentAPIService` 当前按单 Agent、单活动顶层 Run 管理生命周期和确认请求。SubAgent 应作为父 Run 内的嵌套任务，不能直接复用顶层 Run ID 或破坏 `RUN_ACTIVE` 语义。

### 4.4 TUI 只有一个人工确认模态

全屏 TUI 当前使用单个 `ConfirmationScreen` 同步等待用户结果。多个子 Agent 同时请求确认会导致模态竞争。因此：

- Phase 1 只允许无需逐工具交互确认的只读/受限 profile；
- Phase 2 引入串行审批队列；
- 同一时刻最多展示一个确认，并显示 `task_id`、角色和任务摘要。

### 4.5 PluginManager 回合计划不能父子共用单槽

PluginManager 当前在主回合开始时冻结执行计划。子 Agent 不能直接重复调用父 Manager 的 `begin_turn/end_turn` 并覆盖父回合快照。Phase 1 只让外层 `subagent` 工具经过现有 Hook；Phase 2/3 如需子任务专用 Hook，应引入独立的不可变 dispatch context。

---

## 5. 总体架构

```text
┌──────────────────────────────────────────────────────────────┐
│ 用户 / TUI / HTTP API                                        │
└───────────────────────────┬──────────────────────────────────┘
                            │
┌───────────────────────────▼──────────────────────────────────┐
│ LocalToolAgent（父 Agent，唯一调度者）                         │
│  - 主对话历史 / Session / 最终决策                            │
│  - 统一工具审批、Hook、路径和脱敏                              │
│                                                              │
│  ToolDefinition: subagent                                    │
└───────────────────────────┬──────────────────────────────────┘
                            │
┌───────────────────────────▼──────────────────────────────────┐
│ SubAgentCoordinator                                          │
│  - AgentDefinitionRegistry                                   │
│  - SubAgentTaskManager                                       │
│  - 并发/预算/取消树                                           │
│  - 结果聚合与通知                                             │
│  - ApprovalBroker（Phase 2）                                  │
└──────────────┬──────────────────────┬────────────────────────┘
               │                      │
┌──────────────▼──────────┐ ┌────────▼─────────────────────────┐
│ SubAgentExecution A     │ │ SubAgentExecution B              │
│ - 独立 messages         │ │ - 独立 messages                  │
│ - 独立 Runtime 引用      │ │ - 独立 Runtime 引用              │
│ - 过滤后的 ToolDefinition│ │ - 过滤后的 ToolDefinition        │
│ - 独立取消/计数/状态      │ │ - 独立取消/计数/状态              │
└──────────────┬──────────┘ └────────┬─────────────────────────┘
               │                     │
               └──────────┬──────────┘
                          │
             ┌────────────▼────────────┐
             │ 结构化结果 / artifact 引用│
             └─────────────────────────┘
```

### 5.1 模块建议

为避免再次形成 God File，也避免拆成大量只调用一次的小文件，建议新增以下高内聚模块：

```text
omnicrawl/agent/subagents/
├── __init__.py          # 稳定公共导出
├── definitions.py       # 数据模型、frontmatter 解析、来源加载、内置角色
├── coordinator.py       # 统一工具入口、创建模式、预算、结果聚合
├── execution.py         # 独立 Agent Loop、Fork 上下文构建、工具过滤
└── tasks.py             # 后台状态机、通知、取消和快照
```

若 Phase 1 代码量较小，可先合并为：

```text
omnicrawl/agent/subagents.py
```

当单文件超过约 500–700 行或出现“定义加载、执行循环、后台状态”三个独立变化原因时，再按上述边界拆包。不要一开始创建十几个微型模块。

### 5.2 核心对象

| 对象 | 职责 |
|---|---|
| `AgentDefinitionRegistry` | 加载、解析、覆盖、诊断和查询 Agent 定义 |
| `SubAgentCoordinator` | 接收工具参数，规范化请求，控制并发/预算，创建执行实例 |
| `SubAgentExecution` | 持有单个子 Agent 的独立消息、工具、Runtime 引用和取消状态 |
| `SubAgentTaskManager` | 后台任务状态、结果、通知、取消和清理 |
| `ApprovalBroker` | Phase 2 串行人工审批，并附带子任务来源 |
| `SubAgentEventSink` | 将生命周期映射到父 Session、SSE 和 TUI，不让调度器依赖 UI |

---

## 6. Agent 定义

### 6.1 文件格式

Agent 定义采用 YAML frontmatter + Markdown body：

```markdown
---
name: security-reviewer
description: 只读代码安全审查，按严重程度报告证据和修复建议
tools:
  - read_file
  - search_text
disallowedTools:
  - subagent
  - write_file
  - replace_text
  - bash
  - powershell
model: inherit
permissionMode: delegated-read-only
background: false
skills: []
mcpServers: []
---

你是 OmniCrawl 的代码安全审查子 Agent。

只读取和分析，不修改文件，不向用户提问。
最终输出必须包含严重程度、文件路径、行号、触发条件和修复建议。
```

Markdown body 是子 Agent 的 system prompt，不是注入父 Agent 当前回合的 Skill 指令。

### 6.2 数据模型

```python
@dataclass(frozen=True)
class AgentDefinition:
    name: str
    description: str
    system_prompt: str = ""
    tools: tuple[str, ...] = ()
    disallowed_tools: tuple[str, ...] = ()
    model: str = "inherit"
    permission_mode: str = "delegated-read-only"
    background: bool = False
    isolation: str = "shared"
    skills: tuple[str, ...] = ()
    mcp_servers: tuple[str, ...] = ()
    source_path: Path | None = None
    source: str = "builtin"
```

### 6.3 加载来源与优先级

建议采用“离项目越近优先级越高”：

```text
1. 项目级：<workspace>/.omnicrawl/agents/*.md
2. 兼容项目级：<workspace>/.agents/agents/*.md（可选，只读兼容）
3. 用户级：~/.omnicrawl/agents/*.md
4. 内置级：OmniCrawl 包内定义
5. 插件级：已批准插件声明的 Agent 定义
```

同名定义由高优先级覆盖低优先级，并记录 winner/loser 诊断。插件不能覆盖内置、用户或项目定义。

项目当前 Skill 使用 `.claude/skills/`，但 Agent 是 OmniCrawl 自身执行配置，建议使用 `.omnicrawl/agents/`，避免把 Claude 专用目录继续扩展为通用运行配置。

### 6.4 解析与热重载

- 复用 `omnicrawl/extensions/skill.py` 的 frontmatter 解析、UTF-8、名称校验、符号链接去重和碰撞诊断思路；
- 不直接复用 `Skill` 数据类，避免两套语义混合；
- Phase 1 在 Agent 启动或工作区切换时发现；
- 每次按名称取定义时可检查文件 mtime 并安全热重载；
- 热重载失败时保留上一次有效定义并记录 warning；
- 已启动任务使用创建时的不可变定义快照，不受运行中改文件影响。

---

## 7. 内置 Agent

### 7.1 `explore`

用途：代码结构、符号、调用链和配置探索。

```yaml
name: explore
description: 快速只读探索代码和文档，返回文件与行号证据
tools: [list_files, read_file, search_text]
disallowedTools: [subagent, write_file, replace_text, bash, powershell, monitor, memory_write]
permissionMode: delegated-read-only
```

### 7.2 `plan`

用途：结合项目现状制定实施计划，不修改文件。

```yaml
name: plan
description: 只读软件架构与实施计划专家
tools: [list_files, read_file, search_text, memory_search, memory_read]
disallowedTools: [subagent, write_file, replace_text, bash, powershell, monitor, memory_write]
permissionMode: delegated-read-only
```

### 7.3 `verify`

用途：运行构建、测试、类型检查并给出 PASS/FAIL/PARTIAL。

`verify` 已作为默认关闭的受控 profile 落地。只有 `subagents.enable_verify_agent=true` 时才可执行；它不开放 Bash、PowerShell 或任意命令文本，而是通过子任务私有 `verify_command` 选择 Host 固定的检查，并以 `shell=False` 直接启动参数数组。

```yaml
name: verify
description: 运行固定的测试、编译和 Git 差异检查并报告可复现证据
tools: [list_files, read_file, search_text, verify_command]
background: true
disallowedTools: [subagent, write_file, replace_text, bash, powershell, monitor, memory_write]
permissionMode: explicit-command-allowlist
```

首期固定检查为 `unit_tests`、`compileall`、`git_diff_check`。它们均不包含删除或变更性 Git 操作，因此不需要人工确认；后续若新增高风险检查，仍必须经过 ApprovalBroker，不能借 `verify` 绕过审批。

### 7.4 `general-purpose`

通用写能力 Agent 风险最高。Phase 1 不默认启用；Phase 3 仅在以下条件满足时开放：

- `max_depth=1`；
- 写工具仍经过人工审批；
- 多写者使用 worktree 隔离；
- 父 Agent 是唯一合并/应用结果的写者；
- 父取消、关闭和工作区切换能可靠回收。

---

## 8. 统一 `subagent` 工具

### 8.1 为什么使用统一工具

- Agent 类型是动态加载的，不能为每个角色注册一个 Tool；
- 工具列表和系统提示保持稳定；
- 模型只需学习一个任务分发入口；
- 查询、取消和后台任务可沿用项目已有 `monitor` 的 action 风格；
- 不依赖模型一次返回多个 Tool Call 才能并行。

### 8.2 Phase 1 Schema

Phase 1 只提供同步、有界批量执行：

```json
{
  "type": "object",
  "properties": {
    "action": {
      "type": "string",
      "enum": ["run"]
    },
    "tasks": {
      "type": "array",
      "minItems": 1,
      "maxItems": 4,
      "items": {
        "type": "object",
        "properties": {
          "description": {"type": "string", "minLength": 1},
          "prompt": {"type": "string", "minLength": 1},
          "subagent_type": {
            "type": "string",
            "enum": ["当前配置下可执行的角色名称"]
          },
          "context": {"type": "string", "enum": ["fresh"]},
          "model": {"type": "string"}
        },
        "required": ["description", "prompt", "subagent_type"]
      }
    },
    "max_concurrency": {
      "type": "integer",
      "minimum": 1,
      "maximum": 4
    },
    "fail_fast": {"type": "boolean"}
  },
  "required": ["action", "tasks"]
}
```

Phase 1 不允许模型指定任务 ID、工作区路径、工具列表、权限模式或任意 system prompt。描述和提示词由 Host 仅做非空校验；原始 prompt 不进入公开生命周期事件，描述在各公开出口按边界截断。

### 8.3 Phase 2 Schema 扩展

```text
action: run | spawn | list | get | cancel | apply_worktree | discard_worktree | list_worktrees
```

- `run`：同步等待一个或多个任务，返回有序结果；
- `spawn`：后台启动，立即返回 batch/task ID；
- `list`：列出当前父 Session 的任务；
- `get`：获取一个任务的状态和结果；
- `cancel`：取消一个任务或批次。

### 8.4 Phase 3 Schema 扩展

已落地的单任务字段：

```json
{
  "context": "fresh | fork",
  "model": "models.yaml key / alias / profile/model_id / inherit / default（继承父模型）"
}
```

`context=fork` 需显式设置 `subagents.allow_fork=true`；`model` 的优先级高于定义中的 `model`。继承父模型时应省略该字段；显式 `inherit` 与裸 `default` 均表示使用父模型，后者用于兼容函数调用模型为可选字段生成的占位值，绝不作为 Provider 的 wire model 发送。模型值复用现有 Catalog/Profile 解析，不接受调用方传入凭据、Runtime、工具列表或任意 system prompt。

任务级 `isolation` / `run_in_background` / `name` 仍不由工具参数覆盖，继续只读定义 frontmatter。
Worktree 结果由父 Agent 通过顶层 action 显式处理（已落地）：

```json
{
  "action": "apply_worktree | discard_worktree | list_worktrees",
  "task_id": "optional-task-or-session-id",
  "branch": "optional-branch-name",
  "strategy": "checkout | merge",
  "cleanup": false,
  "remove_branch": true
}
```

Fork 模式由 `context="fork"` 明确表达，不依赖“省略 `subagent_type`”这种隐式行为。显式字段更利于 Schema 校验、日志和后续兼容。

### 8.5 返回格式

```json
{
  "batch_id": "batch-7f3c...",
  "status": "completed",
  "results": [
    {
      "task_id": "task-a12d...",
      "description": "检查 Session 恢复边界",
      "agent_type": "explore",
      "status": "completed",
      "summary": "...",
      "evidence": [
        {"path": "omnicrawl/state/session.py", "line": 438, "note": "..."}
      ],
      "artifacts": [],
      "usage": {
        "input_tokens": 1200,
        "output_tokens": 380,
        "cached_input_tokens": 0,
        "model_turns": 3,
        "tool_calls": 6
      },
      "error": null
    }
  ]
}
```

结果必须按输入任务顺序返回，而不是按完成顺序。完整输出过大时写入受控 artifact，`summary` 仅保留有界摘要和引用。

---

## 9. 两种创建模式

### 9.1 定义式

定义式用于固定角色、固定能力边界的任务：

```text
父 Agent
  -> subagent(action=run, subagent_type=explore)
  -> Registry 解析 AgentDefinition
  -> 创建空白子对话
  -> 注入定义 body + 项目规范 + 任务
  -> 按定义过滤工具
  -> 独立 Agent Loop
  -> 返回结构化结果
```

子 Agent 不继承父完整对话，只接收：

- 角色 system prompt；
- 当前工作区项目规范；
- 父 Agent 显式传入的任务 prompt；
- 必要的公开上下文摘要；
- 允许使用的 Skill 元数据；
- 过滤后的工具描述。

### 9.2 Fork 式

Fork 用于和父任务高度相关、需要继承已讨论背景的临时助手。Phase 3.1B 已以默认关闭的只读/受控 verify 形式实现；通用写 Fork 仍未开放。

Fork 上下文应复制：

- 父 Agent 当前回合开始前的稳定 `_history`；
- 本轮已构造且协议完整的公开 messages 快照；
- 当前项目说明、模型选择和 Skill 元数据；
- 子任务指令和 Fork boilerplate。

不得复制：

- `_cancel_check`、回调函数、锁、线程、SDK stream；
- 父 SessionStore 写入对象；
- 父 `_active_runtime_snapshot` 可变引用字段；
- PluginManager 的单槽 turn 状态；
- 隐藏推理；
- 未经脱敏的凭据和配置。

### 9.3 Fork Boilerplate

```text
<fork_boilerplate>
你是从 OmniCrawl 主 Agent 派生的工作进程，不是面向用户的主助手。
不可协商规则：
1. 不得再次创建 SubAgent。
2. 不得向用户提问；遇到需要产品或权限决策的问题应停止并报告。
3. 严格限制在分配任务范围内。
4. 只使用 Host 提供的工具，不得绕过审批、路径和安全策略。
5. 最终只返回结构化工作报告，不输出隐藏推理。
</fork_boilerplate>
```

### 9.4 防嵌套

Phase 1/2：`max_depth=1`，子 Agent 工具列表中没有 `subagent`。

Phase 3 仍默认深度 1。若未来开放递归，必须同时限制：

- 最大深度；
- 单批任务数；
- 整棵任务树总任务数；
- 总并发数；
- 总 Token；
- 总墙钟时间；
- 单节点工具调用数。

---

## 10. 独立执行循环

### 10.1 不能复制完整 `run_stream()`

直接复制 `LocalToolAgent.run_stream()` 会产生两套逐渐分叉的 Agent Loop。建议从主循环中抽出一个高内聚的内部执行器：

```python
@dataclass(frozen=True)
class AgentLoopLimits:
    max_model_turns: int | None = None
    max_tool_calls: int | None = None
    timeout_seconds: float | None = None

@dataclass
class AgentLoopResult:
    final_text: str
    model_turns: int
    tool_calls: int
    usage: TokenUsage
    messages: list[dict[str, Any]]

class AgentLoopRunner:
    def run(
        self,
        *,
        messages: list[dict[str, Any]],
        request_reply: Callable[..., AgentModelReply],
        execute_tool: Callable[..., ToolResult],
        limits: AgentLoopLimits,
        cancel_check: Callable[[], None],
        event_sink: Callable[[str, dict[str, Any]], None],
    ) -> AgentLoopResult:
        ...
```

主 Agent 和 SubAgent 复用同一模型→工具→观察循环，但主 Agent 继续在外层处理：

- 用户消息和 PromptHistory；
- Session 主消息；
- TUI 流式输出；
- `turn.start/end/error/cancelled`；
- `_history` 更新；
- 当前活动 Skill。

子 Agent 外层处理：

- 任务状态；
- 独立消息；
- 预算；
- 结构化结果；
- 子任务事件。

### 10.2 完成条件

子 Agent 满足任一条件结束：

- 模型不再返回工具调用，返回最终文本；
- 超过 Host 为子任务配置的 timeout；
- 父取消、任务取消、关闭或工作区切换；
- 审批被拒绝且任务无法继续；
- 模型或工具发生不可恢复错误。

超时不是普通成功。结果状态应为 `failed` 或 `partial`，并包含明确错误码。

---

## 11. 工具过滤与权限

### 11.1 过滤原则

每层只能收窄权限，不能扩大：

```text
父 Agent 当前工具集
  ∩ Host 全局 SubAgent 允许范围
  ∩ 运行模式范围（同步/后台）
  ∩ AgentDefinition.tools 白名单（若有）
  - AgentDefinition.disallowedTools
  - 全局禁止工具
  - 当前风险策略禁止工具
```

### 11.2 全局禁止工具

所有普通 SubAgent 默认禁止：

- `subagent`：防止递归；
- `monitor`：避免子 Agent 再创建长期外部任务；
- `memory_write`：避免并发写 Memory；
- 工作区切换、Session 管理、模型切换、插件管理等控制面能力；
- 直接面向用户的确认/对话工具；
- 任何能修改父 Agent 全局模式的工具。

### 11.3 工具 profile

建议 Host 内置 profile，而不是让模型直接提交任意 allowlist：

| Profile | 允许能力 | 典型角色 |
|---|---|---|
| `read_only` | 列目录、读文件、搜索、只读 Memory、可信只读 MCP Resource | explore、plan |
| `verify` | read_only + 子任务私有 `verify_command` 固定检查（静态 argv、无 Shell） | verify |
| `standard` | 读写文件、命令、MCP Tool，逐项审批 | Phase 3 general-purpose |
| `worktree_writer` | standard，但工作区绑定独立 worktree | Phase 3 并行写任务 |

### 11.4 MCP 策略

- MCP Tool 不绕过过滤；
- `external` 默认不进入 SubAgent 工具集；
- `restricted` 需要显式 Agent 定义、配置允许和审批；
- `trusted` 仍需检查其 `requires_confirmation` 与操作语义；
- MCP Resource/Prompt 可按只读 profile开放；
- 审计增加可选 `parent_session_id/task_id` 字段；
- 子 Agent 不能直接持有 `_StdioMCPConnection`。

### 11.5 审批模型

#### 当前委派级策略：受限任务免重复确认

当前开放 `read_only` profile，以及默认关闭的 `verify` profile。`subagent` 分发、内部普通只读调用和 `verify_command` 固定检查均不需要重复人工确认；Host 仍执行定义权限收窄、参数、路径、Hook、脱敏和预算校验。`verify_command` 不接受原始命令文本，且首期检查不含删除或变更性 Git 操作。

用户已将未来子工具的人工确认范围收窄为：

- 所有删除意图（专用删除工具、`git rm`/`git clean`、以及命令中的 `rm`、`del`、`Remove-Item`、`find -delete` 等）；
- 变更性 Git 操作（如 `add`、`commit`、`switch`、`merge`、`rebase`、`reset`、`push`）；
- 无法证明为只读的未知 Git 子命令按变更性处理。

`git status`、`git diff`、`git log`、`git show` 等明确只读 Git 查询免确认。该策略不扩大 read_only 或受控 verify 权限；通用命令与写 profile 仍未启用。

#### Phase 2/3：ApprovalBroker

```python
@dataclass(frozen=True)
class ApprovalRequest:
    batch_id: str
    task_id: str
    agent_label: str
    description: str
    tool_name: str
    public_arguments: dict[str, Any]
    risk_summary: str
```

ApprovalBroker 规则：

1. 全局只有一个人工确认在展示；
2. 请求按创建时间排队；
3. 父取消后队列中的请求自动拒绝；
4. 已终态任务的迟到批准无效；
5. TUI/API 显示任务来源；
6. 批准一个工具不批准后续工具；
7. Plugin guard 只能拒绝，不能代替用户批准。

同步任务继续复用当前活动父 Run 的确认处理器。对于已结束父 Run 的后台风险操作，1A-Full
在 API 服务内依据任务创建时冻结的可信 Run/Session 来源创建受限的进程内确认记录；客户端只能在
当前会话通过 `GET /api/v1/subagents/confirmations` 查询、通过
`POST /api/v1/subagents/confirmations/{confirmation_id}` 决议，并可订阅
`GET /api/v1/subagents/events`。任务、批次、服务取消和超时都会拒绝该记录，迟到批准无效；
来源未知、跨会话或跨进程的请求仍安全拒绝。终端与全屏 TUI 复用既有单确认模态，并显示任务来源。

---

## 12. 并发与写冲突

### 12.1 并发控制

```python
@dataclass(frozen=True)
class SubAgentGlobalLimits:
    max_concurrency: int = 2
    max_tasks_per_batch: int = 4
    max_depth: int = 1
    default_timeout_seconds: float = 3600
    model_request_concurrency: int = 2
```

需要至少两个信号量：

- 任务执行信号量：限制同时运行的子 Agent 数量；
- 模型请求信号量：限制同时访问模型 Provider 的请求数。

不能仅根据 `ModelCapabilities.parallel_tool_calls` 判断 Host 能否并行。该字段描述模型是否可能一次返回多个 Tool Call，不等价于 Provider SDK、Runtime 或网关能安全承载任意并发。

### 12.2 Phase 1 并发范围

只允许只读/受限任务并发。`subagent` 工具在父 Agent 的同批 Tool Call 中视为串行屏障，由其内部 Coordinator 负责 fan-out，避免父 Agent 同时触发多个互不知情的调度器批次。

### 12.3 写任务规则

共享工作区中默认只允许一个写者：

- 父 Agent 是唯一写者；或
- 一个获批的前台 SubAgent 是唯一写者；
- 多个写子 Agent 必须使用独立 worktree；
- 子 Agent 完成后只返回分支、diff 和验证证据；
- 父 Agent 决定是否合并或应用，不自动 merge/push/rebase/reset。

当前工作树不干净时，不得静默创建、删除或合并 worktree。必须显示现有修改和冲突风险。

---

## 13. 后台任务

### 13.1 为什么不复用 Monitor

`BackgroundMonitorManager` 管理的是外部命令子进程和日志流；SubAgent 是进程内模型/工具状态机。两者的取消、结果、Token、审批和 Session 语义不同，不应混在同一个 Manager。

### 13.2 状态模型

```text
queued
  -> running
  -> waiting_approval
  -> running
  -> completed | partial | failed | cancelled | timed_out
```

终态不可逆。

```python
@dataclass
class SubAgentTaskState:
    task_id: str
    batch_id: str
    parent_task_id: str | None
    parent_session_id: str
    agent_type: str
    description: str
    status: str
    created_at: float
    started_at: float | None = None
    finished_at: float | None = None
    progress: SubAgentProgress | None = None
    result: SubAgentResult | None = None
    error_code: str = ""
    error_message: str = ""
```

所有计时使用 `time.monotonic()` 计算耗时，展示时间另存 UTC wall clock。

### 13.3 通知

后台任务完成后生成有界通知：

```xml
<subagent-notification task-id="task-..." status="completed">
  <agent>explore</agent>
  <description>检查 Session 恢复边界</description>
  <summary>...</summary>
  <artifact-ref>...</artifact-ref>
</subagent-notification>
```

注入时机：

- 父 Agent 仍在当前 Tool Loop：下一次模型请求前 drain；
- 父 Agent 当前回合已结束：下一次用户回合构造上下文时 drain；
- TUI/API：可实时显示终态事件，但不自动开启新模型回合。

通知必须去重。TaskManager 记录 `notified_at` 或消费游标，不能每轮重复注入同一结果。

### 13.4 取消与关闭

以下动作必须级联取消所有未终态子任务：

- 用户取消父 Run；
- `LocalToolAgent.close()`；
- 工作区切换；
- API service 关闭；
- 父 Session 被归档/恢复到其他会话；
- 任务或批次显式 cancel。

取消流程：设置取消令牌 → 拒绝新工具 → 取消审批等待 → 等待有界时间 → 标记终态 → 再关闭 Runtime/MCP/临时资源。

---

## 14. 上下文、Skill、Memory 与 Session

### 14.1 上下文共享矩阵

| 内容 | 定义式 | Fork | 说明 |
|---|---|---|---|
| 父完整历史 | 否 | 是，公开且协议完整的快照 | Fork Phase 3 |
| 项目 AGENTS.md | 是 | 是 | 使用当前工作区规则 |
| 子 Agent system prompt | 是 | Fork boilerplate + 父基础提示 | 不修改父提示 |
| Skill 元数据 | 可选 | 继承快照 | 正文按需读取 |
| Memory 搜索/读取 | 可选，只读 | 可选，只读 | 默认禁写 |
| MCP Registry | 共享能力描述 | 共享能力描述 | 调用仍走 Host |
| Plugin Runtime | 不新建 | 不新建 | 使用受控 dispatch context |
| 工作区文件 | 共享 | 共享 | 写任务可选 worktree |
| Session 主历史 | 不写普通消息 | 不写普通消息 | 只写 subagent 生命周期事件 |
| Token 统计 | 独立任务统计 | 独立任务统计 | 同时汇总到父 Run usage |

### 14.2 Skill

Skill 是指令与工作流扩展，SubAgent 是执行单元。两者关系为：

- AgentDefinition 可声明 `skills`；
- 子 Agent 获得 Skill 元数据，按现有渐进式披露读取正文；
- 不把 SkillManager 改造成 Agent Loader；
- Skill fork 若未来实现，应复用 SubAgentExecution 底座，而不是再造一套 Agent Loop。

### 14.3 Memory

- Phase 1/2 子 Agent 默认只能 `memory_search/read/expand_related`；
- `memory_write` 由父 Agent 汇总后串行执行；
- 子 Agent 结果中可返回 `memory_candidates`，但不直接落盘；
- 后续若允许并发 Memory 写，必须先为 `MemoryStore` 增加锁和冲突合并测试。

### 14.4 Session

Phase 1/2 不为每个子任务创建普通 Session，避免污染最近会话列表。向父 Session 增加 additive 事件：

```text
subagent_batch_created
subagent_task_queued
subagent_task_started
subagent_task_waiting_approval
subagent_task_completed
subagent_task_partial
subagent_task_failed
subagent_task_cancelled
subagent_task_timed_out
```

这些事件不加入 `MODEL_CONTEXT_EVENT_TYPES`，恢复父对话时不会自动把高频进度喂给模型。`subagent_task_failed` 的 `SUBAGENT_MODEL_ERROR` 只保存错误分类和有限运行元数据，不保存原始异常文本、凭据或完整请求内容；父 Agent 不应仅更换角色重复调用，除非模型配置、网络/运行环境已变化或用户明确要求重试。

完整子任务输出如需持久化，写入父 Session 管控的 artifact 或：

```text
.agent_sessions/artifacts/<session_id>/subagents/<task_id>.json
```

要求：

- UTF-8、版本号、原子写；
- 路径不能逃逸；
- 写前脱敏；
- 大结果分级；
- 不存隐藏推理；
- 父事件只保存摘要和 artifact 引用。

---

## 15. 模型选择与 Runtime

每个 `session_started` 事件还记录 `payload.runtime`：OmniCrawl 版本、进程启动时间、关键源码文件大小/修改时间、已加载模块和源码指纹。该信息用于判断会话是否由修改前的长驻进程创建，不包含密钥或模型请求正文。

### 15.1 选择优先级

```text
调用参数 model（Phase 3）
  > AgentDefinition.model
  > 父 Agent 当前模型
```

模型值应通过当前 `models.yaml` / Profile / Catalog 解析，不硬编码 `haiku/sonnet/opus`。可在内置定义中使用项目模型 key 或 alias；解析失败返回可用模型提示。

### 15.2 Runtime 隔离

子 Agent 不共享父 `_active_runtime_snapshot` 字段。可选实现：

1. **Phase 1 推荐**：每个任务从父 RuntimeManager 获取独立引用快照，并通过模型请求信号量限制并发；
2. Provider SDK 并发不明确时：为子任务创建独立 Runtime；
3. 只有验证过 Adapter/SDK client 线程安全后，才允许共享底层 client/连接池。

Phase 3.1B 采用第 2 种：所有完整 `LLMConfig` 子任务在创建时复制解析后的 Profile/Descriptor，并在执行期创建、引用和关闭自己的 `ModelRuntimeManager`；遗留最小测试夹具保留旧 Runtime 注入兼容路径。父 `/model` 因而只影响之后新建的任务，不能改变已经排队或运行的子任务。

父 Agent 当前回合和所有子任务结束前，不允许切换当前模型，或将模型切换语义明确为“只影响后续新任务”。当前实现采用后者：任务创建时冻结模型快照，运行中 `/model` 只影响下一回合/新任务。

### 15.3 子任务资源边界

当前实现由 Host 统一控制子任务超时、任务批次上限、任务并发、模型请求并发和结果摘要大小；不再为角色或全局配置注入模型回合数、工具调用数或 Token 配额。循环执行器仍保留可选的回合/工具预算字段，供其他受控调用方使用。

```python
@dataclass(frozen=True)
class SubAgentLimits:
    timeout_seconds: float = 3600.0
```

---

## 16. Plugin Hook

### 16.1 Phase 1

`subagent` 是普通内置工具，自然触发现有：

- `tool.call.before`；
- `tool.approval.before/after`；
- `tool.execute.before/after/error`。

因此 Phase 1 不新增 Plugin Core Hook，避免扩大 manifest、权限、Patch allowlist 和失败策略矩阵。

### 16.2 Phase 2/3 可选 Hook

只有出现明确插件需求时新增：

| Hook | 模式 | 能力 |
|---|---|---|
| `subagent.task.before` | guard/transform | 可缩小超时、结果边界或修改标签；不能扩大工具/权限 |
| `subagent.task.after` | observe/notify | 观察脱敏摘要和 usage |
| `subagent.task.error` | notify | 观察错误分类 |
| `subagent.task.cancelled` | notify | 观察取消原因 |

每个任务使用创建时冻结的 Plugin 执行计划。新增 Hook 必须同步更新：

- `CORE_HOOKS`；
- `HOOK_ALLOWED_MODES`；
- `HOOK_PATCH_ALLOWLIST`；
- `HOOK_POLICIES`；
- manifest 权限；
- 审计与协议测试。

---

## 17. API 与 SSE

### 17.1 顶层 Run 保持不变

- 一个 API 服务仍只有一个活动顶层 Agent Run；
- SubAgent task 归属于父 `run_id`；
- 父 Run 终态前，同步任务必须结束；
- 后台任务若允许跨父回合存在，仍归父 Session，不创建新的顶层 Run。

### 17.2 SSE 事件

当前已公开的安全事件包括：

```text
subagent.batch.created
subagent.task.queued
subagent.task.started
subagent.task.running
subagent.task.waiting_approval
subagent.task.approval_cancelled
subagent.task.completed
subagent.task.failed
subagent.task.cancelled
subagent.confirmation.required
subagent.confirmation.resolved
subagent.confirmation.expired
subagent.confirmation.cancelled
```

事件公共字段：

```json
{
  "run_id": "run-...",
  "session_id": "...",
  "batch_id": "batch-...",
  "task_id": "task-...",
  "parent_task_id": null,
  "agent_type": "explore",
  "description": "...",
  "status": "running",
  "sequence": 12
}
```

进度事件只包含：状态、工具名、脱敏参数摘要、工具计数、Token usage 和耗时。不得发送隐藏推理、完整工具输出或 HTML 正文。父 Run 内的事件仍通过 `/runs/{run_id}/events` 发送；后台任务可跨越父 Run 时，API 服务会按创建时冻结的 Session 额外写入 `/subagents/events`，并附带安全的 `parent_run_id`，不会把旧任务投递到后续无关 Run。

### 17.3 确认模型兼容

`PendingConfirmation` 增加可选字段：

```text
task_id?: string
agent_label?: string
batch_id?: string
```

旧客户端忽略新字段仍能工作。

### 17.4 当前 API 控制面

```text
GET    /api/v1/subagents
GET    /api/v1/subagents/events
GET    /api/v1/subagents/confirmations
POST   /api/v1/subagents/confirmations/{confirmation_id}
GET    /api/v1/subagents/{task_id}
POST   /api/v1/subagents/{task_id}/cancel
```

这些接口只操作当前服务/Session 内任务，不提供任意新建远程 SubAgent 的无鉴权入口。后台审批记录仅存在于本进程：任务、批次、服务取消或超时会拒绝请求，服务重启不恢复；来源未知或当前会话外的确认统一不可见。该限制避免把 Session 切换或过期任务变成可被其他会话接管的风险入口。

---

## 18. TUI 交互

### 18.1 展示方式

SubAgent 在对话区按 `batch_id` 聚合为一棵进度树，并按 `task_id` 原地更新节点：

```text
◇ 并行子任务  1/3 完成
├─ ✓ 审查 Agent 变更  explore · 完成 · 00:18
├─ ● 审查全屏 TUI 变更  explore · 运行中 · 00:07
└─ ○ 运行回归测试  verify · 等待中
```

树节点只展示安全元数据：Agent 类型、有界描述、等待/运行/等待审批/完成/失败/取消状态和耗时；不保存或展示原始 prompt、完整结果、Token 明细、最近工具或原始异常。节点不提供独立折叠或交互控制，任务查询和取消继续使用 `/tasks`、`/task` 命令。

### 18.2 Slash Command

参考章节倾向让主 Agent 自己调用 Task 工具。OmniCrawl 已有丰富斜杠命令，为可操作性可补充只读/控制命令：

```text
/tasks
/task <task_id>
/task cancel <task_id>
```

自然语言仍可工作，因为模型可以调用 `subagent(action=list|get|cancel)`。

### 18.3 取消

- `Esc` 取消当前父回合及所有后代；
- 单任务取消通过 `/task cancel`；
- 取消后 UI 立即显示 cancelling，终态到达后显示 cancelled；
- 不允许取消后继续接受审批或执行新工具。

---

## 19. 配置

建议在 `config.yaml` 增加：

```yaml
subagents:
  enabled: false
  max_depth: 1
  max_concurrency: 2
  max_tasks_per_batch: 4
  default_timeout_seconds: 3600
  model_request_concurrency: 2
  allow_background: false
  allow_fork: false
  allow_shared_workspace_writes: false
  enable_verify_agent: false
  verify_command_timeout_seconds: 120
  task_retention_minutes: 60
  result_summary_chars: 6000
```

环境变量只允许关闭能力或收紧限制，不允许扩大权限。例如：

```text
OMNICRAWL_SUBAGENTS_ENABLED
OMNICRAWL_SUBAGENT_MAX_CONCURRENCY
OMNICRAWL_SUBAGENT_TIMEOUT_SECONDS
OMNICRAWL_SUBAGENT_VERIFY_AGENT_ENABLED
OMNICRAWL_SUBAGENT_VERIFY_TIMEOUT_SECONDS
```

初次发布默认 `enabled: false`，完成验证后再考虑默认开启只读 Phase 1。

---

## 20. 错误模型

建议统一错误码：

| 错误码 | 含义 |
|---|---|
| `SUBAGENT_DISABLED` | 子 Agent 功能未启用 |
| `AGENT_TYPE_NOT_FOUND` | 定义不存在 |
| `AGENT_DEFINITION_INVALID` | 定义文件无效 |
| `SUBAGENT_LIMIT_EXCEEDED` | 深度、任务数、并发或超时边界超限 |
| `SUBAGENT_PERMISSION_DENIED` | 委派或内部工具审批被拒绝 |
| `SUBAGENT_TIMEOUT` | 超时 |
| `SUBAGENT_CANCELLED` | 被父任务、用户或系统取消 |
| `SUBAGENT_MODEL_ERROR` | 模型请求失败；子任务不自动重试，结构化错误直接交还父 Agent。错误结果会附带脱敏的 `diagnostic`：`category`、`exception_type`、`provider`、`status_code`、`retryable`、`model_selection`（逻辑模型 key）、`wire_model`（实际发送的模型 ID） |
| `SUBAGENT_TOOL_ERROR` | 工具错误导致任务失败 |
| `SUBAGENT_PARTIAL` | 已有部分结果但未完成 |
| `SUBAGENT_WORKSPACE_CHANGED` | 运行中工作区已切换 |
| `SUBAGENT_RESULT_TOO_LARGE` | 结果超限且 artifact 写入失败 |

批量任务默认 `fail_fast=false`：一个任务失败不取消其他任务。只有 Host 安全错误、父取消或工作区切换才强制取消整批。Host 在工具 Schema 和 Coordinator 参数入口执行两层角色校验；Schema 只暴露当前配置可执行的角色，未知或当前权限开关未启用的角色不会进入线程池。每次子任务模型请求只尝试一次，失败后由父 Agent 决定是否改用自身能力继续，不自动再次消耗子任务模型配额。

OpenAI SDK Client 默认使用 `trust_env=true`，在 Windows 上会隐式读取系统代理。OmniCrawl 的 OpenAI Client 工厂和兼容入口统一使用 `httpx.Client(trust_env=False, follow_redirects=True)`，避免本地代理协议不匹配导致连接阶段的 `SSLEOFError`；当前 Provider 配置没有显式代理字段，因此不继承系统代理。

---

## 21. Session、审计与隐私

### 21.1 父 Session 记录

每个生命周期事件记录：

- batch/task/parent task ID；
- Agent 类型、定义来源和模型 key；
- 状态和时间；
- 预算与 usage；
- 工具名和脱敏摘要；
- 结果摘要或 artifact 引用；
- 错误码和公开错误信息。

### 21.2 不记录

- API Key、Token、Cookie、密码；
- 完整 `config.yaml`/`models.yaml`；
- 隐藏推理；
- 未裁剪的大型工具输出；
- 未经授权的用户文件全文；
- Plugin Worker 原始协议消息。

### 21.3 保留策略

- 运行中和近期终态保存在 TaskManager；
- TaskManager 自己按最早到期时间唤醒，默认 60 分钟后自动清理内存任务与未消费终态通知，不依赖后续查询；
- 父 Session 事件保留摘要；
- artifact 按 Session 清理和归档策略处理；
- 应用关闭前将未终态任务写为 cancelled/interrupted。

---

## 22. 实施计划

### Phase 0：基线与可复用执行循环

1. 锁定当前 `run_stream()`、工具审批、Session、API、TUI 行为测试；
2. 抽取 `AgentLoopRunner`，主 Agent 行为保持不变；
3. 为模型轮次、工具次数、取消和结果建立可测试接口；
4. 不注册 SubAgent 工具。

验收：全量测试不减少，TUI/API 行为无变化。

### Phase 1：定义式同步 SubAgent

1. 新增 AgentDefinition、解析、加载、诊断和内置 explore/plan；
2. 新增 `SubAgentCoordinator`；
3. 注册 `subagent(action=run)`；
4. 仅开放 fresh context、read_only profile；
5. 支持 1–4 个任务、有界并发、有序聚合；
6. 父 Session 写 additive 事件；
7. TUI/API 至少能显示开始和完成；
8. 配置默认关闭。

验收：子任务上下文隔离，无法写文件、执行命令或再次分发。

### Phase 2：后台、审批与任务管理

1. 新增 `SubAgentTaskManager`；
2. 扩展 `spawn/list/get/cancel`；
3. 新增 task-notification drain；
4. 父取消、关闭、工作区切换级联；
5. [x] 新增 ApprovalBroker Core（FIFO、来源、取消、迟到批准失效与窄审批策略）；
6. [x] 开放默认关闭的 verify profile（仅固定检查标识、静态 argv、无 Shell）；
7. [x] 补齐跨父 Run 的后台审批 SSE/TUI/API 控制面（1A-Full）；
8. [x] 增加任务保留和清理（终态任务/未消费通知按 TTL 自动回收，永久关闭时回收清理线程）。

验收：后台任务不阻塞父对话，不重复通知，取消后不再执行工具。

### Phase 3：Fork、模型覆盖与 Worktree

1. [x] 建立协议完整、创建时冻结且脱敏的父公开上下文快照；
2. [x] 注入 Fork boilerplate 与父基础提示；
3. [x] 支持默认关闭的 `context=fork`；
4. [x] 支持 task > 定义 > 父模型的安全 model override，并为任务建立独立 Runtime；
5. [x] 增加独立 Plugin dispatch context；
6. [x] 支持 worktree_writer；
7. [x] 通用写 Agent 逐工具审批；
8. [x] 父 Agent 决定 diff/分支的应用方式。

验收：父子状态不串扰，Fork 不能再次 Fork，多写者不共享同一工作目录。

---

## 23. 预计文件改动

```text
omnicrawl/agent/
├── core.py                         # 持有 Coordinator；复用 AgentLoopRunner
├── tools.py                        # 注册 subagent 工具
├── types.py                        # 保持兼容；可追加可选工具风险元数据
├── execution.py                    # 建议新增：通用 AgentLoopRunner
└── subagents/
    ├── __init__.py
    ├── definitions.py
    ├── coordinator.py
    ├── execution.py
    └── tasks.py

omnicrawl/config/
├── runtime.py                      # subagents 配置读取和校验
└── ...

omnicrawl/state/
├── session_models.py               # 新事件常量（不进入模型投影）
└── session_artifacts.py            # 子任务 artifact

omnicrawl/api/
├── models.py                       # 可选 task 字段和 SSE 事件
├── service.py                      # 父取消级联、事件转发
└── routes/subagents.py             # Phase 2 可选

omnicrawl/ui/fullscreen/
├── turns.py                        # 子任务事件回调协议
├── widgets.py                      # 可折叠任务组
└── __init__.py                     # 接线，不持有调度业务

omnicrawl/commands/slash.py          # /tasks、/task

tests/
├── test_subagent_definitions.py
├── test_subagent_coordinator.py
├── test_subagent_tasks.py
├── test_subagent_fork.py
├── test_subagent_approval.py
├── test_subagent_session.py
├── test_subagent_api.py
└── test_subagent_tui.py
```

实际实施时不要求一次创建全部文件。每个 Phase 只增加当期真正需要的边界。

---

## 24. 测试策略

### 24.1 单元测试

#### 定义与加载

- frontmatter 缺失/损坏；
- name、description、permission/isolation、工具字段校验；
- 项目/用户/内置/插件优先级；
- 同名碰撞诊断；
- 热重载失败回退；
- 定义快照不可变。

#### Coordinator

- task/batch ID 唯一且不接受模型指定；
- 结果按输入顺序；
- `fail_fast` 行为；
- 最大任务数、并发、深度、超时；
- 单任务失败隔离；
- 父取消级联；
- 关闭和工作区切换回收。

#### 隔离

- 父子 messages 不串扰；
- 子 Agent 不写父 `_history`；
- Runtime 引用独立；
- Token 和工具计数独立；
- Plugin dispatch context 不覆盖；
- 子 Agent 看不到 `subagent` 工具。

#### 工具与审批

- read_only profile 无写工具；
- AgentDefinition 白名单/黑名单顺序；
- MCP external/restricted 不被误放行；
- 委派批准不批准写工具；
- ApprovalBroker 串行；
- 迟到批准无效；
- 路径越界和受保护文件仍被拒绝。

#### Session/Artifact

- 新事件可写可读；
- 不进入恢复模型上下文；
- 大结果转 artifact；
- 脱敏有效；
- 旧 Session v1 完全兼容。

### 24.2 集成测试

- 一个父 Run 内并行两个 explore；
- 一个任务失败、另一个完成；
- 父取消时所有任务 cancelled；
- TUI 单确认队列；
- API SSE 顺序和重放；
- 工作区切换期间拒绝新任务；
- MCP/Plugin/Memory 默认关闭时不影响 SubAgent；
- 四种 Provider 使用 Fake Runtime 验证协议路径。

### 24.3 禁止测试依赖

测试不得要求：

- 真实模型 API Key；
- 网络；
- 真实 MCP Server；
- 用户全局 Agent 定义；
- 摄像头或浏览器；
- 修改真实 Git 分支。

使用 `FakeSubAgentRuntime`、`threading.Event`、临时目录和假审批器精确控制并发、取消和错误。

### 24.4 验证命令

```powershell
python -m unittest tests.test_subagent_definitions tests.test_subagent_coordinator
python -m unittest tests.test_subagent_tasks tests.test_subagent_approval
python -m unittest tests.test_agent_context tests.test_agent_hooks tests.test_approval
python -m unittest tests.test_model_runtime tests.test_llm_module_boundaries
python -m unittest tests.test_session_store tests.test_session_consistency tests.test_session_records
python -m unittest tests.test_api tests.test_api_module_boundaries
python -m unittest tests.test_fullscreen_turns tests.test_fullscreen_tui
python -m unittest tests.test_mcp tests.test_plugin_manager tests.test_plugin_security
python -m unittest discover -s tests -q
python -m compileall -q omnicrawl main.py tests
git diff --check
```

---

## 25. 关键 Tradeoff

### 25.1 单任务工具调用 vs 批量任务工具

选择：统一 `subagent` 工具支持 `tasks[]`。

收益：

- Coordinator 能统一控制并发和预算；
- 不依赖模型一次生成多个 Tool Call；
- 容易有序聚合；
- 一个审批界面可展示完整分发计划。

成本：

- Schema 更复杂；
- 单个 Tool Call 可能运行较久；
- 需要限制 tasks 数量和 prompt 大小。

### 25.2 第一版只读 vs 立即支持通用写 Agent

选择：第一版只读。

原因：当前审批 UI、父生命周期、工作树冲突和 PluginManager 快照均不适合多写者。先证明上下文隔离、任务状态、取消和结果回填，再开放写能力，风险显著更低。

### 25.3 子任务独立 Session vs 父 Session 嵌套事件

选择：父 Session additive event + artifact。

收益：不污染最近会话列表，不改变 SessionIndexEntry，不需要跨 Session 关系迁移。

成本：无法直接用现有 Session UI 独立恢复子任务。该能力在真实需求出现后再设计。

### 25.4 Fork 默认后台 vs 显式 context/action

选择：Fork 由 `context=fork` 显式指定；是否后台由 `action=spawn` 决定。

收益：参数语义清楚、易于日志和测试，不依赖字段省略的隐式分支。

成本：比参考实现多一个显式参数。

### 25.5 共享 Runtime vs 独立 Runtime

选择：接口允许两种实现，Phase 1 先用独立引用快照和模型请求信号量；线程安全未证实时退化为独立 Runtime。

收益：兼顾连接复用和正确性。

成本：需要 Provider 并发契约测试。

---

## 26. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| 父子共享可变 Agent 状态 | 历史、取消、Session 串扰 | 独立 `SubAgentExecution`，禁止共享 `LocalToolAgent.run_stream()` |
| 并发人工审批竞争 | TUI/API 状态错误 | Phase 1 只读；Phase 2 ApprovalBroker 串行 |
| 多写者冲突 | 文件覆盖和不可回滚 | 默认单写者；Phase 3 worktree |
| Provider Runtime 非线程安全 | 请求错配或崩溃 | 模型请求信号量；必要时独立 Runtime |
| Plugin turn 快照单槽 | 父子 Hook 计划互相覆盖 | Phase 1 仅外层 tool Hook；后续独立 dispatch context |
| Memory 并发写丢更新 | 索引覆盖 | 子 Agent 默认禁写，由父串行写 |
| MCP 工具权限扩大 | 外部访问或副作用 | 不直通；按风险、定义和审批取交集 |
| 高频进度挤压 SSE/Session | 丢重要事件、磁盘膨胀 | 节流；只记录状态和摘要 |
| Fork 上下文协议不完整 | Provider 拒绝消息 | Phase 3 建立协议快照和 pending tool result 修复测试 |
| 任务关闭不彻底 | 跨工作区继续执行 | 取消树、有界等待、关闭顺序测试 |
| Token 成本失控 | 费用和延迟上升 | 角色模型、任务/Token/时间/并发硬上限 |

---

## 27. 验收标准

### Phase 1 完成定义

- [x] `subagents.enabled=false` 时现有行为完全不变；
- [x] 能加载内置、用户和项目 Agent 定义并报告冲突；
- [x] `subagent(action=run)` 可执行 1–4 个 fresh 只读任务；
- [x] 多任务有界并发、模型请求独立限流、结果按输入顺序返回；
- [x] 子 Agent 不能使用 `subagent`、写文件、执行命令或写 Memory；
- [x] 父 Agent 历史、Session、Runtime 回调和 Plugin 回合状态不被子任务覆盖；
- [x] 父 Session 记录任务生命周期，但恢复上下文不包含高频进度；
- [x] 子结果经过脱敏、裁剪和 artifact 分级；
- [x] 父取消、关闭和工作区切换会取消所有子任务；
- [x] TUI/API 至少能观察任务开始、完成和失败；
- [x] 四种 Provider 的 Fake Runtime 契约测试通过（OpenAI Chat/Responses、Anthropic、Gemini；2026-07-15）；
- [x] 全量 unittest、compileall 和 `git diff --check` 通过（2026-07-15，554 tests）。

### 最终目标完成定义

- [x] 支持定义式和受控 Fork 两种模式（Fork 默认关闭，仅开放 read_only/verify）；
- [x] 支持同步与进程内后台任务（后台默认关闭）；
- [x] 支持 list/get/cancel 和 exactly-once 通知 drain；
- [x] ApprovalBroker Core 串行审批并携带任务来源；跨父 Run 的当前会话远程审批 SSE/API 控制面已实现；
- [x] 模型覆盖通过现有 Catalog/Profile 安全解析，并在任务创建时冻结；
- [x] 通用写 Agent 仅在单写者或 worktree 隔离下运行（default-off；shared 单写锁 + worktree 脏主树门禁 + 父侧 apply/discard）；
- [x] 跨进程任务恢复：Session 事件 → 安全终态快照 → TaskManager 导入；中断任务标记 `SUBAGENT_INTERRUPTED` 且不重跑（2026-07-16）；
- [x] Plugin、MCP、Skill、Memory、Session、API 和 TUI 边界均有回归测试（2026-07-17 补齐 MCP/Skill/Memory 专项矩阵；定向边界回归 336 项通过）；
- [x] 不存在无限递归、权限扩大、隐藏推理泄露或跨工作区残留任务（2026-07-17 完成统一安全审计；修复 Worktree 创建/切换与父 Session 切换生命周期缺口；全量 596 项通过）。

---

## 28. 决策记录

| 决策 | 结果 | 理由 |
|---|---|---|
| 多 Agent 范式 | 主从分发 | Coding 场景更稳定，父 Agent 保持全局权威 |
| 工具入口 | 一个 `subagent` 工具 | 动态 Agent 定义不改变工具列表 |
| Phase 1 上下文 | fresh | 先解决上下文污染和隔离，降低实现风险 |
| Phase 1 权限 | read_only | 当前并发审批和写冲突尚未具备安全基础 |
| 默认深度 | 1 | 防止递归爆炸 |
| 子任务持久化 | 父 Session 事件 + artifact | 保持 Session v1 和最近会话兼容 |
| MCP | 不直通 | 保留 OmniCrawl 风险和审批模型 |
| Memory | 子 Agent 默认只读 | 当前并发写安全未证明 |
| API 模型 | 父 Run 内嵌任务 | 保持单活动 Run 语义 |
| 写并发 | 默认禁止共享工作区多写者 | 避免覆盖和不可回滚冲突 |
| Hook | Phase 1 不新增 `subagent.*` | 先复用现有 tool Hook，减少协议面 |
| Fork | Phase 3.1B 已落地（默认关闭） | 创建时冻结脱敏公开上下文；独立 Runtime，仍不开放写能力 |
| Plugin dispatch context | Phase 3.2 已落地 | 子任务冻结只读 handlers，ContextVar 激活，不覆盖父 turn plan |

---

## 29. 参考来源与证据路径

### 飞书章节

- 第13章：SubAgent，子Agent与任务分发；
- 理论学习：SubAgent 子任务分发；
- 实战演练：动手实现子 Agent；
- Python源码解析：子 Agent 创建与任务管理；
- Go/Java/TypeScript源码解析：子 Agent 创建与任务管理。

原链接：

```text
https://lcnld21ix7n5.feishu.cn/wiki/Dkw3wfBS9iMQoEkGxiIcnpcZnDd?from=from_copylink
```

### 当前项目主要证据

- `omnicrawl/agent/core.py`：Agent 主循环、审批、Hook、工作区和关闭生命周期；
- `omnicrawl/agent/tools.py`：内置和 MCP 工具注册；
- `omnicrawl/agent/types.py`：Tool/Reply 公共类型；
- `omnicrawl/agent/llm_protocol.py`：统一模型请求和 Tool Call 聚合；
- `omnicrawl/llm/runtime.py`：Runtime 快照和热切换；
- `omnicrawl/state/session_models.py`：Session v1 事件和模型上下文投影白名单；
- `omnicrawl/state/session_artifacts.py`：脱敏、大输出和 artifact；
- `omnicrawl/extensions/skill.py`：Markdown frontmatter、发现和诊断模式；
- `omnicrawl/extensions/plugin_manager.py`：Plugin 执行计划和故障策略；
- `omnicrawl/mcp/registry.py`、`security.py`、`client.py`：MCP 风险、审批和审计；
- `omnicrawl/api/service.py`：单活动 Run、取消、确认和 SSE；
- `omnicrawl/ui/fullscreen/turns.py`、`__init__.py`：单回合控制和单确认模态；
- `docs/skill_system_impl.md`、`docs/HOOK_PLUGIN_DESIGN.md`、`docs/session_design.md`：现有扩展和持久化设计。

本文同时作为设计与实施状态记录。Worktree 写隔离、脏主树门禁、shared 单写锁与父侧 apply/discard 工具入口已 default-off 落地；跨进程任务恢复亦已落地（仅控制面快照，不自动重跑）。通用写 Agent 仅在显式开关打开后可用，不得默认开启。

---

## 实施状态补充（Phase 3）

> 更新说明（2026-07-16）：`isolation=worktree`、`permissionMode=standard`、`general-purpose` 写 Agent、
> 逐工具审批（写入/命令）、父 Agent apply/discard API **以及** `subagent` 工具控制面
> （`apply_worktree` / `discard_worktree` / `list_worktrees`）已落地。
> 另已补齐：
> - create/apply 前主工作区 `git status --porcelain` 脏树门禁
> - `standard + isolation=shared` 的进程内单写锁
> - standard 模式 capability_rules 与写权限对齐
>
> 默认仍全部关闭，需显式配置：
>
> - `subagents.allow_worktree=true`
> - `subagents.allow_standard_agent=true`
> - 若坚持 `standard + isolation=shared`，还需 `allow_shared_workspace_writes=true`
>
> 相关模块：
> - `omnicrawl/agent/subagents/worktree.py`
> - `omnicrawl/agent/subagents/coordinator.py`（worktree 控制动作 / shared 单写锁）
> - `omnicrawl/agent/subagents/builtin/general-purpose.md`
> - `LocalToolAgent.apply_subagent_worktree` / `discard_subagent_worktree` / `list_subagent_worktrees`
> - `omnicrawl/agent/tools.py`：`subagent` action 枚举扩展
> - 回归：`tests/test_subagent_worktree.py`
>
> 跨进程 SubAgent 任务恢复已完成（`recovery.py` + `TaskManager.import_recovered_snapshots` + 会话 resume 接线）。
> 2026-07-17 已补齐 Plugin/MCP/Skill/Memory/Session/API/TUI 边界回归和最终安全不变量审计；设计稿列出的最终验收项已全部完成。
