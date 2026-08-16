# Agent 会话系统设计文档

> 文档性质：初始设计方案与实施对照。
> 当前状态（2026-07-13）：持久化 Session ID、JSONL 转录、PromptHistory、恢复/归档/导出、artifact、诊断、锁与索引重建均已落地。第 3 节保留实施前基线用于追溯，不代表当前缺口；当前事实以本说明、`session_store_technical_debt.md`、源码和测试为准。

## 1. 设计目标

本文档参考 Claude Code Deep Dive 的“多轮对话与会话管理”思路，结合本项目 `omnicrawl` 的现有实现，定义一套适合本地 OmniCrawl 的会话设计方案。

目标不是简单保存聊天记录，而是让 Agent 在多轮任务中具备可恢复、可追踪、可压缩、可导出的上下文管理能力：

```text
用户输入
  -> 会话轮次引擎
  -> 消息规范化
  -> 内存消息链
  -> UI 实时渲染
  -> 工具调用与观察结果
  -> 会话转录持久化
  -> 历史检索 / 恢复 / 压缩
```

核心设计原则：

| 原则 | 说明 |
|------|------|
| 单向数据流 | 用户输入、模型回复、工具调用、工具结果都按顺序进入同一条轮次流，避免 UI 状态和模型状态分叉。 |
| 内存优先、磁盘兜底 | 当前轮次使用内存消息链保证交互速度，同时异步写入磁盘，支持异常退出后的恢复。 |
| 转录与历史分离 | 会话转录保存完整上下文；历史记录只保存用户提示，服务于快速检索和复用。 |
| 瞬时状态不持久化 | 等待动画、进度提示、输入占位符、朗读状态等 UI 状态不进入会话文件。 |
| 可恢复优先 | 持久化格式必须能重建模型上下文、工具调用链、审批结果和必要运行状态。 |
| 长会话可压缩 | 当上下文过长时，通过摘要边界替代早期细节，保留任务目标、关键决策和当前状态。 |

## 2. 资料来源与关键启发

参考资料：<https://github.com/Wechat-ggGitHub/claude-code-deep-dive/blob/main/07-%E5%A4%9A%E8%BD%AE%E5%AF%B9%E8%AF%9D%E4%B8%8E%E4%BC%9A%E8%AF%9D%E7%AE%A1%E7%90%86.md>

该文章对 Claude Code 会话系统的拆解给出几个对本项目有直接价值的设计点：

| 设计点 | 可借鉴内容 | 本项目落地方式 |
|--------|------------|----------------|
| SessionId | 每个会话使用唯一标识，所有转录、历史和恢复都围绕该 ID 组织。 | 为每个 Agent 实例生成 `session_id`，用于命名 JSONL 文件和关联历史记录。 |
| JSONL 转录 | 每条消息追加写入，适合流式生成、异常恢复和增量读取。 | 新增 `sessions/<session_id>.jsonl`，按事件追加保存。 |
| 消息链 | 消息通过 `parent_uuid` 或顺序索引形成可恢复链路。 | 当前可先使用线性链路，后续再扩展分支会话。 |
| 历史记录 | 用户提示历史独立于完整会话转录，用于上箭头和搜索复用。 | 新增 `history.jsonl` 或 `sessions/history.jsonl`，只保存用户输入摘要和项目归属。 |
| 恢复流程 | 从转录文件读取、反序列化、过滤无效消息、重建 AppState。 | 恢复时重建 `_history`、当前模型、审批模式、必要工具状态和 UI 展示。 |
| 会话内存 | 长会话中定期提取关键上下文，避免完整历史无限增长。 | 会话压缩摘要同步到当前 Session 的会话级记忆；完整转录仍留在会话存储。 |
| 自动压缩 | Token 接近上限时，使用摘要替换早期消息。 | 在 `_history` 裁剪前生成 compact 摘要，避免简单丢失早期任务背景。 |

## 3. 实施前基线（历史记录）

