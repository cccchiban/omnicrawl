# agent.py 拆分与精简路线

本文档用于指导 `ai_voice_agent/agent.py` 的渐进式拆分。目标不是一次性重写，而是在保证功能正常、外部 API 稳定和测试可回归的前提下，把当前的大文件逐步拆成职责清晰的小模块。

## 1. 当前结论

`agent.py` 当前约 2300 行，主要冗余不是大量“死代码”，而是 `LocalToolAgent` 同时承担了过多职责：

| 职责 | 当前位置 | 问题 |
|------|----------|------|
| Agent 初始化与运行循环 | `LocalToolAgent.__init__`、`run_stream` | 初始化依赖多，运行循环同时处理输入、Skill、会话事件、工具执行和异常转录。 |
| LLM 流式协议 | `_request_agent_reply*`、tool call delta 解析 | Chat Completions 细节和 Agent 主流程耦合，后续更换模型协议风险高。 |
| 工具注册与工具执行 | `_build_tools`、`_build_mcp_tools`、`_tool_*` | 内置工具、记忆工具、MCP 工具混在一个注册函数里。 |
| 工具审批与删除意图识别 | `_approve_tool_call`、`_is_delete_behavior_tool_call` 等 | 规则相对独立，适合单独测试和维护。 |
| 会话与项目门面 | `list_sessions` 到 `resume_session`、`list_projects` 等 | 多数方法只是转调 Store 并包装异常，可抽出门面类。 |
| 历史压缩 | `_compact_history`、`_build_compact_summary` 等 | 是独立策略，适合单独模块化。 |
| 运行环境摘要 | `_runtime_environment_context`、平台检测函数 | 与 Agent 主循环无直接耦合，可作为上下文工具函数。 |

## 2. 拆分原则

| 原则 | 说明 |
|------|------|
| 外部 API 不变 | `main.py`、`qt_chat_session.py`、`slash_commands.py` 仍通过 `LocalToolAgent` 调用，不要求调用方一起大改。 |
| 一步一验 | 每个阶段只移动一类职责，移动后立即运行对应测试。 |
| 先低风险后高风险 | 先抽纯函数和薄包装，再拆运行循环和 LLM 协议。 |
| 保留兼容层 | 新模块落地后，`LocalToolAgent` 可暂时保留同名方法，内部委托新类。 |
| 不趁机改行为 | 拆分阶段只做结构整理，不改变工具审批、会话恢复、Skill 注入、MCP 调用等行为。 |

## 3. 推荐目标结构

建议最终形成以下模块：

| 模块 | 职责 |
|------|------|
| `ai_voice_agent/agent.py` | 保留 `AgentConfig`、`AgentError`、`LocalToolAgent` 对外入口和少量编排逻辑。 |
| `ai_voice_agent/agent_types.py` | `ToolCall`、`ToolResult`、`AgentModelReply`、`ToolDefinition` 等共享数据结构。 |
| `ai_voice_agent/agent_environment.py` | 运行环境摘要、终端/Shell/进程链检测。 |
| `ai_voice_agent/agent_approval.py` | 工具审批、删除意图识别、审查模型返回解析。 |
| `ai_voice_agent/agent_tools.py` | 内置工具注册、WorkspaceTools 适配、MCP Tool/Resource/Prompt 结果格式化。 |
| `ai_voice_agent/agent_llm_protocol.py` | Chat Completions 请求、流式 delta 解析、tool call 聚合、prompt cache key。 |
| `ai_voice_agent/agent_history.py` | `_history` 窗口恢复、确定性压缩摘要。 |
| `ai_voice_agent/agent_session_facade.py` | 会话、提示历史、项目列表相关门面方法。 |

命名可以按实现时的上下文微调，但要避免把新模块做成新的“大杂烩”。

## 4. 分阶段执行计划

### 阶段 0：建立基线

| 项目 | 内容 |
|------|------|
| 目标 | 在拆分前确认当前行为可回归。 |
| 操作 | 记录当前 `agent.py` 行数、公开方法、相关测试状态。 |
| 建议命令 | `python -m pytest tests/test_agent_context.py tests/test_session_store.py tests/test_project_store.py -q` |
| 验收 | 相关测试通过；若已有失败，先记录失败，不把失败归因到拆分。 |
| 回滚 | 无代码变更。 |

### 阶段 1：抽出纯函数和类型

| 项目 | 内容 |
|------|------|
| 目标 | 移动低耦合代码，降低 `agent.py` 体量。 |
| 新文件 | `agent_types.py`、`agent_environment.py` |
| 移动内容 | `ToolCall`、`ToolResult`、`AgentModelReply`、`ToolDefinition`；运行环境摘要和平台检测函数。 |
| 注意 | 只改 import，不改变字段名、默认值和返回文本。 |
| 验证 | `python -m pytest tests/test_agent_context.py -q`，再运行 `python -m compileall ai_voice_agent`。 |
| 预期收益 | `agent.py` 减少约 200 到 300 行，风险低。 |

### 阶段 2：抽出工具审批规则

