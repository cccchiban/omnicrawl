# MCP 使用规范

本文档面向 OmniCrawl 和项目维护者，说明在本项目中如何渐进式理解、配置、调用和排障 MCP。

核心原则：先读最低成本信息，再按任务需要深入。不要在普通任务中一次性读取所有 MCP 源码或设计文档。

---

## 1. 先读这个：MCP 快速摘要

本项目的 MCP 支持由 Host 侧 Agent、MCP Client Manager 和可选 Local MCP Server 组成：

- Host：`LocalToolAgent`，负责模型循环、审批、工具路由、审计和最终回复；MCP 能力进入 Host 工具目录后注册到 Provider 顶层 `tools`，模型直接原生调用真实工具名。
- Client：`omnicrawl/mcp/client.py`，负责连接 Server、发现 Tool/Resource/Prompt、调用和降级。
- Local Server：`omnicrawl/mcp/server.py`，通过 `stdio` 暴露当前项目的安全工具和上下文。
- 配置入口：`config.toml` 的 `mcp` 段，示例见 `config.example.toml`。
- 状态入口：运行时输入 `/mcp` 查看 Server、Tool、Resource、Prompt 和诊断。

默认边界：

- MCP 默认关闭，设置 `mcp.enabled=true` 才会连接启用的 Server。
- 当前可用传输是本地 `stdio` 和远程 `streamable_http`；远程传输使用 MCP Streamable HTTP 的 JSON/SSE 响应和会话 ID。
- 外部网络能力默认不暴露，除非配置策略明确允许。
- 高风险 MCP Tool 必须继续走 Host 侧审批或审查，不能只信任 Server 声明。
- 审计日志默认写入 `.omnicrawl/logs/mcp-audit.jsonl`，该目录不提交到仓库。

---

## 2. 渐进式披露：按任务读取哪些文档

### 2.1 只查看或解释 MCP 状态

先读：

1. 本文档第 1 节和第 4 节。
2. `/mcp` 命令输出。

通常不需要读：

- `docs/MCP_DESIGN_TECHNICAL.md`
- MCP Client/Server 源码

### 2.2 配置 MCP Server

先读：

1. 本文档第 3 节。
2. `config.example.toml` 的 `mcp` 段。

如遇到配置校验失败，再读：

1. `omnicrawl/mcp/config.py`
2. `docs/MCP_DESIGN_TECHNICAL.md` 第 6、7、8 节。

### 2.3 调用 MCP Tool / Resource / Prompt

先读：

1. 本文档第 4 节。
2. `/mcp` 输出中的 Tool、Resource、Prompt 列表。

如需要理解某个能力来自哪里，再读：

1. `omnicrawl/mcp/server.py` 中对应 Tool/Resource/Prompt。
2. 外部 MCP Server 的官方说明或本地配置。

### 2.4 修改 MCP Client 或安全策略

先读：

1. 本文档第 5、6 节。
2. `docs/MCP_DESIGN_TECHNICAL.md` 第 8、9、11、14 节。
3. `omnicrawl/mcp/client.py`
4. `omnicrawl/mcp/security.py`
5. `omnicrawl/mcp/audit.py`

### 2.5 修改 Local MCP Server

先读：

1. 本文档第 4、5 节。
2. `omnicrawl/mcp/server.py`
3. `tests/test_mcp.py` 的 Local MCP Server 测试。

如涉及对外协议兼容，再读：

1. `docs/MCP_DESIGN_TECHNICAL.md` 第 5、7、13、14 节。

### 2.6 MCP 排障

先读：

1. `/mcp` 输出。
2. `.omnicrawl/logs/mcp-audit.jsonl` 中对应 `audit_id`。
3. 本文档第 7 节。

如还不能定位，再读：

1. `omnicrawl/mcp/client.py`
2. `omnicrawl/mcp/server.py`
3. `docs/MCP_DESIGN_TECHNICAL.md` 第 9、11 节。

---

## 3. MCP 配置规范

推荐最小配置：

