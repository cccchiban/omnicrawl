# OmniCrawl 大文件与高耦合模块治理计划

> 文档状态：进行中
> 建立日期：2026-07-12
> 最近检查：2026-07-12
> 当前阶段：P1 SessionStore 第一阶段与安全/单进程一致性最小闭环完成；全屏 UI 治理已开始

## 1. 文档目的

当前项目存在多个职责过多、体量过大或内部类过重的源码文件。此类问题无法通过一次大规模重构安全解决，需要按优先级逐步拆分，并在每个阶段保持现有导入路径、运行行为和测试结果稳定。

本文用于：

1. 记录已确认的问题及其证据，避免后续重复排查；
2. 明确治理顺序，防止同时改动多个核心模块；
3. 为每个阶段提供边界、验收标准和回滚原则；
4. 持续记录进度、关键决策、风险和遗留事项。

本文中的行数和定义数量是 2026-07-12 对当前工作树的静态统计快照，只作为复杂度信号，不作为机械拆分标准。最终判断以职责是否混杂、局部修改成本、依赖方向和可测试性为准。

## 2. 总体结论

当前已确认两个严重的 God File，并发现三个高风险的大类或入口模块：

| 优先级 | 文件或对象 | 快照体量 | 结论 | 状态 |
|---|---|---:|---|---|
| P0 | `omnicrawl/agent/__init__.py` | 原 4263 行，现 18 行 | 已恢复 11 个真实模块，入口仅保留公共导出 | 已完成 |
| P0 | `omnicrawl/mcp/__init__.py` | 原 1834 行，现 44 行 | 已恢复 5 个真实模块，入口仅保留公共导出 | 已完成 |
| P1 | `SessionStore`（`omnicrawl/state/session.py`） | 原模块 1361 行，现门面 757 行 | 已抽离模型、PromptHistory、事件投影和 artifact 策略；生命周期协调仍保留 | 第一阶段完成 |
| P1 | `OmniCrawlApp`（`omnicrawl/ui/fullscreen/__init__.py`） | 约 723 行 | UI God Class，界面与业务编排耦合 | 未开始 |
| P1 | `omnicrawl/api/__init__.py` | 1037 行 | API 模型、路由、鉴权、SSE 和 Agent 运行服务混杂 | 未开始 |
| P2 | `omnicrawl/state/memory.py` | 903 行 | Memory 子系统内部职责偏宽 | 未开始 |
| P2 | `omnicrawl/config/llm.py` | 715 行 | LLM 配置与网络客户端混杂 | 未开始 |
| P2 | `omnicrawl/ui/inline_input.py` | 924 行 | 体量较大但相对内聚，先观察，不机械拆分 | 观察项 |
| P2 | `omnicrawl/workspace/monitor.py` | 635 行 | 后台任务管理与 Windows 平台实现混杂 | 未开始 |

## 3. 判定原则

本计划不以“文件超过多少行就必须拆分”为唯一标准。优先治理同时满足多个以下信号的文件：

- 一个文件存在多个互不相同的变化原因；
- UI、业务编排、配置、存储、安全或外部协议跨层混合；
- 单个类掌握过多状态和生命周期；
- 局部修改需要加载、理解或测试大量无关逻辑；
- 通过动态模块别名制造表面模块边界，实际实现仍在同一文件；
- 测试必须构造完整大对象，难以隔离验证单一行为；
- 文件内部已经存在清楚的原模块标记，说明边界曾经存在；
- 新功能持续被追加到同一入口类或入口文件。

重构时遵循以下约束：

1. 正确性和兼容性优先于减少行数；
2. 恢复自然模块边界，不拆出大量只调用一次的微型方法；
3. 一次只治理一个核心边界，避免 Agent、MCP、Session、API 和 UI 同时重构；
4. 每阶段先补充或确认回归测试，再移动实现；
5. 公共 API、旧导入路径和测试 patch 点必须显式处理；
6. 不借结构重构顺便改变业务行为。

## 4. P0：`omnicrawl/agent/__init__.py`

### 4.1 问题证据

文件头明确说明：原先散落在 `agent/*.py` 的实现被集中到此文件，目的是减少代码文件数量。当前文件包含以下 11 个原模块分段：

