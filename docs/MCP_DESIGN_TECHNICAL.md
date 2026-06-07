# MCP 设计及技术文档

> 参考资料：MCP 中文站《MCP最佳实践：架构设计与实施指南》（https://mcpcn.com/docs/best-practices/）。
> 本文档结合当前 `AI 语音 Agent` 项目现状，描述 MCP 子系统的设计目标、架构边界、接口模型、安全策略、可观测性、部署运维和实施路线。

---

## 1. 背景与目标

当前项目已经具备本地 Agent 循环能力：用户通过终端或语音输入任务，`LocalToolAgent` 调用 LLM，并在受控审批后执行文件读取、文本搜索、写入、命令执行、记忆读写和 Skill 注入等工具。

MCP（Model Context Protocol）的价值在于把这些能力从“项目内部工具协议”抽象为标准化上下文协议，使 Agent 可以：

- 以统一方式连接本地或远程 MCP Server。
- 将现有工具能力暴露为标准 Tool、Resource、Prompt。
- 为后续接入文件系统、数据库、浏览器、知识库、CI、部署平台等外部能力提供一致边界。
- 保持审批、安全、审计和故障隔离，不让模型直接接触高风险执行细节。

### 1.1 建设目标

| 目标 | 说明 |
|------|------|
| 标准化 | 使用 MCP 的 Tool、Resource、Prompt 抽象替代私有工具描述，降低集成成本。 |
| 可控性 | 沿用现有审批模式、路径边界、受保护文件规则和工具输出截断策略。 |
| 可扩展 | 支持多个 MCP Server，并允许按配置启用、禁用和分组。 |
| 可观测 | 对工具调用、延迟、错误、审批结果、外部服务状态形成日志和指标。 |
| 可降级 | MCP Server 故障不影响基础对话；单个工具失败不拖垮整轮 Agent。 |

### 1.2 非目标

- 本期不把 Agent 改造成完全依赖 MCP 的系统；现有内置工具仍保留。
- 本期不默认接入生产数据库、付费外部服务或真实生产环境。
- 本期不默认开放公网 MCP Server；优先使用本地 `stdio` 传输。
- 本期不新增破坏性 Schema 变更，也不改变已有用户交互命令。

---

## 2. 参考最佳实践摘要

根据 MCP 中文站最佳实践页面，本文档采用以下原则：

| 实践方向 | 设计落点 |
|----------|----------|
| 单一职责原则 | MCP Client、MCP Server、审批、安全校验、工具适配、观测采集分层实现。 |
| 防御性编程 | 所有外部输入先校验类型、范围、路径和权限，再进入业务执行。 |
| 故障隔离设计 | 每个 MCP Server 独立生命周期、超时、重试和熔断，不共享不可控状态。 |
| 配置管理 | 通过 `config.json` 与环境变量管理服务器、传输、超时、启用状态和安全策略。 |
| 错误处理与恢复 | 工具错误结构化返回给模型；连接错误支持重连；连续失败触发降级。 |
| 性能优化 | 工具发现缓存、连接复用、输出截断、按需加载 Resource 内容。 |
| 监控与可观测性 | 采集请求数、错误率、延迟、审批拒绝、Server 健康状态等指标。 |
| 安全最佳实践 | 输入验证、访问控制、审计日志、密钥脱敏、最小权限运行。 |
| 部署与运维 | 本地优先，容器化可选；提供健康检查、告警规则和回滚策略。 |

---

## 3. 当前系统与 MCP 角色映射

### 3.1 当前模块

```text
用户输入 / 语音输入
  |
  v
main.py / terminal_ui.py
  |
  v
LocalToolAgent
  |-- OpenAI Responses API 兼容 LLM
  |-- 内置工具：list_files / read_file / search_text / write_file / run_command
  |-- 记忆工具：memory_search / memory_read / memory_write
  |-- SkillManager
  |-- Approval 审批模式：manual / auto / review
```

### 3.2 MCP 角色设计

| MCP 角色 | 本项目对应 | 职责 |
|----------|------------|------|
| Host | AI 语音 Agent 应用 | 管理用户会话、模型请求、工具审批和最终回复。 |
| Client | 新增 `MCPClientManager` | 连接一个或多个 MCP Server，发现工具和资源，执行调用。 |
| Server | 外部 MCP Server 或本项目可选 Local MCP Server | 暴露工具、资源和提示词能力。 |
| Tool | 文件、搜索、命令、记忆、外部服务操作 | 由模型选择调用，但执行前受策略控制。 |
| Resource | 项目文件、文档、记忆、外部知识条目 | 给模型读取上下文，默认只读。 |
| Prompt | 固化任务模板 | 例如代码审查、文档生成、排障流程模板。 |