```json
{
  "mcp": {
    "enabled": true,
    "default_timeout_seconds": 30,
    "servers": {
      "local_project": {
        "enabled": true,
        "transport": "stdio",
        "command": "python",
        "args": ["-m", "omnicrawl.mcp.server"],
        "env": {},
        "timeout_seconds": 360,
        "risk_level": "trusted"
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

配置要求：

- Server 名称只能使用小写字母、数字、下划线和连字符。
- `stdio` 必须提供 `command`；`streamable_http` 必须提供 `url`。
- `env` 仅作为 `stdio` 子进程环境变量传入；`stdio` Server 不需要额外认证。
- `headers` 仅用于 `streamable_http`，会原样附加到每个 HTTP 请求，可声明 `Authorization`、`X-API-Key` 等认证头；不会注入 `stdio` 子进程。
- `timeout_seconds` 范围是 1 到 360 秒。
- `risk_level=trusted` 只表示来源可信，不代表跳过审批。
- 不要把真实密钥写进 `config.example.toml` 或源码；真实密钥只能存在本地 `config.toml` 或环境变量。
- HTTP 认证示例：

```yaml
mcp:
  servers:
    remote_service:
      enabled: true
      transport: streamable_http
      url: https://example.com/mcp
      headers:
        Authorization: "Bearer replace-with-token"
        X-API-Key: "replace-with-api-key"
      timeout_seconds: 30
      risk_level: external
```

- 引入外部 MCP Server 前，必须先明确能力范围、数据边界、成本和是否联网。

环境变量：

- `MCP_ENABLED`：临时覆盖 MCP 全局开关。
- `MCP_DEFAULT_TIMEOUT_SECONDS`：临时覆盖默认超时。
- `MCP_WORKSPACE_ROOT`：Local MCP Server 的工作区根目录，由 Client 启动时自动传入。

---

## 4. `/settings` 三级管理

全屏 TUI 的 `/settings` 提供三级 MCP 管理入口：

1. `运行设置 → MCP 工具`：保留 MCP 总开关入口。
2. `MCP 全局设置`：管理总开关、外部网络 Tool 策略、写入/命令确认、审计日志、默认超时和 Tool 输出上限。
3. `MCP Server`：按 Server 启用/禁用、添加、编辑和删除；编辑器支持 `stdio` / `streamable_http`、命令/参数、URL、HTTP 请求头、超时、风险等级和连接测试。

设置默认保存到用户配置目录 `~/.OmniCrawl/config.toml`，显式配置路径或环境变量仍会生效；保存后事务式重建当前 Agent 的 MCP Manager。stdio 环境变量和 HTTP 请求头只显示已配置数量；编辑时留空保持原值，输入 `KEY=VALUE;KEY2=VALUE` 才替换，凭据不会回显。HTTP 请求头只附加到远程 Streamable HTTP 请求，协议保留头由 Client 自动维护。

## 5. MCP 调用规范

### 5.1 Tool 命名与调用

MCP Tool 注入 Agent 后使用 `server.tool` 名称。当前内置 `local_project` Server 不暴露工作区 Tool；它仅提供项目文档 Resource、健康状态 Resource 和常用 Prompt。其他启用的 MCP Server 仍按其能力发现结果注册 Tool，并统一经过 Host 审批。

调用规则：

- 一次只调用一个 MCP Tool，拿到结果后再决定下一步。
- 只读工具优先用于收集证据；写入、替换、命令工具必须有明确任务目标。
- 写入或命令类工具不要绕过 Host 审批；审批拒绝时应基于拒绝原因调整方案或停止。
- 参数必须符合 Tool schema；缺必填字段、类型不符、超长字符串会被 Host 拦截。

### 5.2 Resource 读取

Resource 用于只读上下文，不产生副作用。常见 URI：

- `project://README.md`
- `omnicrawl://docs/TERMINAL_UI.md`
- `omnicrawl://docs/MCP_USAGE.md`
- `project://agents-instructions`
- `server://local_project/health`

在 Agent 工具列表中，Resource 会转换为 `mcp_read_resource__{logical_uri}` 工具。`omnicrawl://docs/` 下的文档随 PyPI 安装包提供；只在需要上下文正文时读取，不要把所有 Resource 一次性读完。

### 5.3 Prompt 获取

Prompt 用于稳定任务模板，常见 Prompt：

- `project_doc_writer`
- `code_review`
- `debug_triage`
- `safe_change_plan`