以下内容记录设计启动时的项目状态，仅用于说明方案来源；相关主要缺口现已完成落地。

### 3.1 已实现能力

| 能力 | 当前实现 | 说明 |
|------|----------|------|
| 多轮上下文 | `LocalToolAgent._history` | 保存最近若干轮用户和助手消息。 |
| 历史上限 | `AgentConfig.max_history_turns = 6` | `_append_history()` 只保留最近 6 轮，即 12 条 user/assistant 消息。 |
| 新对话 | `/new` 调用 `agent.reset_conversation()` | 清空 `_history`，保留工具、记忆、Skill 和配置。 |
| 轮次内工具链 | `run_stream()` 的 `working_messages` | 当前轮工具调用和工具结果会进入本轮消息链，直到模型给出最终回复。 |
| 项目规范注入 | `_project_instructions_messages()` | 每次请求前重新注入项目规范，但不写入 `_history`，避免历史重复膨胀。 |
| API 会话导出 | `export_current_session_markdown()` | 客户端把当前会话导出到 `~/.omnicrawl/.agent_sessions/exports/`。 |
| 三类记忆 | `project_memory_*` / `session_memory_*` / `user_memory_*` | 分别面向项目技术信息、当前会话压缩状态和用户长期偏好。 |

### 3.2 当时的主要缺口（现已基本完成）

| 缺口 | 影响 |
|------|------|
| 没有持久化 SessionId | 进程退出后无法精确恢复某个会话。 |
| 没有完整 JSONL 转录 | 工具调用过程、审批结果、模型中断点无法长期追踪。 |
| `_history` 只存最终 user/assistant | 轮次内工具调用细节不会进入后续轮次，也不能用于会话恢复。 |
| 历史裁剪是硬裁剪 | 超过 6 轮后早期任务目标、约束和决策直接丢失。 |
| 导出文件在临时目录 | 适合用户查看，不适合作为可恢复会话状态。 |
| 没有恢复入口 | 缺少 `/resume`、`--resume` 或会话选择器。 |
| 没有会话摘要边界 | 长会话不能通过摘要继续保持关键上下文。 |

## 4. 目标架构

建议将会话系统拆成四层：

```text
UI 层
  - TUI 展示与输入
  - HTTP/SSE API 的流式事件和工具确认

会话控制层
  - SessionManager
  - SessionState
  - 会话创建、切换、恢复、结束

轮次引擎层
  - LocalToolAgent.run_stream()
  - 模型请求、工具调用、审批、流式输出

存储层
  - SessionStore
  - JSONL 转录
  - 用户提示历史
  - 会话摘要与索引
```

### 4.1 模块职责

| 模块 | 职责 |
|------|------|
| `SessionManager` | 管理当前会话 ID、元数据、恢复入口、会话列表和会话切换。 |
| `SessionStore` | 负责 JSONL 追加写、读取、索引、归档和异常文件处理。 |
| `SessionState` | 保存可恢复运行态，例如消息链、当前模型、工作区、是否中断。 |
| `PromptHistoryStore` | 保存用户提示历史，支持当前项目、当前会话优先查询。 |
| `SessionCompactor` | 在上下文过长时生成摘要消息并替换早期明细。 |
| `SessionExporter` | 导出 Markdown / JSONL / 摘要，不参与恢复主链。 |

### 4.2 推荐目录结构

```text
~/.omnicrawl/.agent_sessions/
├── index.json
├── history.jsonl
├── projects.json
├── sessions/
│   └── 20260616-201530-a1b2c3.jsonl
├── summaries/
│   └── 20260616-201530-a1b2c3.md
└── exports/
    └── chat_export_20260616_201530.md
```

> 会话目录统一位于用户数据根目录 `~/.omnicrawl/` 下，不随工作区变化。
> 会话不再绑定工作区：同一会话内可以多次切换工作区，转录继续追加，上下文不丢失。

目录说明：