---

## 4. 总体架构

```text
┌──────────────────────────────────────────────────────────────┐
│                       AI 语音 Agent Host                      │
│                                                              │
│  ┌──────────────┐    ┌──────────────┐    ┌────────────────┐ │
│  │ Terminal/TUI │───▶│ LocalToolAgent│───▶│ LLM Responses  │ │
│  └──────────────┘    └──────┬───────┘    └────────────────┘ │
│                              │                               │
│                              ▼                               │
│                     ┌─────────────────┐                      │
│                     │ Tool Router      │                      │
│                     │ - 内置工具        │                      │
│                     │ - MCP 工具        │                      │
│                     └──────┬──────────┘                      │
│                            │                                 │
│         ┌──────────────────┼──────────────────┐              │
│         ▼                  ▼                  ▼              │
│ ┌──────────────┐   ┌────────────────┐  ┌──────────────────┐ │
│ │ Approval     │   │ Security Guard │  │ Observability    │ │
│ │ manual/review│   │ path/env/audit │  │ logs/metrics     │ │
│ └──────────────┘   └────────────────┘  └──────────────────┘ │
│                            │                                 │
│                            ▼                                 │
│                    ┌──────────────────┐                      │
│                    │ MCPClientManager │                      │
│                    └──────┬───────────┘                      │
└───────────────────────────┼──────────────────────────────────┘
                            │
        ┌───────────────────┼───────────────────┐
        ▼                   ▼                   ▼
┌──────────────┐    ┌────────────────┐  ┌──────────────────┐
│ MCP Server A │    │ MCP Server B   │  │ Local MCP Server │
│ filesystem   │    │ browser/db/... │  │ project tools    │
└──────────────┘    └────────────────┘  └──────────────────┘
```

### 4.1 分层职责

| 层级 | 模块 | 职责 |
|------|------|------|
| 会话层 | `main.py`、`terminal_ui.py`、`chat_session.py` | 处理输入、输出、语音播报、中断和斜杠命令。 |
| Agent 层 | `LocalToolAgent` | 管理模型循环、工具选择、历史上下文、Skill 和记忆。 |
| 路由层 | `ToolRouter` | 统一路由内置工具和 MCP 工具，隐藏协议差异。 |
| MCP 客户端层 | `MCPClientManager`、`MCPConnection` | 加载配置、建立连接、发现能力、调用工具、读取资源。 |
| 安全层 | `Approval`、`SecurityGuard` | 审批、路径限制、命令限制、密钥脱敏、审计记录。 |
| 可观测层 | `MCPAuditLogger`、`MCPMetrics` | 记录调用链、耗时、失败原因、审批结果和健康状态。 |
| 服务端层 | `LocalMCPServer`（可选） | 把本项目能力暴露给其他 MCP Host。 |

---

## 5. MCP 能力模型

### 5.1 Tool 设计

工具命名使用 `server_name.tool_name` 的逻辑名称，避免多个 Server 之间重名。

| 工具 | 来源 | 风险级别 | 审批策略 | 说明 |
|------|------|----------|----------|------|
| `workspace.list_files` | 内置或 Local MCP Server | 低 | 可自动批准 | 列出工作区文件，跳过受保护目录。 |
| `workspace.read_file` | 内置或 Local MCP Server | 中 | 默认确认或 review | 读取 UTF-8 文本文件，限制在工作区内。 |
| `workspace.search_text` | 内置或 Local MCP Server | 低 | 可自动批准 | 搜索项目文本，限制最大结果数。 |
| `workspace.write_file` | 内置或 Local MCP Server | 高 | 必须审批 | 写入或覆盖项目文件。 |
| `workspace.replace_text` | 内置或 Local MCP Server | 高 | 必须审批 | 单文件文本替换。 |
| `workspace.run_command` | 内置或 Local MCP Server | 高 | 必须审批 | 在工作区执行命令，设置超时和审计。 |
| `memory.search` | 内置或 Local MCP Server | 低 | 可自动批准 | 查询记忆索引。 |
| `memory.write` | 内置或 Local MCP Server | 中 | review 或确认 | 写入长期记忆。 |
| `external.*` | 外部 MCP Server | 按配置 | 按 Server 策略 | 浏览器、数据库、CI 等外部能力。 |

### 5.2 Resource 设计

Resource 用于只读上下文，不直接产生副作用。