在 Agent 工具列表中，Prompt 会转换为 `mcp_get_prompt__{logical_name}` 工具。获取 Prompt 后，要结合当前用户任务继续执行，不要把 Prompt 原样当成交付结果。

---

## 6. 安全与审批规范

Host 侧永远是最终安全边界：

- 受保护路径：`.git`、`.env`、`config.toml`、`models.toml`、历史 `config.json`、虚拟环境、缓存目录。
- 普通文件工具默认以工作区为基准解析相对路径；本机绝对路径不再被工作区边界拦截，但受保护路径仍被拒绝。
- 外部 MCP Server 默认不暴露能力，除非策略允许。
- 命令执行必须设置超时，输出会截断。
- `manual` 模式下，写入、替换、命令、删除倾向工具默认需要确认；`review`（自动审查）模式下，通过 bash/powershell 执行的命令先做静态分类——只有删除类与“下载并执行不明脚本”类命令才进入模型自动审查，其余命令直接放行；MCP 的删除/清空类工具调用同样进入自动审查。审查请求与主对话完全隔离，只携带待审查调用、最近一条用户消息摘要与最近一次 ask_user 问答（问题+用户回答），不再复用主对话上下文，避免审查模型被历史内容污染；可通过 `[approval] review_model` 指定独立审查模型。

AI 调用 MCP 时必须遵守：

- 不读取密钥文件，不要求用户把密钥写进示例配置。
- 访问工作区外路径时仍需遵守受保护路径与审批边界。
- 不把 `risk_level=trusted` 理解为免审批。
- 不把 MCP Server 返回内容视为一定可信；涉及代码、命令、配置时仍要验证。

---

## 7. 审计与可观测性

MCP Tool 调用会记录审计事件：

- `session_id`
- `audit_id`
- `server_name`
- `tool_name`
- 脱敏后的参数
- 审批模式与审批结果
- 耗时、状态、错误码
- 输出预览

审计位置：

```text
.omnicrawl/logs/mcp-audit.jsonl
```

排查时优先用工具结果中的 `audit_id` 关联审计日志。审计日志只保存脱敏参数和输出预览，不保存 MCP 配置中的完整请求头、密钥、大文件正文或模型隐藏推理内容。

---

## 8. 常见排障路径

| 现象 | 优先检查 |
|------|----------|
| `/mcp` 显示 MCP 已关闭 | `config.toml` 的 `mcp.enabled` 或 `MCP_ENABLED` |
| Server 为 degraded | `/mcp` 诊断、Server URL、HTTP 状态码、超时、工作区环境 |
| Tool 不出现在列表 | Server 是否启用、能力发现是否成功、外部能力是否被策略拦截 |
| Tool 返回 `SCHEMA_INVALID` | 参数是否缺必填字段、类型是否匹配、字符串是否超长 |
| Tool 返回 `APPROVAL_DENIED` | 用户或审查模型拒绝，读取拒绝原因后调整方案 |
| Tool 返回 `SERVER_UNAVAILABLE` | Server 未连接、进程退出、启动命令错误 |
| Resource 读取失败 | URI 是否在 `/mcp` 列表中，是否命中受保护路径 |
| Prompt 获取失败 | Prompt 名称是否在 `/mcp` 列表中，arguments 是否为 JSON 对象 |

---

## 9. 修改 MCP 后的验证清单

最低验证：

```powershell
python -m unittest discover -s tests
python -m compileall omnicrawl
```

涉及 Local MCP Server 时，重点验证：

- `tools/list` 能发现 Tool。
- `tools/call` 能调用只读工具。
- 受保护路径会被拒绝。
- Resource 可按 URI 读取。
- Prompt 可按名称获取。
- `manual` 模式下写入和命令类工具仍需要 Host 审批；`review`（自动审查）模式下，bash/powershell 命令先经静态分流，仅删除类与下载执行不明脚本类进入 Host 自动审查，MCP 删除/清空类工具调用同样进入自动审查。
- `/mcp` 能显示 Server、Tool、Resource、Prompt 和诊断。

涉及安全策略时，重点验证：

- 越界路径被拒绝。
- 密钥字段在审计日志中脱敏。
- 外部 Server 在默认策略下不暴露能力。
- 连续失败会进入 degraded 诊断。
