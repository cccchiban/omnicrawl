# OmniCrawl 本地 API 接入文档

## 1. 启动与配置

安装依赖：

```powershell
pip install -r requirements.txt
```

推荐在 `config.json` 中配置：

```json
{
  "api": {
    "bearer_token": "替换为随机长令牌",
    "host": "127.0.0.1",
    "port": 8765,
    "allowed_origins": ["http://localhost:5173"],
    "confirmation_timeout_seconds": 300
  }
}
```

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
| 400 | `INVALID_MESSAGE`、`INVALID_EVENT_ID` | 请求语义不合法 |
| 401 | `UNAUTHORIZED` | Token 缺失或错误 |
| 404 | `RUN_NOT_FOUND`、`CONFIRMATION_NOT_FOUND`、`ARTIFACT_NOT_FOUND` | 资源不存在 |
| 409 | `RUN_ACTIVE`、`CONFIRMATION_RESOLVED` | 当前状态不允许该操作 |
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
| GET | `/api/v1/monitors` | 列出当前 Agent 受管的后台任务 |
| GET | `/api/v1/monitors/{monitor_id}` | 查询一个后台任务状态 |
| GET | `/api/v1/monitors/{monitor_id}/events` | 后台任务日志 SSE；支持 `cursor`、`Last-Event-ID`、`follow` 和 `max_events` |

服务全局同时只允许一个生成任务。生成期间，会话、项目、模型、推理强度、审批模式和记忆清理等修改接口返回 409。

### 会话

| 方法 | 路径 | 说明 |
|---|---|---|
| GET/POST | `/api/v1/sessions` | 列表或新建会话；列表支持 `limit`、`archived` |
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
| GET | `/api/v1/models` | 从上游 `/models` 获取模型列表 |
| PUT | `/api/v1/models/current` | 切换并保存模型 |
| PUT | `/api/v1/reasoning` | 切换并保存推理强度 |
| PUT | `/api/v1/approval` | 切换并保存审批模式 |
| GET | `/api/v1/history` | 查询 Prompt 历史 |
| GET | `/api/v1/skills` | Skill 列表 |
| GET | `/api/v1/mcp` | MCP 状态 |
| POST | `/api/v1/memory/clean` | 清理过期记忆 |

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
- `run.completed`
- `run.cancelled`
- `run.failed`

后台任务日志使用独立 SSE 路径：标准输出和标准错误事件为 `monitor.output`，启动、停止、完成和失败事件为 `monitor.status`。这些接口只读；启动和停止后台任务仍必须经 Agent 的 `monitor` 内置工具，继续遵循工具审批模式。

断线重连时发送 `Last-Event-ID`，服务会重放该 ID 之后的内存事件。运行记录仅保存在当前服务进程；服务重启后请通过会话事件接口恢复已持久化消息。

模型隐藏推理内容和 HTML artifact 正文不会写入 SSE；`tool.completed` 与 `artifact.available` 仅返回可公开的 artifact 元数据。客户端应使用 artifact 读取接口获取 HTML 正文。

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

收到 `confirmation.required` 后，使用其中的 `confirmation_id` 提交批准或拒绝。若超时未提交，默认拒绝该工具调用。