| URI 模式 | 示例 | 说明 |
|----------|------|------|
| `project://README.md` | `project://README.md` | 项目根目录文档。 |
| `project://docs/{path}` | `project://docs/TERMINAL_UI.md` | 文档目录内容。 |
| `project://agents-instructions` | `project://agents-instructions` | 当前项目 `AGENTS.md` 精简规则。 |
| `memory://{memory_id}` | `memory://20260607-001` | 已确认可读取的记忆内容。 |
| `server://{name}/health` | `server://filesystem/health` | MCP Server 健康状态摘要。 |

Resource 读取必须复用现有路径安全边界：禁止读取 `.git`、`.env`、`config.json`、虚拟环境、缓存目录和工作区外路径。

### 5.3 Prompt 设计

Prompt 用于沉淀稳定任务模板，减少每轮临时提示词重复。

| Prompt | 场景 | 输入 |
|--------|------|------|
| `project_doc_writer` | 编写项目技术文档 | 目标、范围、参考资料、输出文件。 |
| `code_review` | 代码审查 | 文件路径、关注点、风险等级。 |
| `debug_triage` | 排障分析 | 错误日志、复现步骤、期望行为。 |
| `safe_change_plan` | 高风险改动前方案 | 变更目标、影响面、回滚约束。 |

---

## 6. 配置设计

### 6.1 `config.json` 示例

```json
{
  "mcp": {
    "enabled": true,
    "default_timeout_seconds": 30,
    "max_tool_output_chars": 6000,
    "servers": {
      "local_project": {
        "enabled": true,
        "transport": "stdio",
        "command": "python",
        "args": ["-m", "ai_voice_agent.mcp_server"],
        "env": {},
        "risk_level": "trusted"
      },
      "filesystem": {
        "enabled": false,
        "transport": "stdio",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
        "env": {},
        "risk_level": "restricted"
      }
    },
    "policy": {
      "require_confirmation_for_write": true,
      "require_confirmation_for_command": true,
      "allow_external_network_tools": false,
      "audit_log_enabled": true
    }
  }
}
```

### 6.2 配置规则

- 环境变量优先级高于 `config.json`，便于临时覆盖。
- `enabled=false` 的 Server 不建立连接，也不向模型暴露工具。
- `risk_level=trusted` 不代表免审，只表示来源可信；具体是否审批仍由工具风险决定。
- `env` 中禁止配置真实密钥到示例文件；真实密钥只能存在本地 `config.json` 或环境变量。
- 外部网络能力默认关闭，启用前必须确认范围、成本和安全边界。

---

## 7. 接口与数据结构

### 7.1 MCP Server 配置模型

```text
MCPServerConfig
  name: str
  enabled: bool
  transport: "stdio" | "streamable_http"
  command: str | null
  args: list[str]
  url: str | null
  env: dict[str, str]
  timeout_seconds: int
  risk_level: "trusted" | "restricted" | "external"
```

校验约束：

- `stdio` 必须提供 `command`。
- `streamable_http` 必须提供 `url`，且默认不允许明文公网地址。
- `timeout_seconds` 限制在 `1` 到 `300` 秒。
- `name` 只能使用小写字母、数字、下划线和连字符。
- `env` 输出日志时必须脱敏。

### 7.2 工具调用结果

```text
MCPToolResult
  ok: bool
  server_name: str
  tool_name: str
  output: str
  error_code: str | null
  retryable: bool
  duration_ms: int
  audit_id: str
```

设计意图：

- `ok=false` 时仍把结构化错误返回给模型，让模型能继续调整方案。
- `retryable=true` 只表示系统可重试，不代表模型应该无限重试。
- `audit_id` 方便用户把终端记录、日志文件和工具调用关联起来。

### 7.3 能力注册表

```text
MCPCapabilityRegistry
  tools: dict[str, MCPToolMeta]
  resources: dict[str, MCPResourceMeta]
  prompts: dict[str, MCPPromptMeta]
  diagnostics: list[MCPDiagnostic]
```

能力发现流程：

1. 启动时读取 MCP 配置。
2. 对启用的 Server 建立连接。
3. 调用能力发现接口，获取 Tool、Resource、Prompt 元数据。
4. 对名称做命名空间归一化：`server_name.capability_name`。
5. 过滤禁用、高风险或不符合策略的能力。
6. 注入到 Agent 工具描述中，等待模型选择。

---

## 8. 安全设计

### 8.1 输入验证

所有 MCP Tool 参数进入执行前必须完成以下校验：

- JSON 参数必须符合工具 schema。
- 字符串长度、列表长度、数值范围必须有上限。
- 文件路径必须解析为工作区内绝对路径。
- 受保护路径必须拒绝访问。
- 命令执行必须设置超时，输出必须截断。
- 外部 URL、数据库连接、云服务操作默认要求用户确认。