| 项目 | 内容 |
|------|------|
| 目标 | 让删除意图识别和 review 模式审批独立可测。 |
| 新文件 | `agent_approval.py` |
| 移动内容 | 删除相关正则、`TOOL_REVIEW_SYSTEM_PROMPT`、`_is_delete_behavior_tool_call`、`_command_has_delete_intent`、`_parse_tool_review_response` 等。 |
| 保留内容 | 需要调用 LLM 的 `_review_tool_call` 可以先留在 `LocalToolAgent`，只委托纯规则函数；第二步再整体迁移。 |
| 验证 | 增加或迁移审批相关测试；至少运行 `python -m pytest tests/test_agent_context.py -q`。 |
| 风险 | 删除误判会影响工具审批噪音，必须保持原测试用例和边界文本不变。 |

### 阶段 3：精简 Workspace 工具 wrapper

| 项目 | 内容 |
|------|------|
| 目标 | 消除 `list/read/search/replace/write` 这类重复 try/except。 |
| 新文件 | 可放入 `agent_tools.py` |
| 改法 | 用统一适配函数把 `WorkspaceToolError` 转换为 `ToolResult`，保留 `run_command` 的 `ok/output` 特殊处理。 |
| 验证 | 覆盖文件读取、搜索、替换、命令执行相关 Agent 测试；运行 `python -m pytest tests/test_agent_context.py tests/test_mcp.py -q`。 |
| 风险 | 工具返回文案和 `ok` 字段不能变化，否则模型后续判断可能受影响。 |

### 阶段 4：拆出工具注册表

| 项目 | 内容 |
|------|------|
| 目标 | 将 `_build_tools` 和 `_build_mcp_tools` 从 Agent 主类中移出。 |
| 新文件 | `agent_tools.py` |
| 改法 | 提供 `build_agent_tools(context)` 或 `AgentToolRegistry`，由 `LocalToolAgent` 传入必要回调。 |
| 注意 | `ToolDefinition.run` 仍可绑定到 Agent 实例方法，先不要为了“纯净”引入复杂上下文对象。 |
| 验证 | `python -m pytest tests/test_agent_context.py tests/test_mcp.py -q`。 |
| 预期收益 | `agent.py` 再减少约 200 行，并让 MCP/内置工具边界更清楚。 |

### 阶段 5：拆出历史压缩策略

| 项目 | 内容 |
|------|------|
| 目标 | 把确定性压缩规则独立出来，避免和会话写入逻辑缠在一起。 |
| 新文件 | `agent_history.py` |
| 移动内容 | `_restore_history_window`、`_compact_history` 中纯计算部分、`_build_compact_summary`、摘要片段格式化。 |
| 保留内容 | 写入 `compact_summary` 会话事件的动作可暂时留在 Agent，由新模块返回摘要和保留窗口。 |
| 验证 | `python -m pytest tests/test_agent_context.py tests/test_session_store.py -q`。 |
| 风险 | 长会话恢复边界敏感，必须检查摘要消息仍以 `COMPACT_SUMMARY_PREFIX` 开头。 |

### 阶段 6：拆出 LLM 协议层

| 项目 | 内容 |
|------|------|
| 目标 | 把模型请求、流式解析和 tool call delta 聚合从 Agent 主流程中隔离。 |
| 新文件 | `agent_llm_protocol.py` |
| 移动内容 | `_request_agent_reply*`、`_parse_tool_arguments`、`_extract_stream_delta`、tool call delta 聚合、function name 映射、tool schema 推断、prompt cache key。 |
| 改法 | 新类可以命名为 `AgentLLMProtocol`，接收 client、config、workspace_root、system_prompt_provider、tools_provider。 |
| 验证 | 重点运行 `python -m pytest tests/test_agent_context.py tests/test_llm_config.py -q`；如有流式 mock 测试，优先补齐。 |
| 风险 | 这是高风险阶段，容易影响模型流式输出、工具调用 id、token usage 回调和空响应重试。 |

### 阶段 7：拆出会话/项目门面

| 项目 | 内容 |
|------|------|
| 目标 | 将会话和项目相关的薄包装方法从 `LocalToolAgent` 中移出。 |
| 新文件 | `agent_session_facade.py` |
| 改法 | 新类管理 `SessionStore`、`ProjectStore` 和当前 `SessionState`，`LocalToolAgent` 保留同名公开方法并委托。 |
| 注意 | `_history` 和 `_pending_user_text` 仍属于运行态，迁移时要清楚谁负责更新。 |
| 验证 | `python -m pytest tests/test_agent_context.py tests/test_session_store.py tests/test_project_store.py tests/test_qt_ui.py -q`。 |
| 风险 | Qt 侧栏、TUI 斜杠命令依赖这些方法，必须保持返回类型和异常文案。 |

### 阶段 8：收尾精简 `LocalToolAgent`

| 项目 | 内容 |
|------|------|
| 目标 | 让 `LocalToolAgent` 回到编排角色。 |
| 操作 | 删除已无引用的私有方法；检查 import；更新文档；补充测试说明。 |
| 验证 | `python -m pytest -q`，`python -m compileall ai_voice_agent`。 |
| 验收 | `agent.py` 只保留配置、生命周期、运行编排和对外委托，行数明显下降，功能测试通过。 |

