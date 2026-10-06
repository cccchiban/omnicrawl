# OmniCrawl 本地 API 接入文档

## 1. 启动与配置

安装依赖：

```powershell
pip install -r requirements.txt
```

推荐在 `config.toml` 中配置 API 段：

```yaml
api:
  bearer_token: "替换为随机长令牌"
  host: "127.0.0.1"
  port: 8765
  allowed_origins:
    - "http://localhost:5173"
  confirmation_timeout_seconds: 300
```

也可用 JSON 等价写法。完整多模型配置见 `config.example.toml` 与 `models.example.toml`。

令牌也可通过环境变量提供，且优先于配置文件：

```powershell
$env:OMNICRAWL_API_TOKEN = "替换为随机长令牌"
python -m omnicrawl.api
```

服务只接受 `127.0.0.1`、`localhost` 或 `::1`，拒绝 `0.0.0.0`。Token 为空、
CORS Origin 使用 `*` 或监听地址不是回环地址时，服务拒绝启动。

默认地址：

- API：`http://127.0.0.1:8765/api/v1`
- Swagger：`http://127.0.0.1:8765/docs`
- OpenAPI：`http://127.0.0.1:8765/openapi.json`
- 健康检查：`http://127.0.0.1:8765/health`

## 2. 鉴权与响应格式

除 `/health`、`/docs` 和 `/openapi.json` 外，请求必须携带：

```http
Authorization: Bearer <token>
```

成功响应：

```json
{"data": {}}
```

错误响应：

```json
{
  "error": {
    "code": "RUN_ACTIVE",
    "message": "当前已有生成任务运行。",
    "details": {"run_id": "..."}
  }
}
```

常用错误码：

| HTTP | code | 说明 |
|---|---|---|
| 400 | `INVALID_MESSAGE`、`INVALID_EVENT_ID`、`INVALID_ANSWER` | 请求语义不合法 |
| 401 | `UNAUTHORIZED` | Token 缺失或错误 |
| 404 | `RUN_NOT_FOUND`、`CONFIRMATION_NOT_FOUND`、`QUESTION_NOT_FOUND`、`SUBAGENT_CONFIRMATION_NOT_FOUND`、`ARTIFACT_NOT_FOUND`、`SUBAGENT_NOT_FOUND` | 资源不存在或不属于当前会话 |
| 409 | `RUN_ACTIVE`、`CONFIRMATION_RESOLVED`、`QUESTION_RESOLVED` | 当前状态不允许该操作 |
| 503 | `SUBAGENT_UNAVAILABLE` | SubAgent 功能未启用或当前不可用 |
| 422 | `VALIDATION_ERROR` | 请求体或查询参数校验失败 |

## 3. 接口清单

### 系统与生成

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 无鉴权健康检查 |
| GET | `/api/v1/runtime` | 工作区、会话、模型、审批模式和活动任务 |
| POST | `/api/v1/runs` | 提交 `{ "message": "..." }`，返回 `run_id` |
| GET | `/api/v1/runs/{run_id}` | 查询任务状态 |
| GET | `/api/v1/runs/{run_id}/events` | SSE 事件流 |
| POST | `/api/v1/runs/{run_id}/cancel` | 请求取消任务 |
| POST | `/api/v1/runs/{run_id}/confirmations/{id}` | 提交 `{ "approved": true }` |
| POST | `/api/v1/runs/{run_id}/questions/{id}` | 回答 `ask_user` 提问，提交 `{ "answer": "..." }`；select 必须使用已声明选项 |
| GET | `/api/v1/monitors` | 列出当前 Agent 受管的后台任务 |
| GET | `/api/v1/monitors/{monitor_id}` | 查询一个后台任务状态 |
| GET | `/api/v1/monitors/{monitor_id}/events` | 后台任务日志 SSE；支持 `cursor`、`Last-Event-ID`、`follow` 和 `max_events` |
| GET | `/api/v1/subagents` | 列出当前 Agent 当前会话可见的后台 SubAgent 任务 |
| GET | `/api/v1/subagents/events` | 当前会话的后台任务与审批 SSE；支持 `Last-Event-ID`、`follow` |
| GET | `/api/v1/subagents/confirmations` | 列出当前会话等待远程决定的跨父 Run 后台审批 |
| POST | `/api/v1/subagents/confirmations/{confirmation_id}` | 提交 `{ "approved": true }` 决议 |
| GET | `/api/v1/subagents/{task_id}` | 查询当前会话中的单个后台 SubAgent 任务 |
| POST | `/api/v1/subagents/{task_id}/cancel` | 请求取消当前会话中的单个后台 SubAgent 任务 |