| 路径 | 用途 |
|------|------|
| `~/.omnicrawl/.agent_sessions/index.json` | 会话索引，记录会话 ID、标题、创建时工作区、创建时间、更新时间、消息数量。 |
| `~/.omnicrawl/.agent_sessions/history.jsonl` | 用户提示历史，只保存用户提交内容、时间、项目、会话 ID。 |
| `~/.omnicrawl/.agent_sessions/projects.json` | 项目列表（与项目侧栏共享）。 |
| `~/.omnicrawl/.agent_sessions/sessions/*.jsonl` | 完整会话转录，每行一个事件或消息。 |
| `~/.omnicrawl/.agent_sessions/summaries/*.md` | 会话级摘要，用于恢复预热和长会话压缩。 |
| `~/.omnicrawl/.agent_sessions/exports/` | 用户主动导出的长期文件，区别于 `.omnicrawl/.agent_tmp/` 的临时导出。 |

会话目录位于用户数据根下，默认不会进入任何项目仓库；无需在工作区 `.gitignore` 中重复排除。

## 5. 会话生命周期

### 5.1 新建会话

触发场景：

- 程序启动且未指定恢复参数。
- 用户输入 `/new`。
- UI 中“新对话”操作（键盘导航选择并确认）。

流程：

```text
1. 生成 session_id
2. 创建 SessionState
3. 写入 session_started 事件
4. 更新 index.json
5. 初始化空 _history
6. UI 显示新会话状态
```

建议 `session_id` 格式：

```text
YYYYMMDD-HHMMSS-随机短 ID
示例：20260616-201530-a1b2c3
```

该格式兼顾人工可读、按时间排序和低冲突概率。

### 5.2 处理一轮用户输入

当前 `run_stream()` 已经具备轮次引擎雏形。目标设计是在关键节点追加持久化事件：

```text
1. 用户提交输入
2. 记录 user_message
3. 构建 working_messages
4. 调用模型
5. 流式输出 assistant_delta
6. 如有工具调用，记录 tool_call_requested
7. 审批完成后，记录 tool_call_approved / tool_call_denied
8. 工具执行完成，记录 tool_result
9. 模型继续生成
10. 最终回复完成，记录 assistant_message
11. 更新 _history
12. 更新 index.json 的 updated_at
```

持久化应尽量不阻塞 UI。第一期可以同步追加写，确保正确性；后续再改成后台队列。

### 5.3 结束会话

触发场景：

- 用户输入退出词。
- API 服务关闭或客户端取消运行。
- 进程收到退出信号。
- 本轮生成被用户取消。

结束时记录：

| 事件 | 说明 |
|------|------|
| `session_closed` | 正常退出。 |
| `turn_cancelled` | 用户取消当前生成。 |
| `session_interrupted` | 异常中断或进程退出前未完成轮次。 |

中断状态必须写入转录文件，恢复时才能提示用户“是否继续上一轮未完成任务”。

## 6. 消息与事件模型

### 6.1 事件类型

建议 JSONL 每行使用统一 envelope：

```json
{
  "version": 1,
  "session_id": "20260616-201530-a1b2c3",
  "event_id": "01J...",
  "parent_id": null,
  "type": "user_message",
  "created_at": "2026-06-16T20:15:30+08:00",
  "payload": {}
}
```

推荐事件类型：

| 类型 | 是否进入模型上下文 | 是否 UI 展示 | 说明 |
|------|--------------------|--------------|------|
| `session_started` | 否 | 可选 | 会话元数据；`payload.runtime` 记录版本、进程启动时间和关键源码指纹，便于判断是否由旧进程创建。 |
| `user_message` | 是 | 是 | 用户输入。 |
| `assistant_message` | 是 | 是 | 助手最终回复。 |
| `assistant_delta` | 否 | 是 | 流式片段，可选持久化；第一期不建议保存每个 delta。 |
| `tool_call_requested` | 是 | 是 | 模型请求调用工具。 |
| `tool_call_approved` | 否 | 是 | 用户或审批策略批准。 |
| `tool_call_denied` | 是 | 是 | 拒绝结果需要回传给模型。 |
| `tool_result` | 是 | 是 | 工具执行结果。 |
| `system_notice` | 否 | 是 | UI 通知，不进入模型上下文。 |
| `compact_summary` | 是 | 是/可选 | 压缩后的摘要边界。 |
| `session_interrupted` | 否 | 可选 | 标记异常中断。 |
| `session_closed` | 否 | 可选 | 标记正常关闭。 |

