# 触发接口（外部事件拉起独立 loop）

外部组件（定时器、传感器脚本、CI、监控告警、手机快捷指令）需要在某个条件成立时让 Agent
**干一件事**：每天早上收集新闻、土壤湿度跌破阈值时给建议、告警触发后查资料并回复。

**触发接口**把这件能力以本机 REST 形式暴露出去：调用方提交一条自定义消息，OmniCrawl 拉起一个
**独立 loop**（自己的一次会话、自己的内核进程、自己的一次回合），跑完把结果落盘、按需投递到
消息平台。

一句话：**它是 `POST /api/v1/runs` 的无人值守版本——不占用交互会话，不与用户抢单活动名额。**
对外只承诺「拉起 + 投递」，**条件判定留给外部组件**。

## 1. 适用场景

| 场景 | 用法 |
|---|---|
| 每天早上 8 点收集今日新闻并发到 Telegram | 外部定时器（Windows 计划任务 / cron）`POST /v1/triggers` + `notify.type = "telegram"` |
| 花盆土壤湿度低于阈值时给处理建议 | 传感器脚本命中后 `POST /v1/triggers`，`data` 带湿度与阈值 |
| CI 构建失败后自动排查并总结原因 | 流水线失败钩子 `POST /v1/triggers`，`wait_seconds` 同步等结论 |
| 监控盘库/服务心跳异常，让 AI 先做一轮初步诊断 | 告警系统 webhook → 本接口 |
| 想确认服务在不在、并发与队列现状 | `GET /v1/status` |

**不适用**：需要人当场点「批准」的任务。无人值守下没有人在终端前，工具审批只能靠自动审查
（见第 6 节），`ask_user` 不在工具表里。

## 2. 为什么不挂在 `/api/v1/runs` 上

本地 API 的 `POST /api/v1/runs` 是**交互式**入口，两条语义直接冲突：

- **单活动生成名额**：一个 Run 在跑时，下一次提交返回 `RUN_ACTIVE`。无人值守任务动辄跑几分钟，
  用户此刻在 TUI 或手机上说话就会撞上这个错误。
- **共享会话**：Run 写进当前会话。每日新闻、告警诊断这类噪音会污染对话历史，用户下次打开会话
  先看到一堆跟自己无关的转录。

因此触发接口是**独立常驻服务**（新 crate、新端口、新会话根），与 `[api]`（Bearer 鉴权、
交互式）、`[decision_api]`、`[fetch_api]` 同一套「本机共用、脱离宿主」的形态：

- 触发不会占住用户的交互会话与单活动名额；
- 用户在 TUI 里改配置、切模型、切工作区，不会打断正在跑的触发；
- 宿主没起（甚至没装 TUI）也能跑，脚本随时可用。

## 3. 启动与配置

### 3.1 配置（`config.toml` 的 `[trigger_api]` 段）

```toml
[trigger_api]
enabled = false
host = "127.0.0.1"
port = 8769
max_concurrency = 1
queue_limit = 32
default_timeout_seconds = 900
session_mode = "per_trigger"
default_workspace = ""
tools = ["read", "list", "find", "grep", "memory_search", "memory_read",
         "kb_search", "kb_read", "web_search", "fetcher"]
system_prompt_append = ""
retention_hours = 168
```

| 字段 | 默认值 | 说明 |
|---|---|---|
| `enabled` | `false` | 是否随宿主启动常驻服务 |
| `host` | `127.0.0.1` | 只接受回环地址（`127.0.0.1` / `localhost` / `::1`） |
| `port` | `8769` | 与本地 API（8765）、决策接口（8767）、抓取接口（8768）错开 |
| `max_concurrency` | `1` | 同时最多跑几个 loop（每个 slot 一套内核进程与工具表，1–4） |
| `queue_limit` | `32` | 排队上限；满了返回 429，**不静默丢弃** |
| `default_timeout_seconds` | `900` | 单次触发的回合上限；请求可用 `timeout_seconds` 收窄（60–7200） |
| `session_mode` | `per_trigger` | `per_trigger`：每次新会话；`keyed`：允许请求用 `session_key` 复用持久会话 |
| `default_workspace` | 空 | 请求未给 `workspace` 时用它；空表示用 `config.toml` 的当前工作区 |
| `tools` | 只读 + 记忆/知识库 + 联网检索 | 无人值守工具白名单（**白名单语义**，见第 6 节） |
| `system_prompt_append` | 空 | 追加到无人值守约束之后的部署专属提示（见 4.1） |
| `retention_hours` | `168` | 触发记录与事件在**服务进程内**的保留时长 |