| 原模块 | 当前分段起始行（快照） | 主要职责 |
|---|---:|---|
| `types.py` | 30 | Agent、工具调用和模型回复数据类型 |
| `environment.py` | 78 | 操作系统、终端和 Windows 进程环境探测 |
| `history.py` | 255 | 对话历史恢复、压缩和摘要 |
| `tools.py` | 405 | 内置工具定义、注册和调用规范化 |
| `approval_policy.py` | 985 | 工具审批与删除行为判断 |
| `browser_cli.py` | 1138 | bb-browser CLI 生命周期和命令调用 |
| `llm_protocol.py` | 1285 | LLM 请求、响应和流式协议适配 |
| `memory_tools.py` | 1791 | Agent 记忆工具封装 |
| `prompt_context.py` | 1915 | 系统提示词和项目上下文构建 |
| `session_facade.py` | 2168 | 会话持久化门面 |
| `core.py` | 2592 | Agent 主循环、工具循环和工作区切换 |

文件通过向 `sys.modules` 注册别名兼容以下导入路径：

```text
omnicrawl.agent.types
omnicrawl.agent.environment
omnicrawl.agent.history
omnicrawl.agent.tools
omnicrawl.agent.approval_policy
omnicrawl.agent.browser_cli
omnicrawl.agent.llm_protocol
omnicrawl.agent.memory_tools
omnicrawl.agent.prompt_context
omnicrawl.agent.session_facade
omnicrawl.agent.core
```

这些路径看起来是独立模块，实际上都指向同一个 `omnicrawl.agent` 模块。文件内部的 `from .types import ...`、`from .tools import ...` 等导入也没有形成真实物理边界。

### 4.2 主要大定义

| 定义 | 快照体量 | 问题 |
|---|---:|---|
| `LocalToolAgent` | 约 1441 行 | 同时负责工具、MCP、Session、Memory、工作区、请求重试、流式回复和取消控制 |
| `AgentSessionFacade` | 约 388 行 | 会话生命周期和持久化操作较集中 |
| `build_agent_tools()` | 约 191 行 | 工具声明、参数 Schema 和执行绑定集中在单一函数 |
| `run_stream()` | 约 184 行 | 流式 Agent 主流程过长 |
| `AgentLLMProtocol` | 约 182 行 | 请求和响应协议处理较集中 |
| `request_reply_once()` | 约 120 行 | 单次模型请求承担多个分支 |

### 4.3 维护风险

- 导入任一 Agent 子能力都会加载完整巨型模块；
- IDE 跳转、依赖分析和循环依赖定位不直观；
- 测试 patch 路径与真实定义位置脱节；
- `LocalToolAgent` 容易继续吸收所有新功能；
- 真正拆分时，旧导入路径和 monkeypatch 行为可能发生变化；
- Agent 是 UI、API、Session、Memory 和 MCP 的核心交汇点，变更扩散风险最高。

### 4.4 目标结构

```text
omnicrawl/agent/
├── __init__.py
├── types.py
├── environment.py
├── history.py
├── tools.py
├── approval_policy.py
├── browser_cli.py
├── llm_protocol.py
├── memory_tools.py
├── prompt_context.py
├── session_facade.py
├── core.py
└── system_prompt.md
```

`__init__.py` 只保留稳定公共符号的重新导出，不再承载业务实现。

### 4.5 实施清单

- [x] 记录当前 Agent 公共导出和所有旧导入路径；
- [x] 记录测试中的 `patch("omnicrawl.agent.*")` 路径；
- [x] 为模块导入兼容和关键 Agent 流程补充回归测试；
- [x] 按现有 `former module` 标记恢复真实文件；
- [x] 修正真实模块之间的相对导入；
- [x] 将 `agent/__init__.py` 缩减为公共 API 导出；
- [x] 验证历史压缩、工具调用、审批、浏览器、记忆和 Session 路径；
- [x] 验证 TUI 与 API 均可创建并运行 Agent；
- [x] 评估 `LocalToolAgent` 是否仍需按状态所有权继续瘦身；
- [x] 更新本文进度和最终模块依赖说明。

### 4.6 验收标准

- 原有公共导入继续可用，或有明确兼容迁移说明；
- `omnicrawl.agent.core` 等路径对应真实模块；
- `agent/__init__.py` 不再包含核心业务实现；
- Agent 相关单元测试和集成测试通过；
- `python main.py` 启动检查通过；
- API 创建 Agent 的关键路径通过；
- 重构不改变工具审批和工作区安全边界。