### 6.2 模型消息与 UI 事件分离

不是所有 UI 上看到的内容都应该进入模型上下文。

`session_started.payload.runtime` 只保存不含凭据的运行身份：OmniCrawl 版本、进程启动时间、关键源码文件的大小/修改时间、已加载模块列表和源码指纹。源码指纹用于诊断会话与当前工作区代码是否可能不一致，不用于恢复模型上下文。

| 内容 | 处理方式 | 原因 |
|------|----------|------|
| 用户消息 | 进入模型上下文并持久化 | 多轮对话核心输入。 |
| 助手最终回复 | 进入模型上下文并持久化 | 后续轮次需要参考。 |
| 工具调用请求 | 进入当前轮上下文并持久化 | 保证工具调用链可恢复。 |
| 工具结果 | 进入当前轮上下文并持久化 | 模型需要基于观察结果继续推理。 |
| 等待动画 | 不持久化 | 纯 UI 状态。 |
| 输入框占位符 | 不持久化 | 纯 UI 状态。 |
| 朗读状态 | 不进入模型上下文，可记录调试事件 | 与对话语义无关。 |
| token 用量 | 不进入模型上下文，可写元数据 | 用于统计和诊断。 |

### 6.3 工具结果压缩

工具输出可能很大，直接保存和回放会导致上下文膨胀。当前实现分离三类数据：

| 数据层 | 保存策略 |
|----------|----------|
| 模型观察 | 单工具输出 ≤50K 字符完整进入上下文；超过 50K 或同一回合所有工具输出总和超过 200K 时，完整内容落盘到会话 artifact，模型上下文只保留头尾各 2000 字符的预览与文件路径（模型可用 `read_file` 读取完整内容）。预算为固定硬编码，不开放配置。 |
| UI 展示 | 默认折叠标题保持紧凑；展开详情使用工具结果的完整脱敏输出。 |
| Session 转录 | 8KB 以内内联；更大输出写入 `~/.omnicrawl/.agent_sessions/artifacts/`，JSONL 保存摘要、哈希、大小和路径。artifact 不再按 128KB 截断，`artifact_truncated` 仅作为兼容字段保留且当前始终为 `false`。 |

恢复时默认加载模型摘要；只有用户要求复查完整工具输出时，再读取 artifact。Shell 测试/构建命令不得在主命令中使用 `tail`、`head`、`grep`、`rg` 或 PowerShell 输出裁剪器；报告命令必须通过独立的 `diagnostic_command` 执行。

## 7. 会话恢复

### 7.1 恢复入口

建议支持三类入口：

| 入口 | 示例 | 说明 |
|------|------|------|
| 启动参数 | `python main.py --resume 20260616-201530-a1b2c3` | 精确恢复指定会话。 |
| 斜杠命令 | `/resume`、`/sessions` | 在运行中查看并切换会话。 |
| HTTP API | `/api/v1/sessions` | 供后续 Web 或桌面客户端恢复会话。 |

第一期可以先实现 `/sessions` 查看最近会话和 `/resume <session_id>` 恢复。

### 7.2 恢复流程