### 8.2 审批策略

现有 `approval.mode` 继续作为统一审批入口：

| 模式 | MCP 行为 |
|------|----------|
| `manual` | 高风险工具调用前展示工具名、参数、来源 Server 和风险说明，由用户确认。 |
| `review` | 使用模型审查工具调用；通过后执行，拒绝时把原因返回给主 Agent。 |
| `auto` | 仅自动批准低风险工具；高风险工具仍可按策略强制人工确认。 |

工具风险不能只由 Server 声明决定，必须由 Host 侧策略再次判断。

### 8.3 访问控制

- 默认只允许访问当前工作区。
- 默认禁止读取 `config.json`、`.env`、`.git`、虚拟环境和缓存目录。
- 默认禁止删除核心文件或递归删除目录。
- 默认禁止访问生产、真实数据、付费资源。
- 外部 MCP Server 的能力必须按名称和风险等级白名单暴露。

### 8.4 审计日志

每次工具调用记录：

```text
timestamp
session_id
audit_id
server_name
tool_name
arguments_redacted
approval_mode
approval_result
duration_ms
ok
error_code
output_preview
```

审计日志不保存完整密钥、完整大文件内容和模型隐藏推理内容。

---

## 9. 错误处理与故障隔离

### 9.1 错误分类

| 错误码 | 含义 | 是否可重试 |
|--------|------|------------|
| `CONFIG_INVALID` | MCP 配置不合法 | 否 |
| `SERVER_UNAVAILABLE` | Server 进程无法启动或连接失败 | 是 |
| `TOOL_NOT_FOUND` | 模型请求了不存在的工具 | 否 |
| `SCHEMA_INVALID` | 工具参数不符合 schema | 否 |
| `APPROVAL_DENIED` | 用户或自动审查拒绝执行 | 否 |
| `SECURITY_BLOCKED` | 命中安全边界 | 否 |
| `TOOL_TIMEOUT` | 工具执行超时 | 是 |
| `TOOL_FAILED` | 工具自身执行失败 | 视工具而定 |

### 9.2 隔离策略

- 每个 MCP Server 独立连接、独立超时、独立健康状态。
- 单个 Server 启动失败不影响其他 Server。
- 单个工具失败不终止 Agent 循环，除非用户明确停止。
- 连续失败达到阈值后将该 Server 标记为 `degraded`，本轮不再调用。
- 外部 Server 输出超过限制时截断，不把大输出直接塞回上下文。

### 9.3 恢复策略

- `stdio` Server 异常退出时，可按退避策略重启。
- HTTP Server 连接失败时，可短期缓存失败状态，避免每轮重复阻塞。
- 配置修复后通过重启应用重新发现能力。
- 用户可通过未来的 `/mcp` 命令查看 Server 状态和错误诊断。

---

## 10. 性能设计

| 问题 | 策略 |
|------|------|
| 启动发现慢 | 并行连接多个 Server，设置短超时，失败则降级。 |
| 上下文过大 | 只注入工具元数据，不注入 Resource 正文；Resource 按需读取。 |
| 输出过大 | 复用 `max_tool_output_chars` 截断策略。 |
| 重复发现 | Server 能力在会话内缓存，除非配置变更或手动刷新。 |
| 慢工具阻塞 | 每个工具独立超时，命令类工具默认更严格。 |

---

## 11. 可观测性设计

### 11.1 日志

日志分三类：

| 类型 | 内容 |
|------|------|
| 运行日志 | MCP Server 启动、连接、能力发现、健康状态变化。 |
| 调用日志 | 工具调用、参数摘要、审批结果、耗时、错误码。 |
| 安全日志 | 越界路径、受保护文件、危险命令、外部服务阻断。 |

### 11.2 指标

建议指标：

```text
mcp_server_up{server="local_project"}
mcp_tool_calls_total{server="local_project", tool="workspace.read_file", status="ok"}
mcp_tool_duration_seconds_bucket{server="local_project", tool="workspace.read_file"}
mcp_tool_denied_total{server="local_project", reason="approval_denied"}
mcp_security_blocked_total{reason="protected_path"}
mcp_server_restarts_total{server="local_project"}
```

### 11.3 健康检查

| 检查 | 条件 |
|------|------|
| Liveness | Host 进程存活，主循环可响应。 |
| Readiness | 配置合法，启用的关键 MCP Server 已连接，能力发现成功。 |
| Degraded | 非关键 Server 失败，但基础对话和内置工具可用。 |

---

## 12. 部署与运维