## 5. P0：`omnicrawl/mcp/__init__.py`

### 5.1 问题证据

当前文件合并了以下五个原模块：

| 原模块 | 当前分段起始行（快照） | 主要职责 |
|---|---:|---|
| `registry.py` | 24 | Tool、Resource、Prompt 元数据和能力注册 |
| `config.py` | 149 | 配置读取、环境变量覆盖和参数校验 |
| `security.py` | 596 | 风险策略、参数验证和敏感信息脱敏 |
| `audit.py` | 803 | 审计日志持久化 |
| `client.py` | 914 | MCP 生命周期、能力发现、协议连接和进程管理 |

Git 历史中的提交 `6063d58` 删除了原来的 `audit.py`、`client.py`、`config.py`、`registry.py` 和 `security.py`，并将实现合并至 `mcp/__init__.py`。当前同样通过 `sys.modules` 别名模拟旧模块。

### 5.2 主要大定义

| 定义 | 快照体量 | 问题 |
|---|---:|---|
| `MCPClientManager` | 约 486 行 | 生命周期、能力发现、调用、安全、审计和诊断混杂 |
| `_StdioMCPConnection` | 约 235 行 | 子进程和 MCP stdio 协议集中 |
| `_register_capabilities()` | 约 89 行 | 三类能力注册和诊断分支集中 |
| `call_tool()` | 约 89 行 | 调用、安全、审计和异常路径耦合 |
| `_load_server_config()` | 约 88 行 | 单个 Server 的多字段校验集中 |

### 5.3 目标结构

```text
omnicrawl/mcp/
├── __init__.py
├── registry.py
├── config.py
├── security.py
├── audit.py
├── client.py
└── server.py
```

### 5.4 实施清单

- [x] 记录 MCP 公共导出、旧导入和测试 patch 路径；
- [x] 确认关闭 MCP、Server 失败和能力重复时的当前降级行为；
- [x] 按现有五个分段恢复真实文件；
- [x] 保持 `server.py` 的 `python -m omnicrawl.mcp.server` 入口；
- [x] 将 `mcp/__init__.py` 缩减为公共 API 导出；
- [x] 验证配置、注册、安全策略、审计和 stdio 生命周期；
- [x] 再评估 `MCPClientManager` 是否需要拆出连接生命周期对象；
- [x] 更新本文进度和兼容性说明。

### 5.5 验收标准

- MCP 默认关闭时不影响 Agent 启动；
- 原有 MCP 公共导入继续可用；
- 五个逻辑边界对应真实模块；
- 本地 MCP Server 能启动并发现能力；
- Tool、Resource、Prompt 三类能力路径通过；
- 安全确认、敏感信息脱敏和审计行为不变；
- 单个 Server 失败仍然只产生降级诊断，不拖垮 Agent。

## 6. P1：`omnicrawl/state/session.py`

### 6.1 问题

该文件同时包含：

- Session 事件和索引领域模型；
- Prompt 历史模型与存储；
- Session 状态和生命周期；
- JSON/JSONL 序列化；
- 文件索引、搜索和归档；
- 敏感信息脱敏；
- 大型工具输出裁剪。

其中 `SessionStore` 约 750 行，是当前最明显的存储层 God Class。

### 6.2 已落实边界

第一阶段采用平级模块，避免把已有 `omnicrawl.state.session` 模块改成子包而扩大导入路径迁移：

```text
omnicrawl/state/
├── session.py             # 兼容门面和跨文件生命周期协调
├── session_models.py      # 事件、索引、状态模型和格式校验
├── prompt_history.py      # Prompt 历史模型与 JSONL 存储
├── session_projection.py  # 持久化事件到模型上下文的投影
└── session_artifacts.py   # artifact、脱敏和大输出策略
```

`SessionStore` 仍负责会话开始、追加、归档、恢复以及 index/JSONL 的跨文件顺序，避免在结构拆分中改变事务行为。

### 6.3 实施清单