```text
1. 根据 session_id 定位 JSONL 文件
2. 按行读取并校验 version/session_id
3. 过滤损坏行和纯 UI 事件
4. 将 user_message / assistant_message / tool_call / tool_result 还原为模型消息
5. 如果存在 compact_summary，以摘要替代对应历史段
6. 重建 LocalToolAgent._history
7. 恢复 current_model、workspace_root 等元数据
8. 检查最后事件是否为 session_interrupted
9. UI 提示恢复成功和是否继续中断任务
```

恢复时必须坚持“当前用户指令优先”。历史会话只提供上下文，不能覆盖用户在恢复后给出的新指令。

### 7.3 一致性过滤

恢复过程中需要过滤以下不适合继续喂给模型的内容：

| 内容 | 处理方式 |
|------|----------|
| 未完成的 `assistant_delta` | 丢弃或合并为中断提示，不作为完整助手消息。 |
| 没有结果的工具调用 | 转换为“上次工具调用被中断”的系统摘要。 |
| 空白助手消息 | 丢弃。 |
| 损坏 JSON 行 | 跳过并记录诊断。 |
| 旧版本事件 | 通过 migration 转换；无法转换时跳过。 |

## 8. 历史记录系统

会话转录和提示历史应分开：

| 数据 | 目标 | 内容 |
|------|------|------|
| 会话转录 | 恢复完整上下文 | 用户消息、助手消息、工具调用、工具结果、会话事件。 |
| 提示历史 | 快速复用用户输入 | 用户提交的文本、时间、项目、会话 ID、可选粘贴引用。 |

### 8.1 历史记录结构

```json
{
  "display": "帮我修复 API 流式事件顺序问题",
  "timestamp": 1781602530000,
  "project": "D:/PythonProject/Python程序设计/AI课堂任务",
  "session_id": "20260616-201530-a1b2c3",
  "pasted_contents": {}
}
```

### 8.2 查询策略

| 场景 | 策略 |
|------|------|
| 上箭头复用 | 当前会话优先，再按当前项目倒序返回。 |
| 搜索历史 | 当前项目优先，跨会话去重显示。 |
| API 输入建议 | 客户端根据最近提示和关键词过滤。 |
| 新会话 | 仍可使用同项目历史，但不要自动注入模型上下文。 |

## 9. 长会话压缩

> 模型辅助摘要、Prompt Cache、Token 测量、滚动结构化摘要和按需证据恢复的实现基线。功能默认关闭；关闭时完全沿用本地确定性普通压缩。开启后仅在完整回合结束且预计下一次请求达到配置的触发阈值（默认 100K Token，config 中 `context_compaction.trigger_context_tokens`）时批量压缩，两次模型压缩至少冷却 4 个完整回合。`target_summary_tokens = 0` 时摘要不设预算上限、以完整性优先（不再被 token 预算卡住或校验拒绝）。
>
> 摘要采用结构化字段（objective/constraints/decisions/completed/current_state/open_issues/artifacts/exact_evidence，以及过程与负信息字段 read_files/modified_files/failed_attempts/excluded_approaches，九部分覆盖字段 key_concepts/problem_solving_process/user_messages/next_steps）。校验器对“该记的没记”把关：被压缩窗口内成功写入的文件必须被 modified_files 覆盖（事件引用或路径匹配），失败的工具调用必须被 failed_attempts 覆盖，用户消息必须被 user_messages 以原文逐字覆盖，否则带反馈重试；重试仍失败则降级。
>
> 开启 `archive_compacted_events` 时，被压缩窗口的原始事件归档到 `.agent_sessions/archive/compacted/<session>/`（第二级存储），`compact_summary` 事件记录 archive_id，任意被压缩事件均可按需精确恢复；开启 `auto_memory_recall` 时，压缩完成后自动检索长期记忆并把命中结果注入后续上下文（`compaction_memory_recall` 事件留痕）。`context_compaction_measurement` 事件包含覆盖度指标（coverage_ratio、字段计数、退休 token）与归档信息。

当前实现已经采用“摘要替换 + 最近窗口”，并将 `compact_summary` 追加到 Session 转录；本节保留初始设计内容用于追溯。