SubAgent 控制面不提供 `POST /api/v1/subagents`：远程客户端不能经 HTTP 创建或重新分发任务。任务、审批和事件流始终使用当前 Agent 的 owner/session 范围；其他会话的任务或审批统一返回对应的 `*_NOT_FOUND`。跨父 Run 的后台确认仅保存在当前服务进程；服务关闭、任务取消或超时会拒绝待决请求，服务重启后不会恢复。

服务全局同时只允许一个生成任务。生成期间，会话、项目、模型、推理强度、审批模式和记忆清理等修改接口返回 409。

### 会话

| 方法 | 路径 | 说明 |
|---|---|---|
| GET/POST | `/api/v1/sessions` | 列表或新建会话；列表支持 `limit`、`archived` |
| GET | `/api/v1/sessions/diagnostics` | 提示历史损坏诊断总览（不改写磁盘） |
| GET | `/api/v1/sessions/{id}/diagnostics` | 指定会话转录与提示历史诊断 |
| GET | `/api/v1/sessions/{id}/events` | 读取持久化事件 |
| POST | `/api/v1/sessions/{id}/resume` | 恢复会话 |
| PATCH | `/api/v1/sessions/current` | 重命名当前会话 |
| POST | `/api/v1/sessions/current/compact` | 压缩当前会话 |
| POST | `/api/v1/sessions/current/archive` | 归档当前会话 |
| DELETE | `/api/v1/sessions/{id}` | 删除非活动会话 |
| POST | `/api/v1/sessions/current/export` | 导出 Markdown |
| GET | `/api/v1/sessions/{id}/artifacts/{path}` | 读取当前会话 HTML artifact |

### 项目、配置和辅助能力

| 方法 | 路径 | 说明 |
|---|---|---|
| GET/POST/PATCH/DELETE | `/api/v1/projects` | 列表、创建、重命名、移除项目记录 |
| POST | `/api/v1/projects/import` | 导入已有项目 |
| POST | `/api/v1/projects/pin` | 设置置顶状态 |
| POST | `/api/v1/projects/switch` | 切换 Agent 工作区 |
| GET | `/api/v1/models` | 兼容旧扁平模型列表（`id/name/provider`） |
| GET | `/api/v1/models/catalog` | 双列目录：`custom` + `detected` + `diagnostics` |
| POST | `/api/v1/models/refresh` | 强制刷新自动发现缓存 |
| PUT | `/api/v1/models/current` | 原子切换并保存当前模型；持久化失败时运行时保持旧模型 |
| PUT | `/api/v1/reasoning` | 切换并保存推理强度 |
| PUT | `/api/v1/approval` | 切换并保存审批模式 |
| GET | `/api/v1/history` | 查询 Prompt 历史 |
| GET | `/api/v1/skills` | Skill 列表 |
| GET | `/api/v1/mcp` | MCP 状态 |
| POST | `/api/v1/memory/clean` | 清理过期记忆 |

`PUT /api/v1/models/current` 请求体兼容：

```json
{"model": "gpt-5.2"}
```

以及规范选择：

```json
{"source": "custom", "key": "default-chat"}
```

```json
{
  "source": "detected",
  "profile": "openai-main",
  "model_id": "gpt-5.2",
  "protocol": "openai_chat_completions"
}
```

具体请求 Schema 以 `/openapi.json` 为准。

## 4. SSE 事件流

每个事件包含递增 ID、事件名和 JSON 数据：

```text
id: 3
event: assistant.delta
data: {"delta":"你好"}
```

事件类型：

- `run.started`
- `assistant.delta`
- `status.changed`
- `tool.started`
- `tool.completed`
- `confirmation.required`
- `usage.updated`
- `artifact.available`
- `subagent.batch.created`
- `subagent.task.queued`
- `subagent.task.running`
- `subagent.task.started`
- `subagent.task.waiting_approval`
- `subagent.task.approval_cancelled`
- `subagent.task.completed`
- `subagent.task.failed`
- `subagent.task.cancelled`
- `subagent.confirmation.required`
- `subagent.confirmation.resolved`
- `subagent.confirmation.expired`
- `subagent.confirmation.cancelled`
- `run.completed`
- `run.cancelled`
- `run.failed`

后台任务日志使用独立 SSE 路径：标准输出和标准错误事件为 `monitor.output`，启动、停止、完成和失败事件为 `monitor.status`。这些接口只读；启动和停止后台任务仍必须经 Agent 的 `monitor` 内置工具，继续遵循工具审批模式。