段缺失、读盘失败或取值非法（非回环地址、端口越界、并发越界）时按「未启用」处理并回落到默认值：
触发接口是可选的外部入口，配置坏掉不应影响宿主内部功能。

### 3.2 启动方式

```powershell
omnicrawl trigger ensure   # 确保本机有一份可用服务（已就绪则复用，否则拉起并等就绪）
omnicrawl trigger serve    # 前台常驻监听（由 ensure 以脱离宿主的方式拉起）
omnicrawl trigger status   # 打印配置、监听状态、并发与队列
omnicrawl trigger stop     # 停止本机共用的服务
```

服务是**本机共用**的常驻进程：按 PID 文件（`~/.OmniCrawl/trigger_api.pid`）与服务日志
（`~/.OmniCrawl/trigger_api.log`）认定是否已就绪（不发起网络探测），就绪则复用、否则拉起；
进程以「脱离宿主」的方式创建，OmniCrawl 退出（含崩溃）都不回收它。因此外部脚本随时可以调用，
不必依赖宿主是否在跑。

默认地址：`http://127.0.0.1:8769`。

### 3.3 条件判定在哪里

**完全在外部组件**：时间（cron / 计划任务）、温度（传感器脚本）、湿度、CI 状态、告警规则，
都归调用方。触发接口不内置调度器，也不解析 cron 表达式——那会把「谁来判断条件」变成两个
真相源，而外部组件本来就最清楚自己的阈值与迟滞。

三条等价写法（都只是「到点了就发一次 HTTP」）：

```powershell
# Windows 计划任务（每天 08:00）
schtasks /create /tn "OmniCrawl-News" /sc daily /st 08:00 /tr `
  "powershell -NoProfile -Command \"Invoke-RestMethod -Method Post -Uri http://127.0.0.1:8769/v1/triggers -ContentType application/json -Body (@{message='收集今天的人工智能新闻，挑 5 条最重要的，每条一句话'; notify=@{type='telegram'; chat_id=123456789}} | ConvertTo-Json -Depth 5)\""
```

```cron
# crontab：每天 08:00
0 8 * * * curl -s http://127.0.0.1:8769/v1/triggers -H 'Content-Type: application/json' \
  -d '{"message":"收集今天的人工智能新闻，挑 5 条最重要的","notify":{"type":"telegram","chat_id":123456789}}'
```

```python
# 传感器侧：自己判断阈值，命中才触发（迟滞也在这边做）
if moisture < 25 and last_alarm_hours_ago() > 6:
    trigger({"message": "土壤湿度偏低，给出浇水建议与用量",
             "data": {"zone": "阳台", "moisture": moisture, "threshold": 25},
             "idempotency_key": f"soil-{today()}"})