### 9.1 触发条件

第一期可以使用简单规则：

| 条件 | 说明 |
|------|------|
| 消息轮次超过 `max_history_turns` | 准备压缩早期轮次。 |
| 估算 token 超过阈值 | 避免请求超过模型上下文。 |
| 工具结果累计过大 | 优先压缩工具输出。 |

### 9.2 摘要内容

压缩摘要必须包含：

- 用户原始目标。
- 已完成事项。
- 当前未完成事项。
- 关键约束和用户偏好。
- 已修改或重点读取的文件。
- 重要命令输出结论。
- 工具审批或风险决策。
- 下一步建议。

示例：

```markdown
## 会话压缩摘要

- 目标：修复 API 流式事件重连问题。
- 已完成：增加递增事件 ID 和内存事件缓冲。
- 当前状态：验证客户端从 Last-Event-ID 继续读取。
- 关键文件：`omnicrawl/api/service.py`、`omnicrawl/api/routes/runs.py`。
- 约束：不返回模型隐藏推理内容。
- 下一步：补充 SSE 重放测试并执行 API 冒烟验证。
```

### 9.3 压缩后的消息链

```text
system/project instructions
compact_summary
最近 N 轮 user/assistant
当前 user_message
```

`compact_summary` 应写入 JSONL，恢复时作为模型上下文的一部分。

## 10. 与现有记忆系统的关系

本项目将记忆拆分为三个固定作用域，分别绑定独立工具：

| 系统 | 生命周期 | 保存内容 | 是否默认进入上下文 |
|------|----------|----------|--------------------|
| 会话转录 | 单个会话 | 完整对话、工具调用、工具结果 | 恢复该会话时进入。 |
| 会话级记忆 | 单个会话 | 压缩后的目标、约束、决策、文件、完成状态和后续事项 | 仅由当前会话调用，不跨会话。 |
| 项目级记忆 | 当前工作区 | 项目技术事实、架构、配置、实现约束和排障经验 | 由 `project_memory_*` 按需检索。 |
| 用户级记忆 | 用户 | 稳定习惯、偏好和用户纠错 | 由 `user_memory_*` 按需检索。 |
| 提示历史 | 跨会话 | 用户提交过的提示 | 不默认进入，只用于检索复用。 |

存储位置分别为当前工作区 `.omnicrawl/.oclmemory/`、`~/.omnicrawl/Session_memory/<session_id>/` 和 `~/.omnicrawl/User_memory/`。会话压缩只写当前会话级记忆；新会话只绑定新的 Session 目录。删除 Session 时会清理对应会话级记忆，项目级和用户级记忆不受影响。

## 11. UI 设计要求

### 11.1 TUI

建议新增命令：

| 命令 | 功能 |
|------|------|
| `/sessions` | 列出最近会话。 |
| `/resume <session_id>` | 恢复指定会话。 |
| `/undo` | 原子回退最近一轮对话与工作区中被 Git 记录的更改；取消或异常中断时同样回退未完成轮次。 |
| `/rename <title>` | 为当前会话设置标题。 |
| `/compact` | 手动压缩当前会话。 |
| `/export` | 导出当前会话到 Markdown。 |

`/undo` 仍通过仅追加的 `turn_undone` 事件记录被回退轮次的事件 ID，但提交该事件前会先执行副作用恢复。每轮开始和结束时，Host 各执行一次 `git diff HEAD --binary` 生成工作区未提交修改的补丁，并用 `git ls-files --others --exclude-standard` 记录未跟踪文件清单，补丁落盘到 Session artifact 的 `undo/` 目录（不再创建影子 Git 对象库，成本与工作区大小解耦——被 .gitignore 忽略的 RAR/ZIP/DLL 等大文件不会进快照，除非它们被 Git 跟踪且发生修改）。回退时先校验当前工作区仍等于轮次结束状态（冲突则整轮拒绝），再 `git reset --hard HEAD` 复位、`git apply` 轮次起点补丁恢复未提交修改，最后删除本轮新增的未跟踪文件。三类记忆、提示历史与 Session 工具产物不再参与回退（/undo 放弃记忆回退）；非 Git 工作区禁用事务式 undo。