- [x] 绘制 Session 文件、索引和 Prompt 历史的读写路径；
- [x] 添加真实子域模块与兼容导出测试；
- [x] 抽离事件、索引和状态模型；
- [x] 抽离 PromptHistory 模型和存储；
- [x] 抽离恢复投影与 artifact/脱敏策略；
- [x] 保持历史会话文件可继续读取；
- [x] 验证归档、恢复、搜索、artifact 和敏感信息处理的现有行为；
- [x] 单独治理单进程并发写（含归档/恢复文件迁移）、唯一索引临时文件、HTML artifact 与 PromptHistory 的新写入脱敏；
- [ ] 设计跨进程锁、索引崩溃恢复、`fsync` 耐久性策略；
- [ ] 设计事件版本迁移和损坏记录诊断；
- [ ] 在前述一致性问题解决后，再评估是否拆出 index/transcript repository。

### 6.4 第一阶段验证结果

- 关联专项测试：安全持久化、MCP、Agent 和模块边界测试通过；
- 全量测试：以本次最终验证记录为准；
- 当前 `.agent_sessions` 中 41 个索引会话均可加载；
- `omnicrawl.session` 和 `omnicrawl.state.session` 兼容导入保持可用；
- 原有主要非下划线常量和五个 artifact 私有扩展点保留；
- JSON、JSONL、artifact 路径和历史数据格式未改变。

已知限制：数据模型移动到真实模块后，`SessionEvent.__module__` 等反射值发生变化；JSON/JSONL 持久化不受影响，但依赖 Python pickle 或类限定名的外部代码需要迁移。

## 7. P1：`omnicrawl/ui/fullscreen/__init__.py`

### 7.1 问题

`OmniCrawlApp` 约 723 行，同时负责：

- Textual 组件组织和界面生命周期；
- 对话和工具记录渲染；
- Agent 后台线程；
- 工具审批模态框；
- Monitor 轮询；
- Slash Command 分派；
- 模型、工作区和会话切换；
- Context Token 状态显示；
- 取消和退出控制。

UI 结构和业务编排共享一个大对象，使界面改动容易影响线程、审批和 Agent 生命周期。

### 7.2 建议边界

保留 `OmniCrawlApp` 作为 Textual 应用入口，但逐步移出非视觉流程：

- Agent 回合控制器；
- 命令分派器；
- Monitor 状态适配器；
- 可独立测试的状态格式化逻辑。

不要把每个事件处理器都拆成单独类，重点是移交长期状态和业务生命周期。

### 7.3 实施清单

- [ ] 列出 `OmniCrawlApp` 持有的状态及其所有者；
- [ ] 区分 Widget 状态、Agent 运行状态和 Monitor 状态；
- [ ] 为提交消息、取消、审批和工作区切换建立关键路径测试；
- [ ] 抽离 Agent 回合生命周期；
- [ ] 抽离命令分派或复用 `omnicrawl/commands/slash.py`；
- [ ] 验证全屏 TUI 启动和主要交互路径。

## 8. P1：`omnicrawl/api/__init__.py`

### 8.1 问题

文件同时包含 API 数据模型、Bearer 鉴权、CORS、Agent 运行服务、线程状态、人工确认、SSE、Monitor 事件以及多个配置接口。

主要大定义：

- `create_app()`：约 421 行；
- `AgentAPIService`：约 305 行。

### 8.2 建议边界

可按稳定业务资源拆分 Router：

```text
omnicrawl/api/
├── __init__.py
├── app.py
├── models.py
├── service.py
└── routes/
    ├── runs.py
    ├── models.py
    ├── workspace.py
    ├── approval.py
    └── monitor.py
```

路由不应直接重新实现业务逻辑；共享运行状态仍由一个明确的 Service 管理。

### 8.3 实施清单

- [ ] 锁定当前 OpenAPI 路径和响应模型；
- [ ] 为鉴权、运行、确认和 SSE 断线补充回归测试；
- [ ] 将请求/响应模型移出应用工厂；
- [ ] 将路由按资源分组；
- [ ] 保持 `python -m omnicrawl.api` 启动方式；
- [ ] 对比重构前后的 OpenAPI 契约。

## 9. P2 观察与后续治理项

### 9.1 `omnicrawl/state/memory.py`

混合了 Memory 模型、启发式分类、路径安全、Markdown/JSON 索引、搜索排序、生命周期清理和文件存储。建议在 P0/P1 稳定后，优先分离存储与分类策略。