## 5. 每一步的通用检查清单

每次拆分完成后按这个顺序检查：

1. `git diff -- ai_voice_agent/agent.py`：确认本次只移动当前阶段相关代码，没有顺手改行为。
2. `rg "from \\.agent import|LocalToolAgent|ToolCall|ToolResult" ai_voice_agent tests`：确认引用路径是否需要同步。
3. `python -m compileall ai_voice_agent`：先排除语法和 import 问题。
4. 运行阶段对应测试。
5. 记录本阶段改动、验证命令和结果。

## 6. 不建议做的事

| 不建议 | 原因 |
|--------|------|
| 一次拆完所有模块 | 回归面太大，出现问题很难定位是哪一步引入。 |
| 顺手重写 `run_stream` 行为 | 它承载会话恢复、工具调用和中断事件，行为变化影响面最大。 |
| 为拆分引入复杂继承 | 当前问题是职责过宽，不需要再叠加继承层级。 |
| 改动用户可见文案 | 测试和用户习惯可能依赖这些文案；拆分期应保持稳定。 |
| 删除看似兼容的别名逻辑 | 工具名和参数别名是对模型输出容错，删除会降低实际可用性。 |

## 7. 推荐第一步

建议先做“阶段 1：抽出纯函数和类型”。这一阶段最不容易影响运行逻辑，适合作为拆分起点：

1. 新增 `ai_voice_agent/agent_types.py`，移动四个 dataclass。
2. 新增 `ai_voice_agent/agent_environment.py`，移动运行环境相关函数。
3. 修改 `agent.py` import。
4. 运行 `python -m pytest tests/test_agent_context.py -q`。
5. 运行 `python -m compileall ai_voice_agent`。

如果第一步通过，再继续审批规则和工具注册拆分。这样每一步都能独立提交，也方便出现问题时精确回退。

## 8. 执行记录

### 2026-06-17：阶段 6 部分完成

| 项目 | 结果 |
|------|------|
| 目标 | 先拆出 LLM 协议层的请求、重试、流式 delta 解析、tool call 聚合、工具 schema 转换和 prompt cache key 逻辑。 |
| 新文件 | `ai_voice_agent/agent_llm_protocol.py` |
| 保留兼容 | `LocalToolAgent` 保留原私有方法入口，内部委托新协议模块，避免影响现有测试和调用方。 |
| 精简结果 | `ai_voice_agent/agent.py` 从约 1900 行降到约 1657 行。 |
| 验证 | `python -m compileall ai_voice_agent` 通过；`python -m pytest tests/test_agent_context.py tests/test_llm_config.py tests/test_mcp.py -q` 通过，结果为 60 passed。 |
| 已知情况 | 全量 `python -m pytest -q` 当前有 3 个 Qt UI 测试失败，失败点落在既有未提交的 UI 文件改动，不属于本阶段 Agent 拆分范围。 |

### 2026-06-17：阶段 7 部分完成

| 项目 | 结果 |
|------|------|
| 目标 | 先把会话、项目、提示历史相关的薄包装逻辑迁移到门面模块，保留 `LocalToolAgent` 对外 API 和兼容入口。 |
| 新文件 | `ai_voice_agent/agent_session_facade.py` |
| 保留兼容 | `LocalToolAgent` 仍保留 `list_sessions`、`resume_session`、`list_projects`、`_append_session_event` 等原方法，内部委托门面；`object.__new__` 构造的测试对象通过懒加载门面兼容。 |
| 精简结果 | 当前 `ai_voice_agent/agent.py` 约 1736 行；新增门面约 395 行，后续可继续收紧状态边界。 |
| 验证 | `python -m compileall ai_voice_agent` 通过；`python -m pytest tests/test_agent_context.py tests/test_session_store.py tests/test_project_store.py -q` 通过，结果为 42 passed；`python -m pytest tests/test_mcp.py tests/test_llm_config.py -q` 通过，结果为 32 passed。 |
| 已知情况 | 按阶段 7 建议运行含 Qt 的组合测试时，`tests/test_qt_ui.py` 仍有 3 个既有 UI 静态断言失败，失败点在未纳入本次拆分的 UI 文件内容。 |

### 2026-06-18：提交前审查

| 项目 | 结果 |
|------|------|
| 目标 | 合并审查 Agent 拆分与 Qt 前端未提交改动，补齐提交前发现的前后端联动缺口。 |
| 修复 | Qt 审批模式下拉补齐 `setApprovalMode` 桥接和 `/approval:<mode>` 入队；项目工具栏下拉改为复用项目侧栏弹窗，避免误触创建默认项目。 |
| 验证 | `python -m pytest` 通过，结果为 189 passed；`python -m pytest tests/test_qt_ui.py` 通过，结果为 35 passed；`python -m py_compile ...` 通过；`node --check` 校验 `app.js`、`input.js`、`messages.js` 通过；`git diff --check` 通过。 |