正常轮次会同时失效用户消息、工具事件、助手回复、该轮压缩摘要和 `turn_snapshot`；取消或异常中断的轮次会回退未配对用户消息及其中断事件。旧轮次若没有快照但记录了文件/记忆写入或上下文压缩，`/undo` 会拒绝覆盖；旧的纯对话和只读工具轮次仍兼容逻辑回退。Shell、MCP、桌面控制、SubAgent 执行等无法证明副作用只位于受控根目录的操作会写入不可逆账本，该轮不得使用事务式 `/undo`。Git diff 快照不能撤销数据库、远程服务、网络请求、工作区外文件或其他外部系统状态；被删除的未跟踪文件没有内容副本，无法恢复。

### 11.2 HTTP/SSE API

后续前端通过以下接口实现会话区域：

| 控件 | 行为 |
|------|------|
| 新对话 | `POST /api/v1/sessions` 创建新 session。 |
| 最近会话列表 | `GET /api/v1/sessions` 后调用恢复接口。 |
| 重命名 | `PATCH /api/v1/sessions/current`。 |
| 导出 | `POST /api/v1/sessions/current/export`。 |
| 压缩 | `POST /api/v1/sessions/current/compact`。 |

客户端渲染应来自会话事件，而不是维护一份不可恢复的消息状态。这样 TUI 和 API 客户端共享同一套会话恢复能力。

## 12. 数据安全与边界

会话文件可能包含敏感信息，必须默认按本地私有数据处理。

| 风险 | 处理策略 |
|------|----------|
| API Key、Token、密码进入转录 | 写入前做敏感模式提示；必要时支持用户删除或脱敏。 |
| 工具输出过大 | 分级存储和摘要引用，避免 JSONL 膨胀。 |
| 并发写入损坏 | 使用原子追加或写入锁；至少保证单进程顺序写。 |
| 会话文件进入 Git | 会话目录位于 `~/.omnicrawl/`，不在任何项目仓库内，天然不进 Git。 |
| 恢复旧会话覆盖当前指令 | 恢复后仍以用户最新输入为最高优先级。 |
| 路径越界 | SessionStore 只允许访问自身根目录 `~/.omnicrawl/.agent_sessions/`。 |
| 跨工作区切换 | 会话已解除工作区绑定：切换工作区保留当前会话与上下文，转录记录 `workspace_switched` 事件。 |

## 13. 推荐落地顺序

| 阶段 | 内容 | 验收标准 |
|------|------|----------|
| 第一期 | `SessionStore` + JSONL 追加写 + `session_id` | 每轮对话生成可读 JSONL，退出后文件完整。 |
| 第二期 | `/sessions`、`/resume`、`index.json` | 可以列出和恢复最近会话，恢复后继续追问能引用历史。 |
| 第三期 | 提示历史 `history.jsonl` | TUI/API 客户端能按当前项目复用历史输入。 |
| 第四期 | 会话压缩 `compact_summary` | 超过历史上限时不再硬丢上下文，而是生成摘要。 |
| 第五期 | HTTP/SSE 接口与正式导出 | 客户端可恢复、重命名、导出会话。 |
| 第六期 | 工具结果 artifact、脱敏、归档 | 大输出不会拖慢恢复，敏感内容可控。 |

第一期最小可用版本只需要：

```text
~/.omnicrawl/.agent_sessions/
├── index.json
└── sessions/
    └── <session_id>.jsonl
```

以及三个基础接口：

```python
class SessionStore:
    def start_session(self, workspace_root: Path) -> SessionState:
        """创建新会话并写入 session_started。"""

    def append_event(self, session_id: str, event: SessionEvent) -> None:
        """向会话 JSONL 追加一个事件。"""

    def load_session(self, session_id: str) -> SessionState:
        """读取 JSONL 并重建可恢复会话状态。"""
```