- [ ] 记录 Memory 读写格式；
- [ ] 分离纯分类/排序逻辑与文件 I/O；
- [ ] 验证历史 Memory 数据兼容。

### 9.2 `omnicrawl/config/llm.py`

配置模型、配置持久化和 OpenAI Responses 网络客户端存在不同变化原因。建议保留配置在 `config/llm.py`，将网络协议实现迁移到独立 LLM 客户端模块。

- [ ] 明确配置 API 与客户端 API；
- [ ] 将 Responses 请求和流解析移出配置模块；
- [ ] 验证配置环境变量优先级和模型切换。

### 9.3 `omnicrawl/ui/inline_input.py`

虽然达到 924 行，但终端输入、Unicode 宽度、光标、粘贴、补全和历史浏览属于同一复杂交互领域，当前不作为强制拆分目标。

后续仅在出现以下情况时拆分：

- Unicode/display 算法能形成稳定、纯函数边界；
- 输入编辑器测试频繁因无关逻辑受影响；
- 新终端前端需要复用显示宽度或按键解析能力。

### 9.4 `omnicrawl/workspace/monitor.py`

后台任务领域逻辑与 Windows Job Object 的 `ctypes` 平台实现混合。建议在 Monitor 功能稳定后抽离平台进程树控制。

- [ ] 明确跨平台进程生命周期接口；
- [ ] 抽离 Windows Job Object 实现；
- [ ] 验证停止任务时不会残留子进程。

## 10. 分阶段路线图

为降低核心链路同时变化的风险，建议严格按以下阶段推进：

| 阶段 | 工作内容 | 前置条件 | 当前状态 |
|---|---|---|---|
| 0 | 基线测试、公共导入和 patch 点盘点 | 无 | 已完成 |
| 1 | 恢复 `omnicrawl.agent` 真实模块边界 | 阶段 0 完成 | 已完成 |
| 2 | 恢复 `omnicrawl.mcp` 真实模块边界 | 阶段 1 稳定 | 已完成 |
| 3 | 瘦身 `SessionStore` | 阶段 1 稳定 | 第一阶段完成 |
| 4 | 瘦身 `OmniCrawlApp` | Agent 边界稳定 | 未开始 |
| 5 | 拆分 API 应用工厂与 Router | Agent 边界稳定 | 未开始 |
| 6 | 治理 Memory、LLM 配置和 Monitor | P0/P1 完成 | 未开始 |
| 7 | 全量架构复查与文档收尾 | 前述阶段完成 | 未开始 |

不建议把阶段 1 和阶段 2 合并成一次超大提交。虽然两者结构相似，但 Agent 直接依赖 MCP，同时修改会显著增加回归定位难度。

## 11. 每阶段统一验证清单

每个阶段至少执行与改动范围匹配的验证：

- [ ] 静态导入检查；
- [ ] 目标模块单元测试；
- [ ] `python -m unittest discover -s tests`；
- [ ] 必要时执行前端类型检查或构建；
- [ ] `python main.py` 启动冒烟检查；
- [ ] 涉及 API 时验证 `python -m omnicrawl.api` 和 OpenAPI；
- [ ] 涉及 MCP 时验证关闭、正常连接、连接失败三条路径；
- [ ] 检查 Git diff，确认没有夹带业务行为变更；
- [ ] 更新本文状态、验证结果和遗留风险。

若仓库后续改用 `pytest` 作为统一入口，应以项目实际测试配置为准，并在本文记录命令变化。

## 12. 提交与回滚策略

### 12.1 提交原则

- 一个提交只恢复一个模块边界或拆分一个明确状态所有者；
- 纯文件移动和行为调整不要混在同一提交；
- 优先先提交“无行为变化的结构迁移”，再做类职责瘦身；
- 提交信息应明确使用 `refactor:`，并指出具体子系统。

示例：

```text
refactor: restore agent module boundaries
refactor: split MCP registry and client modules
refactor: extract session index storage
```

### 12.2 回滚原则

- 每阶段必须能独立回滚；
- 不在结构迁移中修改磁盘数据格式；
- 不在结构迁移中删除旧配置项；
- 若旧导入兼容失败，优先恢复导出适配层，而不是在调用方批量打补丁；
- 若完整测试无法一次通过，应回退到最近一个稳定阶段重新缩小改动范围。