`/api/v1/subagents/events` 是当前会话的独立后台控制流：即使创建任务的父 Run 已终态，仍会发送安全的生命周期、审批请求、决议、超时和取消事件。事件只包含任务来源、工具名、脱敏参数和风险类别，不包含任务 prompt、隐藏推理或原始工具输出。断线重连可使用 `Last-Event-ID`；游标早于最早保留事件时返回 `EVENT_CURSOR_EXPIRED`。

默认最多保留最近 100 个运行记录，每个运行或会话级后台流最多保留 2000 个事件；活动任务不会因保留上限被回收。运行记录与后台审批均仅保存在当前服务进程，服务重启后请通过会话事件接口恢复已持久化消息。

服务关闭时会先取消活动任务、唤醒待确认请求并等待生成线程退出；若等待超时，不会提前关闭仍被线程使用的 Agent，而是在最后一个生成线程退出后延迟关闭资源。关闭后创建新任务会返回 `SERVICE_CLOSED`。

模型隐藏推理内容和 HTML artifact 正文不会写入 SSE；`tool.completed` 与 `artifact.available` 仅返回可公开的 artifact 元数据。客户端应使用 artifact 读取接口获取 HTML 正文。SubAgent 生命周期事件只包含 batch/task ID、角色、描述、状态、安全摘要、usage 和 artifact 引用；`subagent.task.waiting_approval` 只追加工具名、脱敏参数摘要与风险类别。外层 `subagent` 的 `tool.started` 参数会被压缩为任务数量、角色列表、并发和 `fail_fast`，不会发送完整任务 prompt。

## 5. 调用示例

创建任务：

```powershell
$headers = @{ Authorization = "Bearer $env:OMNICRAWL_API_TOKEN" }
$body = @{ message = "分析当前项目结构" } | ConvertTo-Json
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:8765/api/v1/runs -Headers $headers -ContentType application/json -Body $body
```

浏览器不能使用原生 `EventSource`，因为它无法设置 Bearer Header。使用 `fetch()` 读取流：

```javascript
const response = await fetch(`/api/v1/runs/${runId}/events`, {
  headers: { Authorization: `Bearer ${token}` },
});

const reader = response.body.getReader();
const decoder = new TextDecoder();
let buffer = "";

while (true) {
  const { value, done } = await reader.read();
  if (done) break;
  buffer += decoder.decode(value, { stream: true });
  const blocks = buffer.split("\n\n");
  buffer = blocks.pop() ?? "";
  for (const block of blocks) {
    const event = block.match(/^event: (.+)$/m)?.[1];
    const data = block.match(/^data: (.+)$/m)?.[1];
    if (event && data) handleEvent(event, JSON.parse(data));
  }
}
```

收到父 Run 内的 `confirmation.required` 后，使用其中的 `confirmation_id` 提交批准或拒绝。若超时未提交，默认拒绝该工具调用。由同步 SubAgent 风险工具触发的确认会额外携带可选 `subagent` 对象（`task_id`、`batch_id`、`agent_label`、任务描述）；旧客户端可忽略该字段。

收到 `ask_user.required` 后，使用其中的 `question_id`、`kind`、`question` 和 `options` 展示提问，并向 `/api/v1/runs/{run_id}/questions/{question_id}` 提交 `{ "answer": "..." }`。所有 kind 的提问都带非空 `options`；`select` 只能提交 `options` 中的值，`question`/`confirm` 可提交选项值或自定义文本；取消或超时后问题不可再次回答。

若后台 SubAgent 在父 Run 结束后才请求风险操作，客户端应订阅 `/api/v1/subagents/events` 中的 `subagent.confirmation.required`，或轮询 `/api/v1/subagents/confirmations`，再向 `/api/v1/subagents/confirmations/{confirmation_id}` 提交决议。该入口只接受创建任务时所属的当前会话；任务/批次/服务取消与超时均会拒绝请求，任何迟到批准返回 `CONFIRMATION_RESOLVED` 或 `SUBAGENT_CONFIRMATION_NOT_FOUND`，不会执行工具。

## 6. 源码边界

API 实现已按资源拆分：

```text
rust/crates/omnicrawl-api/src/
├── lib.rs               # 公共导出门面
├── main.rs              # 服务入口
├── app.rs               # 应用工厂、CORS、配置装载
├── openapi.rs           # 请求/响应模型与 OpenAPI 契约
├── service.rs           # AgentAPIService 运行编排
├── error.rs             # 错误面与响应封装
└── routes/              # system/runs/monitors/sessions/projects/configuration/support
```