## 14. 关键实现建议

### 14.1 对 `LocalToolAgent` 的最小改造

建议先在 `LocalToolAgent` 中注入 `SessionManager`：

```text
LocalToolAgent.__init__()
  -> self._session_manager = SessionManager(self.workspace_root)
  -> self._session = self._session_manager.start_or_resume()
```

在 `run_stream()` 的关键节点写事件：

```text
收到用户输入      -> append user_message
模型请求工具      -> append tool_call_requested
工具审批完成      -> append tool_call_approved / denied
工具执行完成      -> append tool_result
最终回复完成      -> append assistant_message
用户取消/异常退出 -> append turn_cancelled / session_interrupted
```

### 14.2 保持现有 `_history` 的兼容

第一期不需要重写模型请求链路。可以继续使用 `_history` 作为运行时上下文，只把 JSONL 作为持久化来源。

恢复时：

```text
JSONL -> 规范化消息 -> 最近窗口/摘要 -> self._history
```

这样风险最低，不会一次性影响工具调用、审批、Skill、MCP 和 UI 流式输出。

### 14.3 导出与恢复分离

会话系统保留临时产物与正式导出两个出口：

| 出口 | 路径 | 用途 |
|------|------|------|
| 临时导出 | `.omnicrawl/.agent_tmp/files/` | 用户临时查看或复制。 |
| 正式导出 | `~/.omnicrawl/.agent_sessions/exports/` | 长期保存、交付或归档。 |

无论哪种导出，都不应作为恢复的唯一数据源；恢复必须以 JSONL 转录为准。

## 15. 验证方案

| 验证项 | 方法 |
|--------|------|
| 新会话创建 | 启动后检查 `index.json` 和 `sessions/<id>.jsonl` 是否生成。 |
| 普通多轮 | 连续提问两轮，确认第二轮能引用第一轮。 |
| 工具调用转录 | 触发 `read` 或 `grep`，确认 JSONL 有 tool call 和 result。 |
| `/new` | 执行后生成新 session，旧 session 保留。 |
| `/resume` | 重启程序后恢复旧 session，追问旧上下文能正确回答。 |
| 中断恢复 | 生成中取消，确认 JSONL 记录中断，恢复时提示状态。 |
| 压缩 | 构造超过历史上限的会话，确认摘要存在且上下文不丢主线。 |
| 敏感信息 | 输入类似 Token 的文本，确认不会进入可公开导出或有脱敏提示。 |

## 16. 未解决问题

| 问题 | 建议决策 |
|------|----------|
| `~/.omnicrawl/.agent_sessions/` 是否进入 Git | 位于用户数据根目录，天然不进 Git。 |
| 是否保存流式 delta | 第一期不保存，只保存最终助手消息；调试模式再开启 delta。 |
| 是否支持分支会话 | 第一期只做线性会话；后续通过 `parent_id` 支持分支。 |
| 是否自动生成会话标题 | 可先用第一条用户消息截断生成，后续再让模型生成标题。 |
| 是否把摘要写入长期记忆 | 已限定为会话级：压缩完成时把选定字段写入当前 `session_id` 的独立记忆目录，不进入项目级或用户级记忆。 |

## 17. 总结

当前项目已经具备可用的多轮对话循环，但会话上下文仍停留在进程内短历史阶段。下一步应优先补齐 `session_id`、JSONL 转录、会话索引和恢复入口，让 Agent 从“当前进程内连续对话”升级为“可恢复的长期工作会话”。

建议实施时保持小步演进：先让会话可保存、可列出、可恢复，再引入历史检索、摘要压缩和 GUI 会话列表。这样既能保留现有 `LocalToolAgent.run_stream()` 的稳定路径，也能逐步补上长任务协作所需的状态管理能力。