## 13. 已知风险

1. Agent 和 MCP 使用 `sys.modules` 模块别名，真实拆分后导入对象身份会变化；
2. 测试大量依赖旧模块路径进行 monkeypatch，移动定义可能导致 patch 失效；
3. `LocalToolAgent` 是 UI、API、Session、Memory 和 MCP 的交汇点，构造顺序和生命周期必须保持；
4. Session 和 Memory 涉及历史磁盘数据，不能只验证新建数据；
5. Textual UI 存在后台线程、异步事件和人工审批，单元测试通过不代表交互路径完全可靠；
6. 当前工作区在建立本文时已有未提交业务修改，开始重构前应先明确这些修改的归属，避免混入结构提交。

## 14. 进度记录

### 2026-07-12：初始盘点

已完成：

- 扫描 `omnicrawl/` 下主要 Python 源码体量；
- 确认 `agent/__init__.py` 和 `mcp/__init__.py` 为严重 God File；
- 确认两者均由历史提交主动合并原模块形成；
- 识别 `LocalToolAgent`、`SessionStore`、`OmniCrawlApp` 和 `MCPClientManager` 等大类；
- 建立 P0、P1、P2 优先级和分阶段治理路线。

尚未开始：

- 未移动任何代码；
- 未改变任何导入路径；
- 未执行结构重构相关测试；
- 未决定各阶段的具体提交日期。

### 2026-07-12：P0 模块边界治理完成

已完成：

- 建立重构前测试基线：`264` 项测试通过；
- 新增 Agent 与 MCP 真实模块身份回归测试；
- 将 `omnicrawl/agent/__init__.py` 中 11 个分段恢复为真实模块；
- 将 `omnicrawl/mcp/__init__.py` 中 5 个分段恢复为真实模块；
- 移除两个入口中的 `sys.modules` 动态伪模块别名；
- 保留 Agent、MCP 原有包级公共导出；
- 兼容包级 `AgentLLMProtocol` 直接导入和现有 monkeypatch 使用位置；
- 保留本轮开始前 `LocalToolAgent.context_window_tokens` 的未提交功能修改；
- 完成独立模块导入、源码编译和内置 MCP Server 启动入口验证；
- 完成重构后全量回归：`268` 项测试通过。

验证结果：

```text
python -m unittest discover -s tests
Ran 268 tests in 21.528s
OK
```

剩余事项：

- P1 的 `SessionStore`、`OmniCrawlApp` 和 API 入口仍未治理；
- 本轮未连接真实外部 MCP Server，外部 Server 兼容性仍依赖现有协议测试和后续联调；
- 当前工作区还有其他既有未提交功能修改，提交前仍需保持拆分改动与其边界清晰。

### 2026-07-12：P1 SessionStore 第一阶段完成

已完成：

- 将原 1361 行会话模块拆为稳定门面和四个高内聚子域；
- 保持 SessionStore 生命周期协调、磁盘格式和事实公共 API；
- 新增 `tests/test_session_module_boundaries.py` 防止职责重新回流；
- 验证 41 个现有索引会话均可读取；
- 完成 98 项关联测试和 270 项全量测试。

后续专项风险：

- index 与 JSONL 缺少统一写锁和崩溃恢复；
- HTML artifact 在现有流程中早于递归脱敏落盘；
- PromptHistory 的显示文本和粘贴内容缺少明确敏感信息策略；
- 事件版本迁移和损坏记录诊断尚未实现。

上述事项涉及行为和数据策略变化，不与本次无行为结构拆分混做。

## 15. 完成定义

只有同时满足以下条件，本治理计划才可标记为完成：

- [x] 两个 P0 God File 已恢复真实模块边界；
- [ ] 三个 P1 对象的状态和职责边界明显收窄；
- [ ] P2 项均已完成治理或记录保留现状的理由；
- [ ] 公共导入、磁盘数据和运行方式保持兼容；
- [ ] TUI、API、MCP、Session 和 Memory 关键路径通过验证；
- [ ] 项目结构文档与实际源码一致；
- [ ] 不再依赖 `sys.modules` 为不存在的源码文件模拟模块边界；
- [ ] 后续新增功能有明确归属，不再默认堆入入口文件或入口类。