```

## 4. 接口清单

### `GET /health`（无鉴权）

```json
{"status": "ok", "service": "omnicrawl-trigger"}
```

### `GET /v1/status`

```json
{
  "data": {
    "ready": true,
    "listen": "127.0.0.1:8769",
    "max_concurrency": 1,
    "running": 0,
    "queued": 2,
    "session_mode": "per_trigger",
    "tools": ["read", "list", "find", "grep", "web_search", "fetcher"],
    "unavailable_reason": null
  }
}
```

### `POST /v1/triggers` — 拉起一个 loop

```json
{
  "message": "土壤湿度偏低，给出浇水建议与用量",
  "data": {"zone": "阳台", "moisture": 18.2, "threshold": 25},
  "title": "土壤湿度告警",
  "session_key": null,
  "workspace": "D:/Proj/omnicrawl仓库",
  "timeout_seconds": 600,
  "wait_seconds": 0,
  "idempotency_key": "soil-2026-11-01",
  "notify": {"type": "telegram", "chat_id": 123456789, "on": "completed"},
  "tags": {"source": "planter", "device": "soil-01"}
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `message` | 是 | 调用方自定义的消息正文（1–20000 字符），原样作为本次回路的用户指令 |
| `data` | 否 | 结构化现场数据（对象，≤ 64000 字符），按固定格式注入消息前部，见 4.1 |
| `title` | 否 | 记录列表与投递标题里显示的名字；缺省取 `message` 前 40 字 |
| `session_key` | 否 | `session_mode = "keyed"` 时的会话键；同名串行且复用同一份会话 |
| `workspace` | 否 | 覆盖工作区；缺省用 `default_workspace`，再缺省用当前工作区 |
| `timeout_seconds` | 否 | 本次回合上限（60–7200），缺省用 `default_timeout_seconds` |
| `wait_seconds` | 否 | `> 0` 时最多阻塞这么久再回（0–300）；0 表示立即返回 `queued` |
| `idempotency_key` | 否 | 去重键（≤ 200 字符，24 小时内有效），见第 6 节 |
| `notify` | 否 | 终态投递目标，见 4.2 |
| `tags` | 否 | 记录用标签（字符串键值），只用于查询与审计，不进模型上下文 |

响应 `202`（排队）或 `200`（`wait_seconds` 内已终态）：

```json
{
  "data": {
    "trigger_id": "trg_7f3c1a2b",
    "state": "queued",
    "title": "土壤湿度告警",
    "session_id": null,
    "created_at": "2026-11-01T08:00:12+08:00",
    "queue_position": 1,
    "duplicate": false
  }
}
```

`duplicate = true` 表示命中了相同 `idempotency_key` 的在跑/已终态记录，此时返回的是**原记录**
（`trigger_id` 与状态都是旧的），调用方可以安全重试。

### `GET /v1/triggers` — 列表

查询参数：`state`（`queued` / `running` / `completed` / `failed` / `cancelled`）、`tag`（`k=v`）、
`limit`（1–100，默认 20）。按创建时间倒序。

### `GET /v1/triggers/{trigger_id}` — 单条状态

```json
{
  "data": {
    "trigger_id": "trg_7f3c1a2b",
    "state": "completed",
    "title": "土壤湿度告警",
    "created_at": "2026-11-01T08:00:12+08:00",
    "started_at": "2026-11-01T08:00:12+08:00",
    "finished_at": "2026-11-01T08:01:03+08:00",
    "session_id": "20261101-080012-1a2b",
    "result": "建议今天傍晚浇一次，约 300 毫升……",
    "error": "",
    "usage": {"model_turns": 4, "tool_calls": 6},
    "notify": {"type": "telegram", "delivered": true, "error": null},
    "tags": {"source": "planter"}
  }
}
```

终态后记录保留 `retention_hours`，过期回收；`state` 未知或已回收返回 `TRIGGER_NOT_FOUND`。

### `GET /v1/triggers/{trigger_id}/events` — SSE

事件名与 `/api/v1/runs/{run_id}/events` **同源**，客户端复用同一套解析：`run.started`、
`assistant.delta`、`status.changed`、`tool.started`、`tool.completed`、`usage.updated`、
`run.completed` / `run.cancelled` / `run.failed`。支持 `Last-Event-ID` 与 `follow`。

无人值守下不会出现 `confirmation.required` 与 `ask_user.required`（工具表里没有会等人工的入口，
审查拒绝直接以 `tool.completed` 的失败结果回到模型）。

`Last-Event-ID` 语义与本地 API 不同的一点：这里只把「游标比已产生的最新事件还大」判为
`EVENT_CURSOR_EXPIRED`（那说明客户端拿的是另一条流）。游标落在保留窗口之前**不是**错误——
早期的增量会被挤出，但终态事件永远是最新一条，照常读得到。游标过期时服务端会先发一条
`stream.error` 事件（带 `earliest_event_id`）再收流。

### `POST /v1/triggers/{trigger_id}/cancel`

置取消标记，跑完收尾线程后进入 `cancelled`。已终态返回原状态（幂等）。

### `GET /v1/triggers/{trigger_id}/transcript`

读取该次触发的会话转录投影（与本地 API 的 `GET /api/v1/sessions/{id}/events` 同形，
按角色聚合后的消息列表），用于回看「那轮到底做了什么、用了哪些工具」。记录过保留期后
该端点随记录一起返回 `TRIGGER_NOT_FOUND`；会话文件本身不删，可在磁盘上长期查阅。

### 状态机

```text
queued ──slot 就绪──▶ running ──▶ completed
  │                     │
  │                     ├──────▶ failed     （模型/工具/服务错误、超时）
  └──服务关闭──────────▶ cancelled
                        └──────▶ cancelled  （cancel 接口）
```

- `queued` 不占 slot；`queue_position` 在入队时给出。
- `running` 期间收到 cancel ⇒ 请求取消并等运行记录进入终态后写 `cancelled`，slot 归还。
- `timeout_seconds` 到期与 cancel 走同一条收尾路径，只是 `error` 记「超时」。
- 服务关闭时：`queued` 全置 `cancelled`，`running` 请求取消；不等待超过 10 秒，超时即杀内核进程树。

## 4.1 注入格式（`message` + `data` 怎么变成一轮输入）

确定性拼装，不做任何模型改写：

```text
[外部触发] 土壤湿度告警
时间：2026-11-01T08:00:12+08:00
来源：omnicrawl 触发接口
数据：
{"device": "soil-01", "moisture": 18.2, "threshold": 25, "zone": "阳台"}

土壤湿度偏低，给出浇水建议与用量
```

- 头部三行固定（标题缺省时省略第一行），`data` 以紧凑单行 JSON 落在「数据：」之后；
- `tags` 不进消息（它是查询元数据，不是现场事实）；
- 空 `data` 时整段「数据：」省略，消息只剩时间/来源与正文。

系统提示词侧自动追加一段固定的无人值守约束（**不可关闭**）：

```text
本次会话由外部事件触发，没有人在终端前：不要调用 ask_user，不要等待人工确认；
拿不到信息就用现有证据给出结论，并在结尾用一句话写明这次做了什么。
```

`[trigger_api] system_prompt_append` 可再追加一段部署专属约束（例如「回复用中文，不超过
300 字」），其余提示词装配与 TUI 完全同源（模板 / AGENTS.md / Skill 索引 / 运行环境）。

## 4.2 结果投递（`notify`）

模型没有「发消息」工具——投递由服务在**终态之后**做，因此不存在「模型忘了发」：

```json
{"type": "telegram", "chat_id": 123456789, "on": "completed"}
```

| 字段 | 说明 |
|---|---|
| `type` | `telegram` / `feishu` / `qqbot` / `none`（只落盘，不投递） |
| `chat_id` / `receive_id` | 目标会话 ID；`telegram` 的 `chat_id` 必须是整数 |
| `on` | `completed`（默认）/ `failed` / `always` |
| `max_chars` | 投递正文上限（200–20000，默认 4000），超出截断并附记录链接 |

- 凭据一律复用已有连接器配置（`[telegram] bot_token`、`[feishu] app_id/app_secret`），
  触发接口不新开一份凭据；
- `telegram` 的 `chat_id` 默认必须落在 `[telegram] allowed_user_ids` 里，否则投递被拒绝并写进
  `notify.error`——本机任何程序都能调这个无鉴权接口，不能让它变成「任意人发任意消息」的跳板；
- **QQ 机器人不能作为出口**：平台自 2025-04 起关闭主动消息，所有下发都必须是对某条入站消息的
  被动回复（带 `msg_id`，且回复次数有额度）。外部触发没有入站消息可回复，因此 `type = "qqbot"`
  会在投递时直接拒绝并说明原因，而不是发一个注定失败的请求；
- 投递失败**不改变触发状态**：`state` 仍是 `completed`，失败原因进 `notify.error` 与日志；
- 正文取终态的 `result`；`failed` / `cancelled` 时取 `error`（不含堆栈、不含工具原始输出）。

## 5. 调用示例

Python（仅标准库，条件自己判）：

```python
import json, urllib.request

def trigger(payload, base="http://127.0.0.1:8769"):
    request = urllib.request.Request(
        f"{base}/v1/triggers",
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        return json.load(response)["data"]

record = trigger({
    "message": "土壤湿度偏低，给出浇水建议与用量",
    "data": {"zone": "阳台", "moisture": 18.2, "threshold": 25},
    "notify": {"type": "telegram", "chat_id": 123456789},
})
print(record["trigger_id"], record["state"])
```

curl（每天早上由 cron 触发）：

```bash
curl -s http://127.0.0.1:8769/v1/triggers \
  -H 'Content-Type: application/json' \
  -d '{"message":"收集今天的人工智能新闻，挑 5 条最重要的，每条一句话",
       "notify":{"type":"telegram","chat_id":123456789}}'
```

PowerShell（同步等结论，CI 场景）：

```powershell
$body = @{
  message = "构建失败了，看看日志、定位原因，给出最小修复建议"
  data = @{ job = "ci-42"; status = "failed"; log_path = "D:/ci/42.log" }
  wait_seconds = 120
} | ConvertTo-Json -Depth 6
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:8769/v1/triggers `
  -ContentType application/json -Body $body | ConvertTo-Json -Depth 8
```

## 6. 语义与安全边界

- **只监听回环、不做鉴权**：任何能访问监听地址的本机程序都能拉起 loop。因此 `host` 只允许回环，
  别放宽到非回环地址——那等于把「本机 shell/文件读写」暴露给整个网络。
- **无人值守审批口径与 `[mqtt] planner = "agent"` 同源**：审批固定为自动审查
  （`approval.mode = review`，危险调用仍被审查闸拦下），人工确认超时压到 5 秒且**等不到即拒绝**；
  没有审查运行期时一律 fail-closed（不因为「没人能批」就放行）。
- **工具白名单**：`[trigger_api] tools` 是白名单而不是黑名单——表外的工具既不声明给模型，宿主也不执行。
  默认只读 + 记忆/知识库 + 联网检索。要放开 `bash` / `write_file`，前提是调用来源可信：
  报文/日志/网页内容会进模型上下文，那是提示注入的入口。
- **`ask_user` 不在表里**：无人值守没有答主。托管提问（`ask_user_custody`）也不启用——
  宁可让模型自己给出结论，也不让决策模型替用户编一个答案。
- **每个 loop 一套独立资源**：一个 slot 一个内核进程 + 一张工具表 + 一次会话；跑完回收。
  并发上限即 slot 数，超出则排队（`queue_limit` 满了 429 `QUEUE_FULL`）。
- **去重**：相同 `idempotency_key` 在 24 小时内只受理一次（排队中、运行中都算），重复请求返回原记录
  并置 `duplicate = true`。传感器抖动、脚本重试、计划任务重复触发都不会把同一个事件跑两遍。
- **超时必收尾**：`timeout_seconds` 到期即请求取消并等运行记录进入终态（不让终态悬挂）；
  收尾后该 slot 才接下一个任务。
- **失败可见**：模型报错、工具失败、投递失败都写进记录与日志；`state = failed` 且 `error` 可读。
  不把「服务故障」伪装成「没事发生」——外部组件据此重试或告警。
- **记录不跨重启恢复**：触发记录、事件流、队列都在服务进程内存里；服务启动时把此前残留的未终态记录
  一并清空（它们所属的进程已经消失，没有任何事实可恢复），新请求从空队列开始。
  **会话转录本身仍然落盘**（`~/.OmniCrawl/trigger/sessions/`），因此「上次那轮到底做了什么」查得到：
  `GET /v1/triggers/{trigger_id}/transcript` 直接读那份 JSONL 投影——记录会丢，回合内容不会丢。
- **`session_mode = "keyed"` 只保证同名串行**：同名 `session_key` 的任务永不并发（后到的排队，
  避免两个 slot 同时往同一份转录里写）。**当前版本不复用历史会话**——每次触发仍然新开会话，
  因此「每天早上追加到同一个每日新闻会话」还没实现，`GET /v1/triggers/{id}/transcript` 各自独立。
  需要跨触发累积上下文时，把状态写进知识库或记忆（那两条本身就是跨会话持久的）。
- **不写用户会话**：会话根是触发接口专用的 `~/.OmniCrawl/trigger/sessions`，与 TUI / 本地 API /
  连接器都不共享，因此无人值守噪音不会进用户的对话历史，`/sessions` 里也翻不到它们
  （要查就走上面的 `transcript` 端点）。
- **不落凭据**：`notify` 只写通道名与目标 ID；投递凭据只在投递那一刻从配置读。

## 7. 与相邻能力的关系

| 能力 | 触发来源 | 出口 | 是否占交互会话 |
|---|---|---|---|
| `[api]` `/runs` | 人（UI / 脚本） | SSE 增量文本 | 是（当前会话） |
| 连接器（telegram / feishu / qqbot） | 人在聊天里说话 | 聊天回复 | 否（各自会话） |
| `[mqtt]` 回路 | 传感器报文 + 确定性规则 | 设备指令（request_id + TTL） | 否 |
| **`[trigger_api]`** | **外部组件自己判定的条件** | **结果 + 消息平台投递** | **否** |

三者都是「外部事件 → 模型 → 动作」，差别在出口：MQTT 出的是**机器可执行的指令**（有动作表、
参数夹取、TTL），触发接口出的是**给人看的结论**（自然语言 + 投递）。因此触发接口不需要动作表，
也无法约束模型必须选择某个动作——它要的是「把这件事办完并说清楚」。

## 8. 错误码

| HTTP | code | 说明 |
|---|---|---|
| 400 | `INVALID_REQUEST` | 缺 `message`、字段类型不符、`data` 超长、`wait_seconds` 越界、`session_key` 在 `per_trigger` 模式下出现 |
| 404 | `TRIGGER_NOT_FOUND` | 记录不存在或已过保留期 |
| 409 | `EVENT_CURSOR_EXPIRED` | 游标超前于该触发的事件流 |
| 429 | `QUEUE_FULL` | 队列已满，调用方稍后重试 |
| 500 | `TRANSCRIPT_UNAVAILABLE` | 会话转录读取失败 |
| 503 | `SERVICE_UNAVAILABLE` | 服务未就绪（内核起不来、正在关闭） |

投递目标不在白名单、通道凭据未配置、QQ 通道不支持主动投递：这些**不是**请求错误（触发本身
已经受理并跑完了），原因写进记录的 `notify.error`。

失败语义由调用方定：接口一律返回**事实**，不为调用方编造结论，也不把失败伪装成成功。

## 9. 实现边界

```
rust/crates/omnicrawl-trigger/
├── src/lib.rs        # 公共导出门面与模块边界
├── src/main.rs       # 二进制：serve / ensure / stop / status
├── src/config.rs     # [trigger_api] 段 → 运行期设置、会话根、内核程序
├── src/app.rs        # 路由、{"data"} / {"error"} 信封、健康检查
├── src/routes.rs     # 七个端点（创建/列表/详情/事件/转录/取消/状态）与入参校验
├── src/request.rs    # 入参校验与 message + data 的确定性注入格式
├── src/records.rs    # 触发记录、事件流、去重、保留期回收
├── src/pool.rs       # slot 池：并发、排队、回合驱动、取消与超时收尾
├── src/notify.rs     # 终态投递（复用连接器 API 与既有凭据）
└── src/server.rs     # 常驻服务生命周期：按记录复用、拉起、停止、PID 与日志
```

配置域在 `rust/crates/omnicrawl-config/src/features/trigger_api.rs`
（`TriggerApiConfig`、`load_trigger_api_configuration` / `save_trigger_api_configuration`）。

**不写第二套 Agent**：一个 loop 就是 `omnicrawl_api::AgentService` 的一个实例——
`AgentService::spawn(options, &kernel)` 起内核、握手、建工具表，`start_run` 起回合，
`events_after` 读事件，`cancel_run` 取消。工具白名单（`RegistryOptions::allowed_tools`）、
自动审查（`ApprovalMode::Review`）、提示词装配（`PromptRuntime`）全部沿用现有那份代码，
口径与 `[mqtt]` 回路一致。

每个触发都新起一个 `AgentService` 而不是复用一个：无人值守的两次触发之间没有任何共享状态，
复用会把上一次的会话（以及它占用的内核进程）带进下一次。代价是一次内核启动，收益是两次触发
之间彻底隔离——这正是「独立 loop」的含义。

## 10. 测试口径

按目录单独跑（`cargo test -p omnicrawl-trigger`），不跑全量构建：

- 段装载：缺段/坏值回落「未启用」，非回环地址、越界并发与超时被拒，工具名去重，
  显式空 `tools` = 一个工具都不给；
- 入参校验：缺 `message`、`data` 超长、`wait_seconds` / `timeout_seconds` 越界、
  `session_key` 在 `per_trigger` 模式下被拒、`notify` 通道与目标校验；
- 注入格式：`message` + `data` 拼装、空 `data` 的省略分支、空标题省略首行；
- 去重：同 `idempotency_key` 第二次返回原记录且 `duplicate = true`；
- 并发：`max_concurrency = 1` 时第二条排队，`queue_limit` 满时 429；
- 取消：排队中的取消立即终态，运行中的取消等实收尾后让出 slot；
- 事件游标：只有超前于流尾才报 `EVENT_CURSOR_EXPIRED`；
- 保留期：终态记录过期回收，活动任务不回收；
- 投递：正文拼装与截断、QQ 通道按平台限制拒绝；
- 服务生命周期：无 PID 文件即未就绪、日志缺监听标记即未就绪、已死 PID 记录被清理。