### 12.1 本地运行

默认推荐本地 `stdio` 方式：

```powershell
python main.py
```

MCP Server 随 Host 启动，由 Host 管理进程生命周期。优点是安全边界清晰、无公网暴露、便于课程项目调试。

### 12.2 容器化建议

如后续需要容器化：

- 使用非 root 用户运行。
- 工作目录只挂载必要项目目录。
- 密钥通过环境变量或密钥管理注入，不写入镜像。
- 健康检查覆盖 Host 和关键 MCP Server。
- 容器日志输出到 stdout/stderr，交由平台采集。

### 12.3 回滚策略

- `mcp.enabled=false` 可关闭整个 MCP 子系统。
- 单个 Server 可通过 `servers.<name>.enabled=false` 禁用。
- 内置工具保留，MCP 失败时不影响基础文件工具和对话能力。
- 配置变更均为非破坏性，回滚只需恢复 `config.json`。

---

## 13. 实施路线

| 阶段 | 内容 | 验证方式 |
|------|------|----------|
| 1 | 增加 MCP 配置模型和读取校验 | 单元测试配置合法/非法场景。 |
| 2 | 实现 `MCPClientManager` 和 Server 能力发现 | 用假 MCP Server 做集成测试。 |
| 3 | 接入 `ToolRouter`，让 Agent 可调用 MCP Tool | 冒烟验证工具发现、调用、失败返回。 |
| 4 | 接入审批、安全、审计日志 | 验证写文件、命令执行、越界路径被拦截。 |
| 5 | 暴露 Resource 和 Prompt | 验证按需读取，不扩大上下文。 |
| 6 | 可选实现 `LocalMCPServer` | 用外部 MCP Host 调用本项目工具。 |
| 7 | 增加 `/mcp` 状态命令 | 验证 Server 列表、健康状态和诊断输出。 |

建议新增文件：

```text
ai_voice_agent/
└── mcp/
    ├── __init__.py
    ├── config.py          # MCP 配置读取与校验
    ├── client.py          # MCP 连接与请求封装
    ├── registry.py        # Tool/Resource/Prompt 能力注册表
    ├── router.py          # 内置工具与 MCP 工具统一路由
    ├── security.py        # MCP 参数安全校验与脱敏
    ├── audit.py           # 审计日志
    └── server.py          # 可选：本项目 Local MCP Server
```

---

## 14. 测试策略

| 测试类型 | 覆盖点 |
|----------|--------|
| 单元测试 | 配置校验、命名空间归一化、schema 校验、错误分类、脱敏逻辑。 |
| 安全测试 | 工作区外路径、受保护路径、危险命令、超长参数、非法 JSON。 |
| 集成测试 | 假 MCP Server 的连接、工具发现、工具调用、Server 退出恢复。 |
| 回归测试 | 原有内置工具、审批模式、Skill、记忆系统不受影响。 |
| 冒烟测试 | `python main.py` 启动后基础对话可用，`/mcp` 能查看状态。 |

最低验收标准：

1. MCP 关闭时，现有功能行为不变。
2. MCP 开启且无 Server 可用时，应用能正常启动并显示降级诊断。
3. 低风险 MCP Tool 可调用并返回结构化结果。
4. 高风险 MCP Tool 必须经过审批或被策略拦截。
5. 外部工具失败不会导致 Agent 主循环崩溃。

---

## 15. 风险与限制

| 风险 | 影响 | 控制措施 |
|------|------|----------|
| 外部 MCP Server 行为不可控 | 可能执行高风险操作 | Host 侧二次审批、能力白名单、安全审计。 |
| 工具数量过多 | Prompt 变大，模型选择变差 | 按 Server 分组、按需暴露、限制工具描述长度。 |
| 输出内容过大 | 上下文膨胀、响应变慢 | 输出截断、分页读取、Resource 按需加载。 |
| 配置错误 | 启动失败或能力缺失 | 配置 schema 校验、清晰错误提示、关闭开关。 |
| 网络工具引入外部成本 | 产生费用或泄露数据 | 默认关闭外部网络工具，启用前人工确认。 |

---

## 16. 结论

本项目适合采用“保留内置工具 + 增量引入 MCP Client + 可选暴露 Local MCP Server”的演进路线。这样可以在不破坏当前语音 Agent 可用性的前提下，逐步获得 MCP 的标准化扩展能力。

第一阶段应优先完成配置模型、MCP Client 管理、工具能力发现、统一路由和安全审批集成；等本地 `stdio` 路径稳定后，再考虑 HTTP 传输、容器化、Prometheus 指标和外部 MCP Server 生态接入。
